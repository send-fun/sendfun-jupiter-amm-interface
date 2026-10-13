//! DEX constant-product pools. Launchpad curves migrate into them.

use jupiter_amm_interface::{
	AccountProvider, Amm, AmmContext, AmmError, AmmLabel, KeyedAccount, Quote,
	QuoteParams, SingleProgramAmm, Swap, SwapAndAccountMetas, SwapParams,
	single_program_amm,
};
use sendfun_sdk::constants::DEX_PROGRAM_ID;
use sendfun_sdk::dex::accounts::Pool;
use sendfun_sdk::dex::trade::{TradeArgs, trade_account_metas, trade_data};
use sendfun_sdk::dex::types::PoolStatus;
use sendfun_sdk::dex::{self, PoolMarket};
use sendfun_sdk::nexus::types::DexFees;
use sendfun_sdk::utils::TradeMode;
use solana_account::ReadableAccount;
use solana_pubkey::Pubkey;

use crate::common;
use crate::market::{Market, MarketAddresses};

/// A send.fun DEX pool: a constant-product market with permanent liquidity.
///
/// Prices the base side off the base vault balance and the quote side off
/// `pool.quote_reserves`. `get_accounts_to_update` includes the base vault.
#[derive(Clone, Debug)]
pub struct SendfunDexAmm {
	pool: Pool,
	fees: Option<DexFees>,
	base_vault_amount: Option<u64>,
	market: Market,
}

single_program_amm!(SendfunDexAmm, DEX_PROGRAM_ID, "send.fun AMM");

impl SendfunDexAmm {
	/// Builds `buy_exact_in` or `sell_exact_in` data, per the tag in `swap`:
	/// the discriminator, then `amount_in` and `min_amount_out` as
	/// little-endian `u64`s. Fails if `swap` is not a send.fun tag.
	pub fn exact_in_data(
		swap: &Swap,
		amount_in: u64,
		min_amount_out: u64,
	) -> Result<Vec<u8>, AmmError> {
		Ok(trade_data(TradeArgs {
			direction: common::direction_from_swap(swap)?.into(),
			mode: TradeMode::ExactIn,
			amount: amount_in,
			limit: min_amount_out,
		}))
	}

	/// Builds `buy_exact_out` or `sell_exact_out` data, per the tag in `swap`:
	/// the discriminator, then `amount_out` and `max_amount_in` as
	/// little-endian `u64`s. Fails if `swap` is not a send.fun tag.
	pub fn exact_out_data(
		swap: &Swap,
		amount_out: u64,
		max_amount_in: u64,
	) -> Result<Vec<u8>, AmmError> {
		Ok(trade_data(TradeArgs {
			direction: common::direction_from_swap(swap)?.into(),
			mode: TradeMode::ExactOut,
			amount: amount_out,
			limit: max_amount_in,
		}))
	}

	fn check_tradable(&self) -> Result<(), AmmError> {
		if self.pool.status == PoolStatus::Active {
			return Ok(());
		}
		Err(AmmError::from("pool is not active"))
	}
}

impl Amm for SendfunDexAmm {
	fn from_keyed_account(
		keyed_account: &KeyedAccount,
		amm_context: &AmmContext,
	) -> Result<Self, AmmError> {
		if keyed_account.account.owner != DEX_PROGRAM_ID {
			return Err(AmmError::from("account is not owned by the DEX"));
		}
		let partner = common::partner_from_params(keyed_account)?;
		let pool = Pool::from_bytes(&keyed_account.account.data)?;
		let market = Market::new(
			&MarketAddresses {
				key: keyed_account.key,
				base_mint: pool.base_mint,
				quote_mint: pool.quote_mint,
				base_vault: pool.base_vault,
				quote_vault: pool.quote_vault,
				platform_config: pool.platform_config,
			},
			partner,
			amm_context.clock_ref.clone(),
		);
		Ok(Self {
			pool,
			fees: None,
			base_vault_amount: None,
			market,
		})
	}

	fn label(&self) -> AmmLabel {
		Self::LABEL
	}

	fn program_id(&self) -> Pubkey {
		Self::PROGRAM_ID
	}

	fn key(&self) -> Pubkey {
		self.market.key
	}

	fn get_reserve_mints(&self) -> Vec<Pubkey> {
		self.market.reserve_mints()
	}

	fn get_accounts_to_update(&self) -> Vec<Pubkey> {
		self.market.accounts_to_update(&[self.market.base_vault])
	}

	/// All or nothing: on error, the last good state stays. A missing partner
	/// config is not an error. It makes the pool unquotable. The pool stays
	/// active.
	fn update(
		&mut self,
		account_provider: impl AccountProvider,
	) -> Result<(), AmmError> {
		let pool = Pool::from_bytes(
			account_provider.try_get(&self.market.key)?.data(),
		)?;
		let (partner_config, read) = self.market.read(&account_provider)?;
		let base_vault_amount = common::token_amount(
			&account_provider.try_get(&self.market.base_vault)?,
		)?;
		self.pool = pool;
		self.fees = partner_config.map(|config| config.dex);
		self.base_vault_amount = Some(base_vault_amount);
		self.market.apply(read, trade_account_metas);
		Ok(())
	}

	fn quote(&self, quote_params: &QuoteParams) -> Result<Quote, AmmError> {
		self.check_tradable()?;
		self.market.quote(quote_params, |request, landing| {
			let (Some(fees), Some(base_vault_amount)) =
				(&self.fees, self.base_vault_amount)
			else {
				return Err(common::not_updated());
			};
			dex::quote(
				&PoolMarket {
					base_vault_amount,
					quote_reserves: self.pool.quote_reserves,
					created_at: self.pool.created_at,
					fees,
					landing,
					creator_fee_bps: self.pool.creator_fee_bps,
				},
				request,
			)
			.map_err(common::market_quote_error)
		})
	}

	fn get_swap_and_account_metas(
		&self,
		swap_params: &SwapParams,
	) -> Result<SwapAndAccountMetas, AmmError> {
		self.check_tradable()?;
		self.market
			.swap_and_account_metas(swap_params, trade_account_metas)
	}

	// A mint's hook can be armed, changed or cleared at any time.
	fn has_dynamic_accounts(&self) -> bool {
		true
	}

	fn supports_exact_out(&self) -> bool {
		true
	}

	// Armed hook programs only. The swap only reads the nexus account, and
	// every runtime has the other programs it invokes.
	fn program_dependencies(&self) -> Vec<(Pubkey, String)> {
		self.market.program_dependencies()
	}

	fn get_accounts_len(&self) -> usize {
		self.market.accounts_len()
	}

	// A pool never closes. `quote` refuses every untradable state.
	fn is_active(&self) -> bool {
		true
	}
}

#[cfg(test)]
mod tests;
