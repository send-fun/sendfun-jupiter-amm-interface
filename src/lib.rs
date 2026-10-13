//! Jupiter routing for the send.fun programs on Solana.
//!
//! - [`SendfunLaunchpadAmm`]: launchpad bonding curves. Label
//!   `send.fun Launchpad`.
//! - [`SendfunDexAmm`]: DEX pools that curves migrate into. Label
//!   `send.fun AMM`.
//!
//! Both implement [`jupiter_amm_interface::Amm`]. Their
//! `get_swap_and_account_metas` returns
//! `Swap::Placeholder { data: Some(vec![tag]) }`, where `tag` is
//! [`SWAP_TAG_BUY`] or [`SWAP_TAG_SELL`].
//!
//! Each quote and swap prices off the partner's `PartnerConfig`.
//! `KeyedAccount.params` must hold `{"partner": "<base58 pubkey>"}`. There is
//! no fallback: `from_keyed_account` fails without it.

mod common;
mod dex;
mod launchpad;
mod market;
#[cfg(test)]
mod test_support;

// Dev-dependencies that only `tests/` uses. Every test target links them.
#[cfg(test)]
use {
	bincode as _, jupiter_amm_test_kit as _, litesvm as _,
	solana_compute_budget_interface as _, solana_keypair as _,
	solana_signer as _, solana_transaction as _,
};

pub use common::{SWAP_TAG_BUY, SWAP_TAG_SELL};
pub use dex::SendfunDexAmm;
pub use launchpad::SendfunLaunchpadAmm;
