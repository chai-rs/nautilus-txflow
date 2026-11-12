# nautilus-txflow: plan

NautilusTrader 0.67.0 Rust adapter for the TxFlow perpetuals exchange. Perps only.
Structure and conventions follow `nautilus-risex`. Signing, nonce and reconciliation patterns
follow the upstream `nautilus-hyperliquid` 0.67.0 adapter where the venue behaves the same.

## Scope

In:

- `common` (consts, enums, URLs, parse), `http` (`/info` + `/exchange` clients, models, rate-limit
  handling), `websocket` (`/ws` market + user streams, `/api-ws` candles, reconnect, resubscribe),
  `book`, `signing` (agent flow, account flow for `approveAgent`, nonce manager), `config`,
  `data` client, `execution` client, `factories`.
- `examples/data_tester.rs`, `examples/exec_tester.rs` (feature `examples`).
- Unit and integration tests with fixtures. No test uses the network.
- Python binding (`src/python`, features `python` and `extension-module`) and `tests/python.rs`.
- Fuzz package `fuzz/` with five targets. Each target body lives in `src/fuzz.rs`, and a unit
  test feeds it pseudo-random and mutated-fixture input.

Out (follow-ups, listed in `STATUS.md`): benches, CI workflow, flatten/revoke bins, outcome
(prediction) markets, builder codes, deposits/withdrawals/transfers, vault trading, TWAP orders,
position TP/SL groups (`normalTpsl`, `positionTpsl`, `modifyTpsl`), hedge position mode.

Step E (12 Nov 2026) closed the gaps against `nautilus-lighter` 0.67.0 that the venue can
serve: book depth, instrument status, live position reports, order lists with TP/SL children
(`normalTpsl`), run-time leverage and margin mode. `STATUS.md` has the result of each item;
the tables below describe steps A to D.

## Parity with the Hyperliquid adapter

| Feature | Hyperliquid adapter | This adapter |
|---|---|---|
| Instruments | `meta` | `/info perpMeta`; delisted markets skipped |
| Book deltas | `l2Book` snapshots | `/ws l2BookDepth` snapshot + delta with `seq`/`prevSeq` |
| Quotes | `bbo` | `/ws bbo` |
| Trades | `trades` | `/ws trades`; history: `/info trades` (latest 300 only) |
| Bars | `candle` | `/api-ws candle`; history: `/info candleSnapshot` |
| Mark, index, funding | `activeAssetCtx` | `/ws activeAssetCtx` (index = `oraclePx`) |
| Funding history | `fundingHistory` | none at the venue; the request returns the current rate |
| Submit limit/market | yes | yes (`FrontendMarket` for market) |
| Trigger orders | yes | yes, grouping `normalTrigger`, one order per action |
| Batch submit | yes, per-order status | yes, max 50 per action; the documented answer has one status; per-order statuses are read if present |
| Modify | `modify`/`batchModify` | `modify2` (one order, in place) |
| Cancel | `cancel`, `cancelByCloid` | same, max 200 per action |
| Cancel all | client-side | client-side: batched cancels over cached open orders |
| Dead-man switch | `scheduleCancel` | none at the venue |
| Fills stream | `userFills`/`userEvents` | none at the venue; fills derived from `orderUpdates` |
| Order reports | `frontendOpenOrders`, `historicalOrders`, `orderStatus` | `frontendOpenOrders`, `historicalOrders`, `orderDetails` |
| Fill reports | `userFills` | `/info userFills` |
| Position reports, account state | `clearinghouseState` | `clearinghouseState` (`/info` and `/ws`) |
| Leverage | `updateLeverage` | `updateLeverage` at connect (from config) |
| Vaults, builder fee, spot, TWAP | partly | no |

## TxFlow versus Hyperliquid

Signing:

1. EIP-712 domain is per network: `TxFlow-Mainnet` chain 869, `TxFlow-Testnet` chain 2511,
   version `1`, zero verifying contract. Hyperliquid: `Exchange`, chain 1337.
2. Agent struct is `Agent(string txflowNetwork,uint32 chainId,uint32 apiVersion,bytes32 connectionId)`.
   Hyperliquid: `Agent(string source,bytes32 connectionId)`.
3. The MessagePack preimage is the action **without** `type`. Hyperliquid keeps `type`.
4. No `expiresAfter` suffix in the hash. The suffix is `nonce` (8 bytes, big endian) plus the
   vault flag, as on Hyperliquid.
5. Key order follows the request examples: order action `grouping, orders, builder`; order body
   `a, b, p, s, r, t, c, m, expiresAt`. Null optionals are omitted.
6. Account-signed actions (`approveAgent`) use the same TxFlow domain and plain type names.
7. `v` is 27/28, signatures must be low-S, the envelope has `vaultAddress` and `signatureChainId`.
8. Nonce: millisecond time, window [block time − 2 days, block time + 1 day], the venue keeps the
   100 highest per signer. Same model as Hyperliquid.

Orders:

9. `a` is `perpMeta.index` (starts at 1), not the position in the universe.
10. `c` (cloid) is a free string, max 64 bytes, must not start with `0x`. Hyperliquid: 16-byte hex.
11. `m` (margin mode) and `expiresAt` exist in the order body. `Gtd` and `FrontendMarket` are
    time-in-force values.
12. Price must be a multiple of `priceTick`. There is no 5-significant-figure rule and no
    documented minimum notional. `szDecimals` can be negative (size step 10).
13. Trigger orders are rejected under grouping `na`; they need `normalTrigger` or a TP/SL group.
14. The `/exchange` response has **one** status for the whole action and no order id. `success`
    means accepted by consensus, not executed.
15. `modify2` amends one order in place (keeps the order id). `s` is the new total size.
16. No cancel-all action, no `scheduleCancel`, no `batchModify`.

Data and transport:

17. `/info` and `/exchange` need the header `x-api-version: 2026-08-16`.
18. `/info` answers `{success, code, data}` or `{success:false, code, message}`.
19. Request `coin` is the instrument id as a string (`"1"`). Response `coin` varies (`BTC-USDC`,
    `BTC`, `"1"`), so responses are matched on `id` / `instrumentId` / the subscription.
20. `/ws` subscribe frames carry the channel type at the top level too. No acknowledgement.
21. Client ping `{"method":"ping"}` every 10 s; the gateway closes after about 16 s of silence.
22. Candles and ticker are on a second endpoint, `/api-ws`.
23. `l2BookDepth` sends snapshot and delta frames with `seq`/`prevSeq`.
24. No live fills stream. `orderUpdates` is the first place where order id and cloid meet.
25. Funding is hourly and published in percent (`0.01` = 0.01 %).
26. Rate limits are flat per IP: `/info` 50/s, `/exchange` 10/s, 10 connections per WS endpoint.

## Decisions

- Instrument id `BTC-USDC-PERP.TXFLOW`; raw symbol `BTC-USDC`; the venue id is `index`.
- `ClientOrderId` is sent as `c` unchanged. Ids over 64 bytes or starting with `0x` are denied.
- `OrderAccepted` comes from `orderUpdates`, not from the HTTP `success`. An HTTP `err` is a
  rejection. A transport failure is left to the stream and to reconciliation.
- Fills come from `orderUpdates`: filled = `origSz − sz`, price from the change of
  `avgFillPrice`, fee from the change of `fee`. Trade id is `<oid>-<cumulative filled>`; fill
  reports from `/info userFills` use the same scheme so both sources agree.
- Market orders: `FrontendMarket` with `p` = last quote moved by `market_order_slippage_bps`.
  A market order with no cached quote is denied. (First plan: send `"bbo"`. Changed: the docs
  do not define `"bbo"` for this case, and a price cap must not be a guess.)
- Stop and if-touched orders: `t.trigger` with `tpsl` `sl` / `tp`, grouping `normalTrigger`,
  one order per action (a batch may be read as one group). Trigger orders are stored outside
  the book: the adapter reads them from `/info triggerOrders` and cancels them with
  `grouping: "trigger"`.
- GTD is denied. (First plan: `Gtd` + `expiresAt`. Changed: the trading guide limits `Gtd` to
  Outcome markets.) Post-only maps to `Alo`.
- Modify (`modify2`) is for limit orders only.
- The envelope carries `confirm: "executed"`: the HTTP answer comes after execution.
- Every order carries `m` from config (default `cross`).
- Book: a snapshot replaces the book; a delta whose `prevSeq` is not the last `seq` is dropped
  and the channel is resubscribed. Size zero removes a level.
- Bars: a candle is emitted when a candle with a later open time arrives, or 2 s after its
  close time when the venue sends nothing more (a 1 s timer checks).
- After a reconnect every subscription is sent again.
- The agent key and account address come from config or environment variables at run time. The
  config `Debug` output hides the key.
- `approveAgent` has a signing function and a test vector. The adapter never sends it.
- Read requests are retried on network errors, 429 and 5xx. Signed requests are never retried.

## Open questions

- Is `approveBuilderFee` required before trading on mainnet?
- How does a batch fail: all or nothing?
- Reject reasons are not in `orderUpdates`. Where can a client read them?
- `modify2.tpsl`: the field table says omit clears TP/SL children, the TP section says omit
  keeps them.
- Is `fee` in `orderUpdates` cumulative? This adapter assumes it is.
- Does `FrontendMarket` use `p` at all?
- Order of `orderUpdates` relative to `clearinghouseState`.

## Sources

- TxFlow docs, `https://docs.txflow.com/` (`llms-full.txt`), downloaded 11 Nov 2026.
- Live public samples from `https://rpc.txflow.com` (`/info`, `/ws`, `/api-ws`), 11 Nov 2026.
- Earlier research, 5 Nov 2026: `stoikovgrid/docs/txflow-api.md`. The docs win where they differ;
  each difference is listed in `STATUS.md`.
- `nautilus-hyperliquid` 0.67.0 (crates.io) and `nautilus-risex` (local).
