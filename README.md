# nautilus-txflow

TxFlow perpetuals exchange adapter for NautilusTrader 0.67.0 (Rust). Perpetual contracts only.
The parity target is `nautilus-hyperliquid` 0.67.0; the structure follows `nautilus-risex`.
The feature gaps against `nautilus-lighter` 0.67.0 that the venue can serve were closed on
12 Nov 2026 (book depth, instrument status, live position reports, TP/SL order lists,
run-time leverage and margin mode); `docs/STATUS.md` has the list and what stays open.

**Status: not run against the venue with a key.** No signed request went to TxFlow, on testnet
or on mainnet. All tests run offline against fixtures and a local mock server. Signing is
confirmed by TxFlow test vectors for a plain limit order and for `approveAgent` only. Read
`docs/STATUS.md` before you connect an account.

## Warnings

- `examples/exec_tester.rs` has `DRY_RUN = true`. With `false` it submits orders on **mainnet**
  with real funds. It was compiled and never run.
- `txflow-flatten` cancels every order and closes every position of the account. It waits for
  Enter before it sends. It was compiled and never run against the venue.
- `txflow-agent-revoke` revokes one agent of the account. The execution client cannot trade
  with that agent afterwards. It waits for Enter before it sends. It was compiled and never run
  against the venue.
- The agent key signs orders. The account (wallet) key is necessary for `txflow-agent-revoke`
  only: do not put it in the config of the client. The agent must be approved for the account
  first (`approveAgent`, signed by the account key); this crate has the signing function and
  sends that action in one place only, `txflow-agent-revoke`, with the zero address.
- There is no dead man's switch at the venue. Open orders stay on the book when the process
  stops.

## Run

```bash
cargo test --features examples
```

```bash
cargo run --example data_tester --features examples
```

```bash
cargo run --bin txflow-trades-probe -- AERO-USDC-PERP.TXFLOW 20
```

```bash
cargo run --example exec_tester --features examples
```

```bash
cargo run --bin txflow-flatten
```

```bash
cargo run --bin txflow-agent-revoke -- my-trading-bot
```

```bash
cargo bench --bench signing
```

`data_tester` and `txflow-trades-probe` read public mainnet data and use no key. `exec_tester`,
`txflow-flatten` and `txflow-agent-revoke` need the environment variables below (`exec_tester`
also reads a `.env` file at run time; the binaries do not). Pass `testnet` as the first
argument of the two binaries to target testnet. The argument of `txflow-agent-revoke` is the
label of the agent; without it the unnamed agent is revoked.

| Variable | Use |
|---|---|
| `TXFLOW_ACCOUNT_ADDRESS` | Account (wallet) address, mainnet |
| `TXFLOW_AGENT_PRIVATE_KEY` | Key of the approved agent, mainnet |
| `TXFLOW_ACCOUNT_PRIVATE_KEY` | Wallet key, mainnet; `txflow-agent-revoke` only |
| `TXFLOW_TESTNET_*` | The same three for testnet |

The process reads the keys at run time only and does not print them. The `Debug` output of the
config hides the key. Public data needs no variable at all.

`txflow-flatten` sends one IOC order per position, priced `market_order_slippage_bps`
(default 50) past the book. A thin book leaves a rest: run it again until it prints
"Nothing to do".

Hosts: mainnet `https://rpc.txflow.com`, testnet `https://rpc.txflow-testnet.xyz`. One host
serves `/info`, `/exchange`, `/ws` and `/api-ws`. `base_url_http` in the config replaces the
host for all four.

## Parity with `nautilus-hyperliquid` and `nautilus-lighter` 0.67.0

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
| Book depth, 10 levels | Cut from the levels the adapter keeps from `/ws l2BookDepth`, one per book frame, with the order count of each level | Emulated |
| Quotes | `/ws bbo` | Direct |
| Trades | `/ws trades` | Direct |
| `request_trades` | `/info trades`, the latest 300 only | Direct, no history |
| Bars (live) | `/api-ws candle` | Direct, emitted after the close |
| `request_bars` | `/info candleSnapshot`: 1m 3m 5m 15m 30m 1h 2h 4h 8h 12h 1d | Direct |
| Mark price, index price | `/ws allAssetCtxs`, one subscription for all markets, one frame each 3 s (index = `oraclePx`) | Direct |
| Funding rate (live) | `/ws allAssetCtxs`, hourly, percent converted to a fraction | Direct |
| `request_funding_rates` | The current rate only | Not on venue (no public history) |
| Book recovery | Sequence gap → resubscribe, new snapshot | Direct |
| Instrument status | `/info perpMeta` polled every `instrument_status_poll_secs` (default 60): `haltTrading` → `HALT`, `delisted` → `NOT_AVAILABLE_FOR_TRADING`, else `TRADING` | Emulated |
| Spot, outcome markets | — | Not done |

### Orders

| Feature | TxFlow | Result |
|---|---|---|
| `LIMIT`, `post_only`, `reduce_only` | `order`, `Gtc` / `Ioc` / `Alo` | Direct |
| `MARKET` | `FrontendMarket`, price = opposite quote ± slippage | Direct, needs a cached quote |
| TIF `GTD` | — | Not on venue for perpetuals (three pages of the docs say that perps reject `Gtd`) |
| TIF `FOK` | — | Not on venue |
| `STOP_MARKET`, `STOP_LIMIT`, `MARKET_IF_TOUCHED`, `LIMIT_IF_TOUCHED` | `order` with `t.trigger`, grouping `normalTrigger` | Direct, mark-price trigger only |
| Trigger on the last price | — | Not on venue (the order request has no trigger-type field; triggers fire on the mark price) |
| Modify | `modify2`, in place | Direct, limit orders only |
| Cancel | `cancel` by order id; `cancelByCloid` before the id is known | Direct |
| Batch cancel | One action, 200 per action | Direct |
| Order list: parent + TP/SL | One `order` action, grouping `normalTpsl`, for a market or limit parent with one or two trigger children that close it | Direct, **not verified** (see Limits) |
| Order list, any other shape | One `order` action, 50 per action; trigger orders one per action | Direct, no link between the orders |
| Cancel-all | Batched cancels over the open orders of the cache | Emulated |
| Leverage | `updateLeverage` at connect, from the config, and `TxflowExecutionClient::update_leverage` at run time | Direct |
| Margin mode | `m` on every order, from the config; `TxflowExecutionClient::update_margin_mode` at run time | Direct |
| Position TP/SL (`positionTpsl`, `modifyTpsl`), OCO without a parent | — | Not done |
| TWAP, vault trading, builder fee | — | Not done |
| Dead man's switch, native cancel-all, batch modify | — | Not on venue |

### Account and reconciliation

| Feature | TxFlow | Result |
|---|---|---|
| Live order events | `/ws orderUpdates` | Direct |
| Live fill events | Derived from the change of each `orderUpdates` row | Emulated (the venue pushes no fills: `/api-ws userFills` is one snapshot) |
| Account state | `/info clearinghouseState` + `/ws clearinghouseState` | Direct |
| Query order | `/info orderDetails`, `frontendOpenOrders`, `triggerOrders` | Direct |
| Order status reports | `frontendOpenOrders`, `triggerOrders`, `historicalOrders` | Direct |
| Fill reports | `/info userFills`, paged | Direct |
| Position reports | `/info clearinghouseState` | Direct |
| Live position reports | `/ws clearinghouseState`, one report per change of a position size | Direct |
| Catch-up after a reconnect | `historicalOrders` since connect | Direct |

### Around the adapter

| Item | State |
|---|---|
| `data_tester`, `exec_tester` examples | Feature `examples` |
| Mock-server integration tests | `tests/data_client.rs`, `tests/exec_client.rs` |
| Python config, enums, factories | Feature `python` (see below), `tests/python.rs` |
| Fuzz targets | `http_parse`, `ws_parse`, `book_apply`, `action_sign`, `wire_format` |
| Benches | `data`, `exec`, `micros`, `signing` (criterion) |
| CI workflow | `.github/workflows/ci.yml`: fmt, clippy, test (also with `examples`), and a `fuzz` job of 30 s per target. Not run yet: nothing was pushed |
| `txflow-trades-probe` | Public trades of one market, no key |
| `txflow-flatten` | Compiles, unit tests of the plan; **never run against the venue** |
| `txflow-agent-revoke` | Compiles, unit tests of the request; **never run against the venue** |

## Limits of the emulated features

- **Fills** come from the order stream, because the venue has no live fills channel: the
  `userFills` subscription of `/api-ws` sends one snapshot and nothing after, and `/ws trades`
  with a `user` filter has the trade id but no order id. One event
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
- **Book depth** has ten levels per side whatever depth the subscription asks. It is sent
  for each book frame, so it follows the book channel (a snapshot, then deltas).
- **Instrument status** changes arrive up to one poll interval late. A market that was
  delisted before the connect is not loaded and has no status.
- **TP/SL order lists** match one shape only: a market or limit order first, then one or two
  trigger orders (`STOP_*` as stop-loss, `*_IF_TOUCHED` as take-profit) that name it as
  parent, on the opposite side, with its quantity. A bracket whose take-profit is a plain
  limit order does not match and is sent without a link, as before. The docs name the
  `normalTpsl` grouping and show no request, so the order of the action follows the
  Hyperliquid convention. The children get their venue order ids from `/info tpslOrders`
  (matched on market and trigger price) and are cancelled with `grouping: "tpsl"`; after a
  restart the adapter no longer knows which orders were TP/SL children. If one order of the
  group cannot be sent, the whole group is denied.
- **Run-time leverage and margin mode** are methods of the Rust client, not commands of the
  execution trait: they send the action in the background and log the answer. After a
  confirmed `update_margin_mode` the orders of that market carry the new mode.
- **Live position reports** are sent when the size of a position changes, not for each frame.
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

No target ran yet on this machine. The `fuzz` job of the CI workflow installs nightly and
`cargo-fuzz` on the runner and runs each target for 30 seconds. The body of each target is a
function in `src/fuzz.rs`, and `cargo test` feeds each one about 400 changed fixtures and
random inputs with a fixed seed.

## License

LGPL-3.0-only.
