//! TxFlow enums and the mapping of venue strings to Nautilus enums.
//!
//! The venue is not consistent about casing (`"GTC"` and `"Gtc"`, `"Filled"` and `"filled"`),
//! so every parser here ignores case.

use nautilus_model::{
    data::BarType,
    enums::{
        AggregationSource, AggressorSide, BarAggregation, LiquiditySide, OrderSide, OrderStatus,
        PriceType, TimeInForce,
    },
};
use serde::{Deserialize, Serialize};
use strum::{AsRefStr, Display, EnumIter, EnumString};

/// TxFlow API environment.
#[derive(
    Copy,
    Clone,
    Debug,
    Default,
    Display,
    PartialEq,
    Eq,
    Hash,
    AsRefStr,
    EnumIter,
    EnumString,
    Serialize,
    Deserialize,
)]
#[serde(rename_all = "lowercase")]
#[strum(ascii_case_insensitive, serialize_all = "lowercase")]
#[cfg_attr(
    feature = "python",
    pyo3::pyclass(
        eq,
        eq_int,
        module = "nautilus_txflow",
        from_py_object,
        rename_all = "SCREAMING_SNAKE_CASE"
    )
)]
pub enum TxflowEnvironment {
    /// Mainnet trading environment.
    #[default]
    Mainnet,
    /// Testnet environment.
    Testnet,
}

impl TxflowEnvironment {
    /// EIP-712 domain name and `txflowNetwork` field of the signed structs.
    #[must_use]
    pub const fn network(self) -> &'static str {
        match self {
            Self::Mainnet => "TxFlow-Mainnet",
            Self::Testnet => "TxFlow-Testnet",
        }
    }

    #[must_use]
    pub const fn chain_id(self) -> u32 {
        match self {
            Self::Mainnet => 869,
            Self::Testnet => 2511,
        }
    }
}

/// Margin mode sent as `m` with every order.
#[derive(
    Copy,
    Clone,
    Debug,
    Default,
    Display,
    PartialEq,
    Eq,
    Hash,
    AsRefStr,
    EnumIter,
    EnumString,
    Serialize,
    Deserialize,
)]
#[serde(rename_all = "lowercase")]
#[strum(ascii_case_insensitive, serialize_all = "lowercase")]
#[cfg_attr(
    feature = "python",
    pyo3::pyclass(
        eq,
        eq_int,
        module = "nautilus_txflow",
        from_py_object,
        rename_all = "SCREAMING_SNAKE_CASE"
    )
)]
pub enum TxflowMarginMode {
    #[default]
    Cross,
    Isolated,
}

/// Returns the venue candle interval and its length in milliseconds for `bar_type`.
///
/// # Errors
///
/// Returns an error if the venue has no such candle.
pub fn bar_interval(bar_type: &BarType) -> anyhow::Result<(&'static str, u64)> {
    let spec = bar_type.spec();
    anyhow::ensure!(
        bar_type.aggregation_source() == AggregationSource::External
            && spec.price_type == PriceType::Last,
        "TxFlow serves external LAST bars only, was {bar_type}"
    );
    const MINUTE: u64 = 60_000;
    let interval = match (spec.step.get(), spec.aggregation) {
        (1, BarAggregation::Minute) => ("1m", MINUTE),
        (3, BarAggregation::Minute) => ("3m", 3 * MINUTE),
        (5, BarAggregation::Minute) => ("5m", 5 * MINUTE),
        (15, BarAggregation::Minute) => ("15m", 15 * MINUTE),
        (30, BarAggregation::Minute) => ("30m", 30 * MINUTE),
        (1, BarAggregation::Hour) => ("1h", 60 * MINUTE),
        (2, BarAggregation::Hour) => ("2h", 120 * MINUTE),
        (4, BarAggregation::Hour) => ("4h", 240 * MINUTE),
        (8, BarAggregation::Hour) => ("8h", 480 * MINUTE),
        (12, BarAggregation::Hour) => ("12h", 720 * MINUTE),
        (1, BarAggregation::Day) => ("1d", 1440 * MINUTE),
        _ => anyhow::bail!("TxFlow has no candle interval for {bar_type}"),
    };
    Ok(interval)
}

/// Parses `"B"`/`"A"`, `"Buy"`/`"Sell"`, `"bid"`/`"ask"`.
///
/// # Errors
///
/// Returns an error for any other value.
pub fn parse_order_side(value: &str) -> anyhow::Result<OrderSide> {
    match value.to_ascii_lowercase().as_str() {
        "b" | "buy" | "bid" | "long" => Ok(OrderSide::Buy),
        "a" | "s" | "sell" | "ask" | "short" => Ok(OrderSide::Sell),
        _ => anyhow::bail!("unknown TxFlow side {value:?}"),
    }
}

/// The side of a public trade is the taker side.
///
/// # Errors
///
/// Returns an error for an unknown side.
pub fn parse_aggressor_side(value: &str) -> anyhow::Result<AggressorSide> {
    Ok(match parse_order_side(value)? {
        OrderSide::Buy => AggressorSide::Buy,
        _ => AggressorSide::Sell,
    })
}

/// # Errors
///
/// Returns an error for an unknown status.
pub fn parse_order_status(value: &str) -> anyhow::Result<OrderStatus> {
    match value.to_ascii_lowercase().as_str() {
        // `Pending`: accepted by consensus and has an order id, not yet in the book
        "pending" | "open" => Ok(OrderStatus::Accepted),
        "partialfilled" | "partiallyfilled" => Ok(OrderStatus::PartiallyFilled),
        "filled" => Ok(OrderStatus::Filled),
        "partialcanceled" | "partialcancelled" | "canceled" | "cancelled" => {
            Ok(OrderStatus::Canceled)
        }
        "rejected" => Ok(OrderStatus::Rejected),
        "triggered" => Ok(OrderStatus::Triggered),
        "expired" => Ok(OrderStatus::Expired),
        _ => anyhow::bail!("unknown TxFlow order status {value:?}"),
    }
}

/// Returns the time in force and whether the order is post-only.
#[must_use]
pub fn parse_time_in_force(value: &str) -> (TimeInForce, bool) {
    match value.to_ascii_lowercase().as_str() {
        "ioc" | "frontendmarket" => (TimeInForce::Ioc, false),
        "alo" => (TimeInForce::Gtc, true),
        "gtd" => (TimeInForce::Gtd, false),
        _ => (TimeInForce::Gtc, false),
    }
}

#[must_use]
pub fn parse_liquidity_side(value: &str) -> LiquiditySide {
    match value.to_ascii_lowercase().as_str() {
        "maker" => LiquiditySide::Maker,
        "taker" => LiquiditySide::Taker,
        _ => LiquiditySide::NoLiquiditySide,
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case("BTC-USDC-PERP.TXFLOW-1-MINUTE-LAST-EXTERNAL", Some(("1m", 60_000)))]
    #[case("BTC-USDC-PERP.TXFLOW-12-HOUR-LAST-EXTERNAL", Some(("12h", 43_200_000)))]
    #[case("BTC-USDC-PERP.TXFLOW-1-DAY-LAST-EXTERNAL", Some(("1d", 86_400_000)))]
    #[case("BTC-USDC-PERP.TXFLOW-10-MINUTE-LAST-EXTERNAL", None)]
    #[case("BTC-USDC-PERP.TXFLOW-1-MINUTE-BID-EXTERNAL", None)]
    #[case("BTC-USDC-PERP.TXFLOW-1-MINUTE-LAST-INTERNAL", None)]
    fn bar_interval_maps_venue_candles(
        #[case] bar_type: &str,
        #[case] expected: Option<(&str, u64)>,
    ) {
        assert_eq!(bar_interval(&BarType::from(bar_type)).ok(), expected);
    }

    #[rstest]
    #[case("Filled", OrderStatus::Filled)]
    #[case("filled", OrderStatus::Filled)]
    #[case("PartialCanceled", OrderStatus::Canceled)]
    #[case("Pending", OrderStatus::Accepted)]
    fn order_status_ignores_case(#[case] value: &str, #[case] expected: OrderStatus) {
        assert_eq!(parse_order_status(value).unwrap(), expected);
    }

    #[test]
    fn sides_and_tif() {
        assert_eq!(parse_order_side("B").unwrap(), OrderSide::Buy);
        assert_eq!(parse_order_side("Sell").unwrap(), OrderSide::Sell);
        assert!(parse_order_side("x").is_err());
        assert_eq!(parse_time_in_force("GTC"), (TimeInForce::Gtc, false));
        assert_eq!(parse_time_in_force("Alo"), (TimeInForce::Gtc, true));
        assert_eq!(
            TxflowEnvironment::from_str("TESTNET").unwrap().chain_id(),
            2511
        );
    }
}
