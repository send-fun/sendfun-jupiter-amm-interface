//! Fixtures for the unit tests of both `Amm`s.

use std::collections::BTreeMap;

use jupiter_amm_interface::{
	AccountProvider, Amm, AmmContext, ClockRef, FeeMode, QuoteParams, SwapMode,
	SwapParams,
};
use sendfun_sdk::constants::TOKEN_2022_PROGRAM_ID;
use sendfun_sdk::nexus::accounts::{
	PARTNER_CONFIG_DISCRIMINATOR, PartnerConfig,
};
use sendfun_sdk::nexus::types::{DexFees, LaunchpadFees};
use serde_json::{Value, json};
use solana_account::{Account, ReadableAccount};
use solana_clock::Clock;
use solana_instruction::AccountMeta;
use solana_pubkey::Pubkey;

use sendfun_sdk::transfer_hook::{
	EXECUTE_DISCRIMINATOR, MintState, find_validation_address,
};
use sendfun_sdk::utils::TradeDirection;
use spl_discriminator::{ArrayDiscriminator, SplDiscriminate};
use spl_tlv_account_resolution::account::ExtraAccountMeta;
use spl_tlv_account_resolution::pubkey_data::PubkeyData;
use spl_tlv_account_resolution::seeds::Seed;
use spl_tlv_account_resolution::state::ExtraAccountMetaList;

use crate::market::Market;

pub const NOW: i64 = 1_000;
pub const JUPITER: Pubkey = Pubkey::new_from_array([70; 32]);
pub const SOURCE: Pubkey = Pubkey::new_from_array([60; 32]);
pub const DESTINATION: Pubkey = Pubkey::new_from_array([61; 32]);
pub const AUTHORITY: Pubkey = Pubkey::new_from_array([62; 32]);
pub const PAYER: Pubkey = Pubkey::new_from_array([63; 32]);
pub const HOOK_PROGRAM: Pubkey = Pubkey::new_from_array([80; 32]);
pub const RENT: Pubkey = Pubkey::new_from_array([81; 32]);
/// A registered partner other than the default. It signs every swap.
pub const PARTNER: Pubkey = Pubkey::new_from_array([90; 32]);

/// The `KeyedAccount.params` that route a market as `partner`.
pub fn partner_params(partner: &Pubkey) -> Value {
	json!({ "partner": partner.to_string() })
}

/// Params that name no usable partner, and the error for each. A misspelled
/// key fails like an empty object.
pub fn invalid_partner_params() -> [(Option<Value>, &'static str); 6] {
	[
		(
			None,
			"params missing: expected `{\"partner\": \"<pubkey>\"}`",
		),
		(Some(json!({})), "params has no `partner`"),
		(
			Some(json!({ "partnerKey": PARTNER.to_string() })),
			"params has no `partner`",
		),
		(Some(json!(PARTNER.to_string())), "params is not an object"),
		(
			Some(json!({ "partner": 7 })),
			"params `partner` is not a string",
		),
		(
			Some(json!({ "partner": "send.fun" })),
			"params `partner` is not a valid pubkey: send.fun",
		),
	]
}

/// A fixed account set for `update`.
pub struct TestAccounts(pub BTreeMap<Pubkey, Account>);

impl AccountProvider for TestAccounts {
	fn get(&self, pubkey: &Pubkey) -> Option<impl ReadableAccount + use<'_>> {
		self.0.get(pubkey).cloned()
	}
}

pub fn account(owner: Pubkey, data: Vec<u8>) -> Account {
	Account {
		lamports: 1_000_000,
		data,
		owner,
		executable: false,
		rent_epoch: 0,
	}
}

pub fn token_account(owner: Pubkey, amount: u64) -> Account {
	let mut data = vec![0; 165];
	data[64..72].copy_from_slice(&amount.to_le_bytes());
	account(owner, data)
}

pub fn bare_mint(token_program: Pubkey) -> Account {
	let mut data = vec![0; 82];
	data[45] = 1;
	account(token_program, data)
}

/// A Token-2022 mint with `extensions` as `(type, payload)` pairs.
pub fn token_2022_mint(extensions: &[(u16, Vec<u8>)]) -> Account {
	let mut data = vec![0; 165];
	// `is_initialized`, then the mint account type.
	data[45] = 1;
	data.push(1);
	for (extension_type, payload) in extensions {
		data.extend_from_slice(&extension_type.to_le_bytes());
		data.extend_from_slice(
			&u16::try_from(payload.len()).unwrap().to_le_bytes(),
		);
		data.extend_from_slice(payload);
	}
	account(TOKEN_2022_PROGRAM_ID, data)
}

/// A Token-2022 mint whose only extension is a hook naming `program`.
pub fn hooked_mint(program: Pubkey) -> Account {
	token_2022_mint(&[(14, [[0; 32], program.to_bytes()].concat())])
}

pub const fn mint_state(token_program: Pubkey) -> MintState {
	MintState {
		token_program,
		transfer_fee: None,
		transfer_hook: None,
		paused: false,
	}
}

pub fn amm_context() -> AmmContext {
	AmmContext {
		clock_ref: ClockRef::from(Clock {
			unix_timestamp: NOW,
			..Clock::default()
		}),
	}
}

pub const fn launchpad_fees(protocol_fee_bps: u16) -> LaunchpadFees {
	LaunchpadFees {
		creation_fee_cents: 0,
		protocol_fee_bps,
		max_creator_fee_bps: 0,
		fee_decay_seconds: 0,
		fee_decay_start_bps: 0,
	}
}

pub const fn dex_fees(protocol_fee_bps: u16) -> DexFees {
	DexFees {
		creation_fee_cents: 0,
		protocol_fee_bps,
		lp_fee_bps: 0,
		max_creator_fee_bps: 0,
		fee_decay_seconds: 0,
		fee_decay_start_bps: 0,
	}
}

/// `partner`'s config account on `platform_config`. The tuple is in PDA seed
/// order.
pub fn partner_config_account(
	(platform_config, partner): (Pubkey, Pubkey),
	launchpad: LaunchpadFees,
	dex: DexFees,
) -> Account {
	let config = PartnerConfig {
		discriminator: PARTNER_CONFIG_DISCRIMINATOR,
		version: 1,
		bump: 255,
		authority: Pubkey::new_from_array([30; 32]),
		partner,
		fee_preset: 0,
		launchpad,
		dex,
		platform_config,
		allowed_creator_fee_modes: 0,
		reserved: [0; 256],
	};
	account(
		sendfun_sdk::constants::NEXUS_PROGRAM_ID,
		borsh::to_vec(&config).unwrap(),
	)
}

struct Execute;

impl SplDiscriminate for Execute {
	const SPL_DISCRIMINATOR: ArrayDiscriminator =
		ArrayDiscriminator::new(EXECUTE_DISCRIMINATOR);
}

pub fn list(extras: &[ExtraAccountMeta]) -> Vec<u8> {
	let mut data =
		vec![0; ExtraAccountMetaList::size_of(extras.len()).unwrap()];
	ExtraAccountMetaList::init::<Execute>(&mut data, extras).unwrap();
	data
}

pub fn fixed(pubkey: Pubkey, writable: bool) -> ExtraAccountMeta {
	ExtraAccountMeta::new_with_pubkey(&pubkey, false, writable).unwrap()
}

pub fn pda(seeds: &[Seed], writable: bool) -> ExtraAccountMeta {
	ExtraAccountMeta::new_with_seeds(seeds, false, writable).unwrap()
}

pub fn pubkey_data(
	account: u8,
	offset: u8,
	writable: bool,
) -> ExtraAccountMeta {
	ExtraAccountMeta::new_with_pubkey_data(
		&PubkeyData::AccountData {
			account_index: account,
			data_index: offset,
		},
		false,
		writable,
	)
	.unwrap()
}

/// A hook list: a fixed account, a writable PDA keyed on the source, and a
/// PDA keyed on the source's owner.
pub fn hook_list() -> Vec<u8> {
	list(&[
		fixed(RENT, false),
		pda(
			&[
				Seed::Literal {
					bytes: b"ledger".to_vec(),
				},
				Seed::AccountKey { index: 0 },
			],
			true,
		),
		pda(
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
		),
	])
}

pub fn validation(mint: &Pubkey) -> Pubkey {
	find_validation_address(mint, &HOOK_PROGRAM)
}

/// The metas that [`hook_list`] resolves to for a leg out of `source`, which
/// `owner` owns.
pub fn hook_metas(
	mint: &Pubkey,
	source: &Pubkey,
	owner: &Pubkey,
) -> Vec<AccountMeta> {
	let pda =
		|seeds: &[&[u8]]| Pubkey::find_program_address(seeds, &HOOK_PROGRAM).0;
	vec![
		AccountMeta::new_readonly(RENT, false),
		AccountMeta::new(pda(&[b"ledger", source.as_ref()]), false),
		AccountMeta::new_readonly(pda(&[b"owner", owner.as_ref()]), false),
		AccountMeta::new_readonly(HOOK_PROGRAM, false),
		AccountMeta::new_readonly(validation(mint), false),
	]
}

pub fn buy(market: &Market, amount: u64, swap_mode: SwapMode) -> QuoteParams {
	QuoteParams {
		amount,
		input_mint: market.quote_mint,
		output_mint: market.base_mint,
		swap_mode,
		fee_mode: FeeMode::default(),
	}
}

pub fn sell(market: &Market, amount: u64, swap_mode: SwapMode) -> QuoteParams {
	QuoteParams {
		input_mint: market.base_mint,
		output_mint: market.quote_mint,
		..buy(market, amount, swap_mode)
	}
}

pub fn quote_error(amm: &impl Amm, params: &QuoteParams) -> String {
	amm.quote(params).unwrap_err().to_string()
}

pub const fn swap_params(
	market: &Market,
	direction: TradeDirection,
	swap_mode: SwapMode,
) -> SwapParams<'static, 'static> {
	let (source_mint, destination_mint) = match direction {
		TradeDirection::Buy => (market.quote_mint, market.base_mint),
		TradeDirection::Sell => (market.base_mint, market.quote_mint),
	};
	SwapParams {
		swap_mode,
		in_amount: 1_000_000_000,
		out_amount: 100_000,
		source_mint,
		destination_mint,
		source_token_account: SOURCE,
		destination_token_account: DESTINATION,
		token_transfer_authority: AUTHORITY,
		user: Pubkey::new_from_array([64; 32]),
		payer: PAYER,
		quote_mint_to_referrer: None,
		jupiter_program_id: &JUPITER,
		missing_dynamic_accounts_as_default: false,
	}
}

pub fn swap_error(amm: &impl Amm, params: &SwapParams<'_, '_>) -> String {
	amm.get_swap_and_account_metas(params)
		.err()
		.unwrap()
		.to_string()
}

/// Asserts that `get_accounts_len` equals the meta count of each swap. An
/// undercount truncates the swap.
pub fn assert_accounts_len_matches_every_swap(amm: &impl Amm, market: &Market) {
	for direction in [TradeDirection::Buy, TradeDirection::Sell] {
		for swap_mode in [SwapMode::ExactIn, SwapMode::ExactOut] {
			let metas = amm
				.get_swap_and_account_metas(&swap_params(
					market, direction, swap_mode,
				))
				.unwrap()
				.account_metas;
			assert_eq!(metas.len(), amm.get_accounts_len());
		}
	}
}
