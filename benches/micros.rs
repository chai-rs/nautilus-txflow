//! Hot-path micros: book stream apply (snapshot, delta) and the 10-level depth cut.

use std::hint::black_box;

use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use nautilus_core::UnixNanos;
use nautilus_model::instruments::InstrumentAny;
use nautilus_txflow::{
    book::BookSync,
    http::{
        models::{InfoEnvelope, Universe},
        parse::parse_instrument,
    },
    websocket::messages::{BookDepth, WsMessage, parse_frame},
};

const FRAMES: &str = include_str!("../tests/fixtures/ws_frames.jsonl");
const PERP_META: &str = include_str!("../tests/fixtures/perpMeta.json");

/// The `l2BookDepth` frame on `line` of the fixture.
fn book_frame(line: usize) -> BookDepth {
    let frame = parse_frame(FRAMES.lines().nth(line).unwrap()).unwrap();
    let Some(WsMessage::BookDepth(frame)) = frame else {
        panic!("line {line} is not an l2BookDepth frame");
    };
    frame
}

fn btc() -> InstrumentAny {
    let meta: InfoEnvelope<Universe> = serde_json::from_str(PERP_META).unwrap();
    let markets = meta.data.unwrap().universe;
    let btc = markets.iter().find(|market| market.index == 1).unwrap();
    parse_instrument(btc, UnixNanos::default()).unwrap()
}

fn bench(c: &mut Criterion) {
    // BTC: the snapshot cut to 5 levels per side (line 0) and the delta that follows it (line 2)
    let (snapshot, delta) = (book_frame(0), book_frame(2));
    let btc = btc();
    let ts = UnixNanos::default();
    let synced = || {
        let mut sync = BookSync::default();
        sync.apply(&snapshot, &btc, ts).unwrap();
        sync
    };

    let mut group = c.benchmark_group("book");
    let mut sync = BookSync::default();
    group.bench_function("apply_snapshot_10", |b| {
        b.iter(|| sync.apply(black_box(&snapshot), &btc, ts).unwrap());
    });
    // A delta applies once after its snapshot, so each pass gets a book of its own.
    group.bench_function("apply_delta_4", |b| {
        b.iter_batched_ref(
            synced,
            |sync| sync.apply(black_box(&delta), &btc, ts).unwrap(),
            BatchSize::SmallInput,
        );
    });
    let mut sync = synced();
    sync.apply(&delta, &btc, ts).unwrap();
    group.bench_function("depth10", |b| {
        b.iter(|| black_box(&sync).depth(&btc, delta.seq, ts, ts).unwrap());
    });
    group.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
