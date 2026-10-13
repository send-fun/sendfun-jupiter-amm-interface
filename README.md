# sendfun-jupiter

[`jupiter-amm-interface`](https://github.com/jup-ag/jupiter-amm-interface)
`Amm` implementations for the two send.fun trading programs. A parity suite
checks each quote against the program binaries.

| Program   | `Amm`                 | Label                | Program ID                                     | Market account                                     |
| --------- | --------------------- | -------------------- | ---------------------------------------------- | -------------------------------------------------- |
| Launchpad | `SendfunLaunchpadAmm` | `send.fun Launchpad` | `5R1uFyEE4oxqkm7hJDqHq6gXaLVF9dxeF3LF4yz1sfLP` | `BondingCurve` at `["bonding_curve", base, quote]` |
| DEX       | `SendfunDexAmm`       | `send.fun AMM`       | `84qj5FPZZdXkQy8mfowyg6RBZ3XKuTds6XS4ZYT1sfDX` | `Pool` at `["pool", base, quote]`                  |

A token trades on a launchpad bonding curve until a buy takes the last of its
supply. Then the token migrates to a DEX pool, and the pool's LP tokens are
burned. Both markets are constant product. The base mint is a Token-2022 mint.
The quote mint is WSOL or another registered SPL or Token-2022 mint.

**Both `Amm`s require `{"partner": "<base58 pubkey>"}` in
`KeyedAccount.params`.** `from_keyed_account` fails without it. See
[Partner](#partner).

```sh
cargo test                  # unit tests + the parity suite
cargo test --test parity    # just the parity suite
```

The tests run offline against committed fixtures. The crate is its own Cargo
workspace with a committed `Cargo.lock`, and the tests need it. A bare
`cargo update` moves three crates that `Cargo.toml` names, and the test kit's
LiteSVM stops compiling. The library builds either way.

## Open question for Jupiter (high priority)

What does your router do with an AMM whose `update()` returns `Err`? The
adapter keeps its last good state and quotes it (see [Quoting](#quoting)). An
error that repeats, such as an account layout that this crate cannot decode
after a program upgrade, keeps a stale market in routing if the router keeps
the AMM.

## Markets

`is_active()` is false only for a curve that is `Completed` or `Migrated`. A
pool is always active.

In these states the market stays active. `quote` and
`get_swap_and_account_metas` refuse until a later `update` reads a tradable
state:

- a curve account that is missing or not owned by the launchpad;
- a paused curve or pool;
- a paused mint;
- a partner with no `PartnerConfig` on the market's platform;
- a [transfer hook](#transfer-hooks) whose list has not loaded, or that cannot
  be resolved;
- a market before its first `update`.

## Partner

Each trade prices off the partner's `PartnerConfig`. Its address is
`["partner", platform_config, partner]` under nexus
(`7rrCurqdWFesbzwtYPo95fM8waTtVXgXwCom45x1sfNX`). `platform_config` is the
platform that the market was created on.

- The default partner, `11111111111111111111111111111111`, does not sign.
- Any other partner signs the transaction, next to the user. Jupiter's router
  passes the signature to each send.fun leg. One signature covers all
  send.fun legs. Without the signature, the program fails the swap with
  `Unauthorized`.
- The program checks only that the partner signed. Anyone who can sign for
  the partner key can trade as that partner.
- A partner with no `PartnerConfig` on a market's platform makes that market
  refuse every quote and swap until the partner is registered there. The
  market stays active.
- Take `params.partner` from Jupiter's own configuration, never from a market
  listing or other outside input. The adapter does not check that the router
  can sign for the key. A registered partner that the router cannot sign for
  quotes, but every swap on its markets fails with `Unauthorized`.

## Quoting

`quote` does no I/O. `update` reads each account that `quote` uses.
`get_accounts_to_update` names them:

- the market;
- the `PartnerConfig`;
- both mints, for the Token-2022 fee schedule, hook and pause state;
- the DEX pool's base vault, whose balance prices the base side;
- the `ExtraAccountMetaList` of each armed hook.

The hook lists change when the hooks change, so `has_dynamic_accounts()` is
true.

`update` is all or nothing. On error, the last good state stays. These are
not errors:

- A curve account that is missing, or that the launchpad does not own, makes
  the market unquotable until the account returns. The market stays active.
- A missing `PartnerConfig`, or one that nexus does not own, makes the market
  unquotable until the account exists. The market stays active.
- A hook list that is not loaded yet, or that goes missing, makes the market
  unquotable until the list loads. The market stays active.

`quote` calls `launchpad::quote` or `dex::quote` from
[`sendfun-sdk`](https://github.com/send-fun/sendfun-sdk-rust).

- **Fee.** A base rate plus a decay premium. The base rate is the partner's
  protocol rate plus the market's `creator_fee_bps`. On the DEX, it also
  includes the partner's LP rate. The market's `creator_fee_bps` does not
  change after creation. `quote` does not charge the partner's
  `max_creator_fee_bps`. The premium falls quadratically from the market's
  `created_at` to zero at the end of the partner's decay window. `quote` reads
  the time from `ClockRef::unix_timestamp`. The nexus authority can change the
  partner's rates at any time.
  `fee_amount` is in the quote mint. `fee_pct` is the effective rate as a
  fraction: 100 bps is `0.01`.
- **Token-2022 transfer fees** apply on either leg, at `ClockRef::epoch`.
  `in_amount` is the amount that leaves the source. `out_amount` is the amount
  that arrives in the destination.
- **Supply cap.** The launchpad does not sell more than `real_base_reserves`.
  An exact-in buy past the cap quotes the capped fill, and `in_amount` can be
  less than `amount`. The unspent input stays in `source_token_account`. On an
  intermediate hop, nothing sweeps it. An exact-out buy past the cap does not
  quote.
- **No quote** for a launchpad sell whose payout plus fee exceeds
  `real_quote_reserves`, or for an output that rounds to zero.

## Swaps

`jupiter-amm-interface` has no `Swap` variant for either program. Both `Amm`s
return `Swap::Placeholder { data: Some(vec![tag]) }`. `tag` is
`SWAP_TAG_BUY = 0` (quote in, base out) or `SWAP_TAG_SELL = 1` (base in,
quote out). `SwapParams` gives the mode.

The instruction data is an 8-byte discriminator and two little-endian `u64`s.
Each `Amm` has helpers that build the data for its own program. Select the
helpers by `Amm::program_id()`:

| Instructions                      | Args                          | Helper           |
| --------------------------------- | ----------------------------- | ---------------- |
| `buy_exact_in`, `sell_exact_in`   | `amount_in`, `min_amount_out` | `exact_in_data`  |
| `buy_exact_out`, `sell_exact_out` | `amount_out`, `max_amount_in` | `exact_out_data` |

The program checks `min_amount_out` against the destination credit, and
`max_amount_in` against the source debit.

All four instructions use one 17-account layout. The metas are that layout in
order, then any [transfer hook](#transfer-hooks) accounts:

| #   | Account                  | Signer                  | Writable | From                                                           |
| --- | ------------------------ | ----------------------- | -------- | -------------------------------------------------------------- |
| 0   | `user`                   | yes                     |          | `token_transfer_authority`                                     |
| 1   | `payer`                  | yes                     | yes      | `payer`                                                        |
| 2   | market (curve / pool)    |                         | yes      | `key()`                                                        |
| 3   | base mint                |                         |          |                                                                |
| 4   | quote mint               |                         |          |                                                                |
| 5   | base vault               |                         | yes      | the market's `base_vault`                                      |
| 6   | quote vault              |                         | yes      | the market's `quote_vault`                                     |
| 7   | user base account        |                         | yes      | buy: `destination_token_account`; sell: `source_token_account` |
| 8   | user quote account       |                         | yes      | buy: `source_token_account`; sell: `destination_token_account` |
| 9   | partner                  | yes, unless the default |          | `params.partner`                                               |
| 10  | partner config           |                         |          | `["partner", platform_config, partner]`                        |
| 11  | base token program       |                         |          | Token-2022                                                     |
| 12  | quote token program      |                         |          | the quote mint's owner                                         |
| 13  | associated token program |                         |          |                                                                |
| 14  | system program           |                         |          |                                                                |
| 15  | event authority          |                         |          | program PDA `["__event_authority"]`                            |
| 16  | program                  |                         |          | `program_id()`                                                 |

- **CPIs:** the token programs; the program itself, through the event
  authority; the ATA program, to create a missing slot 7 or 8, paid by
  `payer`; and each armed hook, through Token-2022. `program_dependencies()`
  lists the armed hooks. The programs do not read the instructions sysvar.
- **Compute** in `LiteSVM`: about 10,800 CU for a launchpad swap and 11,800 CU
  for a DEX swap. A buy that reaches the supply cap costs up to about 13,100
  CU. In the snapshots here, a Token-2022 quote mint with a transfer fee adds
  about 5,000 CU. Creating a missing destination adds the ATA program's cost.
  A leg with the SPL example hook adds about 37,000 to 58,500 CU.

### Transfer hooks

`sendfun_sdk::transfer_hook` resolves an armed hook's accounts offline, from
the accounts that `update` read. For each hooked leg, the metas add the
resolved extras, then the hook program and its `ExtraAccountMetaList`
(`["extra-account-metas", mint]`). An account in both legs appears once. It is
writable if either leg needs it writable. `get_accounts_len()` counts the same
accounts. Before the list loads, `missing_dynamic_accounts_as_default` gives
only the hook program and the list. These metas size a route. Do not send a
swap with them.

The market stays active but refuses every quote and swap while its hook:

- reads data that is not fixed before the swap: the transfer amount, a
  wallet's data, or an extra's data;
- needs an extra to sign;
- has a malformed list, or has no list after `update` asks for it;
- resolves an extra, for every user, onto a trade account when the trade or
  the extra is writable. A read-only extra that repeats the market (the
  curve or pool) is not a clash. A clash with one wallet's own accounts fails
  only that wallet's `get_swap_and_account_metas`.

## Limits

- **Hook depth.** Under Jupiter's CPI, a hook runs at stack height 4. With a
  depth limit of 5, the hook can make one nested CPI. With the SIMD-0268 limit
  of 9, it can make five.
- **Near curve completion.** A buy that lands first reduces the supply left.
  Then an exact-in buy fills less or fails with `SlippageExceeded`, and an
  exact-out buy fails with `InsufficientLiquidity`. A sell that lands first
  adds to the supply left. Then a buy quoted as capped can spend all of
  `amount`. `in_amount` is not a spend limit.
- **Token accounts.** Slots 7 and 8 take only a token account of the right
  mint that `token_transfer_authority` owns. Jupiter's routes meet this. A
  slot with no account data must be the owner's ATA. The program creates it.

## Parity suite

`tests/parity.rs` runs offline against `tests/fixtures/`:
`send.fun_launchpad.so` and `send.fun_amm.so`, the
[`spl-transfer-hook-example`](https://github.com/solana-program/transfer-hook)
hook, and each market's mainnet accounts under `accounts/<market>/`. The two
program binaries are the test kit's dumps of the mainnet programs.

`jupiter-amm-test-kit` runs exact-in swaps as the default partner.
`assert_native` runs both modes, and a signing partner, in `LiteSVM`. It funds
the source with exactly `quote.in_amount`. The source must drain to zero, and
the destination must rise by exactly `quote.out_amount`. Derived-state tests
move one snapshot field, such as the clock, a reserve, or a hook on the quote
mint. Past each edge, the adapter and the program both refuse. Synthetic tests
put a hook, a transfer fee or a pause on a snapshot's base mint. They cover a
DEX base mint that the DEX authority lists. A launchpad base mint never has
these extensions.

To re-snapshot, see the `REFRESH=1` command at the top of `tests/parity.rs`.
A refresh also dumps both program binaries again.

## License

MIT

`tests/fixtures/8mTbJtVKaLQ9pVgEMkjgu2N3EvBKyMXBxkPZvBW8d5Qk.so` is a build of
[`spl-transfer-hook-example`](https://github.com/solana-program/transfer-hook)
0.6.0, which is Apache-2.0. Its license is
`tests/fixtures/spl-transfer-hook-example.LICENSE`. The build is
`--no-default-features` at commit `94dfc28d`. It differs from that commit in
three files: `entrypoint.rs` returns the processor's result without logging
the error, `lib.rs` drops its docs and the unused `state` module, and
`Cargo.toml` is a standalone manifest on `spl-transfer-hook-interface` 2.1.0.
