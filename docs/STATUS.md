# Status

Date: 11 Nov 2026. Plan: `docs/PLAN.md`. Steps A (plan), B (data), C (signing), D (execution),
Python binding and fuzz package are done. The testnet gate below is not started.

## Summary

The crate builds and all offline tests pass. **No signed request went to TxFlow, on testnet or
on mainnet.** `/exchange` was never called. The only venue contact was public, read-only data
on mainnet: the samples behind the market-data fixtures in `tests/fixtures/`, and one run of
`data_tester`.

**Signing and execution are UNVERIFIED against the live venue, by design.** Signing is
confirmed by TxFlow's published test vectors for two actions only: a plain limit `order` and
`approveAgent`. Each other signed body follows the documentation alone. The execution client
was tested against a local mock server whose answers are hand-made from the documentation.

## Verification

All on Rust 1.99.1, 11 Nov 2026.

| Check | Result |
|---|---|
| `cargo fmt --check` (root and `fuzz/`) | Clean |
| `cargo build --all-targets --features examples` | Passes |
| `cargo clippy --all-targets --features examples -- -D warnings` | Clean |
| `cargo test` | 131 passed: 118 unit, 2 data client, 11 execution client |
| `cargo check --manifest-path fuzz/Cargo.toml --target-dir target` | Passes (type check only) |
| `cargo clippy --features python -- -D warnings` (with the patch, see Python) | Clean |
| `cargo test --features python --test python` (with the patch) | 1 passed |
| `data_tester`, 25 s, mainnet, no key | 0 ERROR lines, 0 WARN lines (see Smoke run) |
| `exec_tester` | Compiles. **Never run.** |

The tests use no real key: the `0xab…ab` key of the TxFlow vectors, a `0x01…01` key in the fuzz
code, and a key generated in the test (`tests/exec_client.rs`). The mock server is on
`127.0.0.1`. The tests open no connection to the venue.

Mass status is the default of the `ExecutionClient` trait: it composes the order, fill and
position reports of this client.

The execution-client test ran 30 times in a row after a race in the test harness was fixed
(the mock showed an order before its request had arrived): 30 passes.

### Smoke run

One run, 10 Nov 2026, 19:29:26 to 19:29:51 UTC (11 Nov, 02:29 local time), host
`rpc.txflow.com`, no credentials.

- 242 instruments loaded (`perpMeta` has 250 markets; delisted ones are skipped).
- `BTC-USDC-PERP.TXFLOW` and `ETH-USDC-PERP.TXFLOW`, 13 subscriptions.
- Received: book deltas (snapshot of 200 bid and 200 ask levels, then deltas), 14 quote
  lines, 13 trade lines, 8 mark price, 8 index price and 8 funding rate updates.
- 2671 log lines, 0 ERROR, 0 WARN. The process was stopped by an alarm (exit 142, expected).
- **0 bars**: the window crossed no minute boundary. Live bar emission is covered by a
  recorded candle frame in `tests/data_client.rs`, not by the venue.

## Done, by plan step

| Step | Content | Verified how |
|---|---|---|
| A | `docs/PLAN.md` | — |
| B | `common`, `http`, `websocket`, `book`, `config`, `data`, `factories`, `data_tester` | Unit tests, `tests/data_client.rs`, fixtures recorded from mainnet, smoke run |
| C | `signing` (agent flow, `approveAgent`, nonce manager) | Unit tests with TxFlow vectors (see below) |
| D | `execution` (submit, modify, cancel, cancel-all, batch, trigger orders, reports, mass status, account state), `exec_tester` | Unit tests, `tests/exec_client.rs` against a mock; **not against the venue** |
| Python | `src/python`, `tests/python.rs` | Clippy and 1 test, with the patch |
| Fuzz | `fuzz/`, `src/fuzz.rs` | Type check; unit tests with changed fixtures; **no target ran** |

### Signing: what has a vector

| Part | Proof |
|---|---|
| Domain separators, testnet and mainnet | TxFlow vector |
| `order` action, plain limit, no optional field: MessagePack bytes, connection id, signing hash, `r`/`s`/`v` | TxFlow vector |
| `approveAgent`: struct hash, signing hash | TxFlow vector |
| Vault flag in the connection id | Hyperliquid reference value only |
| `cancel`, `cancelByCloid`, `modify2`, `updateLeverage`, `updateMarginMode` | **None.** Field order from the request examples of the docs |
| Order with `c`, `m`, `expiresAt`, trigger body, cancel `grouping` | **None.** Same |

A wrong field order gives a valid signature of another signer, so the venue answers "agent is
not authorized". That is the first thing to watch on testnet.

### Fixtures

| Fixture | Origin |
|---|---|
| `perpMeta`, `l2Book`, `trades`, `candleSnapshot`, `marketTicker`, `activeAssetCtx`, `ws_frames.jsonl` (market frames only) | Recorded from mainnet, 10 Nov 2026 UTC (cut to size) |
| `clearinghouseState`, `frontendOpenOrders`, `historicalOrders`, `orderDetails`, `triggerOrders`, `userFills`, `error`; the `orderUpdates` frames and every `/exchange` answer built in `tests/exec_client.rs` | **Hand-made from the docs.** No user endpoint was read live |

## The testnet gate

For the account owner, on testnet, with a small size. Each step confirms items of the next
section.

1. Approve an agent key for the account (outside this crate). Set
   `TXFLOW_TESTNET_ACCOUNT_ADDRESS` and `TXFLOW_TESTNET_AGENT_PRIVATE_KEY`.
2. Connect the execution client with no order. Check the account state, the position reports
   and `updateLeverage` (a refusal shows whether `marginMode` is necessary).
3. Post-only limit order far from the market: accepted, modify, cancel by order id.
4. Cancel before the acceptance (by client order id).
5. Post-only order that crosses: read the reject wording.
6. Small market order: fill events, fee, average price; compare with `userFills`.
7. Limit order filled in two parts: two fill events, trade ids, total fee.
8. Stop-market and stop-limit order: listing in `triggerOrders`, cancel, then one that triggers.
9. Batch of three limit orders with one bad price: read the shape of the answer.
10. Disconnect the network for 30 s with one open order: resubscription and catch-up.
11. Only then set `DRY_RUN = false` in `exec_tester`, and only on testnet first.

## Not verified against the venue

### Markers in the code

| Place | Kind | Open point |
|---|---|---|
| `src/execution/mod.rs:97` | unverified | Values of `triggerState`. A listed trigger order with no readable state is taken as open |
| `src/execution/mod.rs:167` | unverified | Wording of the post-only reject. The adapter looks for "post" in the reason |
| `src/execution/mod.rs:185` | unverified | Whether the order stream shows trigger orders. Their ids are read once from `triggerOrders` after the action |
| `src/execution/mod.rs:270` | unverified | A cancelled trigger order may get no row, so the HTTP answer closes it |
| `src/execution/order.rs:109` | unverified | Meaning of `p` on a market trigger order. The adapter sends the trigger price ± slippage (Hyperliquid convention) |
| `src/http/parse.rs:72` | assumption | Maintenance margin = half of the initial margin at maximum leverage |
| `src/http/parse.rs:197` | shortcut | Fill trade id is `<oid>-<cumulative filled>`. A `userFills` window that misses the first fills of an order gives other ids than the live stream |
| `src/http/parse.rs:337` | shortcut | Fee booked in the quote currency whatever `feeToken` says |
| `src/http/client.rs:48` | shortcut | One history request reads at most 50 pages (100,000 rows) or 20 candle windows |
| `src/execution/dispatch.rs:82` | shortcut | Closed orders stay tracked until the map is full, then all of them are dropped |
| `src/execution/dispatch.rs:289` | shortcut | Without an average fill price the limit price stands in |
| `src/execution/mod.rs:509` | shortcut | The catch-up window starts at connect, so each reconnect reads all orders since connect again |

### Other open points

Each needs a signed request or a funded account to settle.

- Every signed action except the two with a vector (see Signing).
- `orderUpdates`: whether `fee` is cumulative (assumed), what `sz` is on a cancelled row,
  whether a new subscription replays the open orders (the catch-up comment assumes it does).
- Order of a `Rejected` row of the stream and the reason in the HTTP answer.
- `modify2`: whether the order keeps its order id and client order id (the docs say in place).
- Batch answers: whether per-order `statuses` exist, and whether a batch fails as a whole.
- `FrontendMarket`: whether the venue uses `p` as a price cap.
- Trigger orders: whether `tpsl` is necessary under `normalTrigger`, whether a batch under
  `normalTrigger` is one group, whether a triggered order keeps its ids, the value formats of
  `triggerOrders`, whether cancel needs `grouping: "trigger"`.
- `updateLeverage` without `marginMode`.
- Wire format of numbers: the adapter keeps the decimals of the step (`"0.0030"`,
  `"65000.0"`). The TxFlow vector uses `"50000.0"` and `"0.01"`, so both forms may be valid;
  not confirmed for trailing zeros in sizes.
- Whether a 4xx answer always means that the action did not run (the docs say so for 429).
- `historicalOrders`: one row per order is assumed.
- `frontendOpenOrders` with `limit: 2000`: the docs give no default or maximum for this type.
- Halted markets are loaded and delisted ones skipped; the meaning of the market state values
  was read from the docs only.
- Live bars against the venue (no bar closed during the smoke run).
- `/api-ws` reconnect and resubscription against the venue (covered by a local test only).
- Testnet: no request of any kind went to `rpc.txflow-testnet.xyz`.

## Documentation: differences and ambiguities

Sources: the docs downloaded on 11 Nov 2026, live public samples, the research of 5 Nov 2026
(`stoikovgrid/docs/txflow-api.md`). The docs win over the research; a live sample wins over
both for its own field.

Differences found:

| Point | Research, 5 Nov | Docs, 11 Nov | Live | Adapter |
|---|---|---|---|---|
| `Gtd` on perps | "perps reject `Gtd`" | The API reference lists `Gtd` with no limit; the trading guide limits it to outcome markets | Not tested | Refuses GTD |
| Candle values on `/api-ws` | — | Numbers | Strings | Reads both |
| Live fills | `/ws trades` with a `user` filter, plus `orderUpdates` | No live fills channel; `orderUpdates` has size, average price, fee | Not tested | `orderUpdates` only |
| Fee ladder | Testnet ladder differs from the docs | Platform ladder | Not read | Not used; fees come from the rows |
| `/info trades` | Looked stale on testnet (newest row 6 days old) | Latest 300 | Mainnet rows were of the capture hour | Used for `request_trades` |

The research and the docs agree on the points the adapter depends on most: domain names and
chain ids, the Agent struct, the MessagePack preimage without `type`, key order, nonce rules,
the envelope, the one-status answer, `modify2`, limits, ping, funding in percent. The
comparison was made point by point for the plan, not line by line.

Ambiguities in the docs:

- Casing differs between pages: `tif` "GTC" and "Gtc", status "filled" and "Filled", order
  type "Limit" and "limit". The adapter compares without case.
- `modify2.tpsl`: one page says that an absent field clears the TP/SL children, another says
  that it keeps them. The adapter never sends the field and has no TP/SL children.
- `historicalOrders.sz` is documented as the remaining size, but the example of a filled order
  shows the full size. The adapter takes the status and `filledAmount` first.
- Whether trigger orders appear in `frontendOpenOrders` and `orderUpdates`, or in
  `triggerOrders` only.
- `p: "bbo"` is named but not defined for market orders. The adapter does not send it.
- Reject reasons are not in `orderUpdates`.
- `triggerState` values are not listed.
- No public funding history endpoint is documented (`userFundings` is per account).

## Not done

Out of scope by the brief:

- Benches, CI workflow.
- Flatten and revoke binaries.
- Outcome (prediction) markets, spot.
- Builder codes, `approveBuilderFee`.
- Deposits, withdrawals, transfers.

Left out by this crate:

- Vault trading, TWAP orders, TP/SL groups (`normalTpsl`, `positionTpsl`), `reversePosition`,
  `closeAll`, hedge position mode.
- Book depth (10 levels) subscription, instrument status.
- `"bbo"` as a market price; a market order needs a cached quote.
- Polling of trigger orders. They are read after the submit and at reconciliation only.
- `userFills.tid` or `/ws trades` with a `user` filter as the source of trade ids.
- Live position reports from `/ws clearinghouseState` (balances only; positions over REST).
- A runtime command for leverage or margin mode (`updateMarginMode` has a builder only).
- A separate WebSocket URL in the config: `base_url_http` gives the host of all four paths.
- A dead man's switch: the venue has none.

## Python

The `python` feature compiles and its test passes, with one condition. The published
`nautilus-core` 0.67.0 needs `pyo3-stub-gen =0.20.0`, and that crate does not compile with
`pyo3` 0.30.1. NautilusTrader carries a local patch for it (`patches/pyo3-stub-gen`, commit
`d53849b216e20f2c122ca01b4dd0da377638b186`, the copy downloaded for `nautilus-risex` on
10 Nov 2026). The published crates do not carry the patch, so the build needs it on the command
line. The command is in `README.md`. It rewrites `Cargo.lock`; the file here is the unpatched
one.

Not confirmed: a route to a wheel that a Python `TradingNode` can load. This crate has no
`cdylib` target and no top-level module. A separate extension module has its own copy of the
Nautilus PyO3 registry, so the official wheel cannot see its factories. The Rust `LiveNode` does
not have this problem.

There is no revoke function (the venue action is out of scope). `TxflowMarginMode` goes to the
config as the enum class.

The module function `txflow` registers its extractors in a process-wide registry. A second call
in one process fails.

## Fuzz

Five targets type-check on 1.99.1: `http_parse`, `ws_parse`, `book_apply`, `action_sign`,
`wire_format`. **No target ran**: `cargo-fuzz` is not installed here and no nightly toolchain
was used. There is no seed corpus.

Substitute: the body of each target is a function in `src/fuzz.rs`, and one unit test per
target feeds it each fixture unchanged, then 400 inputs with a fixed seed (changed fixtures:
cut, a digit replaced, a long run of nines; every fourth input is random bytes). In one
measurement 184 of 404 HTTP inputs and 79 of 408 WebSocket inputs still decoded, so the
parsers behind the decoder were reached.

These tests and a review of the same paths found panics on hostile venue values. All are fixed
and have a test (`hostile_values_are_errors_not_panics`):

- `TxflowOrder::filled()`: `clamp` panicked on a negative size.
- `parse_fill_reports`: decimal overflow in the running total.
- `fill_trade_id`: an id over 36 characters panicked.
- `parse_instrument`: a blank or non-ASCII market or currency name panicked.
- `execution/dispatch.rs` `emit_fill`: unchecked decimal arithmetic.

`src/execution/dispatch.rs` has no fuzz target (it needs an event emitter). The 400-input
tests are a smoke check, not coverage-guided fuzzing.
