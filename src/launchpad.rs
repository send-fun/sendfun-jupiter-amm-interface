//! Launchpad bonding curves, routable while `Funding`.

use jupiter_amm_interface::{
	AccountProvider, Amm, AmmContext, AmmError, AmmLabel, KeyedAccount, Quote,
	QuoteParams, SingleProgramAmm, Swap, SwapAndAccountMetas, SwapParams,
	single_program_amm,
};
use sendfun_sdk::constants::LAUNCHPAD_PROGRAM_ID;
use sendfun_sdk::launchpad::accounts::BondingCurve;
use sendfun_sdk::launchpad::trade::{
	TradeArgs, trade_account_metas, trade_data,
};
use sendfun_sdk::launchpad::types::BondingCurveStatus;
use sendfun_sdk::launchpad::{self, CurveMarket};
use sendfun_sdk::nexus::types::LaunchpadFees;
use sendfun_sdk::utils::TradeMode;
use solana_account::ReadableAccount;
use solana_pubkey::Pubkey;

use crate::common;
use crate::market::{Market, MarketAddresses};

/// A send.fun launchpad bonding curve.
///
/// For a buy past the supply left, exact-in quotes the capped fill and
/// exact-out does not quote. The curve quotes only while `Funding`. It is
/// inactive once `Completed` or `Migrated`. A closed curve account stays active
/// but does not quote. After migration, the token trades on
/// [`SendfunDexAmm`](crate::SendfunDexAmm).
#[derive(Clone, Debug)]
pub struct SendfunLaunchpadAmm {
	curve: BondingCurve,
	/// At the last `update`, the curve account was missing or not owned by
	/// the launchpad.
	closed: bool,
	fees: Option<LaunchpadFees>,
	market: Market,
}

single_program_amm!(
	SendfunLaunchpadAmm,
	LAUNCHPAD_PROGRAM_ID,
	"send.fun Launchpad"
);

impl SendfunLaunchpadAmm {
	/// Builds `buy_exact_in` or `sell_exact_in` data, per the tag in `swap`:
	/// the discriminator, then `amount_in` and `min_amount_out` as
	/// little-endian `u64`s. Fails if `swap` is not a send.fun tag.
	pub fn exact_in_data(
		swap: &Swap,
		amount_in: u64,
		min_amount_out: u64,
	) -> Result<Vec<u8>, AmmError> {
		Ok(trade_data(TradeArgs {
			direction: common::direction_from_swap(swap)?,
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
			direction: common::direction_from_swap(swap)?,
			mode: TradeMode::ExactOut,
			amount: amount_out,
			limit: max_amount_in,
		}))
	}

	/// Curve checks only. [`Market::check_tradable`] has the market checks.
	fn check_tradable(&self) -> Result<(), AmmError> {
		if self.closed {
			return Err(AmmError::from("bonding curve is closed"));
		}
		if self.curve.status != BondingCurveStatus::Funding {
			return Err(AmmError::from("bonding curve is not funding"));
		}
		Ok(())
	}
}

const fn market_addresses(
	key: Pubkey,
	curve: &BondingCurve,
) -> MarketAddresses {
	MarketAddresses {
		key,
		base_mint: curve.base_mint,
		quote_mint: curve.quote_mint,
		base_vault: curve.base_vault,
		quote_vault: curve.quote_vault,
		platform_config: curve.platform_config,
	}
}

impl Amm for SendfunLaunchpadAmm {
	fn from_keyed_account(
		keyed_account: &KeyedAccount,
		amm_context: &AmmContext,
	) -> Result<Self, AmmError> {
		if keyed_account.account.owner != LAUNCHPAD_PROGRAM_ID {
			return Err(AmmError::from(
				"account is not owned by the launchpad",
			));
		}
		let partner = common::partner_from_params(keyed_account)?;
		let curve = BondingCurve::from_bytes(&keyed_account.account.data)?;
		let market = Market::new(
			&market_addresses(keyed_account.key, &curve),
			partner,
			amm_context.clock_ref.clone(),
		);
		Ok(Self {
			curve,
			closed: false,
			fees: None,
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
		self.market.accounts_to_update(&[])
	}

	/// All or nothing: on error, the last good state stays. These are not
	/// errors:
	///
	/// - A curve account that is missing or not owned by the launchpad marks
	///   the curve closed.
	/// - A missing partner config makes the curve unquotable. The curve stays
	///   active.
	fn update(
		&mut self,
		account_provider: impl AccountProvider,
	) -> Result<(), AmmError> {
		let Some(account) = account_provider
			.get(&self.market.key)
			.filter(|account| *account.owner() == LAUNCHPAD_PROGRAM_ID)
		else {
			self.closed = true;
			return Ok(());
		};
		let curve = BondingCurve::from_bytes(account.data())?;
		// Build the new market aside: a failed read must not pair it with the
		// old curve. Only a relaunch of a recycled mint changes the platform.
		let relaunched = (curve.platform_config != self.curve.platform_config)
			.then(|| {
				self.market
					.rebuilt(&market_addresses(self.market.key, &curve))
			});
		let (partner_config, read) = relaunched
			.as_ref()
			.unwrap_or(&self.market)
			.read(&account_provider)?;
		if let Some(market) = relaunched {
			self.market = market;
		}
		self.curve = curve;
		self.closed = false;
		self.fees = partner_config.map(|config| config.launchpad);
		self.market.apply(read, trade_account_metas);
		Ok(())
	}

	fn quote(&self, quote_params: &QuoteParams) -> Result<Quote, AmmError> {
		self.check_tradable()?;
		self.market.quote(quote_params, |request, landing| {
			let fees = self.fees.as_ref().ok_or_else(common::not_updated)?;
			launchpad::quote(
				&CurveMarket {
					virtual_base_reserves: self.curve.virtual_base_reserves,
					virtual_quote_reserves: self.curve.virtual_quote_reserves,
					real_base_reserves: self.curve.real_base_reserves,
					real_quote_reserves: self.curve.real_quote_reserves,
					created_at: self.curve.created_at,
					fees,
					landing,
					creator_fee_bps: self.curve.creator_fee_bps,
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

	// False only for a `Completed` or `Migrated` curve: no instruction leaves
	// either. `quote` refuses every other untradable state, a closed account
	// included.
	fn is_active(&self) -> bool {
		!matches!(
			self.curve.status,
			BondingCurveStatus::Completed | BondingCurveStatus::Migrated
		)
	}
}

#[cfg(test)]
mod tests;
