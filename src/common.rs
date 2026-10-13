use jupiter_amm_interface::{AmmError, KeyedAccount, Swap};
use sendfun_sdk::transfer_hook::{HookError, Unloaded};
use sendfun_sdk::utils::{
	MarketQuoteError, TradeDirection, parse_token_amount,
};
use solana_account::ReadableAccount;
use solana_pubkey::Pubkey;

pub const SWAP_TAG_BUY: u8 = 0;
pub const SWAP_TAG_SELL: u8 = 1;

pub const fn unloaded(missing_dynamic_accounts_as_default: bool) -> Unloaded {
	if missing_dynamic_accounts_as_default {
		Unloaded::StandIn
	} else {
		Unloaded::Refuse
	}
}

pub fn hook_error(error: HookError) -> AmmError {
	match error {
		HookError::NotLoaded => AmmError::from(
			"transfer hook accounts not loaded: call `update` again",
		),
		error => AmmError::Custom(error.to_string()),
	}
}

pub fn market_quote_error(error: MarketQuoteError) -> AmmError {
	AmmError::Custom(error.to_string())
}

pub fn direction_of_pair(
	[base, quote]: [&Pubkey; 2],
	input_mint: &Pubkey,
	output_mint: &Pubkey,
) -> Result<TradeDirection, AmmError> {
	if input_mint == quote && output_mint == base {
		return Ok(TradeDirection::Buy);
	}
	if input_mint == base && output_mint == quote {
		return Ok(TradeDirection::Sell);
	}
	Err(AmmError::from("mint pair does not trade on this market"))
}

/// The tag holds only the direction. All four trade instructions share one
/// account layout, and `SwapParams` holds the mode.
pub fn to_swap(direction: TradeDirection) -> Swap {
	let tag = match direction {
		TradeDirection::Buy => SWAP_TAG_BUY,
		TradeDirection::Sell => SWAP_TAG_SELL,
	};
	Swap::Placeholder {
		data: Some(vec![tag]),
	}
}

pub fn direction_from_swap(swap: &Swap) -> Result<TradeDirection, AmmError> {
	let Swap::Placeholder { data: Some(data) } = swap else {
		return Err(AmmError::from("not a send.fun swap"));
	};
	match data.as_slice() {
		[SWAP_TAG_BUY] => Ok(TradeDirection::Buy),
		[SWAP_TAG_SELL] => Ok(TradeDirection::Sell),
		_ => Err(AmmError::from("unknown send.fun swap tag")),
	}
}

/// Reads the partner from `KeyedAccount.params`: `{"partner": "<pubkey>"}`.
/// No default and no fallback.
pub fn partner_from_params(
	keyed_account: &KeyedAccount,
) -> Result<Pubkey, AmmError> {
	let partner = keyed_account
		.params
		.as_ref()
		.ok_or_else(|| {
			AmmError::from(
				"params missing: expected `{\"partner\": \"<pubkey>\"}`",
			)
		})?
		.as_object()
		.ok_or_else(|| AmmError::from("params is not an object"))?
		.get("partner")
		.ok_or_else(|| AmmError::from("params has no `partner`"))?
		.as_str()
		.ok_or_else(|| AmmError::from("params `partner` is not a string"))?;
	partner.parse().map_err(|_| {
		AmmError::Custom(format!(
			"params `partner` is not a valid pubkey: {partner}"
		))
	})
}

pub fn not_updated() -> AmmError {
	AmmError::from("market not updated: call `update` first")
}

/// The amount offset is the same in SPL Token and Token-2022.
pub fn token_amount(account: &impl ReadableAccount) -> Result<u64, AmmError> {
	parse_token_amount(account.data())
		.ok_or_else(|| AmmError::from("malformed token account"))
}

#[cfg(test)]
mod tests {
	use super::*;

	use sendfun_sdk::constants::DEFAULT_PARTNER;
	use solana_account::Account;

	use crate::test_support::{
		PARTNER, invalid_partner_params, partner_params,
	};

	fn listed(params: Option<serde_json::Value>) -> KeyedAccount {
		KeyedAccount {
			key: Pubkey::new_from_array([5; 32]),
			account: Account::default(),
			params,
		}
	}

	/// The default partner also comes from `params`.
	#[test]
	fn the_partner_comes_from_params() {
		for partner in [DEFAULT_PARTNER, PARTNER] {
			assert_eq!(
				partner_from_params(&listed(Some(partner_params(&partner))))
					.unwrap(),
				partner
			);
		}
	}

	#[test]
	fn params_without_a_partner_are_refused() {
		for (params, error) in invalid_partner_params() {
			assert_eq!(
				partner_from_params(&listed(params))
					.unwrap_err()
					.to_string(),
				error
			);
		}
	}

	#[test]
	fn the_swap_tag_round_trips() {
		for direction in [TradeDirection::Buy, TradeDirection::Sell] {
			assert_eq!(
				direction_from_swap(&to_swap(direction)).unwrap(),
				direction
			);
		}
		assert_eq!(
			to_swap(TradeDirection::Buy),
			Swap::Placeholder {
				data: Some(vec![0])
			}
		);
		assert!(
			direction_from_swap(&Swap::Placeholder { data: None }).is_err()
		);
		assert!(
			direction_from_swap(&Swap::Placeholder {
				data: Some(vec![2])
			})
			.is_err()
		);
	}
}
