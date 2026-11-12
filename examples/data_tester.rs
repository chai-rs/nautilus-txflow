//! Smoke test of the TxFlow live data client on public mainnet data. No account, no key.
//!
//! Run with: `cargo run --example data_tester --features examples`

use log::LevelFilter;
use nautilus_common::{enums::Environment, logging::logger::LoggerConfig};
use nautilus_live::node::LiveNode;
use nautilus_model::{
    data::{BarSpecification, BarType},
    enums::{AggregationSource, BarAggregation, PriceType},
    identifiers::{ClientId, InstrumentId, TraderId},
};
use nautilus_testkit::testers::{DataTester, DataTesterConfig};
use nautilus_txflow::{
    common::consts::TXFLOW, config::data::TxflowDataClientConfig,
    factories::data::TxflowDataClientFactory,
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let eth = InstrumentId::from("ETH-USDC-PERP.TXFLOW");
    let btc = InstrumentId::from("BTC-USDC-PERP.TXFLOW");
    let bar_types = vec![BarType::new(
        btc,
        BarSpecification::new(1, BarAggregation::Minute, PriceType::Last),
        AggregationSource::External,
    )];

    let log_config = LoggerConfig {
        stdout_level: LevelFilter::Info,
        ..Default::default()
    };

    let mut node = LiveNode::builder(TraderId::from("TESTER-001"), Environment::Live)?
        .with_name("TXFLOW-DATA-TESTER-001".to_string())
        .with_logging(log_config)
        .with_delay_post_stop_secs(2)
        .add_data_client(
            Some(TXFLOW.to_string()),
            Box::new(TxflowDataClientFactory::new()),
            Box::new(TxflowDataClientConfig::default()),
        )?
        .build()?;

    let tester_config = DataTesterConfig::builder()
        .client_id(ClientId::new(TXFLOW))
        .instrument_ids(vec![btc, eth])
        .bar_types(bar_types)
        .subscribe_book_deltas(true)
        .manage_book(true)
        .subscribe_quotes(true)
        .subscribe_trades(true)
        .subscribe_mark_prices(true)
        .subscribe_index_prices(true)
        .subscribe_funding_rates(true)
        .subscribe_bars(true)
        // .request_instruments(true)
        // .request_trades(true)
        // .request_bars(true)
        // .request_funding_rates(true)
        // .request_book_snapshot(true)
        .build()?;

    node.add_actor(DataTester::new(tester_config))?;
    node.run().await?;

    Ok(())
}
