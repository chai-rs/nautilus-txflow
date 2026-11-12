//! Venue models for `/info`, `/exchange` and the WebSocket payloads that share their shape.
//!
//! Numbers arrive as strings on most endpoints and as JSON numbers on a few; `Decimal` accepts
//! both. Optional text fields can be `""`, so optional decimals use [`de_opt_decimal`].

use rust_decimal::Decimal;
use serde::Deserialize;
use serde_json::Value;

use crate::common::parse::{de_id, de_opt_decimal, de_opt_u64, de_u64};

/// `/info` envelope: `{success, code, data}` or `{success:false, code, message}`.
#[derive(Debug, Deserialize)]
pub struct InfoEnvelope<T> {
    pub success: bool,
    #[serde(default)]
    pub code: i64,
    pub data: Option<T>,
    #[serde(default)]
    pub message: Option<String>,
}

/// Paged `/info` result. Rows are newest first.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Paged<T> {
    pub data: Vec<T>,
    #[serde(default)]
    pub has_more: bool,
}

/// `/info perpMeta` payload.
#[derive(Debug, Deserialize)]
pub struct Universe {
    pub universe: Vec<PerpMeta>,
}

/// One market of `perpMeta`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PerpMeta {
    /// Market name, `BTC-USDC`.
    pub name: String,
    pub base_currency: String,
    pub quote_currency: String,
    /// Size step. Can be above one (`"10"`).
    pub base_precision: Decimal,
    pub price_tick: Decimal,
    pub max_leverage: Decimal,
    /// The asset id of `/exchange` actions and the `coin` of requests. Starts at 1.
    pub index: u32,
    #[serde(default)]
    pub delisted: bool,
    #[serde(default)]
    pub halt_trading: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Level {
    pub px: Decimal,
    pub sz: Decimal,
}

/// `[bids, asks]`.
pub type Levels = (Vec<Level>, Vec<Level>);

/// `/info l2Book`: 20 levels per side.
#[derive(Debug, Clone, Deserialize)]
pub struct L2Book {
    pub time: u64,
    pub levels: Levels,
}

/// Public trade, from `/info trades` and the `/ws trades` channel.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Trade {
    pub coin: String,
    #[serde(default, deserialize_with = "de_opt_u64")]
    pub instrument_id: Option<u64>,
    /// Taker side: `B` or `A`.
    pub side: String,
    pub px: Decimal,
    pub sz: Decimal,
    pub time: u64,
    #[serde(deserialize_with = "de_id")]
    pub tid: String,
}

/// Candle, from `/info candleSnapshot` and the `/api-ws candle` channel.
#[derive(Debug, Clone, Deserialize)]
pub struct Candle {
    /// Open time, milliseconds.
    #[serde(deserialize_with = "de_u64")]
    pub t: u64,
    /// Close time, milliseconds (the last millisecond of the interval).
    #[serde(rename = "T", deserialize_with = "de_u64")]
    pub close_time: u64,
    /// Instrument id as text.
    #[serde(deserialize_with = "de_id")]
    pub s: String,
    /// Interval, `1m`.
    pub i: String,
    pub o: Decimal,
    pub h: Decimal,
    pub l: Decimal,
    pub c: Decimal,
    pub v: Decimal,
}

/// Mark, oracle and funding of one market: the `ctx` of the `/ws activeAssetCtx` channel.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssetCtx {
    /// Funding rate of the current hour, in percent.
    #[serde(default, deserialize_with = "de_opt_decimal")]
    pub funding: Option<Decimal>,
    #[serde(default, deserialize_with = "de_opt_decimal")]
    pub oracle_px: Option<Decimal>,
    #[serde(default, deserialize_with = "de_opt_decimal")]
    pub mark_px: Option<Decimal>,
    #[serde(default, deserialize_with = "de_opt_u64")]
    pub next_funding_time: Option<u64>,
    #[serde(default, deserialize_with = "de_opt_u64")]
    pub block_time: Option<u64>,
}

/// `/info marketTicker`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MarketTicker {
    /// In percent.
    pub funding_rate: Decimal,
    #[serde(default, deserialize_with = "de_opt_u64")]
    pub next_funding_time: Option<u64>,
    #[serde(default, deserialize_with = "de_opt_u64")]
    pub block_time: Option<u64>,
}

/// An order as sent by `frontendOpenOrders`, `historicalOrders`, `orderDetails` and the
/// `orderUpdates` channel. The four sources use different subsets of these fields.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TxflowOrder {
    #[serde(deserialize_with = "de_u64")]
    pub oid: u64,
    pub coin: String,
    #[serde(default, deserialize_with = "de_opt_u64")]
    pub asset: Option<u64>,
    #[serde(default, deserialize_with = "de_opt_u64")]
    pub instrument_id: Option<u64>,
    pub side: String,
    #[serde(default, deserialize_with = "de_opt_decimal")]
    pub limit_px: Option<Decimal>,
    /// Remaining size.
    #[serde(default, deserialize_with = "de_opt_decimal")]
    pub sz: Option<Decimal>,
    #[serde(default, deserialize_with = "de_opt_decimal")]
    pub orig_sz: Option<Decimal>,
    #[serde(default, deserialize_with = "de_opt_decimal")]
    pub filled_amount: Option<Decimal>,
    #[serde(default, deserialize_with = "de_opt_decimal")]
    pub avg_fill_price: Option<Decimal>,
    /// `frontendOpenOrders` spells the average fill price this way.
    #[serde(default, deserialize_with = "de_opt_decimal")]
    pub avg_fill_px: Option<Decimal>,
    #[serde(default, deserialize_with = "de_opt_decimal")]
    pub fee: Option<Decimal>,
    #[serde(default, deserialize_with = "de_opt_decimal")]
    pub trigger_px: Option<Decimal>,
    #[serde(default)]
    pub is_trigger: bool,
    #[serde(default)]
    pub reduce_only: bool,
    #[serde(default)]
    pub order_type: Option<String>,
    #[serde(default)]
    pub tif: Option<String>,
    #[serde(default)]
    pub cloid: Option<String>,
    /// Creation time; `triggerOrders` calls it `createTime`.
    #[serde(default, alias = "createTime", deserialize_with = "de_opt_u64")]
    pub timestamp: Option<u64>,
    #[serde(default, deserialize_with = "de_opt_u64")]
    pub expires_at: Option<u64>,
    /// `frontendOpenOrders` (`state`) and `triggerOrders` (`triggerState`) only.
    #[serde(default, alias = "triggerState")]
    pub state: Option<String>,
    /// `historicalOrders` and `orderDetails` only.
    #[serde(default)]
    pub status: Option<String>,
}

impl TxflowOrder {
    /// The key for [`crate::common::instruments::InstrumentCache::lookup`].
    #[must_use]
    pub fn instrument_key(&self) -> String {
        self.asset
            .or(self.instrument_id)
            .map_or_else(|| self.coin.clone(), |id| id.to_string())
    }

    #[must_use]
    pub fn avg_px(&self) -> Option<Decimal> {
        self.avg_fill_price
            .or(self.avg_fill_px)
            .filter(|px| !px.is_zero())
    }

    /// Original size; the remaining size where the venue omits it.
    #[must_use]
    pub fn quantity(&self) -> Decimal {
        self.orig_sz.or(self.sz).unwrap_or_default()
    }

    /// Filled size: `filledAmount` where present, else original minus remaining, else zero.
    #[must_use]
    pub fn filled(&self) -> Decimal {
        // `min` then `max`, not `clamp`: `clamp` panics on a negative size from the venue
        self.filled_amount
            .or_else(|| {
                self.sz
                    .and_then(|remaining| self.quantity().checked_sub(remaining))
            })
            .unwrap_or_default()
            .min(self.quantity())
            .max(Decimal::ZERO)
    }
}

/// Row of `historicalOrders`, `orderDetails` and `orderUpdates`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrderRow {
    pub order: TxflowOrder,
    pub status: String,
    #[serde(default, deserialize_with = "de_opt_u64")]
    pub status_timestamp: Option<u64>,
}

/// `/info orderDetails` payload.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrderDetails {
    pub order_details: OrderRow,
}

/// Row of `/info userFills`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserFill {
    pub coin: String,
    #[serde(default, deserialize_with = "de_opt_u64")]
    pub instrument_id: Option<u64>,
    #[serde(deserialize_with = "de_u64")]
    pub oid: u64,
    pub side: String,
    pub px: Decimal,
    pub sz: Decimal,
    pub time: u64,
    #[serde(default, deserialize_with = "de_opt_decimal")]
    pub fee: Option<Decimal>,
    #[serde(default)]
    pub fee_token: Option<String>,
    /// `Maker` or `Taker`; absent when the engine did not record it.
    #[serde(default)]
    pub trade_side: Option<String>,
}

impl UserFill {
    #[must_use]
    pub fn instrument_key(&self) -> String {
        self.instrument_id
            .map_or_else(|| self.coin.clone(), |id| id.to_string())
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MarginSummary {
    #[serde(default, deserialize_with = "de_opt_decimal")]
    pub account_value: Option<Decimal>,
    #[serde(default, deserialize_with = "de_opt_decimal")]
    pub total_margin_used: Option<Decimal>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Position {
    pub coin: String,
    #[serde(default, deserialize_with = "de_opt_u64")]
    pub asset: Option<u64>,
    /// Signed size: positive is long.
    #[serde(default, deserialize_with = "de_opt_decimal")]
    pub szi: Option<Decimal>,
    #[serde(default, deserialize_with = "de_opt_decimal")]
    pub entry_px: Option<Decimal>,
}

impl Position {
    #[must_use]
    pub fn instrument_key(&self) -> String {
        self.asset
            .map_or_else(|| self.coin.clone(), |id| id.to_string())
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct AssetPosition {
    pub position: Position,
}

/// `/info clearinghouseState` and the `/ws clearinghouseState` channel.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClearinghouseState {
    #[serde(default)]
    pub margin_summary: MarginSummary,
    #[serde(default)]
    pub asset_positions: Vec<AssetPosition>,
}

/// `/exchange` response.
///
/// `ok` means the action was accepted by consensus. With `confirm: "executed"` a failed
/// execution arrives as `{"error": "..."}` inside `statuses`.
#[derive(Debug, Clone, Deserialize)]
pub struct ExchangeResponse {
    pub status: String,
    #[serde(default)]
    pub code: i64,
    #[serde(default)]
    pub message: Option<String>,
    /// The gateway puts the error text here instead of `message`.
    #[serde(default)]
    pub response: Option<Value>,
    #[serde(default)]
    pub data: Option<Value>,
}

impl ExchangeResponse {
    /// The node did not answer in time (HTTP 202): the action may still execute.
    #[must_use]
    pub fn is_pending(&self) -> bool {
        self.status == "pending"
    }

    /// The reason if the whole action was refused before execution.
    #[must_use]
    pub fn action_error(&self) -> Option<String> {
        if matches!(self.status.as_str(), "ok" | "pending") {
            return None;
        }
        let text = self.message.clone().or_else(|| {
            self.response.as_ref().map(|value| {
                value
                    .as_str()
                    .map_or_else(|| value.to_string(), str::to_string)
            })
        });
        Some(text.unwrap_or_else(|| format!("status {}, code {}", self.status, self.code)))
    }

    /// One entry per element of `statuses`: `None` for `"success"`, the reason for
    /// `{"error": ..}`.
    #[must_use]
    pub fn status_errors(&self) -> Vec<Option<String>> {
        let statuses = self
            .data
            .as_ref()
            .and_then(|data| data.pointer("/data/statuses"))
            .and_then(Value::as_array);
        statuses.map_or_else(Vec::new, |statuses| {
            statuses
                .iter()
                .map(|status| {
                    status.get("error").map(|error| {
                        error
                            .as_str()
                            .map_or_else(|| error.to_string(), str::to_string)
                    })
                })
                .collect()
        })
    }

    /// The reject reason, or `None` if the action was accepted (`ok`, or `pending` with a
    /// transaction hash).
    #[must_use]
    pub fn reject_reason(&self) -> Option<String> {
        self.action_error()
            .or_else(|| self.status_errors().into_iter().flatten().next())
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;
    use rust_decimal_macros::dec;

    use super::*;

    #[rstest]
    #[case(
        r#"{"status":"ok","code":0,"data":{"type":"PlaceOrder","data":{"statuses":["success"]}}}"#,
        None
    )]
    #[case(r#"{"status":"pending","txHash":"0xab"}"#, None)]
    #[case(
        r#"{"status":"ok","code":0,"data":{"type":"PlaceOrder","data":{"statuses":[{"error":"Insufficient margin in account"}]}}}"#,
        Some("Insufficient margin in account")
    )]
    #[case(
        r#"{"status":"err","code":10000,"message":"Order not found"}"#,
        Some("Order not found")
    )]
    #[case(
        r#"{"status":"err","code":10000,"response":"rate limit exceeded"}"#,
        Some("rate limit exceeded")
    )]
    #[case(r#"{"status":"err"}"#, Some("status err, code 0"))]
    fn exchange_response_reject_reason(#[case] json: &str, #[case] expected: Option<&str>) {
        let response: ExchangeResponse = serde_json::from_str(json).unwrap();
        assert_eq!(response.reject_reason().as_deref(), expected);
    }

    #[rstest]
    // orderUpdates: remaining size only
    #[case(r#"{"oid":1,"coin":"BTC","side":"B","sz":"0.004","origSz":"0.010"}"#, dec!(0.010), dec!(0.006))]
    // historicalOrders: `sz` is not reduced, `filledAmount` is the truth
    #[case(r#"{"oid":1,"coin":"BTC","side":"B","sz":"0.01","origSz":"0.01","filledAmount":"0.01"}"#, dec!(0.01), dec!(0.01))]
    #[case(r#"{"oid":"1","coin":"BTC","side":"B","sz":"","origSz":"0.01","filledAmount":""}"#, dec!(0.01), dec!(0))]
    #[case(r#"{"oid":1,"coin":"BTC","side":"B","sz":"0.02","origSz":"0.01"}"#, dec!(0.01), dec!(0))]
    fn order_quantity_and_filled(
        #[case] json: &str,
        #[case] quantity: Decimal,
        #[case] filled: Decimal,
    ) {
        let order: TxflowOrder = serde_json::from_str(json).unwrap();
        assert_eq!((order.quantity(), order.filled()), (quantity, filled));
    }
}
