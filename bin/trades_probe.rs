//! Prints the most recent public trades of one TxFlow market, without a `LiveNode`.
//!
//! Run with: `cargo run --bin txflow-trades-probe -- [INSTRUMENT_ID] [LIMIT]`
//!
//! Read-only and public: two `POST /info` requests (`perpMeta`, `trades`), no account, no key.
//! The venue returns the latest 300 trades at most.

use std::str::FromStr;

use nautilus_model::identifiers::InstrumentId;
use nautilus_txflow::{
    common::{consts::HTTP_TIMEOUT_SECS, enums::TxflowEnvironment},
    http::client::TxflowHttpClient,
};

/// The market and the number of trades to print, from the arguments after the program name.
fn parse_args(mut args: impl Iterator<Item = String>) -> anyhow::Result<(InstrumentId, usize)> {
    let instrument_id = args.next();
    let instrument_id = instrument_id.as_deref().unwrap_or("AERO-USDC-PERP.TXFLOW");
    let instrument_id = InstrumentId::from_str(instrument_id)
        .map_err(|e| anyhow::anyhow!("invalid instrument id: {e}"))?;
    let limit = args.next().map_or(Ok(20), |limit| limit.parse())?;
    Ok((instrument_id, limit))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let (instrument_id, limit) = parse_args(std::env::args().skip(1))?;

    let client =
        TxflowHttpClient::new(TxflowEnvironment::default(), None, HTTP_TIMEOUT_SECS, None)?;
    client.request_instruments().await?;
    anyhow::ensure!(
        client.instruments().get(&instrument_id).is_some(),
        "{instrument_id} is not an active market"
    );

    let trades = client.request_trades(instrument_id).await?;
    let newest = &trades[trades.len().saturating_sub(limit)..];
    println!("{} trades of {instrument_id}, oldest first", newest.len());
    for trade in newest {
        println!("{trade}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    fn args(args: &[&str]) -> anyhow::Result<(InstrumentId, usize)> {
        parse_args(args.iter().map(ToString::to_string))
    }

    #[rstest]
    #[case(&[], "AERO-USDC-PERP.TXFLOW", 20)]
    #[case(&["BTC-USDC-PERP.TXFLOW"], "BTC-USDC-PERP.TXFLOW", 20)]
    #[case(&["ETH-USDC-PERP.TXFLOW", "300"], "ETH-USDC-PERP.TXFLOW", 300)]
    fn arguments_have_defaults(
        #[case] given: &[&str],
        #[case] instrument_id: &str,
        #[case] limit: usize,
    ) {
        assert_eq!(
            args(given).unwrap(),
            (InstrumentId::from(instrument_id), limit)
        );
    }

    #[rstest]
    #[case(&["BTC-USDC-PERP"])]
    #[case(&["BTC-USDC-PERP.TXFLOW", "many"])]
    #[case(&["BTC-USDC-PERP.TXFLOW", "-1"])]
    fn bad_arguments_are_errors(#[case] given: &[&str]) {
        assert!(args(given).is_err());
    }
}
