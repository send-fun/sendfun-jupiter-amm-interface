use super::*;

use std::collections::BTreeMap;

use jupiter_amm_interface::SwapMode;
use sendfun_sdk::constants::{
	DEFAULT_PARTNER, NEXUS_PROGRAM_ID, SYSTEM_PROGRAM_ID,
	TOKEN_2022_PROGRAM_ID, TOKEN_PROGRAM_ID,
};
use sendfun_sdk::dex::accounts::POOL_DISCRIMINATOR;
use sendfun_sdk::dex::pda::find_pool_pda;
use sendfun_sdk::dex::trade::{
	BuildBuyExactInParams, BuildBuyExactOutParams, BuildSellExactInParams,
	BuildSellExactOutParams, build_buy_exact_in_instruction,
	build_buy_exact_out_instruction, build_sell_exact_in_instruction,
	build_sell_exact_out_instruction,
};
use sendfun_sdk::nexus::pda::find_partner_config_pda;
use sendfun_sdk::utils::find_associated_token_pda;
use solana_account::Account;
use solana_instruction::{AccountMeta, Instruction};

use sendfun_sdk::transfer_hook::{AccountView, MarketHooks, MintState};
use sendfun_sdk::utils::TradeDirection;

use crate::market::Mints;
use crate::test_support::{
	AUTHORITY, DESTINATION, HOOK_PROGRAM, NOW, PARTNER, PAYER, SOURCE,
	TestAccounts, account, amm_context, assert_accounts_len_matches_every_swap,
	bare_mint, buy, dex_fees, hook_list, hook_metas, hooked_mint,
	invalid_partner_params, launchpad_fees, list, mint_state,
	partner_config_account, partner_params, pubkey_data, quote_error, sell,
	swap_error, swap_params, token_account, validation,
};

const BASE_RESERVES: u64 = 400_000_000_000;
const QUOTE_RESERVES: u64 = 25_000_000_000;
const BASE_MINT: Pubkey = Pubkey::new_from_array([10; 32]);
const QUOTE_MINT: Pubkey = Pubkey::new_from_array([11; 32]);

fn pool_key() -> Pubkey {
	find_pool_pda(&BASE_MINT, &QUOTE_MINT).0
}

/// The vaults are ATAs, as `create_pool` makes them. The quote vault is under
/// SPL Token.
fn make_pool() -> Pool {
	let key = pool_key();
	Pool {
		discriminator: POOL_DISCRIMINATOR,
		version: 1,
		bump: 255,
		status: PoolStatus::Active,
		base_mint: BASE_MINT,
		base_decimals: 6,
		quote_mint: QUOTE_MINT,
		quote_decimals: 9,
		coin_creator: Pubkey::new_from_array([12; 32]),
		padding0: [0x0d0d_0d0d_0d0d_0d0d; 4],
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
		lp_mint: Pubkey::new_from_array([16; 32]),
		lp_supply: 100_000,
		base_reserves: BASE_RESERVES,
		quote_reserves: QUOTE_RESERVES,
		quote_ledger: QUOTE_RESERVES,
		created_at: 0,
		protocol_owed: 0,
		creator_owed: 0,
		// Not the default platform, so a fallback to it fails the tests.
		platform_config: Pubkey::new_from_array([17; 32]),
		creator_fee_mode: 0,
		creator_fee_bps: 0,
		reserved: [0; 256],
	}
}

fn keyed_account(
	pool: &Pool,
	params: Option<serde_json::Value>,
) -> KeyedAccount {
	KeyedAccount {
		key: pool_key(),
		account: account(DEX_PROGRAM_ID, borsh::to_vec(pool).unwrap()),
		params,
	}
}

fn make_amm(pool: &Pool) -> SendfunDexAmm {
	make_partner_amm(pool, &DEFAULT_PARTNER)
}

fn make_partner_amm(pool: &Pool, partner: &Pubkey) -> SendfunDexAmm {
	let mut amm = SendfunDexAmm::from_keyed_account(
		&keyed_account(pool, Some(partner_params(partner))),
		&amm_context(),
	)
	.unwrap();
	amm.fees = Some(dex_fees(100));
	amm.market.prime(&Mints {
		base: mint_state(TOKEN_2022_PROGRAM_ID),
		quote: mint_state(TOKEN_PROGRAM_ID),
	});
	amm.base_vault_amount = Some(pool.base_reserves);
	amm
}

#[test]
fn buy_exact_in() {
	let amm = make_amm(&make_pool());
	let q = amm
		.quote(&buy(&amm.market, 1_000_000_000, SwapMode::ExactIn))
		.unwrap();

	assert_eq!(q.in_amount, 1_000_000_000);
	assert_eq!(q.out_amount, 15_236_629_472);
	assert_eq!(q.fee_amount, 10_000_000);
	assert_eq!(q.fee_mint, QUOTE_MINT);
	assert_eq!(q.fee_pct, rust_decimal::Decimal::new(1, 2));
}

#[test]
fn sell_exact_in() {
	let amm = make_amm(&make_pool());
	let q = amm
		.quote(&sell(&amm.market, 10_000_000_000, SwapMode::ExactIn))
		.unwrap();

	assert_eq!(q.in_amount, 10_000_000_000);
	assert_eq!(q.out_amount, 603_658_536);
	assert_eq!(q.fee_amount, 6_097_561);
	assert_eq!(q.fee_mint, QUOTE_MINT);
}

#[test]
fn buy_exact_out() {
	let amm = make_amm(&make_pool());
	let q = amm
		.quote(&buy(&amm.market, 10_000_000_000, SwapMode::ExactOut))
		.unwrap();

	assert_eq!(q.in_amount, 647_500_649);
	assert_eq!(q.out_amount, 10_000_000_000);
	assert_eq!(q.fee_amount, 6_475_007);
}

#[test]
fn sell_exact_out() {
	let amm = make_amm(&make_pool());
	let q = amm
		.quote(&sell(&amm.market, 1_000_000_000, SwapMode::ExactOut))
		.unwrap();

	assert_eq!(q.in_amount, 16_842_105_279);
	assert_eq!(q.out_amount, 1_000_000_000);
	assert_eq!(q.fee_amount, 10_101_011);
}

/// The base side prices off the vault balance, not `pool.base_reserves`.
/// Donated base tokens move the price.
#[test]
fn the_base_side_prices_off_the_vault_balance() {
	let mut amm = make_amm(&make_pool());
	amm.base_vault_amount = Some(440_000_000_000);
	let q = amm
		.quote(&sell(&amm.market, 10_000_000_000, SwapMode::ExactIn))
		.unwrap();

	assert_eq!(q.out_amount, 549_999_999);
	assert_eq!(q.fee_amount, 5_555_556);
}

#[test]
fn a_sell_that_delivers_nothing_does_not_quote() {
	let amm = make_amm(&make_pool());
	assert_eq!(
		quote_error(&amm, &sell(&amm.market, 20, SwapMode::ExactIn)),
		"swap delivers nothing"
	);
}

/// Before the first `update`, the pool is active but does not quote.
#[test]
fn a_pool_stays_active_until_its_first_update() {
	let amm = SendfunDexAmm::from_keyed_account(
		&keyed_account(&make_pool(), Some(partner_params(&PARTNER))),
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

#[test]
fn a_paused_mint_refuses_quotes_and_swaps() {
	let mut amm = make_amm(&make_pool());
	assert!(amm.is_active());
	amm.market.mints.as_mut().unwrap().base = MintState {
		paused: true,
		..mint_state(TOKEN_2022_PROGRAM_ID)
	};
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

#[test]
fn a_hooked_base_mint_trails_every_swap_with_its_accounts() {
	let pool = make_pool();
	let mut amm = make_amm(&pool);
	let one_sol = buy(&amm.market, 1_000_000_000, SwapMode::ExactIn);
	let unhooked_out = amm.quote(&one_sol).unwrap().out_amount;
	let mints = amm.market.mints.as_mut().unwrap();
	mints.base.transfer_hook = Some(HOOK_PROGRAM);
	let Mints { base, quote } = *mints;
	let refresh = |provider: &TestAccounts| {
		MarketHooks::default()
			.plan((&BASE_MINT, &base), (&QUOTE_MINT, &quote))
			.resolve(|key| {
				provider.0.get(key).map(|account| AccountView {
					owner: &account.owner,
					data: &account.data,
				})
			})
	};
	amm.market
		.prime_hooks(refresh(&TestAccounts(BTreeMap::from([(
			validation(&BASE_MINT),
			account(HOOK_PROGRAM, hook_list()),
		)]))));
	assert!(amm.is_active());
	assert_eq!(amm.get_accounts_len(), 22);

	for (side, source, owner) in [
		(TradeDirection::Buy, pool.base_vault, amm.market.key),
		(TradeDirection::Sell, SOURCE, AUTHORITY),
	] {
		let metas = amm
			.get_swap_and_account_metas(&swap_params(
				&amm.market,
				side,
				SwapMode::ExactIn,
			))
			.unwrap()
			.account_metas;
		assert_eq!(metas.len(), amm.get_accounts_len());
		assert_eq!(metas[17..], hook_metas(&BASE_MINT, &source, &owner));
	}
	assert_eq!(amm.quote(&one_sol).unwrap().out_amount, unhooked_out);

	amm.market
		.prime_hooks(refresh(&TestAccounts(BTreeMap::new())));
	let not_loaded = "transfer hook accounts not loaded: call `update` again";
	assert_eq!(quote_error(&amm, &one_sol), not_loaded);
	assert_eq!(
		swap_error(
			&amm,
			&swap_params(&amm.market, TradeDirection::Buy, SwapMode::ExactIn)
		),
		not_loaded
	);
}

#[test]
fn quote_with_decay_premium() {
	let mut pool = make_pool();
	pool.created_at = NOW;
	let mut amm = make_amm(&pool);
	amm.fees = Some(DexFees {
		fee_decay_seconds: 12,
		fee_decay_start_bps: 5_000,
		..dex_fees(100)
	});
	let q = amm
		.quote(&buy(&amm.market, 1_000_000_000, SwapMode::ExactIn))
		.unwrap();

	assert_eq!(q.fee_amount, 500_000_000);
	assert_eq!(q.out_amount, 7_843_137_254);
}

#[test]
fn quote_past_decay_window_no_premium() {
	let mut pool = make_pool();
	pool.created_at = NOW - 100;
	let mut amm = make_amm(&pool);
	amm.fees = Some(DexFees {
		fee_decay_seconds: 12,
		fee_decay_start_bps: 5_000,
		..dex_fees(100)
	});
	let q = amm
		.quote(&buy(&amm.market, 1_000_000_000, SwapMode::ExactIn))
		.unwrap();

	assert_eq!(q.fee_amount, 10_000_000);
}

/// The partner's `max_creator_fee_bps` limits a pool's `creator_fee_bps` at
/// creation only. A pool's rate can be above the max.
#[test]
fn the_pools_creator_rate_prices_the_trade_not_the_partner_max() {
	let fees = DexFees {
		lp_fee_bps: 30,
		max_creator_fee_bps: 100,
		fee_decay_seconds: 12,
		fee_decay_start_bps: 5_000,
		..dex_fees(80)
	};
	// `(fee bps, fee_amount, out_amount)` past the decay window, then halfway
	// through it. The SDK quote tests assert the same values.
	for (creator_fee_bps, standard, halfway) in [
		(
			0,
			(110, 11_000_000, 15_221_824_618),
			(1_333, 133_300_000, 13_402_560_048),
		),
		(
			40,
			(150, 15_000_000, 15_162_593_804),
			(1_363, 136_300_000, 13_357_717_573),
		),
		(
			160,
			(270, 27_000_000, 14_984_791_899),
			(1_453, 145_300_000, 13_223_127_709),
		),
	] {
		for (created_at, (fee_bps, fee_amount, out_amount)) in
			[(NOW - 100, standard), (NOW - 6, halfway)]
		{
			let mut pool = make_pool();
			pool.creator_fee_bps = creator_fee_bps;
			pool.created_at = created_at;
			let mut amm = make_amm(&pool);
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

#[test]
fn a_paused_pool_refuses_quotes_and_swaps() {
	let mut pool = make_pool();
	pool.status = PoolStatus::Paused;
	let amm = make_amm(&pool);

	assert!(amm.is_active());
	assert_eq!(
		quote_error(&amm, &buy(&amm.market, 1_000_000_000, SwapMode::ExactIn)),
		"pool is not active"
	);
	assert_eq!(
		swap_error(
			&amm,
			&swap_params(&amm.market, TradeDirection::Buy, SwapMode::ExactIn)
		),
		"pool is not active"
	);
}

#[test]
fn a_foreign_mint_pair_errors() {
	let amm = make_amm(&make_pool());
	let foreign = Pubkey::new_from_array([99; 32]);
	let params = QuoteParams {
		output_mint: foreign,
		..sell(&amm.market, 1_000_000_000, SwapMode::ExactIn)
	};
	assert_eq!(
		quote_error(&amm, &params),
		"mint pair does not trade on this market"
	);

	let mut swap =
		swap_params(&amm.market, TradeDirection::Sell, SwapMode::ExactIn);
	swap.destination_mint = foreign;
	assert_eq!(
		swap_error(&amm, &swap),
		"mint pair does not trade on this market"
	);
}

#[test]
fn quote_before_update_errors() {
	let mut amm = make_amm(&make_pool());
	amm.base_vault_amount = None;
	assert_eq!(
		quote_error(&amm, &buy(&amm.market, 1_000_000_000, SwapMode::ExactIn)),
		"market not updated: call `update` first"
	);
}

#[test]
fn update_reads_every_account_it_asks_for() {
	let mut pool = make_pool();
	let amm = make_amm(&pool);
	pool.quote_reserves = 26_000_000_000;
	let fees = DexFees {
		lp_fee_bps: 25,
		max_creator_fee_bps: 25,
		..dex_fees(50)
	};
	let accounts = TestAccounts(BTreeMap::from([
		(
			amm.market.key,
			account(DEX_PROGRAM_ID, borsh::to_vec(&pool).unwrap()),
		),
		(
			amm.market.partner_config,
			partner_config_account(
				(pool.platform_config, DEFAULT_PARTNER),
				launchpad_fees(1),
				fees.clone(),
			),
		),
		(pool.base_mint, bare_mint(TOKEN_2022_PROGRAM_ID)),
		(pool.quote_mint, bare_mint(TOKEN_PROGRAM_ID)),
		(
			pool.base_vault,
			token_account(TOKEN_2022_PROGRAM_ID, 390_000_000_000),
		),
	]));

	let mut updated = amm.clone();
	updated.fees = None;
	updated.update(&accounts).unwrap();

	assert_eq!(updated.pool.quote_reserves, 26_000_000_000);
	assert_eq!(updated.fees, Some(fees));
	assert_eq!(updated.base_vault_amount, Some(390_000_000_000));
	assert_eq!(
		updated.get_accounts_to_update(),
		vec![
			amm.market.key,
			amm.market.partner_config,
			pool.base_mint,
			pool.quote_mint,
			pool.base_vault,
		]
	);

	let mut partial = accounts;
	partial.0.remove(&pool.base_vault);
	let mut stale = amm;
	assert_eq!(
		stale.update(&partial).unwrap_err().to_string(),
		format!("Could not find address: {}", pool.base_vault)
	);
	assert_eq!(stale.pool.quote_reserves, QUOTE_RESERVES);
	assert_eq!(stale.fees, Some(dex_fees(100)));
}

/// A hook armed after listing, whose extra is the source's owner. On a buy's
/// base leg, that is the pool, which the trade writes. No user can swap, so
/// the pool does not quote. It stays active.
#[test]
fn a_hook_that_clashes_with_every_swap_does_not_quote() {
	let pool = make_pool();
	let mut amm = make_amm(&pool);
	let mut accounts = TestAccounts(BTreeMap::from([
		(
			amm.market.key,
			account(DEX_PROGRAM_ID, borsh::to_vec(&pool).unwrap()),
		),
		(
			amm.market.partner_config,
			partner_config_account(
				(pool.platform_config, DEFAULT_PARTNER),
				launchpad_fees(1),
				dex_fees(100),
			),
		),
		(pool.base_mint, hooked_mint(HOOK_PROGRAM)),
		(pool.quote_mint, bare_mint(TOKEN_PROGRAM_ID)),
		(
			pool.base_vault,
			token_account(TOKEN_2022_PROGRAM_ID, BASE_RESERVES),
		),
		// The source's owner, writable: on a buy the pool, on a sell the user.
		(
			validation(&BASE_MINT),
			account(HOOK_PROGRAM, list(&[pubkey_data(0, 32, true)])),
		),
	]));
	amm.update(&accounts).unwrap();
	amm.update(&accounts).unwrap();

	assert!(amm.is_active());
	let one_sol = buy(&amm.market, 1_000_000_000, SwapMode::ExactIn);
	assert_eq!(
		quote_error(&amm, &one_sol),
		"transfer hook cannot be resolved ahead of the swap: transfer hook \
		 repeats an account of the trade, writable"
	);

	// The same owner, read-only, routes.
	accounts.0.insert(
		validation(&BASE_MINT),
		account(HOOK_PROGRAM, list(&[pubkey_data(0, 32, false)])),
	);
	amm.update(&accounts).unwrap();
	let mut unhooked = amm.clone();
	unhooked.market.prime_hooks(MarketHooks::default());
	assert_eq!(
		amm.quote(&one_sol).unwrap().out_amount,
		unhooked.quote(&one_sol).unwrap().out_amount
	);
}

/// The owner is checked before the params.
#[test]
fn from_keyed_account_requires_a_partner() {
	for (params, error) in invalid_partner_params() {
		assert_eq!(
			SendfunDexAmm::from_keyed_account(
				&keyed_account(&make_pool(), params),
				&amm_context()
			)
			.err()
			.unwrap()
			.to_string(),
			error
		);
	}
	let foreign = KeyedAccount {
		account: account(
			sendfun_sdk::constants::LAUNCHPAD_PROGRAM_ID,
			Vec::new(),
		),
		..keyed_account(&make_pool(), None)
	};
	assert_eq!(
		SendfunDexAmm::from_keyed_account(&foreign, &amm_context())
			.err()
			.unwrap()
			.to_string(),
		"account is not owned by the DEX"
	);
}

fn pool_accounts(pool: &Pool) -> TestAccounts {
	TestAccounts(BTreeMap::from([
		(
			pool_key(),
			account(DEX_PROGRAM_ID, borsh::to_vec(pool).unwrap()),
		),
		(
			find_partner_config_pda(&pool.platform_config, &DEFAULT_PARTNER).0,
			partner_config_account(
				(pool.platform_config, DEFAULT_PARTNER),
				launchpad_fees(1),
				dex_fees(100),
			),
		),
		(pool.base_mint, bare_mint(TOKEN_2022_PROGRAM_ID)),
		(pool.quote_mint, bare_mint(TOKEN_PROGRAM_ID)),
		(
			pool.base_vault,
			token_account(TOKEN_2022_PROGRAM_ID, BASE_RESERVES),
		),
	]))
}

#[test]
fn each_partner_prices_and_fronts_its_own_swaps() {
	let pool = make_pool();
	let mut accounts = pool_accounts(&pool);
	accounts.0.insert(
		find_partner_config_pda(&pool.platform_config, &PARTNER).0,
		partner_config_account(
			(pool.platform_config, PARTNER),
			launchpad_fees(1),
			dex_fees(250),
		),
	);

	for (partner, signs, fee_amount, out_amount, signers) in [
		(DEFAULT_PARTNER, false, 10_000_000, 15_236_629_472, 2),
		(PARTNER, true, 25_000_000, 15_014_436_958, 3),
	] {
		let (partner_config, _) =
			find_partner_config_pda(&pool.platform_config, &partner);
		let mut amm = make_partner_amm(&pool, &partner);
		amm.fees = None;
		amm.update(&accounts).unwrap();

		assert!(amm.is_active());
		assert_eq!(
			amm.get_accounts_to_update(),
			vec![
				pool_key(),
				partner_config,
				BASE_MINT,
				QUOTE_MINT,
				pool.base_vault
			]
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

/// With no config for the partner on the pool's platform, the program fails
/// each swap. `update` succeeds, and the pool stays active but refuses every
/// quote and swap until the partner registers.
#[test]
fn a_partner_not_registered_on_the_platform_does_not_quote() {
	let pool = make_pool();
	let mut amm = make_partner_amm(&pool, &PARTNER);
	let mut accounts = pool_accounts(&pool);
	let (partner_config, _) =
		find_partner_config_pda(&pool.platform_config, &PARTNER);
	let registered = partner_config_account(
		(pool.platform_config, PARTNER),
		launchpad_fees(1),
		dex_fees(300),
	);
	let one_sol = buy(&amm.market, 1_000_000_000, SwapMode::ExactIn);
	let assert_unregistered = |amm: &SendfunDexAmm| {
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
			vec![
				pool_key(),
				partner_config,
				BASE_MINT,
				QUOTE_MINT,
				pool.base_vault
			]
		);
	};

	// The default partner's config does not price another partner.
	amm.update(&accounts).unwrap();
	assert_eq!(amm.fees, None);
	assert_unregistered(&amm);

	// Anyone can fund this address. Only nexus writes a config.
	accounts.0.insert(
		partner_config,
		Account {
			owner: SYSTEM_PROGRAM_ID,
			..registered.clone()
		},
	);
	amm.update(&accounts).unwrap();
	assert_unregistered(&amm);

	// Owned by nexus but not decodable: malformed, so `update` fails.
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
	assert_eq!(amm.fees, Some(dex_fees(300)));
	assert_eq!(amm.quote(&one_sol).unwrap().fee_amount, 30_000_000);

	accounts.0.remove(&partner_config);
	amm.update(&accounts).unwrap();
	assert_unregistered(&amm);
}

#[test]
fn swap_accounts_route_jupiters_token_accounts() {
	let amm = make_amm(&make_pool());

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
		assert_eq!(metas[5].pubkey, amm.pool.base_vault);
		assert_eq!(metas[6].pubkey, amm.pool.quote_vault);
		assert_eq!(metas[7].pubkey, base_account);
		assert_eq!(metas[8].pubkey, quote_account);
		assert_eq!(metas[9].pubkey, DEFAULT_PARTNER);
		assert!(!metas[9].is_signer);
		assert_eq!(
			metas[10].pubkey,
			find_partner_config_pda(
				&amm.pool.platform_config,
				&DEFAULT_PARTNER
			)
			.0
		);
		assert_eq!(metas[12].pubkey, TOKEN_PROGRAM_ID);
		assert_eq!(metas[16].pubkey, DEX_PROGRAM_ID);
		assert_eq!(
			metas.iter().filter(|meta| meta.is_signer).count(),
			2,
			"only the transfer authority and the payer sign"
		);
	}
}

/// The metas equal the SDK's metas for each trade instruction: the same
/// accounts, order, and signer and writable flags.
#[test]
fn swap_accounts_match_the_sdk_builders() {
	for partner in [DEFAULT_PARTNER, PARTNER] {
		assert_swap_accounts_match_the_sdk_builders(&partner);
	}
}

fn assert_swap_accounts_match_the_sdk_builders(partner: &Pubkey) {
	let amm = make_partner_amm(&make_pool(), partner);
	let user = AUTHORITY;
	let user_base =
		find_associated_token_pda(&user, &BASE_MINT, &TOKEN_2022_PROGRAM_ID).0;
	let user_quote =
		find_associated_token_pda(&user, &QUOTE_MINT, &TOKEN_PROGRAM_ID).0;
	let platform_config = amm.pool.platform_config;

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
				SendfunDexAmm::exact_in_data(&result.swap, 1_000, 900)
			}
			SwapMode::ExactOut => {
				SendfunDexAmm::exact_out_data(&result.swap, 1_000, 1_100)
			}
		};
		assert_eq!(data.unwrap(), expected.data);
	}
}

#[test]
fn get_accounts_len_matches_every_swap() {
	let amm = make_amm(&make_pool());
	assert_accounts_len_matches_every_swap(&amm, &amm.market);
}
