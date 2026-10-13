//! Market state that both `Amm`s share: mints, hooks, quotes and swap
//! account metas.

use std::fmt;
use std::sync::atomic::Ordering;

use jupiter_amm_interface::{
	AccountProvider, AmmError, ClockRef, Quote, QuoteParams,
	SwapAndAccountMetas, SwapMode, SwapParams,
};
use rust_decimal::Decimal;
use sendfun_sdk::constants::NEXUS_PROGRAM_ID;
use sendfun_sdk::nexus::accounts::PartnerConfig;
use sendfun_sdk::nexus::pda::find_partner_config_pda;
use sendfun_sdk::transfer_hook::{
	AccountView, HookSwap, MarketHooks, MintState, Unloaded,
};
use sendfun_sdk::utils::{
	Landing, MarketQuote, QuoteRequest, StandInTrade, TradeAccounts,
	TradeDirection, TradeMode,
};
use solana_account::ReadableAccount;
use solana_instruction::AccountMeta;
use solana_pubkey::Pubkey;

use crate::common;

pub const FIXED_ACCOUNTS_LEN: usize = 17;

pub type TradeMetas = fn(&TradeAccounts) -> Vec<AccountMeta>;

pub struct MarketAddresses {
	pub key: Pubkey,
	pub base_mint: Pubkey,
	pub quote_mint: Pubkey,
	pub base_vault: Pubkey,
	pub quote_vault: Pubkey,
	pub platform_config: Pubkey,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Mints {
	pub base: MintState,
	pub quote: MintState,
}

pub struct MarketRead {
	registered: bool,
	mints: Mints,
	hooks: MarketHooks,
}

#[derive(Clone)]
pub struct Market {
	pub key: Pubkey,
	pub base_mint: Pubkey,
	pub quote_mint: Pubkey,
	pub base_vault: Pubkey,
	pub quote_vault: Pubkey,
	/// Every swap names it. It signs unless it is the default partner.
	pub partner: Pubkey,
	/// `["partner", platform_config, partner]` under nexus.
	pub partner_config: Pubkey,
	/// The partner has no config on the market's platform. The program fails
	/// every swap. `quote` refuses.
	unregistered: bool,
	pub mints: Option<Mints>,
	/// Change only through [`Market::replace_hooks`], which updates
	/// `trailing_len`.
	hooks: MarketHooks,
	/// Cached. Resolving derives a PDA for each extra, which is too slow for
	/// `get_accounts_len`.
	trailing_len: usize,
	clock_ref: ClockRef,
}

impl fmt::Debug for Market {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("Market")
			.field("key", &self.key)
			.field("mints", &self.mints)
			.field("hooks", &self.hooks)
			.finish_non_exhaustive()
	}
}

/// Uses Jupiter's token accounts as given. The program takes any token
/// account of the right mint that `user` owns.
const fn trader(
	market: &TradeAccounts,
	direction: TradeDirection,
	params: &SwapParams<'_, '_>,
) -> TradeAccounts {
	let (user_base_account, user_quote_account) = match direction {
		TradeDirection::Buy => (
			params.destination_token_account,
			params.source_token_account,
		),
		TradeDirection::Sell => (
			params.source_token_account,
			params.destination_token_account,
		),
	};
	TradeAccounts {
		user: params.token_transfer_authority,
		payer: params.payer,
		user_base_account,
		user_quote_account,
		..*market
	}
}

impl Market {
	pub fn new(
		addresses: &MarketAddresses,
		partner: Pubkey,
		clock_ref: ClockRef,
	) -> Self {
		let (partner_config, _) =
			find_partner_config_pda(&addresses.platform_config, &partner);
		Self {
			key: addresses.key,
			base_mint: addresses.base_mint,
			quote_mint: addresses.quote_mint,
			base_vault: addresses.base_vault,
			quote_vault: addresses.quote_vault,
			partner,
			partner_config,
			// `mints: None` refuses every quote and swap until `update`.
			unregistered: false,
			mints: None,
			hooks: MarketHooks::default(),
			trailing_len: 0,
			clock_ref,
		}
	}

	pub fn rebuilt(&self, addresses: &MarketAddresses) -> Self {
		Self::new(addresses, self.partner, self.clock_ref.clone())
	}

	const fn mint_pair(&self) -> [&Pubkey; 2] {
		[&self.base_mint, &self.quote_mint]
	}

	pub fn reserve_mints(&self) -> Vec<Pubkey> {
		vec![self.base_mint, self.quote_mint]
	}

	/// Includes a hook's `ExtraAccountMetaList` only after `update` reads the
	/// hooked mint.
	pub fn accounts_to_update(
		&self,
		program_accounts: &[Pubkey],
	) -> Vec<Pubkey> {
		let mut accounts = vec![
			self.key,
			self.partner_config,
			self.base_mint,
			self.quote_mint,
		];
		accounts.extend_from_slice(program_accounts);
		accounts.extend(self.hooks.validation_accounts());
		accounts
	}

	/// A missing partner config or hook list makes the market unquotable.
	pub fn read(
		&self,
		provider: &impl AccountProvider,
	) -> Result<(Option<PartnerConfig>, MarketRead), AmmError> {
		// Anyone can fund the address. Only nexus writes a config.
		let partner_config = provider
			.get(&self.partner_config)
			.filter(|account| *account.owner() == NEXUS_PROGRAM_ID)
			.map(|account| PartnerConfig::from_bytes(account.data()))
			.transpose()?;
		let mint = |key: &Pubkey| {
			let account = provider.try_get(key)?;
			MintState::load(AccountView {
				owner: account.owner(),
				data: account.data(),
			})
			.map_err(AmmError::from)
		};
		let mints = Mints {
			base: mint(&self.base_mint)?,
			quote: mint(&self.quote_mint)?,
		};
		let plan = self.hooks.plan(
			(&self.base_mint, &mints.base),
			(&self.quote_mint, &mints.quote),
		);
		let lists: Vec<_> = plan
			.accounts()
			.filter_map(|key| provider.get(&key).map(|account| (key, account)))
			.collect();
		let hooks = plan.resolve(|key| {
			lists
				.iter()
				.find(|(list, _)| list == key)
				.map(|(_, account)| AccountView {
					owner: account.owner(),
					data: account.data(),
				})
		});
		let registered = partner_config.is_some();
		Ok((
			partner_config,
			MarketRead {
				registered,
				mints,
				hooks,
			},
		))
	}

	/// Screens the hooks against a buy and a sell. A hook that no trader can
	/// carry makes the market unquotable.
	pub fn apply(&mut self, read: MarketRead, fixed: TradeMetas) {
		let trade = self.stand_in_trade(read.mints.quote.token_program);
		let hooks = read.hooks.screen(&trade, &fixed(&trade));
		self.unregistered = !read.registered;
		self.mints = Some(read.mints);
		self.replace_hooks(hooks);
	}

	#[cfg(test)]
	pub const fn hooks(&self) -> &MarketHooks {
		&self.hooks
	}

	fn replace_hooks(&mut self, hooks: MarketHooks) {
		self.hooks = hooks;
		self.trailing_len = self.measure_trailing();
	}

	#[cfg(test)]
	pub fn prime_hooks(&mut self, hooks: MarketHooks) {
		self.replace_hooks(hooks);
	}

	#[cfg(test)]
	pub const fn prime(&mut self, mints: &Mints) {
		self.unregistered = false;
		self.mints = Some(*mints);
	}

	fn stand_in_trade(&self, quote_token_program: Pubkey) -> StandInTrade {
		TradeAccounts {
			market: self.key,
			base_mint: self.base_mint,
			quote_mint: self.quote_mint,
			base_vault: self.base_vault,
			quote_vault: self.quote_vault,
			partner: self.partner,
			partner_config: self.partner_config,
			quote_token_program,
			..TradeAccounts::default()
		}
		.with_stand_in_trader()
	}

	fn paused(&self) -> bool {
		self.mints
			.is_some_and(|mints| mints.base.paused || mints.quote.paused)
	}

	/// `quote` and `swap_and_account_metas` must refuse on the same grounds.
	/// Every refusal here can clear on a later `update`.
	fn check_tradable(&self, unloaded: Unloaded) -> Result<&Mints, AmmError> {
		let mints = self.mints.as_ref().ok_or_else(common::not_updated)?;
		if self.unregistered {
			return Err(AmmError::Custom(format!(
				"partner {} is not registered on this market's platform",
				self.partner
			)));
		}
		self.hooks
			.check_routable(unloaded)
			.map_err(common::hook_error)?;
		if self.paused() {
			return Err(AmmError::from("a mint on this market is paused"));
		}
		Ok(mints)
	}

	/// The fixed accounts plus the longest merged trailing list of a buy or a
	/// sell. An account that both hooks share counts once. An unloaded list
	/// counts as its stand-ins. An undercount truncates the swap.
	#[expect(
		clippy::arithmetic_side_effects,
		reason = "both are account counts bounded by the 64-meta instruction limit"
	)]
	pub const fn accounts_len(&self) -> usize {
		FIXED_ACCOUNTS_LEN + self.trailing_len
	}

	fn measure_trailing(&self) -> usize {
		// Hooks never read the token program.
		let trade = self.stand_in_trade(Pubkey::default());
		[TradeDirection::Buy, TradeDirection::Sell]
			.into_iter()
			.filter_map(|direction| {
				let swap = HookSwap {
					trade: &trade,
					direction,
					fixed: &[],
				};
				self.hooks.swap_accounts(swap, Unloaded::StandIn).ok()
			})
			.map(|trailing| trailing.len())
			.max()
			.unwrap_or_default()
	}

	/// Names each hook program by its address. The test kit loads
	/// `tests/fixtures/<address>.so`.
	pub fn program_dependencies(&self) -> Vec<(Pubkey, String)> {
		self.hooks
			.programs()
			.into_iter()
			.map(|program| (program, program.to_string()))
			.collect()
	}

	pub fn quote(
		&self,
		params: &QuoteParams,
		price: impl FnOnce(QuoteRequest, Landing) -> Result<MarketQuote, AmmError>,
	) -> Result<Quote, AmmError> {
		let mints = self.check_tradable(Unloaded::Refuse)?;
		let request = QuoteRequest {
			direction: common::direction_of_pair(
				self.mint_pair(),
				&params.input_mint,
				&params.output_mint,
			)?,
			mode: match params.swap_mode {
				SwapMode::ExactIn => TradeMode::ExactIn,
				SwapMode::ExactOut => TradeMode::ExactOut,
			},
			amount: params.amount,
		};
		let epoch = self.clock_ref.epoch.load(Ordering::Relaxed);
		let quote = price(
			request,
			Landing {
				base_fee: mints.base.fee_at(epoch),
				quote_fee: mints.quote.fee_at(epoch),
				unix_timestamp: self
					.clock_ref
					.unix_timestamp
					.load(Ordering::Relaxed),
			},
		)?;
		// The program refuses an exact-out buy past the supply left.
		if request.mode == TradeMode::ExactOut && quote.supply_capped {
			return Err(AmmError::from(
				"fill exceeds the supply left on the curve",
			));
		}
		if quote.out_amount == 0 {
			return Err(AmmError::from("swap delivers nothing"));
		}
		Ok(Quote {
			in_amount: quote.in_amount,
			out_amount: quote.out_amount,
			fee_amount: quote.fee,
			fee_mint: self.quote_mint,
			fee_pct: Decimal::new(i64::from(quote.fee_bps), 4),
		})
	}

	pub fn swap_and_account_metas(
		&self,
		params: &SwapParams<'_, '_>,
		fixed: TradeMetas,
	) -> Result<SwapAndAccountMetas, AmmError> {
		let direction = common::direction_of_pair(
			self.mint_pair(),
			&params.source_mint,
			&params.destination_mint,
		)?;
		let unloaded =
			common::unloaded(params.missing_dynamic_accounts_as_default);
		let mints = self.check_tradable(unloaded)?;
		let trade = trader(
			&self.stand_in_trade(mints.quote.token_program),
			direction,
			params,
		);
		let mut account_metas = fixed(&trade);
		let swap = HookSwap {
			trade: &trade,
			direction,
			fixed: &account_metas,
		};
		let trailing = self
			.hooks
			.swap_accounts(swap, unloaded)
			.map_err(common::hook_error)?;
		account_metas.extend(trailing);
		Ok(SwapAndAccountMetas {
			swap: common::to_swap(direction),
			account_metas,
		})
	}
}
