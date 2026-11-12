//! Frames of the `/ws` and `/api-ws` streams.
//!
//! Data frames are `{"channel": <name>, "data": <payload>}`. Pongs, subscription
//! acknowledgements and error frames carry no market data and are dropped here.

use serde::Deserialize;
use serde_json::value::RawValue;

use crate::{
    common::parse::de_id,
    http::models::{AssetCtx, Candle, ClearinghouseState, Level, Levels, OrderRow, Trade},
};

/// `l2BookDepth` frame: a snapshot (`prevSeq` null) or a delta.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BookDepth {
    /// Instrument id (`perpMeta.index`).
    #[serde(deserialize_with = "de_id")]
    pub id: String,
    pub time: u64,
    pub seq: u64,
    #[serde(default)]
    pub prev_seq: Option<u64>,
    /// `snapshot` or `delta`.
    #[serde(rename = "type")]
    pub kind: String,
    pub levels: Levels,
}

/// `bbo` frame: best bid and best ask. A side is null when it is empty.
#[derive(Debug, Clone, Deserialize)]
pub struct Bbo {
    #[serde(deserialize_with = "de_id")]
    pub coin: String,
    pub time: u64,
    pub bbo: (Option<Level>, Option<Level>),
}

/// `activeAssetCtx` frame.
#[derive(Debug, Clone, Deserialize)]
pub struct ActiveAssetCtx {
    #[serde(deserialize_with = "de_id")]
    pub coin: String,
    pub ctx: AssetCtx,
}

/// A parsed frame, plus the reconnect marker of the client.
#[derive(Debug, Clone)]
pub enum WsMessage {
    /// The socket reconnected and every recorded subscription was sent again.
    Reconnected,
    BookDepth(BookDepth),
    Bbo(Bbo),
    Trades(Vec<Trade>),
    AssetCtx(ActiveAssetCtx),
    Candle(Candle),
    OrderUpdates(Vec<OrderRow>),
    ClearinghouseState(Box<ClearinghouseState>),
}

#[derive(Deserialize)]
struct Frame<'a> {
    #[serde(default)]
    channel: Option<String>,
    /// `/ws` error frames: `{"type":"error","code":..,"message":..}`.
    #[serde(default, rename = "type")]
    kind: Option<String>,
    #[serde(default, borrow)]
    data: Option<&'a RawValue>,
}

/// Parses one text frame. Frames without market or account data give `None`.
///
/// # Errors
///
/// Returns an error if the frame is not JSON or a data payload has an unexpected shape.
pub fn parse_frame(text: &str) -> serde_json::Result<Option<WsMessage>> {
    let frame: Frame<'_> = serde_json::from_str(text)?;
    let channel = frame.channel.as_deref().unwrap_or_default();
    if channel == "error" || frame.kind.as_deref() == Some("error") {
        log::warn!("TxFlow WebSocket error frame: {text}");
        return Ok(None);
    }
    let Some(data) = frame.data else {
        return Ok(None);
    };
    let data = data.get();
    Ok(Some(match channel {
        "l2BookDepth" => WsMessage::BookDepth(serde_json::from_str(data)?),
        "bbo" => WsMessage::Bbo(serde_json::from_str(data)?),
        "trades" => WsMessage::Trades(serde_json::from_str(data)?),
        "activeAssetCtx" => WsMessage::AssetCtx(serde_json::from_str(data)?),
        "candle" => WsMessage::Candle(serde_json::from_str(data)?),
        "orderUpdates" => WsMessage::OrderUpdates(serde_json::from_str(data)?),
        "clearinghouseState" => WsMessage::ClearinghouseState(serde_json::from_str(data)?),
        // subscriptionResponse, pong and channels this adapter does not use
        _ => return Ok(None),
    }))
}

#[cfg(test)]
mod tests {
    use rstest::rstest;
    use rust_decimal_macros::dec;

    use super::*;

    #[rstest]
    #[case(r#"{"method":"PONG"}"#)]
    #[case(r#"{"channel":"pong"}"#)]
    #[case(r#"{"channel":"subscriptionResponse","data":{"method":"subscribe","subscription":{"type":"candle","coin":"1","interval":"1m"}}}"#)]
    #[case(r#"{"type":"error","code":"RISK_RATE_LIMITED","message":"rate limit exceeded"}"#)]
    #[case(r#"{"channel":"error","data":{"code":"RISK_RATE_LIMITED","message":"rate limit exceeded"}}"#)]
    #[case(r#"{"channel":"l2Book","data":{"coin":"BTC-USDC"}}"#)]
    fn frames_without_data_are_dropped(#[case] text: &str) {
        assert!(parse_frame(text).unwrap().is_none());
    }

    #[rstest]
    #[case("not json")]
    #[case(r#"{"channel":"bbo","data":{"coin":"1"}}"#)]
    #[case(r#"{"channel":"trades","data":{"coin":"1"}}"#)]
    fn bad_frames_are_errors(#[case] text: &str) {
        assert!(parse_frame(text).is_err());
    }

    #[rstest]
    fn live_frames_parse() {
        // Captured from wss://rpc.txflow.com on 11 Nov 2026 (levels cut down)
        let bbo = r#"{"channel":"bbo","data":{"coin":"1","time":1794336500000,"bbo":[{"px":"73381.2","sz":"0.6110","n":2},{"px":"73381.3","sz":"0.1","n":1}],"impactBuyPrice":"73390.1","impactSellPrice":"73370.2"}}"#;
        let Some(WsMessage::Bbo(bbo)) = parse_frame(bbo).unwrap() else {
            panic!("expected bbo");
        };
        assert_eq!(bbo.coin, "1");
        assert_eq!(bbo.bbo.0.unwrap().px, dec!(73381.2));

        let ctx = r#"{"channel":"activeAssetCtx","data":{"coin":"1","ctx":{"funding":"0.00078992","openInterest":"12.5","oraclePx":"73452.6","markPx":"73420.2","midPx":"73420.0","impactPxs":["1","2"],"lastPrice":"73418.6","nextFundingTime":1794337200000,"prevFundingTime":1794333600000,"blockTime":1794336500000}}}"#;
        let Some(WsMessage::AssetCtx(ctx)) = parse_frame(ctx).unwrap() else {
            panic!("expected activeAssetCtx");
        };
        assert_eq!(ctx.ctx.mark_px, Some(dec!(73420.2)));
        assert_eq!(ctx.ctx.next_funding_time, Some(1_794_337_200_000));

        let candle = r#"{"channel":"candle","data":{"t":1794336480000,"o":"73406.2","h":"73410.0","l":"73400.0","c":"73407.1","v":"0.4363","n":15,"s":"1","i":"1m","T":1794336539999}}"#;
        let Some(WsMessage::Candle(candle)) = parse_frame(candle).unwrap() else {
            panic!("expected candle");
        };
        assert_eq!((candle.s.as_str(), candle.v), ("1", dec!(0.4363)));

        // The docs show the candle values as numbers, the venue sends strings: both decode
        let documented = r#"{"channel":"candle","data":{"t":1794336480000,"o":73406.2,"h":73410,"l":73400.0,"c":73407.1,"v":0.4363,"n":15,"s":"1","i":"1m","T":1794336539999}}"#;
        let Some(WsMessage::Candle(candle)) = parse_frame(documented).unwrap() else {
            panic!("expected candle");
        };
        assert_eq!(
            (candle.o, candle.h, candle.v),
            (dec!(73406.2), dec!(73410), dec!(0.4363))
        );

        let depth = r#"{"channel":"l2BookDepth","data":{"coin":"BTC-USDC","id":1,"time":1794336500000,"seq":674412,"prevSeq":674411,"type":"delta","levels":[[{"px":"73381.2","sz":"0.6110","n":2}],[]]}}"#;
        let Some(WsMessage::BookDepth(depth)) = parse_frame(depth).unwrap() else {
            panic!("expected l2BookDepth");
        };
        assert_eq!((depth.id.as_str(), depth.prev_seq), ("1", Some(674_411)));
    }

    #[rstest]
    fn order_update_from_the_docs_parses() {
        let text = r#"{"channel":"orderUpdates","data":[{"order":{"asset":1,"coin":"BTC-USDC","side":"B","orderType":"Limit","limitPx":"50000.0","sz":"0.006","oid":77,"timestamp":1794336500000,"origSz":"0.01","avgFillPrice":"49999.5","reduceOnly":false,"isTrigger":false,"cloid":"O-1","fee":"0.05"},"status":"PartialFilled","statusTimestamp":1794336500100}]}"#;
        let Some(WsMessage::OrderUpdates(rows)) = parse_frame(text).unwrap() else {
            panic!("expected orderUpdates");
        };
        assert_eq!(rows[0].order.filled(), dec!(0.004));
        assert_eq!(rows[0].status, "PartialFilled");
    }
}
