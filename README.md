# nautilus-txflow

TxFlow perpetuals exchange adapter for NautilusTrader 0.67.0 (Rust). Perpetual contracts only.
The parity target is `nautilus-hyperliquid` 0.67.0; the structure follows `nautilus-risex`.

**Status: not run against the venue with a key.** No signed request went to TxFlow, on testnet
or on mainnet. All tests run offline against fixtures and a local mock server. Signing is
confirmed by TxFlow test vectors for a plain limit order and for `approveAgent` only. Read
`docs/STATUS.md` before you connect an account.

## Warnings

- `examples/exec_tester.rs` has `DRY_RUN = true`. With `false` it submits orders on **mainnet**
  with real funds. It was compiled and never run.
- The agent key signs orders. The account (wallet) key is not necessary for this adapter: do
  not put it in the config. The agent must be approved for the account first (`approveAgent`,
  signed by the account key); this crate has the signing function and never sends that action.
- There is no dead man's switch at the venue. Open orders stay on the book when the process
  stops.

## Run

```bash
cargo test
```

```bash
cargo run --example data_tester --features examples
```

```bash
cargo run --example exec_tester --features examples
```

`data_tester` reads public mainnet data and uses no key. `exec_tester` needs the environment
variables below (it also reads a `.env` file at run time).

| Variable | Use |
|---|---|
| `TXFLOW_ACCOUNT_ADDRESS` | Account (wallet) address, mainnet |
| `TXFLOW_AGENT_PRIVATE_KEY` | Key of the approved agent, mainnet |
| `TXFLOW_TESTNET_ACCOUNT_ADDRESS` | The same for testnet |
| `TXFLOW_TESTNET_AGENT_PRIVATE_KEY` | The same for testnet |

The process reads the key at run time only and does not print it. The `Debug` output of the
config hides the key. Public data needs no variable at all.

Hosts: mainnet `https://rpc.txflow.com`, testnet `https://rpc.txflow-testnet.xyz`. One host
serves `/info`, `/exchange`, `/ws` and `/api-ws`. `base_url_http` in the config replaces the
host for all four.

## Parity with `nautilus-hyperliquid` 0.67.0

- **Direct**: the venue has an endpoint or a channel for the same job.
- **Emulated**: the adapter builds the feature from other calls. The limits are in the next
  section.
- **Not on venue**: the adapter refuses the command with a reason, or returns less.
- **Not done**: the venue permits it, but this crate does not implement it.

### Data

| Feature | TxFlow | Result |
|---|---|---|
| Instruments | `/info perpMeta`; delisted markets skipped | Direct |
| Book deltas (`L2_MBP`) | `/ws l2BookDepth`, snapshot + deltas with `seq`/`prevSeq` | Direct |
| Book snapshot request | `/info l2Book`, 20 levels per side | Direct |
| Book depth, 10 levels | — | Not done |
| Quotes | `/ws bbo` | Direct |
| Trades | `/ws trades` | Direct |
| `request_trades` | `/info trades`, the latest 300 only | Direct, no history |
| Bars (live) | `/api-ws candle` | Direct, emitted after the close |
| `request_bars` | `/info candleSnapshot`: 1m 3m 5m 15m 30m 1h 2h 4h 8h 12h 1d | Direct |
| Mark price, index price | `/ws activeAssetCtx` (index = `oraclePx`) | Direct |
| Funding rate (live) | `/ws activeAssetCtx`, hourly, percent converted to a fraction | Direct |
| `request_funding_rates` | The current rate only | Not on venue (no public history) |
| Book recovery | Sequence gap → resubscribe, new snapshot | Direct |
| Instrument status | — | Not done |
| Spot, outcome markets | — | Not done |

### Orders

| Feature | TxFlow | Result |
|---|---|---|
| `LIMIT`, `post_only`, `reduce_only` | `order`, `Gtc` / `Ioc` / `Alo` | Direct |
| `MARKET` | `FrontendMarket`, price = opposite quote ± slippage | Direct, needs a cached quote |
| TIF `GTD` | — | Refused (the trading guide limits `Gtd` to outcome markets) |
| TIF `FOK` | — | Not on venue |
| `STOP_MARKET`, `STOP_LIMIT`, `MARKET_IF_TOUCHED`, `LIMIT_IF_TOUCHED` | `order` with `t.trigger`, grouping `normalTrigger` | Direct, mark-price trigger only |
| Modify | `modify2`, in place | Direct, limit orders only |
| Cancel | `cancel` by order id; `cancelByCloid` before the id is known | Direct |
| Batch cancel | One action, 200 per action | Direct |
| Order list | One `order` action, 50 per action; trigger orders one per action | Direct, no link between the orders |
| Cancel-all | Batched cancels over the open orders of the cache | Emulated |
| Leverage | `updateLeverage` at connect, from the config | Direct, no runtime command |
| TP/SL groups, OTO, OCO, bracket | — | Not done |
| TWAP, vault trading, builder fee | — | Not done |
| Dead man's switch, native cancel-all, batch modify | — | Not on venue |

### Account and reconciliation

| Feature | TxFlow | Result |
|---|---|---|
| Live order events | `/ws orderUpdates` | Direct |
| Live fill events | Derived from the change of each `orderUpdates` row | Emulated |
| Account state | `/info clearinghouseState` + `/ws clearinghouseState` | Direct |
| Query order | `/info orderDetails`, `frontendOpenOrders`, `triggerOrders` | Direct |
| Order status reports | `frontendOpenOrders`, `triggerOrders`, `historicalOrders` | Direct |
| Fill reports | `/info userFills`, paged | Direct |
| Position reports | `/info clearinghouseState` | Direct |
| Catch-up after a reconnect | `historicalOrders` since connect | Direct |
| Live position reports from the channel | — | Not done (REST only) |

### Around the adapter

| Item | State |
|---|---|
| `data_tester`, `exec_tester` examples | Feature `examples` |
| Mock-server integration tests | `tests/data_client.rs`, `tests/exec_client.rs` |
| Python config, enums, factories | Feature `python` (see below), `tests/python.rs` |
| Fuzz targets | `http_parse`, `ws_parse`, `book_apply`, `action_sign`, `wire_format` |
| Benches, CI workflow, flatten and revoke binaries | Not done |

## Limits of the emulated features

- **Fills** come from the order stream, because the venue has no live fills channel. One event
  can cover several venue trades: the quantity is the change of the filled size, and the price
  comes from the change of the average fill price. The trade id is
  `<order id>-<cumulative filled>`, not the venue trade id. The liquidity side is known for
  post-only orders (maker) and for market and IOC orders (taker) only.
- **Market orders** need a quote of the instrument in the cache. Subscribe to quotes first.
  The slippage is `market_order_slippage_bps` (default 50).
- **Cancel-all** sends cancels for the orders the cache shows as open. An order the cache does
  not know stays on the book.
- **Trigger orders** are stored outside the book at the venue. The adapter reads them from
  `/info triggerOrders` after the submit and at reconciliation; it does not poll them. Whether
  the order stream reports their trigger and fill is not confirmed.
- **Order acceptance** comes from the order stream, not from the HTTP answer. The answer has
  one status for the whole action and no order id.
- **A batch** gets one answer in the documented format: a refusal rejects every order of the
  action. Per-order statuses are read if the venue sends them.
- **Live bars** are emitted when a later candle arrives, or 2 s after the close time.
- **Rejection reasons** are in the HTTP answer only. A `Rejected` row of the stream has none.

## Python

The `python` feature has `TxflowEnvironment`, `TxflowMarginMode`, the two configs and the two
factories, registered with the Nautilus PyO3 registry as in `nautilus-risex`.

The published `nautilus-core` 0.67.0 does not build its `python` feature from crates.io: it
needs `pyo3-stub-gen =0.20.0`, which does not compile with `pyo3` 0.30.1. NautilusTrader patches
that crate in its own workspace (`patches/pyo3-stub-gen`). Get that directory from the
NautilusTrader repository, then give its path to cargo:

```bash
cargo test --features python --test python --config 'patch.crates-io.pyo3-stub-gen.path="/path/to/nautilus_trader/patches/pyo3-stub-gen"'
```

The command rewrites `Cargo.lock` (the patched crate becomes a path source). Restore the file
afterwards: `git checkout Cargo.lock`, once the file is committed.

With the patch, the feature compiles, clippy is clean, and the test passes (11 Nov 2026, patch
from commit `d53849b216e20f2c122ca01b4dd0da377638b186`).

Not confirmed: a wheel that a Python `TradingNode` can load. This crate has no `cdylib` target,
and a separate extension module has its own copy of the Nautilus registry. Use the Rust
`LiveNode` (see `examples/`).

## Fuzz

The `fuzz/` crate needs a nightly toolchain and `cargo-fuzz`:

```bash
cargo +nightly fuzz run http_parse
```

No target ran yet. The body of each target is a function in `src/fuzz.rs`, and `cargo test`
feeds each one about 400 changed fixtures and random inputs with a fixed seed.

## License

LGPL-3.0-only.
