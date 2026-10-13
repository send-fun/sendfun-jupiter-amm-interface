//! Quote parity against the send.fun programs.
//!
//! Each test loads a mainnet snapshot from `tests/fixtures/accounts/<market>/`
//! and runs the program binaries in `LiteSVM`. Each binary has the name of its
//! `Amm::label()`: `tests/fixtures/send.fun_launchpad.so` (launchpad) and
//! `tests/fixtures/send.fun_amm.so` (DEX). Both are the kit's dumps of the mainnet
//! programs. A transfer hook loads from `tests/fixtures/<program address>.so`,
//! the name that `Amm::program_dependencies` gives it.
//!
//! - **Kit tests** run through `jupiter-amm-test-kit`. They quote each swap,
//!   run the program's exact-in trade, and assert that the destination delta
//!   equals `quote.out_amount`.
//! - **Native tests** run [`assert_native`] in both modes. It funds the source
//!   with exactly `quote.in_amount` and bounds each leg by the quote. The
//!   source must drain to zero. The destination must rise by exactly
//!   `quote.out_amount`.
//! - **Derived states** are the committed snapshot with one field moved to a
//!   state the chain can reach (see [`derive`]): the clock inside a fee-decay
//!   window, a curve's supply or quote at the edge, DEX decay turned on,
//!   reserves booked above the vault, a hook armed on the quote mint, a
//!   partner registered on the platform, a market's own creator rate, a curve
//!   after a program buy at a creator rate. Past each edge, both the adapter
//!   and the program must refuse.
//! - **Synthetic states** put a hook, a transfer fee or a pause on a
//!   snapshot's base mint. They cover a DEX base mint that the DEX authority
//!   lists. A launchpad base mint never has these extensions.
//!
//! ```sh
//! cargo test --test parity
//! # Re-snapshot every market and dump both programs again from mainnet.
//! REFRESH=1 RPC=<mainnet-url> cargo test --test parity -- --test-threads=1
//! ```
//!
//! Under `REFRESH`, only the tests that snapshot a market run. The kit dumps a
//! program only where no file exists. Each of these tests first deletes its
//! program's binary (see `ProgramBinary::redump_on_refresh`). Without `RPC`,
//! no test deletes a binary.
//!
//! A refresh changes every value taken from a snapshot. Re-derive each one from
//! its failing assertion: `CURVE_WSOL_ONE_SOL_FILL`, `PRELWG_TRANSFER_FEE`,
//! each fee that includes the pool's creator rate, and the other `out_amount`s
//! of `a_partners_own_schedule_prices_the_curve` and
//! `a_partners_own_schedule_prices_the_pool`.

use std::collections::BTreeMap;
use std::error::Error;
use std::fs::File;
use std::io::{ErrorKind, Read};
use std::path::{Path, PathBuf};
use std::sync::Once;
use std::sync::atomic::{AtomicUsize, Ordering};

use jupiter_amm_interface::{
	AccountProvider, Amm, AmmContext, AmmError, ClockRef, FeeMode,
	KeyedAccount, QuoteParams, Swap, SwapMode, SwapParams,
};
use jupiter_amm_test_kit::{PoolTest, assert_pool_parity};
use litesvm::LiteSVM;
use rust_decimal::Decimal;
use sendfun_jupiter::{SendfunDexAmm, SendfunLaunchpadAmm};
use sendfun_sdk::constants::DEFAULT_PARTNER;
use sendfun_sdk::dex::accounts::Pool;
use sendfun_sdk::dex::pda::find_pool_pda;
use sendfun_sdk::launchpad::accounts::BondingCurve;
use sendfun_sdk::math::amm::MintFee;
use sendfun_sdk::nexus::accounts::PartnerConfig;
use sendfun_sdk::nexus::pda::find_partner_config_pda;
use sendfun_sdk::nexus::types::{DexFees, LaunchpadFees};
use sendfun_sdk::transfer_fee::mint_fee_at_epoch;
use sendfun_sdk::transfer_hook::EXECUTE_DISCRIMINATOR;
use sendfun_sdk::utils::find_associated_token_pda;
use serde_json::json;
use solana_account::{Account, ReadableAccount};
use solana_clock::Clock;
use solana_compute_budget_interface::ComputeBudgetInstruction;
use solana_instruction::{AccountMeta, Instruction};
use solana_keypair::Keypair;
use solana_pubkey::{Pubkey, pubkey};
use solana_signer::Signer;
use solana_transaction::Transaction;
use spl_discriminator::{ArrayDiscriminator, SplDiscriminate};
use spl_tlv_account_resolution::account::ExtraAccountMeta;
use spl_tlv_account_resolution::seeds::Seed;
use spl_tlv_account_resolution::state::ExtraAccountMetaList;

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

const WSOL: Pubkey = pubkey!("So11111111111111111111111111111111111111112");
const CLOCK_SYSVAR: Pubkey =
	pubkey!("SysvarC1ock11111111111111111111111111111111");
const ATA_PROGRAM: Pubkey =
	pubkey!("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");
const SYSTEM_PROGRAM: Pubkey = pubkey!("11111111111111111111111111111111");
const JUPITER_PROGRAM: Pubkey =
	pubkey!("JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4");

/// The default partner's config on the sendfun platform. It prices every
/// market below.
const DEFAULT_PARTNER_CONFIG: Pubkey =
	pubkey!("B77P3Hvj9xLi4tcFtyjNcf6NjhkQrfJvLkz3nVZnBzec");

/// Launchpad curve quoted in WSOL.
const CURVE_WSOL: Pubkey =
	pubkey!("Eq52yham9BNMbz2pCcqpSFUNAfJZKJFPNEhqHaEdyy7L");
const CURVE_WSOL_BASE: Pubkey =
	pubkey!("Sg18DfCS5Ne8JujHcCsidTctSacmAhRXKM6BCG4send");

/// Launchpad curve quoted in an SPL token.
const CURVE_SPL: Pubkey =
	pubkey!("HafB4R3M8KMmLZxb44qmL3TkxxWNCNMqXr4Zmg2oJMVo");
const CURVE_SPL_BASE: Pubkey =
	pubkey!("8xArgo4qRVBhvYTsL1ECkG6Va5QWhcGCvdPPfU4asend");
const CURVE_SPL_QUOTE: Pubkey =
	pubkey!("HzwqbKZw8HxMN6bF2yFZNrht3c2iXXzpKcFu7uBEDKtr");

/// Launchpad curve quoted in [`PRELWG`], a Token-2022 mint with a transfer
/// fee.
const CURVE_PRELWG: Pubkey =
	pubkey!("ECxRrNU6Ggd73c3AD7UPd6ijfJYdULWFT2jDJZu5gzcd");
const CURVE_PRELWG_BASE: Pubkey =
	pubkey!("8kNhVKSuv5fEXiiSXGfVKd264dbqEP1veN6fZmXcsend");
const PRELWG: Pubkey = pubkey!("PreLWGkkeqG1s4HEfFZSy9moCrJ7btsHuUtfcCeoRua");

/// DEX pool seeded by a migrated curve, quoted in WSOL.
const POOL_WSOL: Pubkey =
	pubkey!("ChJu4A7Gow1DDsWdgHvcpJtxDQQZvA5tavWKn8zZ9kY5");
const POOL_WSOL_BASE: Pubkey =
	pubkey!("eRL5z1LmGvJxXj8MxFZSNXsukVgF23egb6ZxXnRsend");

/// `spl-transfer-hook-example` 0.6.0, built with `--no-default-features`.
const HOOK_PROGRAM: Pubkey =
	pubkey!("8mTbJtVKaLQ9pVgEMkjgu2N3EvBKyMXBxkPZvBW8d5Qk");
const RENT_SYSVAR: Pubkey =
	pubkey!("SysvarRent111111111111111111111111111111111");

/// The log line of a failed Token-2022 instruction.
const TOKEN_2022_FAILED: &str =
	"Program TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb failed";

const TOKEN_2022_PROGRAM: Pubkey =
	pubkey!("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");

/// Token-2022 extension types.
const TRANSFER_FEE_CONFIG_EXTENSION: u16 = 1;
const TRANSFER_FEE_AMOUNT_EXTENSION: u16 = 2;
const TRANSFER_HOOK_EXTENSION: u16 = 14;
const TRANSFER_HOOK_ACCOUNT_EXTENSION: u16 = 15;
const PAUSABLE_CONFIG_EXTENSION: u16 = 26;
/// The TLV region of a Token-2022 mint or account starts after its type byte.
const TLV_START: usize = 166;
/// The older transfer fee entry, 10%. The snapshot's epoch is past it.
const RETIRED_TRANSFER_FEE_BPS: u16 = 1_000;

/// Seconds after creation, for the decay-window snapshots.
const DECAY_ELAPSED_SECONDS: i64 = 5;

const FIXED_ACCOUNTS: usize = 17;

/// The base that a 1 SOL buy takes from the snapshot of the WSOL curve.
const CURVE_WSOL_ONE_SOL_FILL: u64 = 9_669_638_840_005;

/// `LaunchpadError::SlippageExceeded`: a fill short of `min_amount_out`.
const LAUNCHPAD_SLIPPAGE_EXCEEDED: &str = "Custom(6001)";
/// `LaunchpadError::InsufficientLiquidity`: a sell past `real_quote_reserves`,
/// or an exact-out buy past `real_base_reserves`.
const LAUNCHPAD_INSUFFICIENT_LIQUIDITY: &str = "Custom(7100)";
/// `Unauthorized` in both programs: a partner other than the default did not
/// sign.
const PARTNER_UNAUTHORIZED: &str = "Custom(7001)";
/// The program's error for a `partner_config` slot that holds no account:
/// the partner has no config on the market's platform.
const PARTNER_CONFIG_UNINITIALIZED: &str =
	"InstructionError(1, UninitializedAccount)";

fn wsol_curve_swaps(test: PoolTest) -> PoolTest {
	test.add_swap(WSOL, CURVE_WSOL_BASE, 10_000_000)
		.add_swap(WSOL, CURVE_WSOL_BASE, 500_000_000)
		.add_swap(WSOL, CURVE_WSOL_BASE, 3_000_000_000)
		.add_swap(CURVE_WSOL_BASE, WSOL, 1_000_000_000)
		.add_swap(CURVE_WSOL_BASE, WSOL, 5_000_000_000_000)
}

fn pool_swaps(test: PoolTest) -> PoolTest {
	test.add_swap(WSOL, POOL_WSOL_BASE, 10_000_000)
		.add_swap(WSOL, POOL_WSOL_BASE, 1_000_000_000)
		.add_swap(WSOL, POOL_WSOL_BASE, 10_000_000_000)
		.add_swap(POOL_WSOL_BASE, WSOL, 1_000_000_000)
		.add_swap(POOL_WSOL_BASE, WSOL, 10_000_000_000_000)
}

const WSOL_CURVE_NATIVE: [(SwapMode, Pubkey, Pubkey, u64); 8] = [
	(SwapMode::ExactIn, WSOL, CURVE_WSOL_BASE, 10_000_000),
	(SwapMode::ExactIn, WSOL, CURVE_WSOL_BASE, 3_000_000_000),
	(SwapMode::ExactIn, CURVE_WSOL_BASE, WSOL, 1_000_000_000),
	(SwapMode::ExactIn, CURVE_WSOL_BASE, WSOL, 5_000_000_000_000),
	(SwapMode::ExactOut, WSOL, CURVE_WSOL_BASE, 1_000_000_000),
	(
		SwapMode::ExactOut,
		WSOL,
		CURVE_WSOL_BASE,
		20_000_000_000_000,
	),
	(SwapMode::ExactOut, CURVE_WSOL_BASE, WSOL, 10_000_000),
	(SwapMode::ExactOut, CURVE_WSOL_BASE, WSOL, 1_000_000_000),
];

const POOL_NATIVE: [(SwapMode, Pubkey, Pubkey, u64); 8] = [
	(SwapMode::ExactIn, WSOL, POOL_WSOL_BASE, 10_000_000),
	(SwapMode::ExactIn, WSOL, POOL_WSOL_BASE, 10_000_000_000),
	(SwapMode::ExactIn, POOL_WSOL_BASE, WSOL, 1_000_000_000),
	(SwapMode::ExactIn, POOL_WSOL_BASE, WSOL, 10_000_000_000_000),
	(SwapMode::ExactOut, WSOL, POOL_WSOL_BASE, 1_000_000_000),
	(SwapMode::ExactOut, WSOL, POOL_WSOL_BASE, 1_000_000_000_000),
	(SwapMode::ExactOut, POOL_WSOL_BASE, WSOL, 10_000_000),
	(SwapMode::ExactOut, POOL_WSOL_BASE, WSOL, 1_000_000_000),
];

#[test]
fn launchpad_wsol_curve() {
	LAUNCHPAD_BINARY.redump_on_refresh();
	let test = wsol_curve_swaps(pool_test(CURVE_WSOL));
	// `min_amount_out` is 0. The kit asserts the exact output.
	assert_pool_parity::<SendfunLaunchpadAmm>(&test, |swap, in_amount| {
		SendfunLaunchpadAmm::exact_in_data(swap, in_amount, 0).unwrap()
	});
	assert_native::<SendfunLaunchpadAmm>(&NativeTest::new(
		CURVE_WSOL,
		fixtures_dir(&CURVE_WSOL),
		&WSOL_CURVE_NATIVE,
	))
	.unwrap();
}

#[test]
fn launchpad_spl_quoted_curve() {
	LAUNCHPAD_BINARY.redump_on_refresh();
	let test = pool_test(CURVE_SPL)
		.add_swap(CURVE_SPL_QUOTE, CURVE_SPL_BASE, 1_000_000)
		.add_swap(CURVE_SPL_QUOTE, CURVE_SPL_BASE, 25_000_000)
		.add_swap(CURVE_SPL_BASE, CURVE_SPL_QUOTE, 1_000_000_000)
		.add_swap(CURVE_SPL_BASE, CURVE_SPL_QUOTE, 2_000_000_000_000);
	assert_pool_parity::<SendfunLaunchpadAmm>(&test, |swap, in_amount| {
		SendfunLaunchpadAmm::exact_in_data(swap, in_amount, 0).unwrap()
	});
	assert_native::<SendfunLaunchpadAmm>(&NativeTest::new(
		CURVE_SPL,
		fixtures_dir(&CURVE_SPL),
		&[
			(
				SwapMode::ExactIn,
				CURVE_SPL_QUOTE,
				CURVE_SPL_BASE,
				25_000_000,
			),
			(
				SwapMode::ExactIn,
				CURVE_SPL_BASE,
				CURVE_SPL_QUOTE,
				2_000_000_000_000,
			),
			(
				SwapMode::ExactOut,
				CURVE_SPL_QUOTE,
				CURVE_SPL_BASE,
				1_000_000_000,
			),
			(
				SwapMode::ExactOut,
				CURVE_SPL_QUOTE,
				CURVE_SPL_BASE,
				500_000_000_000,
			),
			(
				SwapMode::ExactOut,
				CURVE_SPL_BASE,
				CURVE_SPL_QUOTE,
				1_000_000,
			),
		],
	))
	.unwrap();
}

#[test]
fn dex_wsol_pool() {
	DEX_BINARY.redump_on_refresh();
	let test = pool_swaps(pool_test(POOL_WSOL));
	assert_pool_parity::<SendfunDexAmm>(&test, |swap, in_amount| {
		SendfunDexAmm::exact_in_data(swap, in_amount, 0).unwrap()
	});
	assert_native::<SendfunDexAmm>(&NativeTest::new(
		POOL_WSOL,
		fixtures_dir(&POOL_WSOL),
		&POOL_NATIVE,
	))
	.unwrap();
}

// The quote and the program read one pinned clock. The premium must match to
// the unit.

#[test]
fn launchpad_wsol_curve_inside_the_decay_window() {
	// `REFRESH` re-snapshots live state. A derived state has nothing to
	// refresh.
	if std::env::var_os("REFRESH").is_some() {
		return;
	}
	let dir = derive(&CURVE_WSOL, "curve-decay", |fixtures| {
		let created_at = fixtures.curve(&CURVE_WSOL)?.created_at;
		fixtures.set_clock(
			created_at
				.checked_add(DECAY_ELAPSED_SECONDS)
				.ok_or("clock overflow")?,
		)
	})
	.unwrap();

	// The default partner's fee decays from 5_000 bps to its 200 bps base over
	// 20 seconds: 200 + 4_800 * (15 / 20)^2 = 2_900 bps at 5 seconds.
	let quote = Fixtures::load(&dir)
		.unwrap()
		.amm::<SendfunLaunchpadAmm>(&CURVE_WSOL)
		.unwrap()
		.quote(&exact_in(WSOL, CURVE_WSOL_BASE, 10_000_000))
		.unwrap();
	assert_eq!(quote.fee_pct, Decimal::new(2_900, 4));

	let mut test = wsol_curve_swaps(pool_test(CURVE_WSOL));
	test.fixtures_dir.clone_from(&dir);
	assert_pool_parity::<SendfunLaunchpadAmm>(&test, |swap, in_amount| {
		SendfunLaunchpadAmm::exact_in_data(swap, in_amount, 0).unwrap()
	});
	assert_native::<SendfunLaunchpadAmm>(&NativeTest::new(
		CURVE_WSOL,
		dir,
		&WSOL_CURVE_NATIVE,
	))
	.unwrap();
}

/// Turns on DEX decay for the default partner, 20 seconds from `5_000` bps as
/// on the launchpad, and moves the clock into the window.
#[test]
fn dex_wsol_pool_inside_a_decay_window() {
	if std::env::var_os("REFRESH").is_some() {
		return;
	}
	let dir = derive(&POOL_WSOL, "pool-decay", |fixtures| {
		fixtures.edit_partner_config(&DEFAULT_PARTNER_CONFIG, |config| {
			config.dex.fee_decay_seconds = 20;
			config.dex.fee_decay_start_bps = 5_000;
		})?;
		let created_at = fixtures.pool(&POOL_WSOL)?.created_at;
		fixtures.set_clock(
			created_at
				.checked_add(DECAY_ELAPSED_SECONDS)
				.ok_or("clock overflow")?,
		)
	})
	.unwrap();

	// 15 bps base: 5 protocol plus the pool's creator rate of 10.
	// 15 + ceil(4_985 * (15 / 20)^2) = 2_820 bps.
	let quote = Fixtures::load(&dir)
		.unwrap()
		.amm::<SendfunDexAmm>(&POOL_WSOL)
		.unwrap()
		.quote(&exact_in(WSOL, POOL_WSOL_BASE, 10_000_000))
		.unwrap();
	assert_eq!(quote.fee_pct, Decimal::new(2_820, 4));

	let mut test = pool_swaps(pool_test(POOL_WSOL));
	test.fixtures_dir.clone_from(&dir);
	assert_pool_parity::<SendfunDexAmm>(&test, |swap, in_amount| {
		SendfunDexAmm::exact_in_data(swap, in_amount, 0).unwrap()
	});
	assert_native::<SendfunDexAmm>(&NativeTest::new(
		POOL_WSOL,
		dir,
		&POOL_NATIVE,
	))
	.unwrap();
}

// A market charges its own `creator_fee_bps`. The route partner's
// `max_creator_fee_bps` limits that rate at creation only. A market's rate can
// be above the max.

/// The default partner charges 200 bps protocol. Its fee decays from
/// `5_000` bps over 20 seconds. The test sets its max to 50 bps, between the
/// curve's rates. 5 seconds in, the fee is the base rate plus
/// `ceil((5_000 - base) * (15 / 20)^2)`.
#[test]
fn launchpad_wsol_curve_charges_its_own_creator_rate() {
	if std::env::var_os("REFRESH").is_some() {
		return;
	}
	// `(creator_fee_bps, seconds after creation, fee bps)`. `None` keeps the
	// snapshot's clock, past the decay window.
	for (creator_fee_bps, elapsed, fee_bps) in [
		(0, None, 200),
		(0, Some(DECAY_ELAPSED_SECONDS), 2_900),
		(30, None, 230),
		(30, Some(DECAY_ELAPSED_SECONDS), 2_914),
		(80, None, 280),
		(80, Some(DECAY_ELAPSED_SECONDS), 2_935),
	] {
		let dir = derive(&CURVE_WSOL, "curve-creator-rate", |fixtures| {
			fixtures.edit_partner_config(
				&DEFAULT_PARTNER_CONFIG,
				|config| {
					config.launchpad.max_creator_fee_bps = 50;
				},
			)?;
			fixtures.edit_curve(&CURVE_WSOL, |curve| {
				curve.creator_fee_bps = creator_fee_bps;
			})?;
			let Some(elapsed) = elapsed else {
				return Ok(());
			};
			let created_at = fixtures.curve(&CURVE_WSOL)?.created_at;
			fixtures.set_clock(
				created_at.checked_add(elapsed).ok_or("clock overflow")?,
			)
		})
		.unwrap();

		let quote = Fixtures::load(&dir)
			.unwrap()
			.amm::<SendfunLaunchpadAmm>(&CURVE_WSOL)
			.unwrap()
			.quote(&exact_in(WSOL, CURVE_WSOL_BASE, 10_000_000))
			.unwrap();
		assert_eq!(quote.fee_pct, Decimal::new(fee_bps, 4));

		let mut test = wsol_curve_swaps(pool_test(CURVE_WSOL));
		test.fixtures_dir.clone_from(&dir);
		assert_pool_parity::<SendfunLaunchpadAmm>(&test, |swap, in_amount| {
			SendfunLaunchpadAmm::exact_in_data(swap, in_amount, 0).unwrap()
		});
		assert_native::<SendfunLaunchpadAmm>(&NativeTest::new(
			CURVE_WSOL,
			dir,
			&WSOL_CURVE_NATIVE,
		))
		.unwrap();
	}
}

/// The default partner charges 5 bps protocol on the DEX. Its max is 10 bps,
/// between the pool's rates. For the cases inside the window, the test turns
/// on its DEX decay: 20 seconds from `5_000` bps.
#[test]
fn dex_wsol_pool_charges_its_own_creator_rate() {
	if std::env::var_os("REFRESH").is_some() {
		return;
	}
	// `(creator_fee_bps, seconds after creation, fee bps)`. `None` keeps the
	// snapshot's clock and the partner's DEX decay off.
	for (creator_fee_bps, elapsed, fee_bps) in [
		(0, None, 5),
		(0, Some(DECAY_ELAPSED_SECONDS), 2_815),
		(4, None, 9),
		(4, Some(DECAY_ELAPSED_SECONDS), 2_817),
		(25, None, 30),
		(25, Some(DECAY_ELAPSED_SECONDS), 2_826),
	] {
		let dir = derive(&POOL_WSOL, "pool-creator-rate", |fixtures| {
			fixtures.edit_pool(&POOL_WSOL, |pool| {
				pool.creator_fee_bps = creator_fee_bps;
			})?;
			let Some(elapsed) = elapsed else {
				return Ok(());
			};
			fixtures.edit_partner_config(
				&DEFAULT_PARTNER_CONFIG,
				|config| {
					config.dex.fee_decay_seconds = 20;
					config.dex.fee_decay_start_bps = 5_000;
				},
			)?;
			let created_at = fixtures.pool(&POOL_WSOL)?.created_at;
			fixtures.set_clock(
				created_at.checked_add(elapsed).ok_or("clock overflow")?,
			)
		})
		.unwrap();

		let quote = Fixtures::load(&dir)
			.unwrap()
			.amm::<SendfunDexAmm>(&POOL_WSOL)
			.unwrap()
			.quote(&exact_in(WSOL, POOL_WSOL_BASE, 10_000_000))
			.unwrap();
		assert_eq!(quote.fee_pct, Decimal::new(fee_bps, 4));

		let mut test = pool_swaps(pool_test(POOL_WSOL));
		test.fixtures_dir.clone_from(&dir);
		assert_pool_parity::<SendfunDexAmm>(&test, |swap, in_amount| {
			SendfunDexAmm::exact_in_data(swap, in_amount, 0).unwrap()
		});
		assert_native::<SendfunDexAmm>(&NativeTest::new(
			POOL_WSOL,
			dir,
			&POOL_NATIVE,
		))
		.unwrap();
	}
}

/// The WSOL curve at a 30 bps creator rate, after the program's 1 SOL buy.
/// The buy's fee shares stay in the quote vault, outside
/// `real_quote_reserves`. Each later quote must match the program.
#[test]
fn launchpad_wsol_curve_that_traded_at_a_creator_rate() {
	if std::env::var_os("REFRESH").is_some() {
		return;
	}
	let dir = derive(&CURVE_WSOL, "curve-creator-rate-traded", |fixtures| {
		fixtures.edit_curve(&CURVE_WSOL, |curve| {
			curve.creator_fee_bps = 30;
		})?;
		fixtures.trade::<SendfunLaunchpadAmm>(
			&CURVE_WSOL,
			(WSOL, CURVE_WSOL_BASE, ONE_SOL),
		)
	})
	.unwrap();

	let snapshot = Fixtures::load(&fixtures_dir(&CURVE_WSOL)).unwrap();
	let traded = Fixtures::load(&dir).unwrap();
	let before = snapshot.curve(&CURVE_WSOL).unwrap();
	let after = traded.curve(&CURVE_WSOL).unwrap();
	// The vault takes the 1 SOL. The protocol is owed 200 bps of it. The
	// creator is owed 30 bps. The curve books the rest.
	assert_eq!(
		traded
			.token_amount(&after.quote_vault)
			.unwrap()
			.checked_sub(snapshot.token_amount(&before.quote_vault).unwrap())
			.unwrap(),
		ONE_SOL
	);
	assert_eq!(
		after
			.protocol_owed
			.checked_sub(before.protocol_owed)
			.unwrap(),
		20_000_000
	);
	assert_eq!(
		after.creator_owed.checked_sub(before.creator_owed).unwrap(),
		3_000_000
	);
	assert_eq!(
		after
			.real_quote_reserves
			.checked_sub(before.real_quote_reserves)
			.unwrap(),
		977_000_000
	);

	let mut test = wsol_curve_swaps(pool_test(CURVE_WSOL));
	test.fixtures_dir.clone_from(&dir);
	assert_pool_parity::<SendfunLaunchpadAmm>(&test, |swap, in_amount| {
		SendfunLaunchpadAmm::exact_in_data(swap, in_amount, 0).unwrap()
	});
	assert_native::<SendfunLaunchpadAmm>(&NativeTest::new(
		CURVE_WSOL,
		dir,
		&WSOL_CURVE_NATIVE,
	))
	.unwrap();
}

#[test]
fn a_buy_that_takes_exactly_the_supply_left_completes_the_curve() {
	if std::env::var_os("REFRESH").is_some() {
		return;
	}
	let snapshot = Fixtures::load(&fixtures_dir(&CURVE_WSOL)).unwrap();
	let quote = snapshot
		.amm::<SendfunLaunchpadAmm>(&CURVE_WSOL)
		.unwrap()
		.quote(&exact_in(WSOL, CURVE_WSOL_BASE, 1_000_000_000))
		.unwrap();
	assert_eq!(quote.out_amount, CURVE_WSOL_ONE_SOL_FILL);

	let dir = derive(&CURVE_WSOL, "curve-exact-cap", |fixtures| {
		fixtures.edit_curve(&CURVE_WSOL, |curve| {
			curve.real_base_reserves = CURVE_WSOL_ONE_SOL_FILL;
		})
	})
	.unwrap();
	assert_native::<SendfunLaunchpadAmm>(&NativeTest::new(
		CURVE_WSOL,
		dir,
		&[
			(SwapMode::ExactIn, WSOL, CURVE_WSOL_BASE, 1_000_000_000),
			(
				SwapMode::ExactOut,
				WSOL,
				CURVE_WSOL_BASE,
				CURVE_WSOL_ONE_SOL_FILL,
			),
		],
	))
	.unwrap();
}

/// With the supply one unit short, the program prices the fill at the cap.
/// The capped fill still costs the whole input.
#[test]
fn a_buy_one_unit_past_the_supply_left_still_spends_its_input() {
	if std::env::var_os("REFRESH").is_some() {
		return;
	}
	let dir = derive(&CURVE_WSOL, "curve-cap-minus-one", |fixtures| {
		fixtures.edit_curve(&CURVE_WSOL, |curve| {
			curve.real_base_reserves = CURVE_WSOL_ONE_SOL_FILL - 1;
		})
	})
	.unwrap();
	let quote = Fixtures::load(&dir)
		.unwrap()
		.amm::<SendfunLaunchpadAmm>(&CURVE_WSOL)
		.unwrap()
		.quote(&exact_in(WSOL, CURVE_WSOL_BASE, 1_000_000_000))
		.unwrap();
	assert_eq!(quote.in_amount, 1_000_000_000);
	assert_eq!(quote.out_amount, CURVE_WSOL_ONE_SOL_FILL - 1);

	assert_native::<SendfunLaunchpadAmm>(&NativeTest::new(
		CURVE_WSOL,
		dir,
		&[(SwapMode::ExactIn, WSOL, CURVE_WSOL_BASE, 1_000_000_000)],
	))
	.unwrap();
}

/// Well past the cap, exact-in quotes and fills exactly the capped amount.
/// Exact-out fills all or nothing. The program refuses a swap bounded by the
/// uncapped quote.
#[test]
fn a_buy_well_past_the_supply_left_quotes_and_fills_the_cap() {
	if std::env::var_os("REFRESH").is_some() {
		return;
	}
	let supply_left = CURVE_WSOL_ONE_SOL_FILL / 2;
	let dir = derive(&CURVE_WSOL, "curve-half-cap", |fixtures| {
		fixtures.edit_curve(&CURVE_WSOL, |curve| {
			curve.real_base_reserves = supply_left;
		})
	})
	.unwrap();
	let fixtures = Fixtures::load(&dir).unwrap();
	let amm = fixtures.amm::<SendfunLaunchpadAmm>(&CURVE_WSOL).unwrap();
	let capped = amm
		.quote(&exact_in(WSOL, CURVE_WSOL_BASE, 1_000_000_000))
		.unwrap();
	assert_eq!(capped.out_amount, supply_left);
	// The exact-out price of the supply left. The exact-in quote must agree
	// with it to the unit.
	let cost = amm
		.quote(&QuoteParams {
			swap_mode: SwapMode::ExactOut,
			..exact_in(WSOL, CURVE_WSOL_BASE, supply_left)
		})
		.unwrap()
		.in_amount;
	assert_eq!(capped.in_amount, cost);

	let mut harness = Harness::new(&fixtures, &amm).unwrap();
	let source = harness.funded_ata(&WSOL, 1_000_000_000).unwrap();
	let destination = harness.funded_ata(&CURVE_WSOL_BASE, 0).unwrap();
	let swap = |harness: &Harness<'_>, swap_mode| {
		swap_instruction(
			&amm,
			&harness.swap_params(
				(swap_mode, WSOL, CURVE_WSOL_BASE),
				(source, destination),
			),
		)
		.unwrap()
	};

	// Bounded by the uncapped fill.
	let (instruction, tag) = swap(&harness, SwapMode::ExactIn);
	let error = harness
		.send(Instruction {
			data: SendfunLaunchpadAmm::exact_in_data(
				&tag,
				1_000_000_000,
				CURVE_WSOL_ONE_SOL_FILL,
			)
			.unwrap(),
			..instruction
		})
		.unwrap_err()
		.to_string();
	assert!(
		error.contains(LAUNCHPAD_SLIPPAGE_EXCEEDED),
		"program error: {error}"
	);

	// The uncapped fill, exactly.
	let (instruction, tag) = swap(&harness, SwapMode::ExactOut);
	let error = harness
		.send(Instruction {
			data: SendfunLaunchpadAmm::exact_out_data(
				&tag,
				CURVE_WSOL_ONE_SOL_FILL,
				1_000_000_000,
			)
			.unwrap(),
			..instruction
		})
		.unwrap_err()
		.to_string();
	assert!(
		error.contains(LAUNCHPAD_INSUFFICIENT_LIQUIDITY),
		"program error: {error}"
	);
	assert_eq!(harness.token_amount(&destination).unwrap(), 0);
	assert_eq!(harness.token_amount(&source).unwrap(), 1_000_000_000);

	// Bounded by the adapter's own capped quote: the program fills it exactly.
	let (instruction, tag) = swap(&harness, SwapMode::ExactIn);
	harness
		.send(Instruction {
			data: SendfunLaunchpadAmm::exact_in_data(
				&tag,
				1_000_000_000,
				capped.out_amount,
			)
			.unwrap(),
			..instruction
		})
		.unwrap();
	assert_eq!(
		harness.token_amount(&destination).unwrap(),
		capped.out_amount
	);
	assert_eq!(
		harness.token_amount(&source).unwrap(),
		1_000_000_000_u64.checked_sub(capped.in_amount).unwrap()
	);
}

/// A sell pays out of `real_quote_reserves` only. At exactly the debit, it
/// fills. One unit short, the adapter and the program refuse.
#[test]
fn a_sell_against_the_quote_held() {
	if std::env::var_os("REFRESH").is_some() {
		return;
	}
	let sell_amount = 5_000_000_000_000;
	let snapshot = Fixtures::load(&fixtures_dir(&CURVE_WSOL)).unwrap();
	let quote = snapshot
		.amm::<SendfunLaunchpadAmm>(&CURVE_WSOL)
		.unwrap()
		.quote(&exact_in(CURVE_WSOL_BASE, WSOL, sell_amount))
		.unwrap();
	// WSOL has no transfer fee, so the vault debit is the payout plus the fee.
	let debit = quote.out_amount.checked_add(quote.fee_amount).unwrap();
	let short_debit = debit.checked_sub(1).unwrap();

	let exact = derive(&CURVE_WSOL, "curve-quote-held", |fixtures| {
		fixtures.edit_curve(&CURVE_WSOL, |curve| {
			curve.real_quote_reserves = debit;
		})
	})
	.unwrap();
	assert_native::<SendfunLaunchpadAmm>(&NativeTest::new(
		CURVE_WSOL,
		exact,
		&[(SwapMode::ExactIn, CURVE_WSOL_BASE, WSOL, sell_amount)],
	))
	.unwrap();

	let short = derive(&CURVE_WSOL, "curve-quote-held-minus-one", |fixtures| {
		fixtures.edit_curve(&CURVE_WSOL, |curve| {
			curve.real_quote_reserves = short_debit;
		})
	})
	.unwrap();
	let fixtures = Fixtures::load(&short).unwrap();
	let amm = fixtures.amm::<SendfunLaunchpadAmm>(&CURVE_WSOL).unwrap();
	let params = exact_in(CURVE_WSOL_BASE, WSOL, sell_amount);
	assert_eq!(
		amm.quote(&params).unwrap_err().to_string(),
		"sell exceeds the quote the curve holds"
	);
	let error = native_exact_in_error(&fixtures, &amm, &params).unwrap();
	assert!(
		error.contains(LAUNCHPAD_INSUFFICIENT_LIQUIDITY),
		"program error: {error}"
	);
}

/// `quote_reserves` booked 10% above the quote vault. The adapter and the
/// program price the quote side off `quote_reserves`.
#[test]
fn dex_wsol_pool_with_reserves_booked_above_the_vault() {
	if std::env::var_os("REFRESH").is_some() {
		return;
	}
	let dir = derive(&POOL_WSOL, "pool-reserves-110", |fixtures| {
		let booked =
			scale(fixtures.pool(&POOL_WSOL)?.quote_reserves, (11, 10))?;
		fixtures.edit_pool(&POOL_WSOL, |pool| pool.quote_reserves = booked)
	})
	.unwrap();
	let mut test = pool_swaps(pool_test(POOL_WSOL));
	test.fixtures_dir.clone_from(&dir);
	assert_pool_parity::<SendfunDexAmm>(&test, |swap, in_amount| {
		SendfunDexAmm::exact_in_data(swap, in_amount, 0).unwrap()
	});
	assert_native::<SendfunDexAmm>(&NativeTest::new(
		POOL_WSOL,
		dir,
		&POOL_NATIVE,
	))
	.unwrap();
}

// Synthetic: these tests arm a hook on a snapshot's base mint. They cover a DEX
// base mint that the DEX authority lists.
// `launchpad_prelwg_quoted_curve_with_a_hook` covers a hook on the quote mint.

/// A buy moves base out of the vault. A sell moves base into it. The hook runs
/// once per swap.
#[test]
fn launchpad_wsol_curve_with_a_hooked_base_mint() {
	if std::env::var_os("REFRESH").is_some() {
		return;
	}
	let dir = derive(&CURVE_WSOL, "curve-hooked", |fixtures| {
		let base_vault = fixtures.curve(&CURVE_WSOL)?.base_vault;
		fixtures.install_hook(&CURVE_WSOL_BASE, &base_vault)
	})
	.unwrap();
	let fixtures = Fixtures::load(&dir).unwrap();
	let amm = fixtures.amm::<SendfunLaunchpadAmm>(&CURVE_WSOL).unwrap();
	// The hook adds five accounts: the list's three extras, the hook and the list.
	assert_eq!(amm.get_accounts_len(), 22);
	assert_eq!(
		amm.program_dependencies(),
		vec![(HOOK_PROGRAM, HOOK_PROGRAM.to_string())]
	);
	native_exact_in_without_hook_accounts_fails(&fixtures, &amm).unwrap();

	let mut test = wsol_curve_swaps(pool_test(CURVE_WSOL));
	test.fixtures_dir.clone_from(&dir);
	assert_pool_parity::<SendfunLaunchpadAmm>(&test, |swap, in_amount| {
		SendfunLaunchpadAmm::exact_in_data(swap, in_amount, 0).unwrap()
	});
	assert_native::<SendfunLaunchpadAmm>(&NativeTest {
		hooked: true,
		..NativeTest::new(CURVE_WSOL, dir, &WSOL_CURVE_NATIVE)
	})
	.unwrap();
}

#[test]
fn dex_wsol_pool_with_a_hooked_base_mint() {
	if std::env::var_os("REFRESH").is_some() {
		return;
	}
	let dir = derive(&POOL_WSOL, "pool-hooked", |fixtures| {
		let base_vault = fixtures.pool(&POOL_WSOL)?.base_vault;
		fixtures.install_hook(&POOL_WSOL_BASE, &base_vault)
	})
	.unwrap();
	let fixtures = Fixtures::load(&dir).unwrap();
	let amm = fixtures.amm::<SendfunDexAmm>(&POOL_WSOL).unwrap();
	assert_eq!(amm.get_accounts_len(), 22);
	native_exact_in_without_hook_accounts_fails(&fixtures, &amm).unwrap();

	let mut test = pool_swaps(pool_test(POOL_WSOL));
	test.fixtures_dir.clone_from(&dir);
	assert_pool_parity::<SendfunDexAmm>(&test, |swap, in_amount| {
		SendfunDexAmm::exact_in_data(swap, in_amount, 0).unwrap()
	});
	assert_native::<SendfunDexAmm>(&NativeTest {
		hooked: true,
		..NativeTest::new(POOL_WSOL, dir, &POOL_NATIVE)
	})
	.unwrap();
}

/// Token-2022 invokes a hook that has no list
/// (spl-transfer-hook-interface #7099). The hook fails without it, so no swap
/// on that mint can land.
#[test]
fn a_hook_armed_without_a_list_does_not_route() {
	if std::env::var_os("REFRESH").is_some() {
		return;
	}
	let dir = derive(&CURVE_WSOL, "curve-hook-listless", |fixtures| {
		let base_vault = fixtures.curve(&CURVE_WSOL)?.base_vault;
		fixtures.arm_hook(&CURVE_WSOL_BASE, &base_vault)
	})
	.unwrap();
	let fixtures = Fixtures::load(&dir).unwrap();
	let mut amm = fixtures.amm::<SendfunLaunchpadAmm>(&CURVE_WSOL).unwrap();
	let amount = 10_000_000;
	let params = exact_in(WSOL, CURVE_WSOL_BASE, amount);

	// Active throughout, but unquotable.
	assert!(amm.is_active());
	assert_eq!(
		amm.quote(&params).unwrap_err().to_string(),
		"transfer hook accounts not loaded: call `update` again"
	);

	amm.update(&fixtures).unwrap();
	assert!(amm.is_active());
	assert_eq!(
		amm.quote(&params).unwrap_err().to_string(),
		"transfer hook has no ExtraAccountMetaList"
	);

	// The fixed accounts do not depend on the list. Use a listed market's.
	let hooked = Fixtures::load(
		&derive(&CURVE_WSOL, "curve-hooked", |fixtures| {
			let base_vault = fixtures.curve(&CURVE_WSOL)?.base_vault;
			fixtures.install_hook(&CURVE_WSOL_BASE, &base_vault)
		})
		.unwrap(),
	)
	.unwrap();
	let listed = hooked.amm::<SendfunLaunchpadAmm>(&CURVE_WSOL).unwrap();

	let mut harness = Harness::new(&fixtures, &listed).unwrap();
	let source = harness.funded_ata(&WSOL, amount).unwrap();
	let destination = harness.funded_ata(&CURVE_WSOL_BASE, 0).unwrap();
	let (instruction, swap) = swap_instruction(
		&listed,
		&harness.swap_params(
			(SwapMode::ExactIn, WSOL, CURVE_WSOL_BASE),
			(source, destination),
		),
	)
	.unwrap();
	let mut accounts = instruction.accounts[..FIXED_ACCOUNTS].to_vec();
	accounts.push(AccountMeta::new_readonly(HOOK_PROGRAM, false));
	let error = harness
		.send(Instruction {
			accounts,
			data: SendfunLaunchpadAmm::exact_in_data(&swap, amount, 0).unwrap(),
			..instruction
		})
		.unwrap_err()
		.to_string();
	assert!(
		error.contains(&format!("Program {HOOK_PROGRAM} invoke [3]")),
		"Token-2022 did not invoke the hook: {error}"
	);
	assert!(
		error.contains("NotEnoughAccountKeys"),
		"program error: {error}"
	);
}

/// A writable base-mint extra makes slot 3 writable for the whole trade. The
/// adapter refuses it (`WritableDuplicate`), and so does the program.
#[test]
fn a_hook_wanting_a_trade_account_writable_does_not_route() {
	if std::env::var_os("REFRESH").is_some() {
		return;
	}
	let dir = derive(&CURVE_WSOL, "curve-hook-writable", |fixtures| {
		let base_vault = fixtures.curve(&CURVE_WSOL)?.base_vault;
		fixtures.arm_hook(&CURVE_WSOL_BASE, &base_vault)?;
		fixtures
			.set_list(&CURVE_WSOL_BASE, &writable_extra_list(&CURVE_WSOL_BASE)?)
	})
	.unwrap();
	let fixtures = Fixtures::load(&dir).unwrap();
	let mut amm = fixtures.amm::<SendfunLaunchpadAmm>(&CURVE_WSOL).unwrap();
	let amount = 10_000_000;
	let params = exact_in(WSOL, CURVE_WSOL_BASE, amount);

	amm.update(&fixtures).unwrap();
	assert!(amm.is_active());
	assert_eq!(
		amm.quote(&params).unwrap_err().to_string(),
		"transfer hook cannot be resolved ahead of the swap: transfer hook repeats an account of the trade, writable"
	);

	// The swap that the adapter refuses to build: the trade's 17 accounts, the
	// extra (the base mint, writable), the hook program and its list.
	let listed = Fixtures::load(
		&derive(&CURVE_WSOL, "curve-hooked", |fixtures| {
			let base_vault = fixtures.curve(&CURVE_WSOL)?.base_vault;
			fixtures.install_hook(&CURVE_WSOL_BASE, &base_vault)
		})
		.unwrap(),
	)
	.unwrap()
	.amm::<SendfunLaunchpadAmm>(&CURVE_WSOL)
	.unwrap();
	let mut harness = Harness::new(&fixtures, &listed).unwrap();
	let source = harness.funded_ata(&WSOL, amount).unwrap();
	let destination = harness.funded_ata(&CURVE_WSOL_BASE, 0).unwrap();
	let (instruction, swap) = swap_instruction(
		&listed,
		&harness.swap_params(
			(SwapMode::ExactIn, WSOL, CURVE_WSOL_BASE),
			(source, destination),
		),
	)
	.unwrap();
	let (validation, _) = Pubkey::find_program_address(
		&[b"extra-account-metas", CURVE_WSOL_BASE.as_ref()],
		&HOOK_PROGRAM,
	);
	let mut accounts = instruction.accounts[..FIXED_ACCOUNTS].to_vec();
	accounts.push(AccountMeta::new(CURVE_WSOL_BASE, false));
	accounts.push(AccountMeta::new_readonly(HOOK_PROGRAM, false));
	accounts.push(AccountMeta::new_readonly(validation, false));
	let error = harness
		.send(Instruction {
			accounts,
			data: SendfunLaunchpadAmm::exact_in_data(&swap, amount, 0).unwrap(),
			..instruction
		})
		.unwrap_err()
		.to_string();
	// The launchpad fails with `AccountBorrowFailed` before it calls
	// Token-2022. The hook never runs.
	assert!(
		error.contains("AccountBorrowFailed"),
		"program error: {error}"
	);
	assert!(
		!error.contains(&HOOK_PROGRAM.to_string()),
		"the hook ran, so the borrow check was not what refused: {error}"
	);
}

/// Checks `PAUSABLE_CONFIG_EXTENSION`. With a wrong type, the adapter quotes a
/// paused market. Synthetic: it pauses the base mint. A launchpad base mint
/// cannot pause.
#[test]
fn a_paused_mint_does_not_route() {
	if std::env::var_os("REFRESH").is_some() {
		return;
	}
	let dir = derive(&CURVE_WSOL, "curve-paused", |fixtures| {
		fixtures.pause_mint(&CURVE_WSOL_BASE)
	})
	.unwrap();
	let fixtures = Fixtures::load(&dir).unwrap();
	let mut amm = fixtures.amm::<SendfunLaunchpadAmm>(&CURVE_WSOL).unwrap();
	let amount = 10_000_000;
	let params = exact_in(WSOL, CURVE_WSOL_BASE, amount);

	amm.update(&fixtures).unwrap();
	assert!(amm.is_active());
	assert_eq!(
		amm.quote(&params).unwrap_err().to_string(),
		"a mint on this market is paused"
	);

	// A pause adds no accounts. This is the swap that a stale router sends.
	let unpaused = Fixtures::load(&fixtures_dir(&CURVE_WSOL)).unwrap();
	let live = unpaused.amm::<SendfunLaunchpadAmm>(&CURVE_WSOL).unwrap();

	let mut harness = Harness::new(&fixtures, &live).unwrap();
	let source = harness.funded_ata(&WSOL, amount).unwrap();
	let destination = harness.funded_ata(&CURVE_WSOL_BASE, 0).unwrap();
	let (instruction, swap) = swap_instruction(
		&live,
		&harness.swap_params(
			(SwapMode::ExactIn, WSOL, CURVE_WSOL_BASE),
			(source, destination),
		),
	)
	.unwrap();
	let error = harness
		.send(Instruction {
			data: SendfunLaunchpadAmm::exact_in_data(&swap, amount, 0).unwrap(),
			..instruction
		})
		.unwrap_err()
		.to_string();
	// Must be `MintPaused`. A wrong extension type leaves the mint unpaused.
	assert!(
		error.contains(&format!(
			"{TOKEN_2022_FAILED}: custom program error: 0x43"
		)),
		"program error: {error}"
	);
	assert_eq!(harness.token_amount(&destination).unwrap(), 0);
}

/// Without the hook accounts, the buy fails in Token-2022. With them, the hook
/// runs once.
fn native_exact_in_without_hook_accounts_fails<A: NativeData>(
	fixtures: &Fixtures,
	amm: &A,
) -> TestResult {
	let mints = amm.get_reserve_mints();
	let [base, quote] = mints.as_slice() else {
		return Err("expected two reserve mints".into());
	};
	let mut harness = Harness::new(fixtures, amm)?;
	let source = harness.funded_ata(quote, 10_000_000)?;
	let destination = harness.funded_ata(base, 0)?;
	let (instruction, swap) = swap_instruction(
		amm,
		&harness.swap_params(
			(SwapMode::ExactIn, *quote, *base),
			(source, destination),
		),
	)?;
	let data = A::data(&swap, SwapMode::ExactIn, (10_000_000, 0))?;
	let mut unhooked = Instruction {
		data: data.clone(),
		..instruction.clone()
	};
	unhooked.accounts.truncate(FIXED_ACCOUNTS);
	match harness.send(unhooked) {
		Ok(_) => {
			return Err("a hooked transfer ran without its accounts".into());
		}
		Err(error) if error.to_string().contains(TOKEN_2022_FAILED) => {}
		Err(error) => return Err(format!("unexpected failure: {error}").into()),
	}
	let logs = harness.send(Instruction {
		data,
		..instruction
	})?;
	assert_eq!(hook_invocations(&logs), vec![3]);
	Ok(())
}

/// The stack height of each run of the hook. It is 3 under a top-level trade.
fn hook_invocations(logs: &[String]) -> Vec<u8> {
	let prefix = format!("Program {HOOK_PROGRAM} invoke [");
	logs.iter()
		.filter_map(|line| line.strip_prefix(&prefix)?.strip_suffix(']'))
		.filter_map(|height| height.parse().ok())
		.collect()
}

struct Execute;

impl SplDiscriminate for Execute {
	const SPL_DISCRIMINATOR: ArrayDiscriminator =
		ArrayDiscriminator::new(EXECUTE_DISCRIMINATOR);
}

/// A hook list: a fixed account, a writable PDA keyed on the source, and a PDA
/// keyed on the source's owner, read from the source's data.
fn hook_list() -> TestResult<Vec<u8>> {
	let metas = [
		ExtraAccountMeta::new_with_pubkey(&RENT_SYSVAR, false, false)?,
		ExtraAccountMeta::new_with_seeds(
			&[
				Seed::Literal {
					bytes: b"ledger".to_vec(),
				},
				Seed::AccountKey { index: 0 },
			],
			false,
			true,
		)?,
		// 32 bytes at 32 of account 0: its owner.
		ExtraAccountMeta::new_with_seeds(
			&[
				Seed::Literal {
					bytes: b"owner".to_vec(),
				},
				Seed::AccountData {
					account_index: 0,
					data_index: 32,
					length: 32,
				},
			],
			false,
			false,
		)?,
	];
	let mut data = vec![0; ExtraAccountMetaList::size_of(metas.len())?];
	ExtraAccountMetaList::init::<Execute>(&mut data, &metas)?;
	Ok(data)
}

/// A list whose only extra is `account`, writable. For a list that asks for a
/// privilege the trade does not grant, pass an address that the trade has
/// read-only, such as the base mint.
fn writable_extra_list(account: &Pubkey) -> TestResult<Vec<u8>> {
	let metas = [ExtraAccountMeta::new_with_pubkey(account, false, true)?];
	let mut data = vec![0; ExtraAccountMetaList::size_of(metas.len())?];
	ExtraAccountMetaList::init::<Execute>(&mut data, &metas)?;
	Ok(data)
}

/// The end of the TLV entries of a Token-2022 mint or account: the end of the
/// data, or the first all-zero type.
fn tlv_end(data: &[u8]) -> TestResult<usize> {
	let mut offset = TLV_START;
	while let Some(&[type_low, type_high, length_low, length_high]) =
		data.get(offset..offset.checked_add(4).ok_or("offset overflow")?)
	{
		if u16::from_le_bytes([type_low, type_high]) == 0 {
			break;
		}
		offset = offset
			.checked_add(4)
			.and_then(|end| {
				end.checked_add(usize::from(u16::from_le_bytes([
					length_low,
					length_high,
				])))
			})
			.ok_or("extension length overflow")?;
	}
	if offset > data.len() {
		return Err("extension runs past the account".into());
	}
	Ok(offset)
}

/// The payload of the Token-2022 extension of type `wanted` in a mint's or an
/// account's data.
fn extension_payload(data: &mut [u8], wanted: u16) -> TestResult<&mut [u8]> {
	let mut offset = TLV_START;
	loop {
		let start = offset.checked_add(4).ok_or("offset overflow")?;
		let Some(&[type_low, type_high, length_low, length_high]) =
			data.get(offset..start)
		else {
			return Err(format!("no extension of type {wanted}").into());
		};
		let end = start
			.checked_add(usize::from(u16::from_le_bytes([
				length_low,
				length_high,
			])))
			.ok_or("extension length overflow")?;
		match u16::from_le_bytes([type_low, type_high]) {
			0 => return Err(format!("no extension of type {wanted}").into()),
			found if found == wanted => {
				return Ok(data
					.get_mut(start..end)
					.ok_or("extension runs past the account")?);
			}
			_ => offset = end,
		}
	}
}

// Synthetic: these tests add a transfer fee to the base mint. They cover a DEX
// base mint that the DEX authority lists. A newer entry applies at the
// snapshot's epoch. A different older entry applies to a wrong-epoch quote. The
// maximum fee caps large swaps.

/// 1.37% on the base mint, capped at `20_000_000_000` units.
const BASE_TRANSFER_FEE: TransferFee = (137, 20_000_000_000);

/// [`PRELWG`]'s fee at every snapshot epoch: 1%, uncapped.
const PRELWG_TRANSFER_FEE: MintFee = MintFee {
	bps: 100,
	maximum_fee: u64::MAX,
};

const PRELWG_CURVE_NATIVE: [(SwapMode, Pubkey, Pubkey, u64); 8] = [
	(SwapMode::ExactIn, PRELWG, CURVE_PRELWG_BASE, 12_345_679),
	(SwapMode::ExactIn, PRELWG, CURVE_PRELWG_BASE, 1_000_000_000),
	(SwapMode::ExactIn, CURVE_PRELWG_BASE, PRELWG, 1_234_567_891),
	(
		SwapMode::ExactIn,
		CURVE_PRELWG_BASE,
		PRELWG,
		20_000_000_000_000,
	),
	(SwapMode::ExactOut, PRELWG, CURVE_PRELWG_BASE, 987_654_321),
	(
		SwapMode::ExactOut,
		PRELWG,
		CURVE_PRELWG_BASE,
		100_000_000_000_000,
	),
	(SwapMode::ExactOut, CURVE_PRELWG_BASE, PRELWG, 1_000_001),
	(SwapMode::ExactOut, CURVE_PRELWG_BASE, PRELWG, 100_000_000),
];

const PRELWG_POOL_NATIVE: [(SwapMode, Pubkey, Pubkey, u64); 8] = [
	(SwapMode::ExactIn, PRELWG, CURVE_PRELWG_BASE, 12_345_679),
	(SwapMode::ExactIn, PRELWG, CURVE_PRELWG_BASE, 10_000_000_000),
	(SwapMode::ExactIn, CURVE_PRELWG_BASE, PRELWG, 1_234_567_891),
	(
		SwapMode::ExactIn,
		CURVE_PRELWG_BASE,
		PRELWG,
		10_000_000_000_000,
	),
	(SwapMode::ExactOut, PRELWG, CURVE_PRELWG_BASE, 987_654_321),
	(
		SwapMode::ExactOut,
		PRELWG,
		CURVE_PRELWG_BASE,
		1_000_000_000_000,
	),
	(SwapMode::ExactOut, CURVE_PRELWG_BASE, PRELWG, 1_000_001),
	(SwapMode::ExactOut, CURVE_PRELWG_BASE, PRELWG, 1_000_000_000),
];

/// Amounts whose fees round, added to each market's round amounts.
const WSOL_CURVE_ODD_NATIVE: [(SwapMode, Pubkey, Pubkey, u64); 2] = [
	(SwapMode::ExactIn, CURVE_WSOL_BASE, WSOL, 1_234_567_891),
	(SwapMode::ExactOut, WSOL, CURVE_WSOL_BASE, 987_654_321),
];
const POOL_ODD_NATIVE: [(SwapMode, Pubkey, Pubkey, u64); 2] = [
	(SwapMode::ExactIn, POOL_WSOL_BASE, WSOL, 1_234_567_891),
	(SwapMode::ExactOut, WSOL, POOL_WSOL_BASE, 987_654_321),
];

/// Token-2022 withholds the base mint's fee on the base leg of every swap:
/// out of the vault on a buy, into it on a sell.
#[test]
fn launchpad_wsol_curve_with_a_transfer_fee_on_the_base_mint() {
	if std::env::var_os("REFRESH").is_some() {
		return;
	}
	let dir = derive(&CURVE_WSOL, "curve-base-transfer-fee", |fixtures| {
		let base_vault = fixtures.curve(&CURVE_WSOL)?.base_vault;
		fixtures.install_base_transfer_fee(&CURVE_WSOL_BASE, &base_vault)
	})
	.unwrap();

	let mut test = wsol_curve_swaps(pool_test(CURVE_WSOL));
	test.fixtures_dir.clone_from(&dir);
	assert_pool_parity::<SendfunLaunchpadAmm>(&test, |swap, in_amount| {
		SendfunLaunchpadAmm::exact_in_data(swap, in_amount, 0).unwrap()
	});
	assert_native::<SendfunLaunchpadAmm>(&NativeTest::new(
		CURVE_WSOL,
		dir,
		&[WSOL_CURVE_NATIVE.as_slice(), &WSOL_CURVE_ODD_NATIVE].concat(),
	))
	.unwrap();
}

#[test]
fn dex_wsol_pool_with_a_transfer_fee_on_the_base_mint() {
	if std::env::var_os("REFRESH").is_some() {
		return;
	}
	let dir = derive(&POOL_WSOL, "pool-base-transfer-fee", |fixtures| {
		let base_vault = fixtures.pool(&POOL_WSOL)?.base_vault;
		fixtures.install_base_transfer_fee(&POOL_WSOL_BASE, &base_vault)
	})
	.unwrap();

	let mut test = pool_swaps(pool_test(POOL_WSOL));
	test.fixtures_dir.clone_from(&dir);
	assert_pool_parity::<SendfunDexAmm>(&test, |swap, in_amount| {
		SendfunDexAmm::exact_in_data(swap, in_amount, 0).unwrap()
	});
	assert_native::<SendfunDexAmm>(&NativeTest::new(
		POOL_WSOL,
		dir,
		&[POOL_NATIVE.as_slice(), &POOL_ODD_NATIVE].concat(),
	))
	.unwrap();
}

/// The curve books only the quote that lands in its vault. Sellers receive
/// the payout less the transfer fee.
#[test]
fn launchpad_prelwg_quoted_curve() {
	LAUNCHPAD_BINARY.redump_on_refresh();
	let test = pool_test(CURVE_PRELWG)
		.add_swap(PRELWG, CURVE_PRELWG_BASE, 10_000_000)
		.add_swap(PRELWG, CURVE_PRELWG_BASE, 500_000_000)
		.add_swap(PRELWG, CURVE_PRELWG_BASE, 2_000_000_000)
		.add_swap(CURVE_PRELWG_BASE, PRELWG, 1_000_000_000)
		.add_swap(CURVE_PRELWG_BASE, PRELWG, 10_000_000_000_000);
	assert_pool_parity::<SendfunLaunchpadAmm>(&test, |swap, in_amount| {
		SendfunLaunchpadAmm::exact_in_data(swap, in_amount, 0).unwrap()
	});
	// A refresh re-reads the mint. A fee change by its issuer fails here.
	let dir = fixtures_dir(&CURVE_PRELWG);
	assert_eq!(
		Fixtures::load(&dir).unwrap().transfer_fee(&PRELWG).unwrap(),
		Some(PRELWG_TRANSFER_FEE)
	);
	assert_native::<SendfunLaunchpadAmm>(&NativeTest::new(
		CURVE_PRELWG,
		dir,
		&PRELWG_CURVE_NATIVE,
	))
	.unwrap();
}

/// [`PRELWG`] has a `TransferHook` extension with no program. The test sets the
/// program, as the hook authority can. The hook runs once per swap, on the
/// quote leg: into the vault on a buy, out of it on a sell.
#[test]
fn launchpad_prelwg_quoted_curve_with_a_hook() {
	if std::env::var_os("REFRESH").is_some() {
		return;
	}
	let dir = derive(&CURVE_PRELWG, "prelwg-hooked", |fixtures| {
		fixtures.point_hook(&PRELWG, &HOOK_PROGRAM)?;
		fixtures.set_list(&PRELWG, &hook_list()?)
	})
	.unwrap();
	let fixtures = Fixtures::load(&dir).unwrap();
	let amm = fixtures.amm::<SendfunLaunchpadAmm>(&CURVE_PRELWG).unwrap();
	// The hook adds five accounts: the list's three extras, the hook and the list.
	assert_eq!(amm.get_accounts_len(), 22);
	assert_eq!(
		amm.program_dependencies(),
		vec![(HOOK_PROGRAM, HOOK_PROGRAM.to_string())]
	);
	native_exact_in_without_hook_accounts_fails(&fixtures, &amm).unwrap();

	let mut test = pool_test(CURVE_PRELWG)
		.add_swap(PRELWG, CURVE_PRELWG_BASE, 10_000_000)
		.add_swap(PRELWG, CURVE_PRELWG_BASE, 2_000_000_000)
		.add_swap(CURVE_PRELWG_BASE, PRELWG, 1_000_000_000)
		.add_swap(CURVE_PRELWG_BASE, PRELWG, 10_000_000_000_000);
	test.fixtures_dir.clone_from(&dir);
	assert_pool_parity::<SendfunLaunchpadAmm>(&test, |swap, in_amount| {
		SendfunLaunchpadAmm::exact_in_data(swap, in_amount, 0).unwrap()
	});
	assert_native::<SendfunLaunchpadAmm>(&NativeTest {
		hooked: true,
		..NativeTest::new(CURVE_PRELWG, dir, &PRELWG_CURVE_NATIVE)
	})
	.unwrap();
}

/// No snapshot has a pool quoted in [`PRELWG`]. This test builds the pool that
/// [`CURVE_PRELWG`] migrates into (see [`Fixtures::requote_pool_on_prelwg`]).
/// Token-2022 takes the fee on the quote leg: into the vault on a buy, out of
/// it on a sell.
#[test]
fn dex_prelwg_quoted_pool() {
	if std::env::var_os("REFRESH").is_some() {
		return;
	}
	let (pool, _) = find_pool_pda(&CURVE_PRELWG_BASE, &PRELWG);
	let dir = derive(&POOL_WSOL, "pool-prelwg", |fixtures| {
		fixtures.requote_pool_on_prelwg(&Fixtures::load(&fixtures_dir(
			&CURVE_PRELWG,
		))?)
	})
	.unwrap();
	assert_eq!(
		Fixtures::load(&dir).unwrap().transfer_fee(&PRELWG).unwrap(),
		Some(PRELWG_TRANSFER_FEE)
	);

	let mut test = pool_test(pool)
		.add_swap(PRELWG, CURVE_PRELWG_BASE, 10_000_000)
		.add_swap(PRELWG, CURVE_PRELWG_BASE, 1_000_000_000)
		.add_swap(PRELWG, CURVE_PRELWG_BASE, 10_000_000_000)
		.add_swap(CURVE_PRELWG_BASE, PRELWG, 1_000_000_000)
		.add_swap(CURVE_PRELWG_BASE, PRELWG, 10_000_000_000_000);
	test.fixtures_dir.clone_from(&dir);
	assert_pool_parity::<SendfunDexAmm>(&test, |swap, in_amount| {
		SendfunDexAmm::exact_in_data(swap, in_amount, 0).unwrap()
	});
	assert_native::<SendfunDexAmm>(&NativeTest::new(
		pool,
		dir,
		&PRELWG_POOL_NATIVE,
	))
	.unwrap();
}

/// `(basis points, maximum fee)`.
type TransferFee = (u16, u64);

/// A `TransferFeeConfig` payload: no authorities, nothing withheld, `fee` from
/// `epoch`, and a much higher fee before `epoch`.
fn transfer_fee_config(
	epoch: u64,
	(basis_points, maximum_fee): TransferFee,
) -> Vec<u8> {
	let entry = |epoch: u64, basis_points: u16| {
		[
			epoch.to_le_bytes().as_slice(),
			&maximum_fee.to_le_bytes(),
			&basis_points.to_le_bytes(),
		]
		.concat()
	};
	[
		[0; 32].as_slice(),
		&[0; 32],
		&0_u64.to_le_bytes(),
		&entry(0, RETIRED_TRANSFER_FEE_BPS),
		&entry(epoch, basis_points),
	]
	.concat()
}

/// Appends a Token-2022 extension entry. Adds lamports for the rent of the
/// larger account.
fn append_extension(
	account: &mut Account,
	extension_type: u16,
	payload: &[u8],
) -> TestResult {
	if account.data.len() <= TLV_START {
		return Err("not a Token-2022 account with extensions".into());
	}
	let end = tlv_end(&account.data)?;
	account.data.truncate(end);
	account
		.data
		.extend_from_slice(&extension_type.to_le_bytes());
	account
		.data
		.extend_from_slice(&u16::try_from(payload.len())?.to_le_bytes());
	account.data.extend_from_slice(payload);
	account.lamports = account
		.lamports
		.checked_add(10_000_000)
		.ok_or("lamports overflow")?;
	Ok(())
}

// No snapshot has a non-default partner. Each test registers a new one.

/// `PartnerConfig.fee_preset` for rates set per partner, not by a preset.
const CUSTOM_FEES_PRESET: u8 = 255;

/// The registered partner's rates. Its protocol rates differ from the default
/// partner's 200 bps on the launchpad and 5 bps on the DEX. Its creator maxes
/// differ from each snapshot market's creator rate: 0 on the curve and 10 on
/// the pool.
const SIGNED_PARTNER_FEES: (LaunchpadFees, DexFees) = (
	LaunchpadFees {
		creation_fee_cents: 0,
		protocol_fee_bps: 100,
		max_creator_fee_bps: 50,
		fee_decay_seconds: 0,
		fee_decay_start_bps: 0,
	},
	DexFees {
		creation_fee_cents: 0,
		protocol_fee_bps: 3,
		lp_fee_bps: 0,
		max_creator_fee_bps: 7,
		fee_decay_seconds: 0,
		fee_decay_start_bps: 0,
	},
);

const ONE_SOL: u64 = 1_000_000_000;

/// Zero fees. A quote at these rates shows that `from_keyed_account` did not
/// default the partner.
const FREE_PARTNER_FEES: (LaunchpadFees, DexFees) = (
	LaunchpadFees {
		creation_fee_cents: 0,
		protocol_fee_bps: 0,
		max_creator_fee_bps: 0,
		fee_decay_seconds: 0,
		fee_decay_start_bps: 0,
	},
	DexFees {
		creation_fee_cents: 0,
		protocol_fee_bps: 0,
		lp_fee_bps: 0,
		max_creator_fee_bps: 0,
		fee_decay_seconds: 0,
		fee_decay_start_bps: 0,
	},
);

/// The slots of the partner and its config in both programs' trade metas.
const PARTNER_SLOT: usize = 9;
const PARTNER_CONFIG_SLOT: usize = 10;

/// The registered partner's protocol rates price each swap: 100 bps on the
/// curve and 3 bps on the pool. Its creator maxes of 50 and 7 bps are not in
/// the fee. The fee also includes the market's creator rate: 0 bps on the
/// curve, 10 bps on the pool. The partner signs, and the program fills each
/// swap at the quote.
#[test]
fn launchpad_wsol_curve_routed_as_a_signed_partner() {
	if std::env::var_os("REFRESH").is_some() {
		return;
	}
	let partner = Keypair::new();
	let dir = derive(&CURVE_WSOL, "curve-signed-partner", |fixtures| {
		fixtures.register_partner(&partner.pubkey()).map(drop)
	})
	.unwrap();

	let quote = Fixtures::load(&dir)
		.unwrap()
		.partner_amm::<SendfunLaunchpadAmm>(&CURVE_WSOL, &partner.pubkey())
		.unwrap()
		.quote(&exact_in(WSOL, CURVE_WSOL_BASE, 1_000_000_000))
		.unwrap();
	assert_eq!(quote.fee_pct, Decimal::new(100, 4));
	assert_eq!(quote.fee_amount, 10_000_000);

	assert_native::<SendfunLaunchpadAmm>(&NativeTest {
		partner: Some(partner),
		..NativeTest::new(CURVE_WSOL, dir, &WSOL_CURVE_NATIVE)
	})
	.unwrap();
}

#[test]
fn dex_wsol_pool_routed_as_a_signed_partner() {
	if std::env::var_os("REFRESH").is_some() {
		return;
	}
	let partner = Keypair::new();
	let dir = derive(&POOL_WSOL, "pool-signed-partner", |fixtures| {
		fixtures.register_partner(&partner.pubkey()).map(drop)
	})
	.unwrap();

	let quote = Fixtures::load(&dir)
		.unwrap()
		.partner_amm::<SendfunDexAmm>(&POOL_WSOL, &partner.pubkey())
		.unwrap()
		.quote(&exact_in(WSOL, POOL_WSOL_BASE, 1_000_000_000))
		.unwrap();
	assert_eq!(quote.fee_pct, Decimal::new(13, 4));
	assert_eq!(quote.fee_amount, 1_300_000);

	assert_native::<SendfunDexAmm>(&NativeTest {
		partner: Some(partner),
		..NativeTest::new(POOL_WSOL, dir, &POOL_NATIVE)
	})
	.unwrap();
}

/// The partner's own rates price the curve, not the default partner's
/// 200 bps.
#[test]
fn a_partners_own_schedule_prices_the_curve() {
	if std::env::var_os("REFRESH").is_some() {
		return;
	}
	let buy = exact_in(WSOL, CURVE_WSOL_BASE, ONE_SOL);
	let swaps = [(SwapMode::ExactIn, WSOL, CURVE_WSOL_BASE, ONE_SOL)];

	let default = Fixtures::load(&fixtures_dir(&CURVE_WSOL))
		.unwrap()
		.amm::<SendfunLaunchpadAmm>(&CURVE_WSOL)
		.unwrap()
		.quote(&buy)
		.unwrap();
	assert_eq!(default.fee_pct, Decimal::new(200, 4));
	assert_eq!(default.fee_amount, 20_000_000);
	assert_eq!(default.out_amount, CURVE_WSOL_ONE_SOL_FILL);

	let partner = Keypair::new();
	let dir = derive(&CURVE_WSOL, "curve-free-partner", |fixtures| {
		fixtures
			.register_partner_at(&partner.pubkey(), FREE_PARTNER_FEES)
			.map(drop)
	})
	.unwrap();
	let free = Fixtures::load(&dir)
		.unwrap()
		.partner_amm::<SendfunLaunchpadAmm>(&CURVE_WSOL, &partner.pubkey())
		.unwrap()
		.quote(&buy)
		.unwrap();
	assert_eq!(free.fee_pct, Decimal::ZERO);
	assert_eq!(free.fee_amount, 0);
	// With no fee, the 200 bps that the default partner takes buys base.
	assert_eq!(free.in_amount, default.in_amount);
	assert_eq!(free.out_amount, 9_863_554_127_627);

	assert_native::<SendfunLaunchpadAmm>(&NativeTest {
		partner: Some(partner),
		..NativeTest::new(CURVE_WSOL, dir, &swaps)
	})
	.unwrap();
	assert_native::<SendfunLaunchpadAmm>(&NativeTest::new(
		CURVE_WSOL,
		fixtures_dir(&CURVE_WSOL),
		&swaps,
	))
	.unwrap();
}

/// [`a_partners_own_schedule_prices_the_curve`] on the DEX. The default
/// partner's rate is 5 bps. Every fee includes the pool's creator rate of 10
/// bps. The free partner's quote pays it.
#[test]
fn a_partners_own_schedule_prices_the_pool() {
	if std::env::var_os("REFRESH").is_some() {
		return;
	}
	let buy = exact_in(WSOL, POOL_WSOL_BASE, ONE_SOL);
	let swaps = [(SwapMode::ExactIn, WSOL, POOL_WSOL_BASE, ONE_SOL)];

	let default = Fixtures::load(&fixtures_dir(&POOL_WSOL))
		.unwrap()
		.amm::<SendfunDexAmm>(&POOL_WSOL)
		.unwrap()
		.quote(&buy)
		.unwrap();
	assert_eq!(default.fee_pct, Decimal::new(15, 4));
	assert_eq!(default.fee_amount, 1_500_000);
	assert_eq!(default.out_amount, 2_425_233_288_145);

	let partner = Keypair::new();
	let dir = derive(&POOL_WSOL, "pool-free-partner", |fixtures| {
		fixtures
			.register_partner_at(&partner.pubkey(), FREE_PARTNER_FEES)
			.map(drop)
	})
	.unwrap();
	let free = Fixtures::load(&dir)
		.unwrap()
		.partner_amm::<SendfunDexAmm>(&POOL_WSOL, &partner.pubkey())
		.unwrap()
		.quote(&buy)
		.unwrap();
	assert_eq!(free.fee_pct, Decimal::new(10, 4));
	assert_eq!(free.fee_amount, 1_000_000);
	assert_eq!(free.in_amount, default.in_amount);
	assert_eq!(free.out_amount, 2_426_433_552_535);

	assert_native::<SendfunDexAmm>(&NativeTest {
		partner: Some(partner),
		..NativeTest::new(POOL_WSOL, dir, &swaps)
	})
	.unwrap();
	assert_native::<SendfunDexAmm>(&NativeTest::new(
		POOL_WSOL,
		fixtures_dir(&POOL_WSOL),
		&swaps,
	))
	.unwrap();
}

#[test]
fn a_partner_that_does_not_sign_is_refused() {
	if std::env::var_os("REFRESH").is_some() {
		return;
	}
	native_exact_in_without_partner_signature_fails::<SendfunLaunchpadAmm>(
		&CURVE_WSOL,
	)
	.unwrap();
	native_exact_in_without_partner_signature_fails::<SendfunDexAmm>(
		&POOL_WSOL,
	)
	.unwrap();
}

#[test]
fn a_partner_not_registered_on_the_platform_does_not_route() {
	if std::env::var_os("REFRESH").is_some() {
		return;
	}
	native_exact_in_for_an_unregistered_partner_fails::<SendfunLaunchpadAmm>(
		&CURVE_WSOL,
	)
	.unwrap();
	native_exact_in_for_an_unregistered_partner_fails::<SendfunDexAmm>(
		&POOL_WSOL,
	)
	.unwrap();
}

/// An unsigned partner meta fails with `Unauthorized`. The same buy, signed,
/// fills.
fn native_exact_in_without_partner_signature_fails<A: NativeData>(
	market: &Pubkey,
) -> TestResult {
	let partner = Keypair::new();
	let mut fixtures = Fixtures::load(&fixtures_dir(market))?;
	let partner_config = fixtures.register_partner(&partner.pubkey())?;
	let amm = fixtures.partner_amm::<A>(market, &partner.pubkey())?;
	let mints = amm.get_reserve_mints();
	let [base, quote] = mints.as_slice() else {
		return Err("expected two reserve mints".into());
	};
	let mut harness =
		Harness::new(&fixtures, &amm)?.with_partner(Some(&partner));
	let source = harness.funded_ata(quote, 10_000_000)?;
	let destination = harness.funded_ata(base, 0)?;
	let (instruction, swap) = swap_instruction(
		&amm,
		&harness.swap_params(
			(SwapMode::ExactIn, *quote, *base),
			(source, destination),
		),
	)?;
	assert_eq!(
		instruction.accounts.get(PARTNER_SLOT..=PARTNER_CONFIG_SLOT),
		Some(
			[
				AccountMeta::new_readonly(partner.pubkey(), true),
				AccountMeta::new_readonly(partner_config, false),
			]
			.as_slice()
		)
	);
	let signed = Instruction {
		data: A::data(&swap, SwapMode::ExactIn, (10_000_000, 0))?,
		..instruction
	};
	let mut unsigned = signed.clone();
	unsigned
		.accounts
		.get_mut(PARTNER_SLOT)
		.ok_or("no partner slot")?
		.is_signer = false;
	match harness.send(unsigned) {
		Ok(_) => {
			return Err("a partner's swap ran without its signature".into());
		}
		Err(error) if error.to_string().contains(PARTNER_UNAUTHORIZED) => {}
		Err(error) => return Err(format!("unexpected failure: {error}").into()),
	}
	harness.send(signed)?;
	assert_eq!(harness.token_amount(&source)?, 0);
	Ok(())
}

/// With no config for the partner on the market's platform, the market stays
/// active but refuses the quote and the swap. The program refuses the signed
/// buy that the adapter builds for the registered partner.
fn native_exact_in_for_an_unregistered_partner_fails<A: NativeData>(
	market: &Pubkey,
) -> TestResult {
	let partner = Keypair::new();
	let snapshot = Fixtures::load(&fixtures_dir(market))?;
	let unregistered = snapshot.partner_amm::<A>(market, &partner.pubkey())?;
	let mut registered = Fixtures::load(&fixtures_dir(market))?;
	registered.register_partner(&partner.pubkey())?;
	let amm = registered.partner_amm::<A>(market, &partner.pubkey())?;
	assert!(amm.is_active());
	assert!(unregistered.is_active());

	let mints = amm.get_reserve_mints();
	let [base, quote] = mints.as_slice() else {
		return Err("expected two reserve mints".into());
	};
	let refusal = format!(
		"partner {} is not registered on this market's platform",
		partner.pubkey()
	);
	assert_eq!(
		unregistered
			.quote(&exact_in(*quote, *base, 10_000_000))
			.err()
			.ok_or("quoted for an unregistered partner")?
			.to_string(),
		refusal
	);

	let mut harness =
		Harness::new(&snapshot, &amm)?.with_partner(Some(&partner));
	let source = harness.funded_ata(quote, 10_000_000)?;
	let destination = harness.funded_ata(base, 0)?;
	let params = harness
		.swap_params((SwapMode::ExactIn, *quote, *base), (source, destination));
	assert_eq!(
		unregistered
			.get_swap_and_account_metas(&params)
			.err()
			.ok_or("built a swap for an unregistered partner")?
			.to_string(),
		refusal
	);
	let (instruction, swap) = swap_instruction(&amm, &params)?;
	match harness.send(Instruction {
		data: A::data(&swap, SwapMode::ExactIn, (10_000_000, 0))?,
		..instruction
	}) {
		Ok(_) => {
			Err("the program took a swap from an unregistered partner".into())
		}
		Err(error)
			if error.to_string().contains(PARTNER_CONFIG_UNINITIALIZED) =>
		{
			assert_eq!(harness.token_amount(&source)?, 10_000_000);
			Ok(())
		}
		Err(error) => Err(format!("unexpected failure: {error}").into()),
	}
}

const fn exact_in(
	input_mint: Pubkey,
	output_mint: Pubkey,
	amount: u64,
) -> QuoteParams {
	QuoteParams {
		amount,
		input_mint,
		output_mint,
		swap_mode: SwapMode::ExactIn,
		fee_mode: FeeMode::Normal,
	}
}

/// `amount * numerator / denominator`, floored.
fn scale(amount: u64, (numerator, denominator): (u64, u64)) -> TestResult<u64> {
	Ok(amount
		.checked_mul(numerator)
		.and_then(|scaled| scaled.checked_div(denominator))
		.ok_or("scale overflow")?)
}

/// The `KeyedAccount.params` that route a market as `partner`.
fn partner_params(partner: &Pubkey) -> serde_json::Value {
	json!({ "partner": partner.to_string() })
}

/// A send.fun program binary, at the path where the kit reads and dumps it.
struct ProgramBinary {
	path: &'static str,
	deleted: Once,
}

static LAUNCHPAD_BINARY: ProgramBinary = ProgramBinary {
	path: "tests/fixtures/send.fun_launchpad.so",
	deleted: Once::new(),
};
static DEX_BINARY: ProgramBinary = ProgramBinary {
	path: "tests/fixtures/send.fun_amm.so",
	deleted: Once::new(),
};

impl ProgramBinary {
	/// Deletes the file once when `REFRESH` and `RPC` are set. The kit dumps a
	/// program only where no file exists. Panics if the delete fails for a
	/// reason other than a missing file.
	fn redump_on_refresh(&self) {
		if std::env::var_os("REFRESH").is_none()
			|| std::env::var_os("RPC").is_none()
		{
			return;
		}
		self.deleted.call_once(|| {
			if let Err(error) = std::fs::remove_file(self.path) {
				assert_eq!(
					error.kind(),
					ErrorKind::NotFound,
					"remove {}: {error}",
					self.path
				);
			}
		});
	}
}

/// The kit's test for `pool`, routed as the default partner. Every snapshot has
/// the default partner's config.
fn pool_test(pool: Pubkey) -> PoolTest {
	PoolTest {
		params: Some(partner_params(&DEFAULT_PARTNER)),
		..PoolTest::new(pool)
	}
}

/// The kit's fixture layout: `tests/fixtures/accounts/<market>`, relative to
/// the crate root, where cargo runs tests.
fn fixtures_dir(market: &Pubkey) -> PathBuf {
	Path::new("tests/fixtures/accounts").join(market.to_string())
}

/// One market's snapshot, as the kit saves it: a bincode `Account` in each
/// `<pubkey>.bin`.
struct Fixtures(BTreeMap<Pubkey, Account>);

impl AccountProvider for Fixtures {
	fn get(&self, pubkey: &Pubkey) -> Option<impl ReadableAccount + use<'_>> {
		self.0.get(pubkey).cloned()
	}
}

impl Fixtures {
	fn load(dir: &Path) -> TestResult<Self> {
		let mut accounts = BTreeMap::new();
		for entry in std::fs::read_dir(dir)? {
			let path = entry?.path();
			let key = path
				.file_stem()
				.and_then(|stem| stem.to_str())
				.ok_or("fixture file name is not a pubkey")?
				.parse()?;
			accounts
				.insert(key, bincode::deserialize_from(File::open(&path)?)?);
		}
		Ok(Self(accounts))
	}

	fn account(&self, pubkey: &Pubkey) -> TestResult<&Account> {
		Ok(self
			.0
			.get(pubkey)
			.ok_or_else(|| format!("{pubkey} not in the snapshot"))?)
	}

	fn account_mut(&mut self, pubkey: &Pubkey) -> TestResult<&mut Account> {
		Ok(self
			.0
			.get_mut(pubkey)
			.ok_or_else(|| format!("{pubkey} not in the snapshot"))?)
	}

	fn clock(&self) -> TestResult<Clock> {
		Ok(bincode::deserialize(&self.account(&CLOCK_SYSVAR)?.data)?)
	}

	fn set_clock(&mut self, unix_timestamp: i64) -> TestResult {
		let clock = Clock {
			unix_timestamp,
			..self.clock()?
		};
		self.account_mut(&CLOCK_SYSVAR)?.data = bincode::serialize(&clock)?;
		Ok(())
	}

	fn curve(&self, key: &Pubkey) -> TestResult<BondingCurve> {
		Ok(BondingCurve::from_bytes(&self.account(key)?.data)?)
	}

	fn pool(&self, key: &Pubkey) -> TestResult<Pool> {
		Ok(Pool::from_bytes(&self.account(key)?.data)?)
	}

	fn edit_curve(
		&mut self,
		key: &Pubkey,
		change: impl FnOnce(&mut BondingCurve),
	) -> TestResult {
		let mut curve = self.curve(key)?;
		change(&mut curve);
		self.account_mut(key)?.data = borsh::to_vec(&curve)?;
		Ok(())
	}

	fn edit_pool(
		&mut self,
		key: &Pubkey,
		change: impl FnOnce(&mut Pool),
	) -> TestResult {
		let mut pool = self.pool(key)?;
		change(&mut pool);
		self.account_mut(key)?.data = borsh::to_vec(&pool)?;
		Ok(())
	}

	fn token_amount(&self, account: &Pubkey) -> TestResult<u64> {
		let data = &self.account(account)?.data;
		let amount = data.get(64..72).ok_or("token account too short")?;
		Ok(u64::from_le_bytes(amount.try_into()?))
	}

	/// Runs the program's exact-in trade on this snapshot. Replaces each
	/// account with its state after the trade. Fails if the trade closes an
	/// account.
	fn trade<A: NativeData>(
		&mut self,
		market: &Pubkey,
		(input_mint, output_mint, amount): (Pubkey, Pubkey, u64),
	) -> TestResult {
		let amm = self.amm::<A>(market)?;
		let mut harness = Harness::new(self, &amm)?;
		let source = harness.funded_ata(&input_mint, amount)?;
		let destination = harness.funded_ata(&output_mint, 0)?;
		let (instruction, swap) = swap_instruction(
			&amm,
			&harness.swap_params(
				(SwapMode::ExactIn, input_mint, output_mint),
				(source, destination),
			),
		)?;
		harness.send(Instruction {
			data: A::data(&swap, SwapMode::ExactIn, (amount, 0))?,
			..instruction
		})?;

		let mut traded = BTreeMap::new();
		for key in self.0.keys() {
			let account = harness
				.svm
				.get_account(key)
				.ok_or_else(|| format!("{key} is gone after the trade"))?;
			traded.insert(*key, account);
		}
		self.0 = traded;
		Ok(())
	}

	/// Arms `mint`'s hook with no list. Gives `vault` the
	/// `TransferHookAccount` extension, which Token-2022 sets during a
	/// transfer. A user's ATA gets it at creation.
	fn arm_hook(&mut self, mint: &Pubkey, vault: &Pubkey) -> TestResult {
		// An authority of all zeroes: nobody can change the hook.
		let hook = [[0; 32], HOOK_PROGRAM.to_bytes()].concat();
		append_extension(
			self.account_mut(mint)?,
			TRANSFER_HOOK_EXTENSION,
			&hook,
		)?;
		append_extension(
			self.account_mut(vault)?,
			TRANSFER_HOOK_ACCOUNT_EXTENSION,
			&[0],
		)
	}

	/// Sets the program of `mint`'s `TransferHook` extension to `program`. It
	/// adds no extension: the mint's vaults and ATAs already have
	/// `TransferHookAccount`.
	fn point_hook(&mut self, mint: &Pubkey, program: &Pubkey) -> TestResult {
		let payload = extension_payload(
			&mut self.account_mut(mint)?.data,
			TRANSFER_HOOK_EXTENSION,
		)?;
		// `TransferHook`: the authority, then the program.
		payload
			.get_mut(32..64)
			.ok_or("transfer hook extension too short")?
			.copy_from_slice(program.as_ref());
		Ok(())
	}

	/// `PausableConfig`: the authority, then the `paused` flag that
	/// `MintState::load` reads.
	fn pause_mint(&mut self, mint: &Pubkey) -> TestResult {
		let paused = [[0; 32].as_slice(), &[1]].concat();
		append_extension(
			self.account_mut(mint)?,
			PAUSABLE_CONFIG_EXTENSION,
			&paused,
		)
	}

	/// [`Fixtures::arm_hook`], plus the [`hook_list`] that the extras resolve
	/// from.
	fn install_hook(&mut self, mint: &Pubkey, vault: &Pubkey) -> TestResult {
		self.arm_hook(mint, vault)?;
		self.set_list(mint, &hook_list()?)
	}

	/// Writes `list` as `mint`'s `ExtraAccountMetaList` under the hook program.
	fn set_list(&mut self, mint: &Pubkey, list: &[u8]) -> TestResult {
		let (validation, _) = Pubkey::find_program_address(
			&[b"extra-account-metas", mint.as_ref()],
			&HOOK_PROGRAM,
		);
		self.0.insert(
			validation,
			Account {
				lamports: 10_000_000,
				data: list.to_vec(),
				owner: HOOK_PROGRAM,
				executable: false,
				rent_epoch: 0,
			},
		);
		Ok(())
	}

	/// Gives `mint` [`BASE_TRANSFER_FEE`]. Gives `vault` the
	/// `TransferFeeAmount` extension, which holds the withheld fee. A user's
	/// ATA gets it from the ATA program.
	fn install_base_transfer_fee(
		&mut self,
		mint: &Pubkey,
		vault: &Pubkey,
	) -> TestResult {
		let config =
			transfer_fee_config(self.clock()?.epoch, BASE_TRANSFER_FEE);
		append_extension(
			self.account_mut(mint)?,
			TRANSFER_FEE_CONFIG_EXTENSION,
			&config,
		)?;
		append_extension(
			self.account_mut(vault)?,
			TRANSFER_FEE_AMOUNT_EXTENSION,
			&[0; 8],
		)
	}

	/// `mint`'s transfer fee at the snapshot's epoch.
	fn transfer_fee(&self, mint: &Pubkey) -> TestResult<Option<MintFee>> {
		let epoch = self.clock()?.epoch;
		let account = self.account(mint)?;
		Ok(mint_fee_at_epoch(&account.data, &account.owner, epoch)?)
	}

	/// Moves the WSOL pool onto [`CURVE_PRELWG`]'s mints, as the pool that the
	/// curve migrates into. The mints and the clock come from the curve's
	/// snapshot, whose epoch charges [`PRELWG`]'s fee.
	fn requote_pool_on_prelwg(&mut self, curve_fixtures: &Self) -> TestResult {
		let curve = curve_fixtures.curve(&CURVE_PRELWG)?;
		let (key, bump) = find_pool_pda(&curve.base_mint, &curve.quote_mint);
		let mut pool = self.pool(&POOL_WSOL)?;
		let pool_account = self
			.0
			.remove(&POOL_WSOL)
			.ok_or("pool not in the snapshot")?;

		let mut vault_at = |template: &Pubkey, old: &Pubkey, mint: &Pubkey| {
			let amount = self
				.0
				.remove(old)
				.ok_or("vault not in the snapshot")?
				.data
				.get(64..72)
				.ok_or("token account too short")?
				.to_vec();
			let mut vault = curve_fixtures.account(template)?.clone();
			vault
				.data
				.get_mut(32..64)
				.ok_or("token account too short")?
				.copy_from_slice(key.as_ref());
			vault
				.data
				.get_mut(64..72)
				.ok_or("token account too short")?
				.copy_from_slice(&amount);
			let (address, _) =
				find_associated_token_pda(&key, mint, &TOKEN_2022_PROGRAM);
			self.0.insert(address, vault);
			TestResult::Ok(address)
		};
		let base_vault =
			vault_at(&curve.base_vault, &pool.base_vault, &curve.base_mint)?;
		let quote_vault =
			vault_at(&curve.quote_vault, &pool.quote_vault, &curve.quote_mint)?;

		for copied in [curve.base_mint, curve.quote_mint, CLOCK_SYSVAR] {
			self.0
				.insert(copied, curve_fixtures.account(&copied)?.clone());
		}
		pool.bump = bump;
		pool.base_mint = curve.base_mint;
		pool.base_decimals = curve.base_decimals;
		pool.quote_mint = curve.quote_mint;
		pool.quote_decimals = curve.quote_decimals;
		pool.base_vault = base_vault;
		pool.quote_vault = quote_vault;
		self.0.insert(
			key,
			Account {
				data: borsh::to_vec(&pool)?,
				..pool_account
			},
		);
		Ok(())
	}

	fn edit_partner_config(
		&mut self,
		key: &Pubkey,
		change: impl FnOnce(&mut PartnerConfig),
	) -> TestResult {
		let mut config = PartnerConfig::from_bytes(&self.account(key)?.data)?;
		change(&mut config);
		self.account_mut(key)?.data = borsh::to_vec(&config)?;
		Ok(())
	}

	/// Registers `partner` at [`SIGNED_PARTNER_FEES`] on the default partner's
	/// platform, as nexus's `add_partner` does. Every snapshot market is on
	/// that platform. Returns the config address.
	fn register_partner(&mut self, partner: &Pubkey) -> TestResult<Pubkey> {
		self.register_partner_at(partner, SIGNED_PARTNER_FEES)
	}

	fn register_partner_at(
		&mut self,
		partner: &Pubkey,
		(launchpad, dex): (LaunchpadFees, DexFees),
	) -> TestResult<Pubkey> {
		let template = self.account(&DEFAULT_PARTNER_CONFIG)?.clone();
		let default = PartnerConfig::from_bytes(&template.data)?;
		let (key, bump) =
			find_partner_config_pda(&default.platform_config, partner);
		let config = PartnerConfig {
			bump,
			authority: *partner,
			partner: *partner,
			fee_preset: CUSTOM_FEES_PRESET,
			launchpad,
			dex,
			..default
		};
		self.0.insert(
			key,
			Account {
				data: borsh::to_vec(&config)?,
				..template
			},
		);
		Ok(key)
	}

	/// The AMM as Jupiter holds it: built from the market account, routed as
	/// the default partner, then updated from the snapshot, on the snapshot's
	/// clock.
	fn amm<A: Amm>(&self, market: &Pubkey) -> TestResult<A> {
		self.partner_amm(market, &DEFAULT_PARTNER)
	}

	/// [`Fixtures::amm`], routed as `partner`.
	fn partner_amm<A: Amm>(
		&self,
		market: &Pubkey,
		partner: &Pubkey,
	) -> TestResult<A> {
		let mut amm = A::from_keyed_account(
			&KeyedAccount {
				key: *market,
				account: self.account(market)?.clone(),
				params: Some(partner_params(partner)),
			},
			&AmmContext {
				clock_ref: ClockRef::from(self.clock()?),
			},
		)?;
		amm.update(self)?;
		Ok(amm)
	}
}

/// Each call gets its own directory. Parallel tests with the same `name`
/// would delete each other's accounts.
fn derive(
	market: &Pubkey,
	name: &str,
	change: impl FnOnce(&mut Fixtures) -> TestResult,
) -> TestResult<PathBuf> {
	static CALLS: AtomicUsize = AtomicUsize::new(0);

	let mut fixtures = Fixtures::load(&fixtures_dir(market))?;
	change(&mut fixtures)?;

	let call = CALLS.fetch_add(1, Ordering::Relaxed);
	let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
		.join("derived")
		.join(format!("{name}-{call}"))
		.join(market.to_string());
	if dir.exists() {
		std::fs::remove_dir_all(&dir)?;
	}
	std::fs::create_dir_all(&dir)?;
	for (pubkey, account) in &fixtures.0 {
		bincode::serialize_into(
			File::create(dir.join(format!("{pubkey}.bin")))?,
			account,
		)?;
	}
	Ok(dir)
}

/// An AMM's `exact_in_data` or `exact_out_data`.
type DataBuilder = fn(&Swap, u64, u64) -> Result<Vec<u8>, AmmError>;

/// The instruction data for either mode, for each program.
trait NativeData: Amm {
	const EXACT_IN: DataBuilder;
	const EXACT_OUT: DataBuilder;

	/// Exact-in: `(amount_in, min_amount_out)`. Exact-out: `(amount_out,
	/// max_amount_in)`.
	fn data(
		swap: &Swap,
		swap_mode: SwapMode,
		(amount, limit): (u64, u64),
	) -> TestResult<Vec<u8>> {
		let build = match swap_mode {
			SwapMode::ExactIn => Self::EXACT_IN,
			SwapMode::ExactOut => Self::EXACT_OUT,
		};
		Ok(build(swap, amount, limit)?)
	}
}

impl NativeData for SendfunLaunchpadAmm {
	const EXACT_IN: DataBuilder = Self::exact_in_data;
	const EXACT_OUT: DataBuilder = Self::exact_out_data;
}

impl NativeData for SendfunDexAmm {
	const EXACT_IN: DataBuilder = Self::exact_in_data;
	const EXACT_OUT: DataBuilder = Self::exact_out_data;
}

/// Swaps to run in the program on one market's snapshot:
/// `(mode, input mint, output mint, amount)`. `amount` is the input for
/// exact-in and the output for exact-out.
struct NativeTest {
	market: Pubkey,
	fixtures_dir: PathBuf,
	swaps: Vec<(SwapMode, Pubkey, Pubkey, u64)>,
	/// Every swap must run [`HOOK_PROGRAM`] exactly once.
	hooked: bool,
	/// Routes every swap as this partner, not the default partner. The
	/// partner signs each swap.
	partner: Option<Keypair>,
}

impl NativeTest {
	fn new(
		market: Pubkey,
		fixtures_dir: PathBuf,
		swaps: &[(SwapMode, Pubkey, Pubkey, u64)],
	) -> Self {
		Self {
			market,
			fixtures_dir,
			swaps: swaps.to_vec(),
			hooked: false,
			partner: None,
		}
	}
}

/// Funds the source with exactly `quote.in_amount`, then runs the program
/// instruction bounded by the quote. The source must drain to zero. The
/// destination must rise by `quote.out_amount`.
fn assert_native<A: NativeData>(test: &NativeTest) -> TestResult {
	let fixtures = Fixtures::load(&test.fixtures_dir)?;
	let partner = test.partner.as_ref();
	let amm = fixtures.partner_amm::<A>(
		&test.market,
		&partner.map_or(DEFAULT_PARTNER, Signer::pubkey),
	)?;

	for &(swap_mode, input_mint, output_mint, amount) in &test.swaps {
		let quote = amm.quote(&QuoteParams {
			amount,
			input_mint,
			output_mint,
			swap_mode,
			fee_mode: FeeMode::Normal,
		})?;
		let (amounts, pinned) = match swap_mode {
			SwapMode::ExactIn => {
				((quote.in_amount, quote.out_amount), quote.in_amount)
			}
			SwapMode::ExactOut => {
				((quote.out_amount, quote.in_amount), quote.out_amount)
			}
		};
		assert_eq!(pinned, amount, "{swap_mode:?} must pin the requested leg");

		let mut harness = Harness::new(&fixtures, &amm)?.with_partner(partner);
		let source = harness.funded_ata(&input_mint, quote.in_amount)?;
		let destination = harness.funded_ata(&output_mint, 0)?;
		let (instruction, swap) = swap_instruction(
			&amm,
			&harness.swap_params(
				(swap_mode, input_mint, output_mint),
				(source, destination),
			),
		)?;
		let logs = harness
			.send(Instruction {
				data: A::data(&swap, swap_mode, amounts)?,
				..instruction
			})
			.map_err(|error| {
				format!(
					"{swap_mode:?} {input_mint} -> {output_mint} {amount}: \
					 {error}"
				)
			})?;
		assert_eq!(
			hook_invocations(&logs),
			if test.hooked { vec![3] } else { vec![] },
			"{swap_mode:?} {input_mint} -> {output_mint} {amount}: hook runs"
		);

		assert_eq!(
			harness.token_amount(&destination)?,
			quote.out_amount,
			"{swap_mode:?} {input_mint} -> {output_mint} {amount}: \
			 destination delta"
		);
		assert_eq!(
			harness.token_amount(&source)?,
			0,
			"{swap_mode:?} {input_mint} -> {output_mint} {amount}: the \
			 program took other than quote.in_amount {}",
			quote.in_amount
		);
	}
	Ok(())
}

/// Runs an unbounded exact-in trade that the adapter refused to quote.
/// Returns the program's error. Fails if the program accepts the trade.
fn native_exact_in_error<A: NativeData>(
	fixtures: &Fixtures,
	amm: &A,
	params: &QuoteParams,
) -> TestResult<String> {
	let mut harness = Harness::new(fixtures, amm)?;
	let source = harness.funded_ata(&params.input_mint, params.amount)?;
	let destination = harness.funded_ata(&params.output_mint, 0)?;
	let (instruction, swap) = swap_instruction(
		amm,
		&harness.swap_params(
			(SwapMode::ExactIn, params.input_mint, params.output_mint),
			(source, destination),
		),
	)?;
	match harness.send(Instruction {
		data: A::data(&swap, SwapMode::ExactIn, (params.amount, 0))?,
		..instruction
	}) {
		Ok(_) => Err("the program accepted a trade the adapter refused".into()),
		Err(error) => Ok(error.to_string()),
	}
}

/// The AMM's instruction for a swap, with empty data, and the `Swap` tag.
fn swap_instruction(
	amm: &impl Amm,
	params: &SwapParams<'_, '_>,
) -> TestResult<(Instruction, Swap)> {
	let metas = amm.get_swap_and_account_metas(params)?;
	Ok((
		Instruction {
			program_id: amm.program_id(),
			accounts: metas.account_metas,
			data: Vec::new(),
		},
		metas.swap,
	))
}

/// A new `LiteSVM` with one snapshot, the AMM's program and its dependencies,
/// and a funded user who signs everything.
struct Harness<'a> {
	svm: LiteSVM,
	fixtures: &'a Fixtures,
	user: Keypair,
	/// Signs each instruction that names it as a signer.
	partner: Option<&'a Keypair>,
}

impl<'a> Harness<'a> {
	fn new(fixtures: &'a Fixtures, amm: &impl Amm) -> TestResult<Self> {
		// The kit's names: the label for the AMM, and each dependency's own
		// name.
		let mut svm = LiteSVM::new();
		let programs = std::iter::once((
			amm.program_id(),
			amm.label().to_lowercase().replace(' ', "_"),
		))
		.chain(amm.program_dependencies());
		for (program_id, name) in programs {
			let mut program = Vec::new();
			File::open(Path::new("tests/fixtures").join(format!("{name}.so")))?
				.read_to_end(&mut program)?;
			svm.add_program(program_id, &program)
				.map_err(|error| format!("add program {name}: {error:?}"))?;
		}
		for (pubkey, account) in &fixtures.0 {
			svm.set_account(*pubkey, account.clone())
				.map_err(|error| format!("seed {pubkey}: {error:?}"))?;
		}
		let user = Keypair::new();
		svm.airdrop(&user.pubkey(), 100_000_000_000)
			.map_err(|error| format!("airdrop: {error:?}"))?;
		Ok(Self {
			svm,
			fixtures,
			user,
			partner: None,
		})
	}

	/// The same harness. It also signs as `partner` when an instruction names
	/// it as a signer.
	fn with_partner(self, partner: Option<&'a Keypair>) -> Self {
		Self { partner, ..self }
	}

	/// Jupiter's swap params. The user is the transfer authority, the payer
	/// and the owner of both token accounts.
	fn swap_params(
		&self,
		(swap_mode, source_mint, destination_mint): (SwapMode, Pubkey, Pubkey),
		(source, destination): (Pubkey, Pubkey),
	) -> SwapParams<'static, 'static> {
		let user = self.user.pubkey();
		SwapParams {
			swap_mode,
			in_amount: 0,
			out_amount: 0,
			source_mint,
			destination_mint,
			source_token_account: source,
			destination_token_account: destination,
			token_transfer_authority: user,
			user,
			payer: user,
			quote_mint_to_referrer: None,
			jupiter_program_id: &JUPITER_PROGRAM,
			missing_dynamic_accounts_as_default: false,
		}
	}

	/// Returns the transaction's logs.
	fn send(&mut self, instruction: Instruction) -> TestResult<Vec<String>> {
		let partner = self.partner.filter(|partner| {
			instruction
				.accounts
				.iter()
				.any(|meta| meta.is_signer && meta.pubkey == partner.pubkey())
		});
		let signers: Vec<&Keypair> =
			std::iter::once(&self.user).chain(partner).collect();
		let transaction = Transaction::new_signed_with_payer(
			&[
				ComputeBudgetInstruction::set_compute_unit_limit(1_400_000),
				instruction,
			],
			Some(&self.user.pubkey()),
			&signers,
			self.svm.latest_blockhash(),
		);
		let sent =
			self.svm.send_transaction(transaction).map_err(|failed| {
				format!("{:?}: {:?}", failed.err, failed.meta.logs)
			})?;
		Ok(sent.logs)
	}

	/// The user's ATA for `mint`. The ATA program creates it, then the harness
	/// sets its balance to `amount`.
	fn funded_ata(&mut self, mint: &Pubkey, amount: u64) -> TestResult<Pubkey> {
		let token_program = self.fixtures.account(mint)?.owner;
		let owner = self.user.pubkey();
		let (ata, _) = Pubkey::find_program_address(
			&[owner.as_ref(), token_program.as_ref(), mint.as_ref()],
			&ATA_PROGRAM,
		);
		self.send(Instruction {
			program_id: ATA_PROGRAM,
			accounts: vec![
				AccountMeta::new(owner, true),
				AccountMeta::new(ata, false),
				AccountMeta::new_readonly(owner, false),
				AccountMeta::new_readonly(*mint, false),
				AccountMeta::new_readonly(SYSTEM_PROGRAM, false),
				AccountMeta::new_readonly(token_program, false),
			],
			data: vec![0],
		})?;

		if amount > 0 {
			let mut account =
				self.svm.get_account(&ata).ok_or("ATA not created")?;
			// `amount` follows `mint` and `owner` in both token programs.
			account
				.data
				.get_mut(64..72)
				.ok_or("token account too short")?
				.copy_from_slice(&amount.to_le_bytes());
			if *mint == WSOL {
				// Lamports must back a native account's balance.
				account.lamports = account
					.lamports
					.checked_add(amount)
					.ok_or("lamports overflow")?;
			}
			self.svm
				.set_account(ata, account)
				.map_err(|error| format!("fund {ata}: {error:?}"))?;
		}
		Ok(ata)
	}

	fn token_amount(&self, account: &Pubkey) -> TestResult<u64> {
		let data = self
			.svm
			.get_account(account)
			.ok_or("token account missing")?
			.data;
		let amount = data.get(64..72).ok_or("token account too short")?;
		Ok(u64::from_le_bytes(amount.try_into()?))
	}
}
