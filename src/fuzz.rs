//! Bodies of the fuzz targets in `fuzz/`.
//!
//! Each function takes arbitrary bytes and must not panic. The unit tests below call the same
//! functions with pseudo-random and changed fixture inputs, so the targets are checked even
//! where `cargo fuzz` is not installed.

use std::sync::LazyLock;

use alloy_primitives::Address;
use nautilus_core::UnixNanos;
use nautilus_model::{
    data::BarType,
    identifiers::AccountId,
    instruments::{Instrument, InstrumentAny},
};
use rust_decimal::Decimal;
use serde::de::DeserializeOwned;

use crate::{
    book::{BookSync, snapshot_deltas},
    common::{
        enums::{TxflowEnvironment, TxflowMarginMode},
        parse::{
            millis_to_nanos, parse_decimal, price_from_decimal, quantity_from_decimal, wire_decimal,
        },
    },
    data::parse_quote,
    http::{
        models::{
            Candle, ClearinghouseState, ExchangeResponse, InfoEnvelope, L2Book, MarketTicker,
            OrderDetails, OrderRow, Paged, PerpMeta, Trade, TxflowOrder, Universe, UserFill,
        },
        parse::{
            parse_account_balance, parse_bar, parse_fill_reports, parse_instrument,
            parse_order_status_report, parse_position_status_report, parse_ticker_funding,
            parse_trade_tick,
        },
    },
    signing::{
        TxflowSigner,
        action::{
            Action, CancelAction, CancelBody, CancelByCloidAction, CancelByCloidBody,
            CancelGrouping, Grouping, ModifyAction, ModifyBody, OrderAction, OrderBody, OrderKind,
            Tif, Tpsl, UpdateLeverage, UpdateMarginMode,
        },
        connection_id,
    },
    websocket::messages::{WsMessage, parse_frame},
};

const TS: UnixNanos = UnixNanos::new(1);

static INSTRUMENT: LazyLock<InstrumentAny> = LazyLock::new(|| {
    let meta: PerpMeta = serde_json::from_str(
        r#"{"name":"BTC-USDC","baseCurrency":"BTC","quoteCurrency":"USDC",
            "basePrecision":"0.0001","priceTick":"0.1","maxLeverage":"50","index":1}"#,
    )
    .expect("the market literal is valid");
    parse_instrument(&meta, TS).expect("the market literal is valid")
});

// Not a real key: the byte 0x01 repeated, used only to reach the signing code.
static SIGNER: LazyLock<TxflowSigner> = LazyLock::new(|| {
    TxflowSigner::new(
        &format!("0x{}", "01".repeat(32)),
        TxflowEnvironment::Testnet,
    )
    .expect("the fixed key is a valid scalar")
});

fn account() -> AccountId {
    AccountId::from("TXFLOW-001")
}

fn bar_type() -> BarType {
    BarType::from("BTC-USDC-PERP.TXFLOW-1-MINUTE-LAST-EXTERNAL")
}

fn payload<T: DeserializeOwned>(data: &[u8]) -> Option<T> {
    serde_json::from_slice::<InfoEnvelope<T>>(data).ok()?.data
}

fn order_row(row: &OrderRow) {
    let _ = parse_order_status_report(
        &row.order,
        &row.status,
        row.status_timestamp,
        account(),
        &INSTRUMENT,
        TS,
    );
}

fn trades(trades: &[Trade]) {
    for trade in trades {
        let _ = parse_trade_tick(trade, &INSTRUMENT, TS);
    }
}

fn clearinghouse(state: &ClearinghouseState) {
    let _ = parse_account_balance(state);
    for row in &state.asset_positions {
        let _ = parse_position_status_report(&row.position, account(), &INSTRUMENT, TS);
    }
}

/// `/info` and `/exchange` answers: decode as every payload type, then run its parser.
pub fn http_parse(data: &[u8]) {
    if let Some(universe) = payload::<Universe>(data) {
        for meta in &universe.universe {
            let _ = parse_instrument(meta, TS);
        }
    }
    if let Some(book) = payload::<L2Book>(data) {
        let _ = snapshot_deltas(&INSTRUMENT, &book.levels, 0, millis_to_nanos(book.time), TS);
    }
    if let Some(rows) = payload::<Vec<Trade>>(data) {
        trades(&rows);
    }
    if let Some(candles) = payload::<Vec<Candle>>(data) {
        for candle in &candles {
            let _ = parse_bar(candle, bar_type(), &INSTRUMENT, TS);
        }
    }
    if let Some(ticker) = payload::<MarketTicker>(data) {
        let _ = parse_ticker_funding(&ticker, INSTRUMENT.id(), TS);
    }
    if let Some(orders) = payload::<Vec<TxflowOrder>>(data) {
        for order in &orders {
            let status = order.status.as_deref().unwrap_or("Open");
            let _ = parse_order_status_report(order, status, None, account(), &INSTRUMENT, TS);
        }
    }
    if let Some(page) = payload::<Paged<OrderRow>>(data) {
        page.data.iter().for_each(order_row);
    }
    if let Some(details) = payload::<OrderDetails>(data) {
        order_row(&details.order_details);
    }
    if let Some(page) = payload::<Paged<UserFill>>(data) {
        let _ = parse_fill_reports(page.data, account(), |_| Some(INSTRUMENT.clone()), TS);
    }
    if let Some(state) = payload::<ClearinghouseState>(data) {
        clearinghouse(&state);
    }
    if let Ok(response) = serde_json::from_slice::<ExchangeResponse>(data) {
        let _ = (
            response.is_pending(),
            response.action_error(),
            response.status_errors(),
            response.reject_reason(),
        );
    }
}

/// One WebSocket text frame: decode, then run the parser of its channel.
pub fn ws_parse(data: &[u8]) {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    match parse_frame(text) {
        Ok(Some(WsMessage::BookDepth(frame))) => {
            let _ = BookSync::default().apply(&frame, &INSTRUMENT, TS);
        }
        Ok(Some(WsMessage::Bbo(bbo))) => {
            let _ = parse_quote(&bbo, &INSTRUMENT, TS);
        }
        Ok(Some(WsMessage::Trades(rows))) => trades(&rows),
        Ok(Some(WsMessage::AssetCtx(ctx))) => {
            let _ = crate::http::parse::parse_asset_ctx(&ctx.ctx, &INSTRUMENT, TS);
        }
        Ok(Some(WsMessage::Candle(candle))) => {
            let _ = parse_bar(&candle, bar_type(), &INSTRUMENT, TS);
        }
        Ok(Some(WsMessage::OrderUpdates(rows))) => rows.iter().for_each(order_row),
        Ok(Some(WsMessage::ClearinghouseState(state))) => clearinghouse(&state),
        Ok(Some(WsMessage::Reconnected) | None) | Err(_) => {}
    }
}

/// A stream of `l2BookDepth` frames, one per line, through one book.
pub fn book_apply(data: &[u8]) {
    let mut book = BookSync::default();
    for line in String::from_utf8_lossy(data).lines() {
        if let Ok(Some(WsMessage::BookDepth(frame))) = parse_frame(line) {
            let _ = book.apply(&frame, &INSTRUMENT, TS);
        }
    }
}

/// Reads values off the input; gives zeros once it is used up.
struct Input<'a>(&'a [u8]);

impl Input<'_> {
    fn take(&mut self, n: usize) -> &[u8] {
        let (head, tail) = self.0.split_at(n.min(self.0.len()));
        self.0 = tail;
        head
    }

    fn byte(&mut self) -> u8 {
        self.take(1).first().copied().unwrap_or(0)
    }

    fn flag(&mut self) -> bool {
        self.byte() & 1 == 1
    }

    fn u64(&mut self) -> u64 {
        let mut bytes = [0; 8];
        let head = self.take(8);
        bytes[..head.len()].copy_from_slice(head);
        u64::from_le_bytes(bytes)
    }

    fn u32(&mut self) -> u32 {
        self.u64() as u32
    }

    fn text(&mut self) -> String {
        let n = usize::from(self.byte() % 80);
        String::from_utf8_lossy(self.take(n)).into_owned()
    }

    fn maybe<T>(&mut self, value: impl FnOnce(&mut Self) -> T) -> Option<T> {
        self.flag().then(|| value(self))
    }
}

fn margin_mode(input: &mut Input) -> TxflowMarginMode {
    if input.flag() {
        TxflowMarginMode::Cross
    } else {
        TxflowMarginMode::Isolated
    }
}

fn order_body(input: &mut Input) -> OrderBody {
    let t = if input.flag() {
        OrderKind::Limit {
            tif: [Tif::Gtc, Tif::Ioc, Tif::Alo, Tif::Gtd, Tif::FrontendMarket]
                [usize::from(input.byte() % 5)],
        }
    } else {
        OrderKind::Trigger {
            is_market: input.flag(),
            trigger_px: input.text(),
            tpsl: input.maybe(|i| if i.flag() { Tpsl::Tp } else { Tpsl::Sl }),
        }
    };
    OrderBody {
        a: input.u32(),
        b: input.flag(),
        p: input.text(),
        s: input.text(),
        r: input.flag(),
        t,
        c: input.maybe(Input::text),
        m: input.maybe(margin_mode),
        expires_at: input.maybe(Input::u64),
    }
}

fn action(input: &mut Input) -> Action {
    let grouping = |input: &mut Input| input.maybe(|_| CancelGrouping::Trigger);
    match input.byte() % 6 {
        0 => Action::Order(OrderAction {
            grouping: if input.flag() {
                Grouping::Na
            } else {
                Grouping::NormalTrigger
            },
            orders: (0..input.byte() % 4).map(|_| order_body(input)).collect(),
        }),
        1 => Action::Cancel(CancelAction {
            cancels: (0..input.byte() % 4)
                .map(|_| CancelBody {
                    a: input.u32(),
                    o: input.u64(),
                    grouping: grouping(input),
                })
                .collect(),
        }),
        2 => Action::CancelByCloid(CancelByCloidAction {
            cancels: (0..input.byte() % 4)
                .map(|_| CancelByCloidBody {
                    asset: input.u32(),
                    cloid: input.text(),
                    grouping: grouping(input),
                })
                .collect(),
        }),
        3 => Action::Modify2(ModifyAction {
            order: ModifyBody {
                o: input.u64(),
                p: input.text(),
                s: input.text(),
            },
        }),
        4 => Action::UpdateLeverage(UpdateLeverage {
            asset: input.u32(),
            leverage: input.u32() as i32,
            margin_mode: input.maybe(margin_mode),
        }),
        _ => Action::UpdateMarginMode(UpdateMarginMode {
            asset: input.u32(),
            is_cross: input.flag(),
        }),
    }
}

/// An action built from the input: MessagePack bytes, action hash, EIP-712 digest, signature.
///
/// # Panics
///
/// Panics if the digest of the signer differs from the hash of the same MessagePack bytes
/// under another nonce or vault: the nonce and the vault must be part of the hash.
pub fn action_sign(data: &[u8]) {
    let mut input = Input(data);
    let nonce = input.u64();
    let vault = input.maybe(|i| Address::left_padding_from(i.take(20)));
    let action = action(&mut input);

    let Ok(bytes) = action.msgpack() else {
        return;
    };
    let hash = connection_id(&bytes, nonce, vault);
    assert_ne!(hash, connection_id(&bytes, nonce.wrapping_add(1), vault));
    assert_ne!(
        hash,
        connection_id(&bytes, nonce, vault.xor(Some(Address::ZERO)))
    );
    let _ = SIGNER.action_digest(&action, nonce, vault);
    let _ = SIGNER.sign_action(&action, nonce, vault);
    let _ = SIGNER.envelope(&action, nonce);
}

/// Price and size strings of an order. The input is `value,step` as text, or raw bytes.
///
/// # Panics
///
/// Panics if a formatted value does not read back as the value it was made from.
pub fn wire_format(data: &[u8]) {
    let mut input = Input(data);
    let mut decimal = || Decimal::new(input.u64() as i64, u32::from(input.byte() % 29));
    let binary = Some((decimal(), decimal()));
    let text = std::str::from_utf8(data)
        .ok()
        .and_then(|text| text.split_once(','))
        .and_then(|(value, step)| Some((parse_decimal(value).ok()?, parse_decimal(step).ok()?)));

    for (value, step) in [binary, text].into_iter().flatten() {
        if let Ok(wire) = wire_decimal(value, step) {
            assert_eq!(parse_decimal(&wire).ok(), Some(value), "{value} on {step}");
        }
        let precision = step.scale() as u8;
        let _ = price_from_decimal(value, precision);
        let _ = quantity_from_decimal(value, precision);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Knuth's MMIX generator: fixed seed, same inputs on every run.
    struct Lcg(u64);

    impl Lcg {
        fn next(&mut self, below: usize) -> usize {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (self.0 >> 33) as usize % below.max(1)
        }
    }

    fn fixture(name: &str) -> String {
        let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
        std::fs::read_to_string(path).unwrap()
    }

    /// Runs `target` on each seed, then 400 times on random bytes or on a seed with a few
    /// bytes swapped for number characters, a run of nines put in, or its end cut off.
    fn hammer(target: fn(&[u8]), seeds: &[String]) {
        const NUMBER_BYTES: &[u8] = b"0123456789-.eE\"";
        let mut lcg = Lcg(0x7478_666c_6f77);
        for seed in seeds {
            target(seed.as_bytes());
        }
        for round in 0..400 {
            let mut input: Vec<u8> = if seeds.is_empty() || round % 4 == 0 {
                (0..lcg.next(200)).map(|_| lcg.next(256) as u8).collect()
            } else {
                seeds[lcg.next(seeds.len())].as_bytes().to_vec()
            };
            for _ in 0..=lcg.next(3) {
                let at = lcg.next(input.len());
                // Aim at a number: a changed digit keeps the JSON valid and reaches the parsers
                let digit = (at..input.len())
                    .find(|&i| input[i].is_ascii_digit())
                    .unwrap_or(at);
                match lcg.next(8) {
                    0 => input.truncate(at),
                    1 => drop(input.splice(digit..digit, *b"99999999999999999999")),
                    _ if input.is_empty() => {}
                    _ => input[digit] = NUMBER_BYTES[lcg.next(NUMBER_BYTES.len())],
                }
            }
            target(&input);
        }
    }

    fn ws_frames() -> Vec<String> {
        fixture("ws_frames.jsonl")
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// Values a real fuzzer reaches and the changed fixtures do not; each one panicked once.
    #[test]
    fn hostile_values_are_errors_not_panics() {
        let meta: PerpMeta = serde_json::from_str(
            r#"{"name":"BTC \u20ac","baseCurrency":" ","quoteCurrency":"USDC",
                "basePrecision":"0.0001","priceTick":"0.1","maxLeverage":"50","index":1}"#,
        )
        .unwrap();
        assert!(parse_instrument(&meta, TS).is_err());

        let order: TxflowOrder = serde_json::from_str(
            r#"{"oid":1,"coin":"1","side":"B","origSz":"-79228162514264337593543950335",
                "sz":"79228162514264337593543950335"}"#,
        )
        .unwrap();
        assert_eq!(order.filled(), Decimal::ZERO);

        assert!(crate::http::parse::fill_trade_id(u64::MAX, Decimal::MAX).is_err());
    }

    #[test]
    fn http_parse_survives_changed_fixtures() {
        let seeds: Vec<String> = [
            "perpMeta.json",
            "l2Book.json",
            "trades.json",
            "candleSnapshot.json",
            "marketTicker.json",
            "frontendOpenOrders.json",
            "triggerOrders.json",
            "historicalOrders.json",
            "orderDetails.json",
            "userFills.json",
            "clearinghouseState.json",
            "error.json",
        ]
        .map(fixture)
        .into_iter()
        .chain([
            r#"{"status":"ok","code":0,"data":{"type":"PlaceOrder","data":{"statuses":["success"]}}}"#.to_string(),
            r#"{"status":"err","code":10000,"message":"Post only order would match"}"#.to_string(),
        ])
        .collect();
        hammer(http_parse, &seeds);
    }

    #[test]
    fn ws_parse_survives_changed_frames() {
        hammer(ws_parse, &ws_frames());
    }

    #[test]
    fn book_apply_survives_changed_streams() {
        let frames = ws_frames();
        // Snapshot then delta, as the venue sends them, plus each frame alone
        let mut seeds = vec![format!("{}\n{}", frames[0], frames[2])];
        seeds.extend(frames);
        hammer(book_apply, &seeds);
    }

    #[test]
    fn action_sign_survives_random_actions() {
        hammer(action_sign, &[]);
    }

    #[test]
    fn wire_format_survives_changed_decimals() {
        let seeds = ["65000.0,0.1", "0.0030,0.0001", "120,10", "0,1"].map(str::to_string);
        hammer(wire_format, &seeds);
    }
}
