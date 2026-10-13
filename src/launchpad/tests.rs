use super::*;

use std::collections::BTreeMap;

use jupiter_amm_interface::SwapMode;
use sendfun_sdk::constants::{
	DEFAULT_PARTNER, NEXUS_PROGRAM_ID, SYSTEM_PROGRAM_ID,
	TOKEN_2022_PROGRAM_ID, TOKEN_PROGRAM_ID,
};
use sendfun_sdk::launchpad::accounts::BONDING_CURVE_DISCRIMINATOR;
use sendfun_sdk::launchpad::pda::find_bonding_curve_pda;
use sendfun_sdk::launchpad::trade::{
	BuildBuyExactInParams, BuildBuyExactOutParams, BuildSellExactInParams,
	BuildSellExactOutParams, build_buy_exact_in_instruction,
	build_buy_exact_out_instruction, build_sell_exact_in_instruction,
	build_sell_exact_out_instruction,
};
use sendfun_sdk::nexus::pda::find_partner_config_pda;
use sendfun_sdk::transfer_fee::{TransferFee, TransferFeeConfig};
use sendfun_sdk::utils::find_associated_token_pda;
use solana_account::Account;
use solana_instruction::{AccountMeta, Instruction};

use sendfun_sdk::transfer_hook::{MarketHooks, MintState};
use sendfun_sdk::utils::TradeDirection;
use spl_tlv_account_resolution::seeds::Seed;

use crate::market::{FIXED_ACCOUNTS_LEN, Mints};
use crate::test_support::{
	AUTHORITY, DESTINATION, HOOK_PROGRAM, NOW, PARTNER, PAYER, SOURCE,
	TestAccounts, account, amm_context, assert_accounts_len_matches_every_swap,
	bare_mint, buy, dex_fees, fixed, hook_list, hook_metas, hooked_mint,
	invalid_partner_params, launchpad_fees, list, mint_state,
	partner_config_account, partner_params, pda, pubkey_data, quote_error,
	sell, swap_error, swap_params, validation,
};

const INITIAL_VIRTUAL_QUOTE: u64 = 30_000_000_000;
const INITIAL_VIRTUAL_BASE: u64 = 1_000_000_000_000_000;
const INITIAL_REAL_BASE: u64 = 500_000_000_000_000;
const BASE_MINT: Pubkey = Pubkey::new_from_array([20; 32]);
const QUOTE_MINT: Pubkey = Pubkey::new_from_array([21; 32]);

fn curve_key() -> Pubkey {
	find_bonding_curve_pda(&BASE_MINT, &QUOTE_MINT).0
}

/// The vaults are ATAs, as `create_token` makes them. The quote vault is under
/// SPL Token.
fn make_curve() -> BondingCurve {
	let key = curve_key();
	BondingCurve {
		discriminator: BONDING_CURVE_DISCRIMINATOR,
		version: 1,
		bump: 254,
		status: BondingCurveStatus::Funding,
		base_mint: BASE_MINT,
		base_decimals: 6,
		base_supply: 1_000_000_000_000_000,
		initial_virtual_base: INITIAL_VIRTUAL_BASE,
		initial_virtual_quote: INITIAL_VIRTUAL_QUOTE,
		initial_real_base: INITIAL_REAL_BASE,
		quote_mint: QUOTE_MINT,
		quote_decimals: 9,
		coin_creator: Pubkey::new_from_array([22; 32]),
		padding0: [0x1717_1717_1717_1717; 4],
		partner: Pubkey::new_from_array([24; 32]),
		base_vault: find_associated_token_pda(
			&key,
			&BASE_MINT,
			&TOKEN_2022_PROGRAM_ID,
		)
		.0,
		quote_vault: find_associated_token_pda(
			&key,
			&QUOTE_MINT,
			&TOKEN_PROGRAM_ID,
		)
		.0,
		virtual_base_reserves: INITIAL_VIRTUAL_BASE,
		virtual_quote_reserves: INITIAL_VIRTUAL_QUOTE,
		real_base_reserves: INITIAL_REAL_BASE,
		real_quote_reserves: 0,
		created_at: 0,
		protocol_owed: 0,
		creator_owed: 0,
		// Not the default platform, so a fallback to it fails the tests.
		platform_config: Pubkey::new_from_array([27; 32]),
		creator_fee_mode: 0,
		creator_fee_bps: 0,
		dex_creator_fee_bps: 0,
		reserved: [0; 256],
	}
}

fn keyed_account(
	curve: &BondingCurve,
	params: Option<serde_json::Value>,
) -> KeyedAccount {
	KeyedAccount {
		key: curve_key(),
		account: account(LAUNCHPAD_PROGRAM_ID, borsh::to_vec(curve).unwrap()),
		params,
	}
}

fn make_amm(curve: &BondingCurve) -> SendfunLaunchpadAmm {
	make_partner_amm(curve, &DEFAULT_PARTNER)
}

fn make_partner_amm(
	curve: &BondingCurve,
	partner: &Pubkey,
) -> SendfunLaunchpadAmm {
	let mut amm = SendfunLaunchpadAmm::from_keyed_account(
		&keyed_account(curve, Some(partner_params(partner))),
		&amm_context(),
	)
	.unwrap();
	amm.fees = Some(launchpad_fees(100));
	amm.market.prime(&Mints {
		base: mint_state(TOKEN_2022_PROGRAM_ID),
		quote: mint_state(TOKEN_PROGRAM_ID),
	});
	amm
}

fn set_quote_mint(amm: &mut SendfunLaunchpadAmm, state: MintState) {
	amm.market.mints.as_mut().unwrap().quote = state;
}

fn transfer_fee(basis_points: u16) -> TransferFeeConfig {
	let entry = TransferFee {
		epoch: 0.into(),
		maximum_fee: u64::MAX.into(),
		transfer_fee_basis_points: basis_points.into(),
	};
	TransferFeeConfig {
		older_transfer_fee: entry,
		newer_transfer_fee: entry,
		..TransferFeeConfig::default()
	}
}

#[test]
fn buy_exact_in() {
	let amm = make_amm(&make_curve());
	let q = amm
		.quote(&buy(&amm.market, 1_000_000_000, SwapMode::ExactIn))
		.unwrap();

	// The same reserves and 1% fee as the SDK quote tests.
	assert_eq!(q.in_amount, 1_000_000_000);
	assert_eq!(q.out_amount, 31_945_788_964_181);
	assert_eq!(q.fee_amount, 10_000_000);
	assert_eq!(q.fee_mint, QUOTE_MINT);
	assert_eq!(q.fee_pct, rust_decimal::Decimal::new(1, 2));
}

#[test]
fn buy_exact_out() {
	let amm = make_amm(&make_curve());
	let q = amm
		.quote(&buy(&amm.market, 10_000_000_000_000, SwapMode::ExactOut))
		.unwrap();

	assert_eq!(q.in_amount, 306_091_217);
	assert_eq!(q.out_amount, 10_000_000_000_000);
	assert_eq!(q.fee_amount, 3_060_913);
}

#[test]
fn sell_exact_in() {
	let mut curve = make_curve();
	curve.real_quote_reserves = 5_000_000_000;
	let amm = make_amm(&curve);
	let q = amm
		.quote(&sell(&amm.market, 10_000_000_000_000, SwapMode::ExactIn))
		.unwrap();

	assert_eq!(q.in_amount, 10_000_000_000_000);
	assert_eq!(q.out_amount, 294_059_404);
	assert_eq!(q.fee_amount, 2_970_298);
	assert_eq!(q.fee_mint, QUOTE_MINT);
}

#[test]
fn sell_exact_out() {
	let mut curve = make_curve();
	curve.real_quote_reserves = 5_000_000_000;
	let amm = make_amm(&curve);
	let q = amm
		.quote(&sell(&amm.market, 1_000_000_000, SwapMode::ExactOut))
		.unwrap();

	assert_eq!(q.in_amount, 34_843_205_607_004);
	assert_eq!(q.out_amount, 1_000_000_000);
	assert_eq!(q.fee_amount, 10_101_011);
}

/// The program refuses a sell whose vault debit, payout plus fee, exceeds
/// `real_quote_reserves`.
#[test]
fn a_sell_past_the_quote_held_does_not_quote() {
	let mut curve = make_curve();
	// 294_059_404 to the seller plus the 2_970_298 fee.
	curve.real_quote_reserves = 297_029_702;
	let amm = make_amm(&curve);
	let params = sell(&amm.market, 10_000_000_000_000, SwapMode::ExactIn);
	assert_eq!(amm.quote(&params).unwrap().out_amount, 294_059_404);

	curve.real_quote_reserves = 297_029_701;
	let amm = make_amm(&curve);
	assert_eq!(
		quote_error(&amm, &params),
		"sell exceeds the quote the curve holds"
	);
}

/// Exact-in quotes the capped fill. Exact-out does not quote: the program
/// refuses it.
#[test]
fn a_buy_past_the_supply_left_caps_the_fill() {
	let mut curve = make_curve();
	curve.real_base_reserves = 31_945_788_964_181;
	let amm = make_amm(&curve);
	let one_sol = buy(&amm.market, 1_000_000_000, SwapMode::ExactIn);
	let exact_fill = buy(&amm.market, 31_945_788_964_181, SwapMode::ExactOut);
	assert_eq!(amm.quote(&one_sol).unwrap().out_amount, 31_945_788_964_181);
	assert_eq!(
		amm.quote(&exact_fill).unwrap().out_amount,
		31_945_788_964_181
	);

	curve.real_base_reserves = 31_945_788_964_180;
	let amm = make_amm(&curve);
	let q = amm.quote(&one_sol).unwrap();
	assert_eq!(q.in_amount, 1_000_000_000);
	assert_eq!(q.out_amount, 31_945_788_964_180);
	assert_eq!(
		quote_error(&amm, &exact_fill),
		"fill exceeds the supply left on the curve"
	);

	curve.real_base_reserves = 1_000_000_000_000;
	let amm = make_amm(&curve);
	let q = amm.quote(&one_sol).unwrap();
	assert_eq!(q.in_amount, 30_333_365);
	assert_eq!(q.out_amount, 1_000_000_000_000);
	assert_eq!(
		quote_error(
			&amm,
			&buy(&amm.market, 1_000_000_000_001, SwapMode::ExactOut)
		),
		"fill exceeds the supply left on the curve"
	);
}

/// A 20 bps quote-mint fee: only `998_000_000` reaches the vault. The platform
/// fee is priced on that amount.
#[test]
fn a_quote_mint_transfer_fee_prices_what_reaches_the_vault() {
	let mut amm = make_amm(&make_curve());
	set_quote_mint(
		&mut amm,
		MintState {
			transfer_fee: Some(transfer_fee(20)),
			..mint_state(TOKEN_2022_PROGRAM_ID)
		},
	);
	let q = amm
		.quote(&buy(&amm.market, 1_000_000_000, SwapMode::ExactIn))
		.unwrap();

	assert_eq!(q.in_amount, 1_000_000_000);
	assert_eq!(q.out_amount, 31_883_934_501_139);
	assert_eq!(q.fee_amount, 9_980_000);
}

/// `40_000` base units are worth one quote unit. The fee rounds up and takes
/// all of it.
#[test]
fn a_sell_that_delivers_nothing_does_not_quote() {
	let mut curve = make_curve();
	curve.real_quote_reserves = 5_000_000_000;
	let amm = make_amm(&curve);
	assert_eq!(
		quote_error(&amm, &sell(&amm.market, 40_000, SwapMode::ExactIn)),
		"swap delivers nothing"
	);
}

#[test]
fn a_paused_mint_refuses_quotes_and_swaps() {
	let mut amm = make_amm(&make_curve());
	assert!(amm.is_active());
	set_quote_mint(
		&mut amm,
		MintState {
			paused: true,
			..mint_state(TOKEN_2022_PROGRAM_ID)
		},
	);
	assert!(amm.is_active());
	assert_eq!(
		quote_error(&amm, &buy(&amm.market, 1_000_000_000, SwapMode::ExactIn)),
		"a mint on this market is paused"
	);
	// A swap built without quoting first refuses on the same ground.
	assert_eq!(
		swap_error(
			&amm,
			&swap_params(&amm.market, TradeDirection::Buy, SwapMode::ExactIn)
		),
		"a mint on this market is paused"
	);
}

/// Before the first `update`, the curve is active but does not quote.
#[test]
fn a_curve_stays_active_until_its_first_update() {
	let amm = SendfunLaunchpadAmm::from_keyed_account(
		&keyed_account(&make_curve(), Some(partner_params(&PARTNER))),
		&amm_context(),
	)
	.unwrap();

	assert!(amm.is_active());
	let not_updated = "market not updated: call `update` first";
	assert_eq!(
		quote_error(&amm, &buy(&amm.market, 1_000_000_000, SwapMode::ExactIn)),
		not_updated
	);
	assert_eq!(
		swap_error(
			&amm,
			&swap_params(&amm.market, TradeDirection::Buy, SwapMode::ExactIn)
		),
		not_updated
	);
}

/// A pending hook list keeps the market active.
#[test]
fn a_market_whose_hook_list_has_not_loaded_stays_active_but_does_not_quote() {
	let curve = make_curve();
	let mut amm = make_amm(&curve);
	let mut accounts = TestAccounts(BTreeMap::from([
		(
			amm.market.key,
			account(LAUNCHPAD_PROGRAM_ID, borsh::to_vec(&curve).unwrap()),
		),
		(
			amm.market.partner_config,
			partner_config_account(
				(curve.platform_config, DEFAULT_PARTNER),
				launchpad_fees(100),
				dex_fees(1),
			),
		),
		(curve.base_mint, bare_mint(TOKEN_2022_PROGRAM_ID)),
		(curve.quote_mint, hooked_mint(HOOK_PROGRAM)),
	]));

	amm.update(&accounts).unwrap();
	assert_eq!(
		quote_error(&amm, &buy(&amm.market, 1_000_000_000, SwapMode::ExactIn)),
		"transfer hook accounts not loaded: call `update` again"
	);
	assert!(amm.is_active());

	accounts
		.0
		.insert(validation(&QUOTE_MINT), account(HOOK_PROGRAM, hook_list()));
	amm.update(&accounts).unwrap();
	assert!(amm.is_active());
	assert_accounts_len_matches_every_swap(&amm, &amm.market);
}

/// With `missing_dynamic_accounts_as_default`, an unloaded list yields the
/// hook program and the list address in place of the extras.
/// `get_accounts_len` counts them.
#[test]
fn a_crawl_gets_stand_in_metas_for_a_hook_list_it_has_not_loaded() {
	let curve = make_curve();
	let mut amm = make_amm(&curve);
	let accounts = TestAccounts(BTreeMap::from([
		(
			amm.market.key,
			account(LAUNCHPAD_PROGRAM_ID, borsh::to_vec(&curve).unwrap()),
		),
		(
			amm.market.partner_config,
			partner_config_account(
				(curve.platform_config, DEFAULT_PARTNER),
				launchpad_fees(100),
				dex_fees(1),
			),
		),
		(curve.base_mint, bare_mint(TOKEN_2022_PROGRAM_ID)),
		(curve.quote_mint, hooked_mint(HOOK_PROGRAM)),
	]));
	amm.update(&accounts).unwrap();

	let mut params =
		swap_params(&amm.market, TradeDirection::Buy, SwapMode::ExactIn);
	assert_eq!(
		swap_error(&amm, &params),
		"transfer hook accounts not loaded: call `update` again"
	);

	params.missing_dynamic_accounts_as_default = true;
	let metas = amm
		.get_swap_and_account_metas(&params)
		.unwrap()
		.account_metas;
	assert_eq!(
		metas[FIXED_ACCOUNTS_LEN..]
			.iter()
			.map(|meta| meta.pubkey)
			.collect::<Vec<_>>(),
		vec![HOOK_PROGRAM, validation(&QUOTE_MINT)]
	);
	assert_eq!(metas.len(), amm.get_accounts_len());
}

/// Both legs share one trailing list. An account that both hooks share, such
/// as the hook program, appears once.
#[test]
fn two_hooks_sharing_a_program_are_counted_once() {
	let curve = make_curve();
	let mut amm = make_amm(&curve);
	let accounts = TestAccounts(BTreeMap::from([
		(
			amm.market.key,
			account(LAUNCHPAD_PROGRAM_ID, borsh::to_vec(&curve).unwrap()),
		),
		(
			amm.market.partner_config,
			partner_config_account(
				(curve.platform_config, DEFAULT_PARTNER),
				launchpad_fees(100),
				dex_fees(1),
			),
		),
		(curve.base_mint, hooked_mint(HOOK_PROGRAM)),
		(curve.quote_mint, hooked_mint(HOOK_PROGRAM)),
		(validation(&BASE_MINT), account(HOOK_PROGRAM, hook_list())),
		(validation(&QUOTE_MINT), account(HOOK_PROGRAM, hook_list())),
	]));
	// One `update` loads both lists. `update` reads a hook list when the
	// provider has it, whether or not `get_accounts_to_update` named it.
	amm.update(&accounts).unwrap();
	assert!(amm.is_active());

	assert_accounts_len_matches_every_swap(&amm, &amm.market);
}

/// A list that loads grows `get_accounts_len`. A cleared hook returns it to
/// the fixed layout.
#[test]
fn accounts_len_tracks_every_update_of_the_hooks() {
	let curve = make_curve();
	let mut amm = make_amm(&curve);
	let mut accounts = TestAccounts(BTreeMap::from([
		(
			amm.market.key,
			account(LAUNCHPAD_PROGRAM_ID, borsh::to_vec(&curve).unwrap()),
		),
		(
			amm.market.partner_config,
			partner_config_account(
				(curve.platform_config, DEFAULT_PARTNER),
				launchpad_fees(100),
				dex_fees(1),
			),
		),
		(curve.base_mint, bare_mint(TOKEN_2022_PROGRAM_ID)),
		(curve.quote_mint, hooked_mint(HOOK_PROGRAM)),
	]));

	amm.update(&accounts).unwrap();
	assert_eq!(amm.get_accounts_len(), 19);

	accounts
		.0
		.insert(validation(&QUOTE_MINT), account(HOOK_PROGRAM, hook_list()));
	amm.update(&accounts).unwrap();
	assert_eq!(amm.get_accounts_len(), 22);
	assert_accounts_len_matches_every_swap(&amm, &amm.market);

	accounts
		.0
		.insert(curve.quote_mint, bare_mint(TOKEN_2022_PROGRAM_ID));
	amm.update(&accounts).unwrap();
	assert_eq!(amm.get_accounts_len(), FIXED_ACCOUNTS_LEN);
	assert_accounts_len_matches_every_swap(&amm, &amm.market);
}

#[test]
fn a_hooked_quote_mint_trails_every_swap_with_its_accounts() {
	let curve = make_curve();
	let mut amm = make_amm(&curve);
	let mut accounts = TestAccounts(BTreeMap::from([
		(
			amm.market.key,
			account(LAUNCHPAD_PROGRAM_ID, borsh::to_vec(&curve).unwrap()),
		),
		(
			amm.market.partner_config,
			partner_config_account(
				(curve.platform_config, DEFAULT_PARTNER),
				launchpad_fees(100),
				dex_fees(1),
			),
		),
		(curve.base_mint, bare_mint(TOKEN_2022_PROGRAM_ID)),
		(curve.quote_mint, hooked_mint(HOOK_PROGRAM)),
	]));

	// The first `update` sees the hook but not its list. The market stays
	// active but does not quote.
	amm.update(&accounts).unwrap();
	assert!(amm.is_active());
	let one_sol = buy(&amm.market, 1_000_000_000, SwapMode::ExactIn);
	assert_eq!(
		quote_error(&amm, &one_sol),
		"transfer hook accounts not loaded: call `update` again"
	);
	assert_eq!(
		amm.get_accounts_to_update(),
		vec![
			amm.market.key,
			amm.market.partner_config,
			curve.base_mint,
			curve.quote_mint,
			validation(&QUOTE_MINT),
		]
	);

	accounts
		.0
		.insert(validation(&QUOTE_MINT), account(HOOK_PROGRAM, hook_list()));
	amm.update(&accounts).unwrap();
	assert_eq!(amm.quote(&one_sol).unwrap().out_amount, 31_945_788_964_181);
	assert_eq!(amm.get_accounts_len(), 22);
	assert_eq!(
		amm.program_dependencies(),
		vec![(HOOK_PROGRAM, HOOK_PROGRAM.to_string())]
	);

	let mut unhooked = amm.clone();
	unhooked.market.prime_hooks(MarketHooks::default());
	for (side, source, owner) in [
		(TradeDirection::Buy, SOURCE, AUTHORITY),
		(TradeDirection::Sell, curve.quote_vault, amm.market.key),
	] {
		for swap_mode in [SwapMode::ExactIn, SwapMode::ExactOut] {
			let params = swap_params(&amm.market, side, swap_mode);
			let metas = amm
				.get_swap_and_account_metas(&params)
				.unwrap()
				.account_metas;
			assert_eq!(metas.len(), amm.get_accounts_len());
			assert_eq!(
				metas[..17],
				unhooked
					.get_swap_and_account_metas(&params)
					.unwrap()
					.account_metas
			);
			assert_eq!(metas[17..], hook_metas(&QUOTE_MINT, &source, &owner));
		}
	}

	// A list that reads the transfer amount cannot route.
	accounts.0.insert(
		validation(&QUOTE_MINT),
		account(
			HOOK_PROGRAM,
			list(&[pda(
				&[Seed::InstructionData {
					index: 8,
					length: 8,
				}],
				false,
			)]),
		),
	);
	amm.update(&accounts).unwrap();
	assert!(amm.is_active());
	assert_eq!(
		quote_error(&amm, &one_sol),
		"transfer hook cannot be resolved ahead of the swap: a seed reads \
		 the transfer amount"
	);
	assert_eq!(amm.get_accounts_len(), 17);

	// Nor a list whose extra clashes with the trade for every user: the
	// curve's quote vault, which the trade writes, or a writable source
	// owner (the user on a buy, the curve on a sell).
	for (extra, reason) in [
		(
			fixed(curve.quote_vault, false),
			"transfer hook repeats an account of the trade, writable",
		),
		(
			pubkey_data(0, 32, true),
			"transfer hook repeats an account of the trade, writable",
		),
	] {
		accounts.0.insert(
			validation(&QUOTE_MINT),
			account(HOOK_PROGRAM, list(&[extra])),
		);
		amm.update(&accounts).unwrap();
		assert!(amm.is_active());
		let unroutable = format!(
			"transfer hook cannot be resolved ahead of the swap: {reason}"
		);
		assert_eq!(quote_error(&amm, &one_sol), unroutable);
		assert_eq!(
			swap_error(
				&amm,
				&swap_params(
					&amm.market,
					TradeDirection::Sell,
					SwapMode::ExactIn
				)
			),
			unroutable
		);
	}

	// A read-only source or destination owner routes on a buy and a sell.
	for (extra, buy_owner, sell_owner) in [
		(pubkey_data(0, 32, false), AUTHORITY, amm.market.key),
		(pubkey_data(2, 32, false), amm.market.key, AUTHORITY),
	] {
		accounts.0.insert(
			validation(&QUOTE_MINT),
			account(HOOK_PROGRAM, list(&[extra])),
		);
		amm.update(&accounts).unwrap();
		assert_eq!(amm.quote(&one_sol).unwrap().out_amount, 31_945_788_964_181);
		assert_eq!(amm.get_accounts_len(), 20);
		for (side, owner) in [
			(TradeDirection::Buy, buy_owner),
			(TradeDirection::Sell, sell_owner),
		] {
			let metas = amm
				.get_swap_and_account_metas(&swap_params(
					&amm.market,
					side,
					SwapMode::ExactIn,
				))
				.unwrap()
				.account_metas;
			assert_eq!(
				metas[17..],
				[
					AccountMeta::new_readonly(owner, false),
					AccountMeta::new_readonly(HOOK_PROGRAM, false),
					AccountMeta::new_readonly(validation(&QUOTE_MINT), false),
				]
			);
		}
	}

	// A clash with one wallet fails only that wallet's swaps.
	accounts.0.insert(
		validation(&QUOTE_MINT),
		account(HOOK_PROGRAM, list(&[fixed(SOURCE, false)])),
	);
	amm.update(&accounts).unwrap();
	assert!(amm.is_active());
	assert_eq!(
		swap_error(
			&amm,
			&swap_params(&amm.market, TradeDirection::Buy, SwapMode::ExactIn)
		),
		"transfer hook repeats an account of the trade, writable"
	);

	accounts
		.0
		.insert(curve.quote_mint, bare_mint(TOKEN_2022_PROGRAM_ID));
	amm.update(&accounts).unwrap();
	assert_eq!(*amm.market.hooks(), MarketHooks::default());
	assert_eq!(amm.get_accounts_to_update().len(), 4);
}

#[test]
fn quote_with_decay_premium() {
	let mut curve = make_curve();
	curve.created_at = NOW;
	let mut amm = make_amm(&curve);
	amm.fees = Some(LaunchpadFees {
		fee_decay_seconds: 12,
		fee_decay_start_bps: 5_000,
		..launchpad_fees(100)
	});
	let q = amm
		.quote(&buy(&amm.market, 1_000_000_000, SwapMode::ExactIn))
		.unwrap();

	// At t=0 the fee is the full 5_000 bps, not the 100 bps base.
	assert_eq!(q.fee_amount, 500_000_000);
	assert_eq!(q.out_amount, 16_393_442_622_950);
	assert_eq!(q.fee_pct, rust_decimal::Decimal::new(5, 1));
}

#[test]
fn quote_past_decay_window_no_premium() {
	let mut curve = make_curve();
	curve.created_at = NOW - 100;
	let mut amm = make_amm(&curve);
	amm.fees = Some(LaunchpadFees {
		fee_decay_seconds: 12,
		fee_decay_start_bps: 5_000,
		..launchpad_fees(100)
	});
	let q = amm
		.quote(&buy(&amm.market, 1_000_000_000, SwapMode::ExactIn))
		.unwrap();

	assert_eq!(q.fee_amount, 10_000_000);
}

/// The partner's `max_creator_fee_bps` limits a curve's `creator_fee_bps` at
/// creation only. A curve's rate can be above the max.
#[test]
fn the_curves_creator_rate_prices_the_trade_not_the_partner_max() {
	let fees = LaunchpadFees {
		max_creator_fee_bps: 50,
		fee_decay_seconds: 12,
		fee_decay_start_bps: 5_000,
		..launchpad_fees(100)
	};
	// `(fee bps, fee_amount, out_amount)` past the decay window, then halfway
	// through it. The SDK quote tests assert the same values.
	for (creator_fee_bps, standard, halfway) in [
		(
			0,
			(100, 10_000_000, 31_945_788_964_181),
			(1_325, 132_500_000, 28_103_992_872_762),
		),
		(
			30,
			(130, 13_000_000, 31_852_066_995_836),
			(1_348, 134_800_000, 28_031_569_534_621),
		),
		(
			80,
			(180, 18_000_000, 31_695_823_381_318),
			(1_385, 138_500_000, 27_915_039_774_476),
		),
	] {
		for (created_at, (fee_bps, fee_amount, out_amount)) in
			[(NOW - 100, standard), (NOW - 6, halfway)]
		{
			let mut curve = make_curve();
			curve.creator_fee_bps = creator_fee_bps;
			curve.created_at = created_at;
			let mut amm = make_amm(&curve);
			amm.fees = Some(fees.clone());
			let q = amm
				.quote(&buy(&amm.market, 1_000_000_000, SwapMode::ExactIn))
				.unwrap();

			assert_eq!(q.fee_pct, rust_decimal::Decimal::new(fee_bps, 4));
			assert_eq!(q.fee_amount, fee_amount);
			assert_eq!(q.out_amount, out_amount);
		}
	}
}

/// A `Completed` or `Migrated` curve is inactive. A `Paused` curve stays active.
/// All three refuse.
#[test]
fn a_curve_that_is_not_funding_refuses_quotes_and_swaps() {
	for (status, active) in [
		(BondingCurveStatus::Completed, false),
		(BondingCurveStatus::Migrated, false),
		(BondingCurveStatus::Paused, true),
	] {
		let mut curve = make_curve();
		curve.status = status;
		let amm = make_amm(&curve);

		assert_eq!(amm.is_active(), active);
		assert_eq!(
			quote_error(
				&amm,
				&buy(&amm.market, 1_000_000_000, SwapMode::ExactIn)
			),
			"bonding curve is not funding"
		);
		assert_eq!(
			swap_error(
				&amm,
				&swap_params(
					&amm.market,
					TradeDirection::Buy,
					SwapMode::ExactIn
				)
			),
			"bonding curve is not funding"
		);
	}
}

#[test]
fn a_foreign_mint_pair_errors() {
	let amm = make_amm(&make_curve());
	let foreign = Pubkey::new_from_array([99; 32]);
	let params = QuoteParams {
		input_mint: foreign,
		..buy(&amm.market, 1_000_000_000, SwapMode::ExactIn)
	};
	assert_eq!(
		quote_error(&amm, &params),
		"mint pair does not trade on this market"
	);

	let mut swap =
		swap_params(&amm.market, TradeDirection::Buy, SwapMode::ExactIn);
	swap.source_mint = foreign;
	assert_eq!(
		swap_error(&amm, &swap),
		"mint pair does not trade on this market"
	);
}

#[test]
fn quote_before_update_errors() {
	let mut amm = make_amm(&make_curve());
	amm.fees = None;
	assert_eq!(
		quote_error(&amm, &buy(&amm.market, 1_000_000_000, SwapMode::ExactIn)),
		"market not updated: call `update` first"
	);
}

#[test]
fn from_keyed_account_requires_a_partner() {
	for (params, error) in invalid_partner_params() {
		assert_eq!(
			SendfunLaunchpadAmm::from_keyed_account(
				&keyed_account(&make_curve(), params),
				&amm_context()
			)
			.err()
			.unwrap()
			.to_string(),
			error
		);
	}
}

/// The owner is checked before the params.
#[test]
fn from_keyed_account_rejects_a_foreign_owner() {
	let curve = make_curve();
	let keyed_account = KeyedAccount {
		key: curve_key(),
		account: account(
			sendfun_sdk::constants::DEX_PROGRAM_ID,
			borsh::to_vec(&curve).unwrap(),
		),
		params: None,
	};
	assert_eq!(
		SendfunLaunchpadAmm::from_keyed_account(
			&keyed_account,
			&AmmContext::default()
		)
		.err()
		.unwrap()
		.to_string(),
		"account is not owned by the launchpad"
	);
}

#[test]
fn update_reads_every_account_it_asks_for() {
	let mut curve = make_curve();
	let amm = make_amm(&curve);
	curve.virtual_quote_reserves = 31_000_000_000;
	curve.creator_fee_bps = 30;
	let fees = LaunchpadFees {
		max_creator_fee_bps: 50,
		..launchpad_fees(150)
	};
	let accounts = TestAccounts(BTreeMap::from([
		(
			amm.market.key,
			account(LAUNCHPAD_PROGRAM_ID, borsh::to_vec(&curve).unwrap()),
		),
		(
			amm.market.partner_config,
			partner_config_account(
				(curve.platform_config, DEFAULT_PARTNER),
				fees.clone(),
				dex_fees(1),
			),
		),
		(curve.base_mint, bare_mint(TOKEN_2022_PROGRAM_ID)),
		(curve.quote_mint, bare_mint(TOKEN_PROGRAM_ID)),
	]));

	let mut updated = amm.clone();
	updated.fees = None;
	updated.update(&accounts).unwrap();

	assert_eq!(updated.curve.virtual_quote_reserves, 31_000_000_000);
	assert_eq!(updated.curve.creator_fee_bps, 30);
	assert_eq!(updated.fees, Some(fees));
	assert_eq!(
		updated.market.mints,
		Some(Mints {
			base: mint_state(TOKEN_2022_PROGRAM_ID),
			quote: mint_state(TOKEN_PROGRAM_ID),
		})
	);
	assert_eq!(
		updated.get_accounts_to_update(),
		vec![
			amm.market.key,
			amm.market.partner_config,
			curve.base_mint,
			curve.quote_mint,
		]
	);
	// 150 bps protocol plus the curve's 30 bps. The partner's max of 50 bps is
	// not in the fee.
	let q = updated
		.quote(&buy(&updated.market, 1_000_000_000, SwapMode::ExactIn))
		.unwrap();
	assert_eq!(q.fee_amount, 18_000_000);

	// All or nothing: a missing account fails `update`, and the last good
	// state stays.
	let mut partial = accounts;
	partial.0.remove(&curve.quote_mint);
	let mut stale = amm;
	assert_eq!(
		stale.update(&partial).unwrap_err().to_string(),
		format!("Could not find address: {}", curve.quote_mint)
	);
	assert_eq!(stale.curve.virtual_quote_reserves, INITIAL_VIRTUAL_QUOTE);
	assert_eq!(stale.curve.creator_fee_bps, 0);
	assert_eq!(stale.fees, Some(launchpad_fees(100)));
}

/// The market does not quote after the first or the second `update`. It stays
/// active.
#[test]
fn a_hook_without_a_list_does_not_quote() {
	let curve = make_curve();
	let mut amm = make_amm(&curve);
	let accounts = TestAccounts(BTreeMap::from([
		(
			amm.market.key,
			account(LAUNCHPAD_PROGRAM_ID, borsh::to_vec(&curve).unwrap()),
		),
		(
			amm.market.partner_config,
			partner_config_account(
				(curve.platform_config, DEFAULT_PARTNER),
				launchpad_fees(100),
				dex_fees(1),
			),
		),
		(curve.base_mint, bare_mint(TOKEN_2022_PROGRAM_ID)),
		(curve.quote_mint, hooked_mint(HOOK_PROGRAM)),
	]));
	let one_sol = buy(&amm.market, 1_000_000_000, SwapMode::ExactIn);

	amm.update(&accounts).unwrap();
	assert_eq!(
		quote_error(&amm, &one_sol),
		"transfer hook accounts not loaded: call `update` again"
	);

	amm.update(&accounts).unwrap();
	assert!(amm.is_active());
	assert_eq!(
		quote_error(&amm, &one_sol),
		"transfer hook has no ExtraAccountMetaList"
	);
	assert!(
		amm.get_swap_and_account_metas(&swap_params(
			&amm.market,
			TradeDirection::Sell,
			SwapMode::ExactOut,
		))
		.is_err()
	);
}

/// `recycle_token` closes a curve account. The market stays active but does
/// not quote, and quotes again if the account returns.
#[test]
fn a_closed_curve_does_not_quote() {
	let curve = make_curve();
	let mut amm = make_amm(&curve);
	let accounts = TestAccounts(BTreeMap::from([
		(
			amm.market.key,
			account(LAUNCHPAD_PROGRAM_ID, borsh::to_vec(&curve).unwrap()),
		),
		(
			amm.market.partner_config,
			partner_config_account(
				(curve.platform_config, DEFAULT_PARTNER),
				launchpad_fees(100),
				dex_fees(1),
			),
		),
		(curve.base_mint, bare_mint(TOKEN_2022_PROGRAM_ID)),
		(curve.quote_mint, bare_mint(TOKEN_PROGRAM_ID)),
	]));
	let one_sol = buy(&amm.market, 1_000_000_000, SwapMode::ExactIn);

	let mut closed = TestAccounts(accounts.0.clone());
	closed.0.remove(&amm.market.key);
	closed.0.remove(&curve.base_mint);
	amm.update(&closed).unwrap();
	assert!(amm.is_active());
	assert_eq!(quote_error(&amm, &one_sol), "bonding curve is closed");
	assert_eq!(
		swap_error(
			&amm,
			&swap_params(&amm.market, TradeDirection::Buy, SwapMode::ExactIn)
		),
		"bonding curve is closed"
	);

	amm.update(&accounts).unwrap();
	assert!(amm.is_active());
	assert_eq!(amm.quote(&one_sol).unwrap().out_amount, 31_945_788_964_181);
}

/// Lamports sent to a closed curve's address make a system account with no
/// data. The curve stays closed, and `update` does not fail.
#[test]
fn a_closed_curve_funded_by_anyone_stays_closed() {
	let curve = make_curve();
	let mut amm = make_amm(&curve);
	let accounts = curve_accounts(&curve);
	amm.update(&accounts).unwrap();
	assert!(amm.is_active());

	let mut funded = TestAccounts(accounts.0.clone());
	funded
		.0
		.insert(curve_key(), account(SYSTEM_PROGRAM_ID, Vec::new()));
	funded.0.remove(&curve.base_mint);
	amm.update(&funded).unwrap();
	assert!(amm.is_active());
	let one_sol = buy(&amm.market, 1_000_000_000, SwapMode::ExactIn);
	assert_eq!(quote_error(&amm, &one_sol), "bonding curve is closed");
}

fn curve_accounts(curve: &BondingCurve) -> TestAccounts {
	TestAccounts(BTreeMap::from([
		(
			curve_key(),
			account(LAUNCHPAD_PROGRAM_ID, borsh::to_vec(curve).unwrap()),
		),
		(
			find_partner_config_pda(&curve.platform_config, &DEFAULT_PARTNER).0,
			partner_config_account(
				(curve.platform_config, DEFAULT_PARTNER),
				launchpad_fees(100),
				dex_fees(1),
			),
		),
		(curve.base_mint, bare_mint(TOKEN_2022_PROGRAM_ID)),
		(curve.quote_mint, bare_mint(TOKEN_PROGRAM_ID)),
	]))
}

#[test]
fn each_partner_prices_and_fronts_its_own_swaps() {
	let curve = make_curve();
	let mut accounts = curve_accounts(&curve);
	accounts.0.insert(
		find_partner_config_pda(&curve.platform_config, &PARTNER).0,
		partner_config_account(
			(curve.platform_config, PARTNER),
			launchpad_fees(250),
			dex_fees(1),
		),
	);

	for (partner, signs, fee_amount, out_amount, signers) in [
		(DEFAULT_PARTNER, false, 10_000_000, 31_945_788_964_181, 2),
		(PARTNER, true, 25_000_000, 31_476_997_578_692, 3),
	] {
		let (partner_config, _) =
			find_partner_config_pda(&curve.platform_config, &partner);
		let mut amm = make_partner_amm(&curve, &partner);
		amm.fees = None;
		amm.update(&accounts).unwrap();

		assert!(amm.is_active());
		assert_eq!(
			amm.get_accounts_to_update(),
			vec![curve_key(), partner_config, BASE_MINT, QUOTE_MINT]
		);
		let q = amm
			.quote(&buy(&amm.market, 1_000_000_000, SwapMode::ExactIn))
			.unwrap();
		assert_eq!(q.fee_amount, fee_amount);
		assert_eq!(q.out_amount, out_amount);
		for side in [TradeDirection::Buy, TradeDirection::Sell] {
			let metas = amm
				.get_swap_and_account_metas(&swap_params(
					&amm.market,
					side,
					SwapMode::ExactIn,
				))
				.unwrap()
				.account_metas;
			assert_eq!(metas[9], AccountMeta::new_readonly(partner, signs));
			assert_eq!(
				metas[10],
				AccountMeta::new_readonly(partner_config, false)
			);
			assert_eq!(
				metas.iter().filter(|meta| meta.is_signer).count(),
				signers
			);
		}
		assert_accounts_len_matches_every_swap(&amm, &amm.market);
	}
}

/// With no config for the partner on the curve's platform, the program fails
/// each swap. `update` succeeds, and the curve stays active but refuses every
/// quote and swap until the partner registers.
#[test]
fn a_partner_not_registered_on_the_platform_does_not_quote() {
	let curve = make_curve();
	let mut amm = make_partner_amm(&curve, &PARTNER);
	let mut accounts = curve_accounts(&curve);
	let (partner_config, _) =
		find_partner_config_pda(&curve.platform_config, &PARTNER);
	let registered = partner_config_account(
		(curve.platform_config, PARTNER),
		launchpad_fees(300),
		dex_fees(1),
	);
	let one_sol = buy(&amm.market, 1_000_000_000, SwapMode::ExactIn);
	let assert_unregistered = |amm: &SendfunLaunchpadAmm| {
		let unregistered = format!(
			"partner {PARTNER} is not registered on this market's platform"
		);
		assert!(amm.is_active());
		assert_eq!(quote_error(amm, &one_sol), unregistered);
		assert_eq!(
			swap_error(
				amm,
				&swap_params(
					&amm.market,
					TradeDirection::Sell,
					SwapMode::ExactOut
				)
			),
			unregistered
		);
		assert_eq!(
			amm.get_accounts_to_update(),
			vec![curve_key(), partner_config, BASE_MINT, QUOTE_MINT]
		);
	};

	amm.update(&accounts).unwrap();
	assert_eq!(amm.fees, None);
	assert_unregistered(&amm);

	// Anyone can fund the address. Only nexus writes a config.
	accounts.0.insert(
		partner_config,
		Account {
			owner: SYSTEM_PROGRAM_ID,
			..registered.clone()
		},
	);
	amm.update(&accounts).unwrap();
	assert_unregistered(&amm);

	accounts
		.0
		.insert(partner_config, account(NEXUS_PROGRAM_ID, vec![0; 173]));
	assert_eq!(
		amm.update(&accounts).unwrap_err().to_string(),
		"invalid account discriminator"
	);
	assert_unregistered(&amm);

	accounts.0.insert(partner_config, registered);
	amm.update(&accounts).unwrap();
	assert!(amm.is_active());
	assert_eq!(amm.fees, Some(launchpad_fees(300)));
	assert_eq!(amm.quote(&one_sol).unwrap().fee_amount, 30_000_000);

	accounts.0.remove(&partner_config);
	amm.update(&accounts).unwrap();
	assert_unregistered(&amm);
}

#[test]
fn a_curve_relaunched_on_another_platform_uses_its_partner_config() {
	let curve = make_curve();
	let mut amm = make_partner_amm(&curve, &PARTNER);
	let mut relaunched = make_curve();
	relaunched.platform_config = Pubkey::new_from_array([28; 32]);
	let (partner_config, _) =
		find_partner_config_pda(&relaunched.platform_config, &PARTNER);
	// The old platform's config outlives the curve.
	let mut accounts = TestAccounts(BTreeMap::from([
		(
			amm.market.key,
			account(LAUNCHPAD_PROGRAM_ID, borsh::to_vec(&relaunched).unwrap()),
		),
		(
			amm.market.partner_config,
			partner_config_account(
				(curve.platform_config, PARTNER),
				launchpad_fees(100),
				dex_fees(1),
			),
		),
		(curve.base_mint, bare_mint(TOKEN_2022_PROGRAM_ID)),
		(curve.quote_mint, bare_mint(TOKEN_PROGRAM_ID)),
	]));
	let one_sol = buy(&amm.market, 1_000_000_000, SwapMode::ExactIn);

	// The first `update` after a relaunch reads the old platform's config
	// address. The next `update` reads the new one.
	amm.update(&accounts).unwrap();
	assert!(amm.is_active());
	assert_eq!(
		quote_error(&amm, &one_sol),
		format!(
			"partner {PARTNER} is not registered on this market's platform"
		)
	);
	assert_eq!(
		amm.get_accounts_to_update(),
		vec![
			amm.market.key,
			partner_config,
			curve.base_mint,
			curve.quote_mint
		]
	);

	accounts.0.insert(
		partner_config,
		partner_config_account(
			(relaunched.platform_config, PARTNER),
			launchpad_fees(300),
			dex_fees(1),
		),
	);
	amm.update(&accounts).unwrap();
	assert!(amm.is_active());
	assert_eq!(amm.fees, Some(launchpad_fees(300)));
	assert_eq!(amm.quote(&one_sol).unwrap().fee_amount, 30_000_000);
	let metas = amm
		.get_swap_and_account_metas(&swap_params(
			&amm.market,
			TradeDirection::Buy,
			SwapMode::ExactIn,
		))
		.unwrap()
		.account_metas;
	assert_eq!(metas[9], AccountMeta::new_readonly(PARTNER, true));
	assert_eq!(metas[10], AccountMeta::new_readonly(partner_config, false));
}

/// Jupiter's token accounts go in unchanged. The transfer authority owns them
/// and is the program's `user`.
#[test]
fn swap_accounts_route_jupiters_token_accounts() {
	let mut amm = make_amm(&make_curve());
	set_quote_mint(&mut amm, mint_state(TOKEN_2022_PROGRAM_ID));

	for (side, base_account, quote_account) in [
		(TradeDirection::Buy, DESTINATION, SOURCE),
		(TradeDirection::Sell, SOURCE, DESTINATION),
	] {
		let result = amm
			.get_swap_and_account_metas(&swap_params(
				&amm.market,
				side,
				SwapMode::ExactIn,
			))
			.unwrap();
		let metas = &result.account_metas;

		assert_eq!(result.swap, crate::common::to_swap(side));
		assert_eq!(metas.len(), 17);
		assert_eq!(metas[0].pubkey, AUTHORITY);
		assert!(metas[0].is_signer);
		assert!(!metas[0].is_writable);
		assert_eq!(metas[1].pubkey, PAYER);
		assert!(metas[1].is_signer);
		assert!(metas[1].is_writable);
		assert_eq!(metas[2].pubkey, amm.market.key);
		assert_eq!(metas[5].pubkey, amm.curve.base_vault);
		assert_eq!(metas[6].pubkey, amm.curve.quote_vault);
		assert_eq!(metas[7].pubkey, base_account);
		assert!(metas[7].is_writable);
		assert_eq!(metas[8].pubkey, quote_account);
		assert!(metas[8].is_writable);
		// The default partner, which never signs.
		assert_eq!(metas[9].pubkey, DEFAULT_PARTNER);
		assert!(!metas[9].is_signer);
		assert_eq!(
			metas[10].pubkey,
			find_partner_config_pda(
				&amm.curve.platform_config,
				&DEFAULT_PARTNER
			)
			.0
		);
		assert_eq!(metas[11].pubkey, TOKEN_2022_PROGRAM_ID);
		assert_eq!(metas[12].pubkey, TOKEN_2022_PROGRAM_ID);
		assert_eq!(metas[16].pubkey, LAUNCHPAD_PROGRAM_ID);
		assert_eq!(
			metas.iter().filter(|meta| meta.is_signer).count(),
			2,
			"only the transfer authority and the payer sign"
		);
	}
}

/// Given the user's ATAs, the metas equal the SDK's metas for each trade
/// instruction: the same accounts, order and flags. Checked for the default
/// partner and a signing partner.
#[test]
fn swap_accounts_match_the_sdk_builders() {
	for partner in [DEFAULT_PARTNER, PARTNER] {
		assert_swap_accounts_match_the_sdk_builders(&partner);
	}
}

fn assert_swap_accounts_match_the_sdk_builders(partner: &Pubkey) {
	let amm = make_partner_amm(&make_curve(), partner);
	let user = AUTHORITY;
	let user_base =
		find_associated_token_pda(&user, &BASE_MINT, &TOKEN_2022_PROGRAM_ID).0;
	let user_quote =
		find_associated_token_pda(&user, &QUOTE_MINT, &TOKEN_PROGRAM_ID).0;
	let platform_config = amm.curve.platform_config;

	let sdk: [(TradeDirection, SwapMode, Instruction); 4] = [
		(
			TradeDirection::Buy,
			SwapMode::ExactIn,
			build_buy_exact_in_instruction(&BuildBuyExactInParams {
				user: &user,
				payer: Some(&PAYER),
				base_mint: &BASE_MINT,
				quote_mint: &QUOTE_MINT,
				partner,
				platform_config: &platform_config,
				quote_token_program: &TOKEN_PROGRAM_ID,
				amount_in: 1_000,
				min_amount_out: 900,
			}),
		),
		(
			TradeDirection::Buy,
			SwapMode::ExactOut,
			build_buy_exact_out_instruction(&BuildBuyExactOutParams {
				user: &user,
				payer: Some(&PAYER),
				base_mint: &BASE_MINT,
				quote_mint: &QUOTE_MINT,
				partner,
				platform_config: &platform_config,
				quote_token_program: &TOKEN_PROGRAM_ID,
				amount_out: 1_000,
				max_amount_in: 1_100,
			}),
		),
		(
			TradeDirection::Sell,
			SwapMode::ExactIn,
			build_sell_exact_in_instruction(&BuildSellExactInParams {
				user: &user,
				payer: Some(&PAYER),
				base_mint: &BASE_MINT,
				quote_mint: &QUOTE_MINT,
				partner,
				platform_config: &platform_config,
				quote_token_program: &TOKEN_PROGRAM_ID,
				amount_in: 1_000,
				min_amount_out: 900,
			}),
		),
		(
			TradeDirection::Sell,
			SwapMode::ExactOut,
			build_sell_exact_out_instruction(&BuildSellExactOutParams {
				user: &user,
				payer: Some(&PAYER),
				base_mint: &BASE_MINT,
				quote_mint: &QUOTE_MINT,
				partner,
				platform_config: &platform_config,
				quote_token_program: &TOKEN_PROGRAM_ID,
				amount_out: 1_000,
				max_amount_in: 1_100,
			}),
		),
	];

	for (side, swap_mode, expected) in sdk {
		let (source, destination) = match side {
			TradeDirection::Buy => (user_quote, user_base),
			TradeDirection::Sell => (user_base, user_quote),
		};
		let params = SwapParams {
			source_token_account: source,
			destination_token_account: destination,
			..swap_params(&amm.market, side, swap_mode)
		};
		let result = amm.get_swap_and_account_metas(&params).unwrap();
		assert_eq!(result.account_metas, expected.accounts);

		let data = match swap_mode {
			SwapMode::ExactIn => {
				SendfunLaunchpadAmm::exact_in_data(&result.swap, 1_000, 900)
			}
			SwapMode::ExactOut => {
				SendfunLaunchpadAmm::exact_out_data(&result.swap, 1_000, 1_100)
			}
		};
		assert_eq!(data.unwrap(), expected.data);
	}
}

#[test]
fn get_accounts_len_matches_every_swap() {
	let amm = make_amm(&make_curve());
	assert_accounts_len_matches_every_swap(&amm, &amm.market);
}
