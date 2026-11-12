//! Parsing and wire-format helpers shared by the HTTP and WebSocket layers.

use std::str::FromStr;

use nautilus_core::UnixNanos;
use nautilus_model::types::{Price, Quantity};
use rust_decimal::Decimal;
use serde::{Deserialize, Deserializer, de};

/// # Errors
///
/// Returns an error if `value` is not a decimal number.
pub fn parse_decimal(value: &str) -> anyhow::Result<Decimal> {
    Decimal::from_str(value).map_err(|e| anyhow::anyhow!("invalid decimal {value:?}: {e}"))
}

/// # Errors
///
/// Returns an error if `value` does not fit a price of `precision`.
pub fn price_from_decimal(value: Decimal, precision: u8) -> anyhow::Result<Price> {
    Price::from_decimal_dp(value, precision)
        .map_err(|e| anyhow::anyhow!("invalid price {value}: {e}"))
}

/// # Errors
///
/// Returns an error if `value` does not fit a quantity of `precision`.
pub fn quantity_from_decimal(value: Decimal, precision: u8) -> anyhow::Result<Quantity> {
    Quantity::from_decimal_dp(value, precision)
        .map_err(|e| anyhow::anyhow!("invalid quantity {value}: {e}"))
}

#[must_use]
pub const fn millis_to_nanos(millis: u64) -> UnixNanos {
    UnixNanos::new(millis.saturating_mul(1_000_000))
}

/// Formats `value` for an order: a non-negative multiple of `step`, written with the decimals
/// of `step`. The venue hashes the string exactly as sent.
///
/// # Errors
///
/// Returns an error if `step` is not positive, `value` is negative or off the step, or
/// `value` with the decimals of `step` has more than 28 significant digits.
pub fn wire_decimal(value: Decimal, step: Decimal) -> anyhow::Result<String> {
    anyhow::ensure!(step > Decimal::ZERO, "step must be positive, was {step}");
    anyhow::ensure!(
        !value.is_sign_negative(),
        "value must not be negative: {value}"
    );
    let remainder = value
        .checked_rem(step)
        .ok_or_else(|| anyhow::anyhow!("{value} cannot be checked against step {step}"))?;
    anyhow::ensure!(remainder.is_zero(), "{value} is not a multiple of {step}");
    let scale = step.normalize().scale();
    // Not `format!("{value:.scale$}")`: `rust_decimal` panics if that text is longer than 32
    // characters.
    let mut wire = value;
    wire.rescale(scale);
    anyhow::ensure!(
        wire.scale() == scale,
        "{value} does not fit {scale} decimals"
    );
    Ok(wire.to_string())
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Loose {
    Number(u64),
    String(String),
    Null,
}

/// Integer sent as a number or as a string (`asset` is both, depending on the endpoint).
///
/// # Errors
///
/// Returns an error if the value is neither.
pub fn de_u64<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
    de_opt_u64(deserializer)?.ok_or_else(|| de::Error::custom("expected an integer"))
}

/// As [`de_u64`]; `null` and `""` are `None`.
///
/// # Errors
///
/// Returns an error if the value is not an integer.
pub fn de_opt_u64<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<u64>, D::Error> {
    match Loose::deserialize(deserializer)? {
        Loose::Number(value) => Ok(Some(value)),
        Loose::String(value) if value.is_empty() => Ok(None),
        Loose::String(value) => value.parse().map(Some).map_err(de::Error::custom),
        Loose::Null => Ok(None),
    }
}

/// Identifier sent as a number or as a string, kept as text.
///
/// # Errors
///
/// Returns an error if the value is neither.
pub fn de_id<'de, D: Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    match Loose::deserialize(deserializer)? {
        Loose::Number(value) => Ok(value.to_string()),
        Loose::String(value) => Ok(value),
        Loose::Null => Err(de::Error::custom("expected an identifier")),
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum LooseDecimal {
    Decimal(Decimal),
    // matched only to tell text from other shapes
    String(#[allow(dead_code)] String),
    Null,
}

/// Decimal sent as a string or a number; `null`, `""` and unparsable text are `None`.
///
/// # Errors
///
/// Returns an error if the value is not a scalar.
pub fn de_opt_decimal<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Decimal>, D::Error> {
    Ok(match LooseDecimal::deserialize(deserializer)? {
        LooseDecimal::Decimal(value) => Some(value),
        LooseDecimal::String(_) | LooseDecimal::Null => None,
    })
}

#[cfg(test)]
mod tests {
    use rstest::rstest;
    use rust_decimal_macros::dec;

    use super::*;

    #[rstest]
    #[case(dec!(50000), dec!(0.1), Some("50000.0"))]
    #[case(dec!(50000.10), dec!(0.1), Some("50000.1"))]
    #[case(dec!(0.01), dec!(0.0001), Some("0.0100"))]
    #[case(dec!(120), dec!(10), Some("120"))]
    #[case(dec!(125), dec!(10), None)]
    #[case(dec!(50000.15), dec!(0.1), None)]
    #[case(dec!(-1), dec!(0.1), None)]
    #[case(dec!(1), dec!(0), None)]
    #[case(dec!(1.2000), dec!(0.01), Some("1.20"))]
    #[case(dec!(0), dec!(0.0001), Some("0.0000"))]
    #[case(dec!(102576720), dec!(0.00000000000000000002048), None)]
    fn wire_decimal_enforces_the_step(
        #[case] value: Decimal,
        #[case] step: Decimal,
        #[case] expected: Option<&str>,
    ) {
        assert_eq!(wire_decimal(value, step).ok().as_deref(), expected);
    }

    #[derive(Deserialize)]
    struct Row {
        #[serde(deserialize_with = "de_u64")]
        asset: u64,
        #[serde(default, deserialize_with = "de_opt_decimal")]
        px: Option<Decimal>,
        #[serde(deserialize_with = "de_id")]
        tid: String,
    }

    #[rstest]
    #[case(r#"{"asset":"7","px":"1.5","tid":12}"#, 7, Some(dec!(1.5)), "12")]
    #[case(r#"{"asset":7,"px":1.5,"tid":"0xab"}"#, 7, Some(dec!(1.5)), "0xab")]
    #[case(r#"{"asset":7,"px":"","tid":"a"}"#, 7, None, "a")]
    #[case(r#"{"asset":7,"px":null,"tid":"a"}"#, 7, None, "a")]
    #[case(r#"{"asset":7,"tid":"a"}"#, 7, None, "a")]
    fn loose_fields_accept_numbers_and_strings(
        #[case] json: &str,
        #[case] asset: u64,
        #[case] px: Option<Decimal>,
        #[case] tid: &str,
    ) {
        let row: Row = serde_json::from_str(json).unwrap();
        assert_eq!((row.asset, row.px, row.tid.as_str()), (asset, px, tid));
    }
}
