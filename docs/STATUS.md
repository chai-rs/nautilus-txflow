# Status

Date: 13 Nov 2026. Plan: `docs/PLAN.md`. Steps A (plan), B (data), C (signing), D (execution),
Python binding and fuzz package are done (11 Nov). Step E, the feature gaps against
`nautilus-lighter` 0.67.0, was added on 12 Nov: see "Step E" below. The tooling around the
adapter (benches, three binaries, CI workflow) was added on 13 Nov: see "Tooling" below. The
testnet gate is not started.

## Summary

The crate builds and all offline tests pass. **No signed request went to TxFlow, on testnet or
on mainnet.** `/exchange` was never called. The only venue contact was public, read-only data
on mainnet: the samples behind the market-data fixtures in `tests/fixtures/`, one run of
`data_tester` (both on 10 Nov), and a second run of `data_tester` after the tooling step
(12 Nov UTC). Step E made no request to the venue at all. `txflow-flatten`,
`txflow-agent-revoke` and `exec_tester` compile and were **never run against the venue**.

**Signing and execution are UNVERIFIED against the live venue, by design.** Signing is
confirmed by TxFlow's published test vectors for two actions only: a plain limit `order` and
`approveAgent`. Each other signed body follows the documentation alone. The execution client
was tested against a local mock server whose answers are hand-made from the documentation.

## Verification

All on Rust 1.99.1, 13 Nov 2026, after the tooling step.

| Check | Result |
|---|---|
| `cargo fmt --check` (root and `fuzz/`) | Clean |
| `cargo build --all-targets --features examples` | Passes |
| `cargo clippy --all-targets --features examples -- -D warnings` (and without the feature) | Clean |
| `cargo test --features examples` (and without the feature) | 170 passed: 127 unit, 25 in the binaries (12 `txflow-flatten`, 7 `txflow-agent-revoke`, 6 `txflow-trades-probe`), 3 data client, 15 execution client (before the tooling step: 145 = 127 + 3 + 15; no test was removed) |
| `cargo bench --no-run` | Builds 4 benches |
| `cargo bench` | One run, see Benches |
| `cargo build --bins` | Builds the 3 binaries |
| `cargo check --manifest-path fuzz/Cargo.toml` | Passes (type check only) |
| `cargo clippy --features python -- -D warnings` (with the patch, see Python) | Clean on 12 Nov. **Not run again after the tooling step** (one field was added to `Position`) |
| `cargo test --features python --test python` (with the patch) | 1 passed (run again on 12 Nov 17:45 UTC, after the tooling step and the `allAssetCtxs` fix) |
| `data_tester`, 80 s, mainnet, no key, after step E and the tooling step | 0 ERROR lines, 0 WARN lines; book depth and instrument status received (see Smoke run) |
| `txflow-trades-probe` | Compiles. **Not run**: it sends two requests in the same second, over the limit this step worked under. Its two calls are those of the test `market_data_requests_parse_the_live_fixtures` |
| `exec_tester`, `txflow-flatten`, `txflow-agent-revoke` | **Compile; never run against the venue.** |
| `.github/workflows/ci.yml` | Parsed as YAML. **Never ran**: nothing was pushed |

The tests use no real key: the `0xab…ab` key of the TxFlow vectors, a `0x01…01` key in the fuzz
code, and a key generated in the test (`tests/exec_client.rs`). The mock server is on
`127.0.0.1`. The tests open no connection to the venue.

Mass status is the default of the `ExecutionClient` trait: it composes the order, fill and
position reports of this client.

The execution-client test ran 30 times in a row after a race in the test harness was fixed
(the mock showed an order before its request had arrived): 30 passes. After step E the two
mock suites ran 12 times in a row: 12 passes. One new test
(`leverage_and_margin_mode_change_at_run_time`) waits 300 ms for an answer that the client
reads in the background.

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

Second run, after step E and the tooling step: 12 Nov 2026, 17:29:19 to 17:30:39 UTC (13 Nov,
00:29 local time), host `rpc.txflow.com`, no credentials.

- 242 instruments loaded.
- `AERO-USDC-PERP.TXFLOW`, `BTC-USDC-PERP.TXFLOW` and `ETH-USDC-PERP.TXFLOW`, 25 subscriptions.
- **Book depth** (step E, first time live): 427 depth snapshots (AERO 106, BTC 185, ETH 136),
  ten levels per side.
- **Instrument status** (step E, first time live): one `TRADING` status per instrument at the
  subscription. The window had one poll and no change, so no second status.
- 187 quote lines, 71 trade lines (AERO 57 and 3), the managed books printed.
- **1 bar** (BTC, 1 minute), emitted at 17:30:02, 2 s after the close: the first live bar.
- **Mark price, index price and funding rate arrived for ETH only** (26, 26 and 1 updates),
  though all three instruments were subscribed. ETH was subscribed last. The first run had
  8 updates for two instruments at the same rate, so it most likely had one instrument too.
  Cause and fix: see "Mark, index and funding for several markets" below.
- 14112 log lines, 0 ERROR, 0 WARN. The process was stopped by an alarm (exit 142, expected).

### Mark, index and funding for several markets (fixed 12 Nov 2026)

Cause: the venue keeps one `activeAssetCtx` subscription per connection. A probe of the public
socket (12 Nov 2026, coins 1, 4 and 64 subscribed 1.1 s apart) gave 1 frame for coin 1, none
for coin 4 and 4 frames for coin 64: each new subscription replaces the one before.

Fix: the three kinds now share one `allAssetCtxs` subscription (`CTX_CHANNEL` in
`src/data/mod.rs`). Its frame has every perp market (242 markets, about 65 KB, one frame each
3 s in the probe); markets without a subscriber are not parsed. Fixture: line 9 of
`tests/fixtures/ws_frames.jsonl`, cut to 2 markets (coin 1 from the recorded `activeAssetCtx`
frame, coin 64 from the live `allAssetCtxs` frame).

Third run, after the fix: 12 Nov 2026, 17:42:19 UTC, 78 s, same three instruments, no
credentials. Mark price 25, index price 25 and funding rate 1 for **each** of AERO, BTC and
ETH. Book depth 65 / 143 / 82, quotes 26 / 30 / 36, trades 4 / 35 / 5, one status each, 1 BTC
bar. 9706 log lines, 0 ERROR, 0 WARN.

shortcut: mark and index updates come each 3 s (the cadence of `allAssetCtxs`), not each
block; upgrade to one connection per market on `activeAssetCtx` if that is too slow.

## Done, by plan step

| Step | Content | Verified how |
|---|---|---|
| A | `docs/PLAN.md` | — |
| B | `common`, `http`, `websocket`, `book`, `config`, `data`, `factories`, `data_tester` | Unit tests, `tests/data_client.rs`, fixtures recorded from mainnet, smoke run |
| C | `signing` (agent flow, `approveAgent`, nonce manager) | Unit tests with TxFlow vectors (see below) |
| D | `execution` (submit, modify, cancel, cancel-all, batch, trigger orders, reports, mass status, account state), `exec_tester` | Unit tests, `tests/exec_client.rs` against a mock; **not against the venue** |
| E | Book depth, instrument status, live position reports, TP/SL order lists, run-time leverage and margin mode | Unit tests and the two mock-server suites; **nothing against the venue** (see Step E) |
| Python | `src/python`, `tests/python.rs` | Clippy and 1 test, with the patch |
| Fuzz | `fuzz/`, `src/fuzz.rs` | Type check; unit tests with changed fixtures; **no target ran** |
| Tooling | Benches, `txflow-trades-probe`, `txflow-flatten`, `txflow-agent-revoke`, CI workflow, `data_tester` with book depth and instrument status | Unit tests of the binaries, one bench run, one `data_tester` run; **no binary ran against the venue** (see Tooling) |

### Step E: gaps against `nautilus-lighter` 0.67.0

"Mock" is a test in `tests/` against the stand-in on `127.0.0.1`. No item was checked against
the venue: step E sent no request, public or signed.

| # | Feature | Result | Where | Verified how |
|---|---|---|---|---|
| 1 | Instrument status | Done. `/info perpMeta` is read at connect and then every `instrument_status_poll_secs` (new config field, default 60). `haltTrading` gives `HALT`, `delisted` gives `NOT_AVAILABLE_FOR_TRADING`, else `TRADING`. A subscriber gets the status known now, then each change | `src/data/mod.rs`, `src/http/client.rs`, `src/http/parse.rs`, `src/config/data.rs`, `src/python/config.rs` | Unit (`market_status_follows_the_flags`); mock (`book_depth_is_cut_from_the_stream_and_status_follows_the_market_list`: `TRADING`, then `HALT` after the stand-in sets the flag). The flags are in the `perpMeta` fixture recorded on 10 Nov; a halt was never seen live |
| 2 | Book depth, 10 levels | Done. `BookSync` keeps the levels of the applied `l2BookDepth` frames; the best ten per side go out after each frame, with the order count `n` of each level | `src/book.rs`, `src/data/mod.rs`, `src/http/models.rs` | Unit (`depth_is_cut_from_the_kept_levels`); mock (same test as 1, with the recorded book frames) |
| 3 | Fills with the venue trade id | **Not done.** The current path stays. Reason below | — | — |
| 4 | Live position reports | Done. A `/ws clearinghouseState` frame gives one position report for each market whose size changed since the frame before; a market that left the list is reported flat | `src/execution/mod.rs` | Mock (`clearinghouse_frames_become_position_reports_once_per_change`), frame hand-made |
| 5 | Order list with TP/SL | Done, **unverified**. A market or limit parent followed by one or two trigger children that close it goes out as one `order` action with grouping `normalTpsl`. Other shapes are sent as before | `src/execution/order.rs`, `src/execution/mod.rs`, `src/signing/action.rs`, `src/http/client.rs`, `src/http/models.rs` | Unit (`tpsl_group_is_a_parent_with_its_closing_triggers`, `tpsl_pair_becomes_one_order_per_leg`, two cases of `json_and_msgpack_agree`); mock (`order_list_with_tp_and_sl_is_one_tpsl_action`, `order_list_of_another_shape_is_sent_without_a_link`), answers hand-made |
| 6 | Run-time leverage and margin mode | Done. `TxflowExecutionClient::update_leverage` and `update_margin_mode` send `updateLeverage` and `updateMarginMode` in the background and log the answer. After a confirmed margin mode switch the orders of that market carry the new `m` | `src/execution/mod.rs` | Mock (`leverage_and_margin_mode_change_at_run_time`): both signed envelopes, the `m` of the next order, a refusal, the two input errors |
| 7a | Trigger on the last price | **Not done: not on the venue.** Reason below | — | The existing refusal test stays |
| 7b | GTD with `expiresAt` | **Not done: not on the venue for perps.** Reason below | — | The existing refusal test stays |

Item 3, why the fills still come from `orderUpdates`:

- The `userFills` subscription is on `/api-ws`, and its page says that it sends the fill history
  once, as a snapshot, and nothing after ("Nothing is pushed after that", `isSnapshot` "Always
  `true`: this is the only push"). It is a history download, not a stream.
- The flow page "Place and track a perp order" gives `orderUpdates` and `clearinghouseState`
  as the live sources, and `/info userFills` for history, "indexed, so it can lag the stream
  briefly".
- `/ws trades` with a `user` filter is live and has `tid`, but no order id, so a trade cannot
  be given to one of two orders of the same market.
- What is left is a hybrid: take each `orderUpdates` row with a new filled size as a signal,
  then read `/info userFills` (or subscribe to `/api-ws userFills` again) for the real rows.
  It was not built: every fill event would wait for an index that lags by an unknown time,
  the `Filled` and `Canceled` events would have to wait with it, and a row that never shows
  needs a fallback id, which brings back the double-fill risk. This is a decision for the
  account owner after the testnet gate (step 7 there measures the lag).
- `/info userFills` does have `tid`. The fill reports still use `<oid>-<cumulative filled>`,
  so that the live events and the reports agree.

Item 7a: the `trigger` body of an order has three fields (`isMarket`, `triggerPx`, `tpsl`).
`triggerType` exists in answers only (`frontendOpenOrders`, `triggerOrders`,
`historicalTpslOrders`), and the one value shown is `"mark"`. The trading guide says that
triggers are "evaluated against the mark price". There is nothing to send.

Item 7b: the docs do not contradict themselves on this. The `order` page lists `Gtd` and
`expiresAt` because the action is shared with outcome markets. Three other pages say that
perpetuals reject it: the table "What each market supports" (GTD: perps "—"), the matching
engine page ("perps reject `GTD`") and the instrument model page ("perps reject GTD").

### Tooling (13 Nov)

The patterns are those of `nautilus-risex`: bench files `data`, `exec`, `micros`, `signing`;
binaries in `bin/` with no feature gate; the same CI job. One difference: the binaries here
have unit tests, so their `[[bin]]` entries have no `test = false`.

| Item | Result | Where | Verified how |
|---|---|---|---|
| Benches | Done | `benches/`, `Cargo.toml` (`criterion` 0.8.2, dev-dependency) | `cargo bench`, one run (see Benches) |
| `txflow-trades-probe` | Done. Public trades of one market (the latest 300 at most), no key | `bin/trades_probe.rs` | Unit tests of the arguments. **Not run** (see Verification) |
| `txflow-flatten` | Done. Prints the plan (one cancel per open order, trigger order and TP/SL leg; one reduce-only IOC order per position) and sends nothing before Enter. `testnet` as first argument. Reads `TXFLOW_ACCOUNT_ADDRESS` and `TXFLOW_AGENT_PRIVATE_KEY` (or `TXFLOW_TESTNET_*`) at run time; no `.env` file | `bin/flatten.rs`, `src/http/models.rs` (`Position.leverage`) | 12 unit tests on the fixtures: the cancels and their family, the unknown market, the positions, the margin mode, the close order of a long and of a short position. **Compiles; never run against the venue** |
| `txflow-agent-revoke` | Done: the docs describe the revocation (below). Prints the account, the label and the agent, sends nothing before Enter. Reads `TXFLOW_ACCOUNT_PRIVATE_KEY` (or `TXFLOW_TESTNET_ACCOUNT_PRIVATE_KEY`) at run time and never prints it | `bin/agent_revoke.rs` | 7 unit tests: the arguments, the request body against the documented one, the null name, the label check. **Compiles; never run against the venue** |
| CI | Done. Job `check`: `fmt --check`, clippy `-D warnings` and tests, without and with `--features examples`, toolchain 1.99.1. Job `fuzz`: nightly and `cargo-fuzz` installed on the runner, each target of `fuzz/fuzz_targets` for 30 s (`-max_total_time=30`) | `.github/workflows/ci.yml` | YAML parsed. **Never ran** |
| `data_tester` | `AERO-USDC-PERP.TXFLOW` added; book depth and instrument status subscribed | `examples/data_tester.rs` | One run of 80 s (see Smoke run) |

Agent revocation, from the docs (`llms-full.txt` of 11 Nov, page `/api/exchange/approveAgent`):

- Lines 14386 to 14402, "Revoking an agent": send `approveAgent` with `agentAddress` set to
  the zero address and the label in `agentName`. The label must exist, or the venue answers
  `Agent with this label does not exist`. Lines 14329 and 4059 say the same.
- Line 14381: approving an existing label with a new address replaces the old agent. This is
  the key rotation; the binary does not do it (it needs the new agent address and is an
  approval, not a revocation).
- Lines 14419 to 14425: the action is signed by the account key, with the `ApproveAgent`
  struct that has a vector.

What the two signing binaries do that no test against the venue covers:

- `txflow-flatten` takes each order once, from the most specific of the three lists
  (`tpslOrders`, then `triggerOrders`, then `frontendOpenOrders`), and puts cancels of all
  three families in one `cancel` action (the execution client does the same).
- The close order carries `m`, the margin mode of the position, read from `leverage.type` of
  `clearinghouseState` (the `order` page, line 14767: the margin mode is part of the position
  key). The docs list no values of `leverage.type`.
- A market that is not in the instrument list (a delisted one) stops the plan before
  anything is sent.
- If `tpslOrders` cannot be read, the flatten goes on without the TP/SL orders and says so.
  If `approvedAgents` cannot be read, the revocation goes on without the label check.
- One IOC order per position: a thin book leaves a rest. Run again until "Nothing to do".
- Hedge mode (two positions of one market) is not handled apart: each row gets its own
  reduce-only order.

### Signing: what has a vector

| Part | Proof |
|---|---|
| Domain separators, testnet and mainnet | TxFlow vector |
| `order` action, plain limit, no optional field: MessagePack bytes, connection id, signing hash, `r`/`s`/`v` | TxFlow vector |
| `approveAgent`: struct hash, signing hash | TxFlow vector |
| Vault flag in the connection id | Hyperliquid reference value only |
| `cancel`, `cancelByCloid`, `modify2`, `updateLeverage`, `updateMarginMode` | **None.** Field order from the request examples of the docs |
| Order with `c`, `m`, `expiresAt`, trigger body, cancel `grouping` | **None.** Same |
| `order` with grouping `normalTpsl`, cancel with `grouping: "tpsl"` (step E) | **None.** The docs list the two values and show no request with them |
| `approveAgent` with the zero address (`txflow-agent-revoke`) | **None** for this address. The docs show the action body and no signature. Struct and code are those of the `approveAgent` vector |

A wrong field order gives a valid signature of another signer, so the venue answers "agent is
not authorized". That is the first thing to watch on testnet.

### Fixtures

| Fixture | Origin |
|---|---|
| `perpMeta`, `l2Book`, `trades`, `candleSnapshot`, `marketTicker`, `activeAssetCtx`, `ws_frames.jsonl` (market frames only) | Recorded from mainnet, 10 Nov 2026 UTC (cut to size) |
| `clearinghouseState`, `frontendOpenOrders`, `historicalOrders`, `orderDetails`, `triggerOrders`, `userFills`, `error`; the `orderUpdates` frames and every `/exchange` answer built in `tests/exec_client.rs` | **Hand-made from the docs.** No user endpoint was read live |
| Step E: `tpslOrders` (an empty list), the TP/SL pair rows in `tests/exec_client.rs` and `src/http/models.rs`, the `/ws clearinghouseState` frames (built from the `clearinghouseState` fixture) | **Hand-made from the docs.** |
| Step E: the halted `perpMeta` market in `tests/data_client.rs` | The recorded fixture with `haltTrading` set to `true` by the test |

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
11. Run-time settings: `update_margin_mode` to isolated, then `update_leverage`, then one
    order: check `marginMode` and leverage of the order in `frontendOpenOrders`.
12. TP/SL order list (limit parent far from the market, stop-market and market-if-touched
    children): whether the action is accepted in the order sent, whether `tpslOrders` lists
    the pair before the parent fills, what `side` and `cloid` of the pair row are, whether the
    children show in `orderUpdates` and `frontendOpenOrders`, cancel of one child with
    `grouping: "tpsl"`, what happens to the children when the parent is cancelled.
13. Position reports: after the fill of step 6, whether the `clearinghouseState` frame comes
    before or after the `orderUpdates` row.
14. Only then set `DRY_RUN = false` in `exec_tester`, and only on testnet first.
15. `txflow-flatten testnet` with one resting order, one trigger order, one TP/SL pair and
    one small position: the three cancel families in one action, the close order with `m`,
    the values of `leverage.type`. Run it a second time: "Nothing to do".
16. Last: `txflow-agent-revoke testnet <label>`: the `approvedAgents` answer, the revocation,
    then one order with the revoked agent (it must be refused), and whether the orders the
    agent left on the book are still there.

## Not verified against the venue

### Markers in the code

| Place | Kind | Open point |
|---|---|---|
| `src/execution/mod.rs:104` | unverified | Values of `triggerState`. A listed trigger order with no readable state is taken as open |
| `src/execution/mod.rs:185` | unverified | Wording of the post-only reject. The adapter looks for "post" in the reason |
| `src/execution/mod.rs:209` | unverified | Step E. Whether `tpslOrders` lists a TP/SL pair before its parent fills. A child that is not found stays submitted |
| `src/execution/mod.rs:238` | shortcut | Step E. Which orders are TP/SL children is known for the session only; after a restart their cancel goes out with `grouping: "trigger"` |
| `src/execution/mod.rs:248` | unverified | Whether the order stream shows trigger orders. Their ids are read once from `triggerOrders` after the action |
| `src/execution/mod.rs:333` | unverified | A cancelled trigger order or TP/SL child may get no row, so the HTTP answer closes it |
| `src/execution/mod.rs:495` | unverified | Step E. `tpslOrders` was never read live. A refusal or an answer of another shape leaves the pairs out of the order reports, with a warning |
| `src/execution/mod.rs:784` | unverified | Step E. Shape of a `normalTpsl` action: parent first, then the children with `tpsl` set (Hyperliquid convention) |
| `src/execution/order.rs:109` | unverified | Meaning of `p` on a market trigger order. The adapter sends the trigger price ± slippage (Hyperliquid convention) |
| `src/http/models.rs:258` | unverified | Step E. `side` of a `tpslOrders` row is read as the side of both orders |
| `src/http/parse.rs:72` | assumption | Maintenance margin = half of the initial margin at maximum leverage |
| `src/http/parse.rs:210` | shortcut | Fill trade id is `<oid>-<cumulative filled>`. A `userFills` window that misses the first fills of an order gives other ids than the live stream |
| `src/http/parse.rs:349` | shortcut | Fee booked in the quote currency whatever `feeToken` says |
| `src/http/client.rs:52` | shortcut | One history request reads at most 50 pages (100,000 rows) or 20 candle windows |
| `src/execution/dispatch.rs:82` | shortcut | Closed orders stay tracked until the map is full, then all of them are dropped |
| `src/execution/dispatch.rs:289` | shortcut | Without an average fill price the limit price stands in |
| `src/execution/mod.rs:646` | shortcut | The catch-up window starts at connect, so each reconnect reads all orders since connect again |
| `bin/flatten.rs:75` | unverified | Values of `leverage.type` of a position. "Cross" and "Isolated" are read without case; another value sends the close order without `m` |
| `bin/flatten.rs:108` | unverified | Whether one order shows in two of `tpslOrders`, `triggerOrders`, `frontendOpenOrders`. It is cancelled once, with the family of the most specific list |
| `bin/flatten.rs:252` | unverified | `tpslOrders` was never read live. If it fails, the flatten leaves the TP/SL orders out and says so |
| `bin/flatten.rs:332` | shortcut | One IOC order per position; run again until "Nothing to do" |
| `bin/agent_revoke.rs:47` | unverified | Shape of the `approvedAgents` answer, from the example of the docs |
| `bin/agent_revoke.rs:97` | unverified | The revocation body: no signed example in the docs |

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
- Live bars against the venue: one bar seen (second smoke run), emitted 2 s after the close
  by the timer. A bar emitted by the next candle was not seen.
- `/api-ws` reconnect and resubscription against the venue (covered by a local test only).
- Testnet: no request of any kind went to `rpc.txflow-testnet.xyz`.

Added by step E:

- TP/SL order lists, all of it: the signed `normalTpsl` action; whether one child alone is
  accepted (the `modify2` page asks for exactly two); whether the children must be
  reduce-only; the rows of `tpslOrders`; the match of a child on market and trigger price
  (an older pair with the same trigger price can be taken if the new pair is not listed
  yet); whether a fired child keeps its order id in `orderUpdates`; cancel of one leg with
  `grouping: "tpsl"`. When the parent is cancelled or one child fires, the venue removes
  children; the adapter learns it only from a row of the order stream or from the next
  open-order check of the engine.
- `updateMarginMode` and `updateLeverage` with `marginMode`: signed bodies without a vector.
  That an order with `m` set follows a margin mode switch is from the `order` page only.
- `/ws clearinghouseState`: the frame was never read live. Its order relative to
  `orderUpdates` is unknown: a position report that arrives before its fill makes the engine
  log a position difference for a moment (the engine does not act on it).
- `haltTrading` and `delisted` of `perpMeta` during a session: the fields are in the recorded
  answer, a change was never seen. The poll interval of 60 s is a choice, not a venue value.
- Book depth uses the order count `n` of the levels; `n` is in every recorded frame.

Added by the tooling step:

- `txflow-flatten` and `txflow-agent-revoke`, all of it: no request of either went to the
  venue (see Tooling for the list).
- Whether the venue cancels the open orders of an agent when the agent is revoked. The docs do
  not say; the binary tells the user to flatten first.
- Mark price, index price and funding rate with more than one instrument: the second smoke
  run got them for the last subscribed instrument only. Not investigated.
- The CI workflow never ran. The `fuzz` job has no cache, so each run builds `cargo-fuzz`.
- The bench frames are the fixture frames, cut to size: a snapshot of 5 levels per side. A
  live snapshot has 200 per side.

## Documentation: differences and ambiguities

Sources: the docs downloaded on 11 Nov 2026, live public samples, the research of 5 Nov 2026
(`stoikovgrid/docs/txflow-api.md`). The docs win over the research; a live sample wins over
both for its own field.

Differences found:

| Point | Research, 5 Nov | Docs, 11 Nov | Live | Adapter |
|---|---|---|---|---|
| `Gtd` on perps | "perps reject `Gtd`" | The `order` page lists `Gtd` with no limit (the action is shared with outcome markets); three guide pages say that perps reject it | Not tested | Refuses GTD |
| Candle values on `/api-ws` | — | Numbers | Strings | Reads both |
| Live fills | `/ws trades` with a `user` filter, plus `orderUpdates` | No live fills channel; `orderUpdates` has size, average price, fee. `/api-ws userFills` is one snapshot. `/ws trades` takes a `user` filter but its rows have no order id | Not tested | `orderUpdates` only |
| Trade id of a fill | — | `/info userFills` and `/api-ws userFills` have `tid` (the first version of this file said that they have none) | Not tested | Not used, see Step E item 3 |
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
- `normalTpsl` and `positionTpsl` are values of `grouping` with no description and no request
  example. The `children` of an order row are shown as `[""]`.
- `tpslOrders` has one `cloid` and one `side` for the pair; neither is described.
- No public funding history endpoint is documented (`userFundings` is per account).

## Not done

Out of scope by the brief:

- Outcome (prediction) markets, spot.
- Builder codes, `approveBuilderFee`.
- Deposits, withdrawals, transfers.

Left out by this crate:

- Vault trading, TWAP orders, position TP/SL (`positionTpsl`, `modifyTpsl`), `modify2.tpsl`,
  `reversePosition`, `closeAll`, hedge position mode.
- Order lists of another shape than parent + TP/SL children: no link (OCO without a parent,
  a bracket whose take-profit is a plain limit order).
- `"bbo"` as a market price; a market order needs a cached quote.
- Polling of trigger orders and TP/SL children. They are read after the submit and at
  reconciliation only.
- `userFills.tid` as the trade id of live fills (Step E item 3).
- Leverage and margin mode as commands of the execution trait or of the Python binding: the
  trait has no such command, so they are methods of the Rust client.
- A separate WebSocket URL in the config: `base_url_http` gives the host of all four paths.
- Agent approval and key rotation (`approveAgent` with an agent address): `txflow-agent-revoke`
  revokes only. No Python binding for the revocation.
- A fill check in `txflow-flatten`: it sends one IOC order per position and is run again.

Not on the venue, so refused or answered with less (unchanged by step E): FOK, GTD on perps,
triggers on the last price, funding-rate history, trade history by time range, a dead man's
switch, batch modify, a native cancel-all.

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

The revocation is a binary only (`txflow-agent-revoke`); there is no Python function for it.
`TxflowMarginMode` goes to the
config as the enum class. `TxflowDataClientConfig` takes `instrument_status_poll_secs` (step E).
`update_leverage` and `update_margin_mode` have no Python binding.

The module function `txflow` registers its extractors in a process-wide registry. A second call
in one process fails.

## Benches

One run each on an Apple-silicon Mac, 13 Nov 2026 (`cargo bench`, criterion 0.8.2, default
settings). The numbers are indicative only.

| Bench | Time |
|---|---|
| `signing/sign_action` (full EIP-712 agent signature of an order action) | 48.5 µs |
| `signing/msgpack` (MessagePack of the order action) | 244 ns |
| `signing/connection_id` | 154 ns |
| `decode/book_snapshot_10` (511-byte frame, 5 levels per side) | 2.26 µs |
| `decode/book_delta_4` (288-byte frame) | 1.01 µs |
| `book/apply_snapshot_10` | 611 ns |
| `book/apply_delta_4` | 192 ns |
| `book/depth10` (10-level depth cut) | 749 ns |
| `exec/order_body` (build the request of a limit order, no signature) | 157 ns |
| `exec/market_price` | 47.5 ns |
| `exec/wire_decimal` | 51.0 ns |

The order action is the one of the docs' signing vector. The book frames are lines 1 and 3 of
`tests/fixtures/ws_frames.jsonl`, recorded from mainnet and cut to size: the snapshot has 5
levels per side, where a live one has 200, so `decode/book_snapshot_10` and
`book/apply_snapshot_10` say nothing about a full snapshot. After the delta the book has 6
bids and 7 asks, so `book/depth10` cuts fewer than ten levels. The three `book` numbers are
from a second run of that bench, made after `apply_delta_4` was changed to keep the drop of
the book out of the measurement (first run: 633 ns, 249 ns, 803 ns).

## Fuzz

Five targets type-check on 1.99.1: `http_parse`, `ws_parse`, `book_apply`, `action_sign`,
`wire_format`. **No target ran**: `cargo-fuzz` is not installed here and no nightly toolchain
was used. The `fuzz` job of `.github/workflows/ci.yml` installs both on the runner and runs
each target for 30 seconds; that job never ran. There is no seed corpus.

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
