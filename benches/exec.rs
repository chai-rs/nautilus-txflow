//! Order path before signing: a Nautilus order to the venue's `OrderBody`.

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use nautilus_core::UnixNanos;
use nautilus_model::{
    data::QuoteTick,
    enums::{OrderSide, OrderType},
    instruments::Instrument,
    orders::OrderTestBuilder,
    types::{Price, Quantity},
};
use nautilus_txflow::{
    common::{enums::TxflowMarginMode, parse::wire_decimal},
    execution::order::{market_price, order_body},
    http::{
        models::{InfoEnvelope, Universe},
        parse::parse_instrument,
    },
};
use rust_decimal_macros::dec;

const PERP_META: &str = include_str!("../tests/fixtures/perpMeta.json");
const BTC: u32 = 1;

fn bench(c: &mut Criterion) {
    let ts = UnixNanos::default();
    let meta: InfoEnvelope<Universe> = serde_json::from_str(PERP_META).unwrap();
    let markets = meta.data.unwrap().universe;
    let btc = markets.iter().find(|market| market.index == BTC).unwrap();
    let btc = parse_instrument(btc, ts).unwrap();
    let tick = btc.price_increment().as_decimal();
    let order = OrderTestBuilder::new(OrderType::Limit)
        .instrument_id(btc.id())
        .side(OrderSide::Buy)
        .quantity(Quantity::from("0.0030"))
        .price(Price::from("65000.0"))
        .build();
    let quote = QuoteTick::new(
        btc.id(),
        Price::from("73382.0"),
        Price::from("73382.2"),
        Quantity::from("2.0166"),
        Quantity::from("5.2217"),
        ts,
        ts,
    );

    let mut group = c.benchmark_group("exec");
    group.bench_function("wire_decimal", |b| {
        b.iter(|| wire_decimal(black_box(dec!(65000)), tick).unwrap());
    });
    group.bench_function("market_price", |b| {
        b.iter(|| market_price(OrderSide::Buy, Some(black_box(&quote)), tick, 50).unwrap());
    });
    group.bench_function("order_body", |b| {
        b.iter(|| {
            order_body(
                black_box(&order),
                BTC,
                &btc,
                None,
                50,
                TxflowMarginMode::default(),
            )
            .unwrap()
        });
    });
    group.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
