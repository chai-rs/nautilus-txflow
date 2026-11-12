//! WebSocket data path: recorded `l2BookDepth` frames to `WsMessage`.

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use nautilus_txflow::websocket::messages::parse_frame;

const FRAMES: &str = include_str!("../tests/fixtures/ws_frames.jsonl");

// Lines of the fixture, both BTC. The recorded snapshot was cut to 5 levels per side (the
// live one has 200); the delta has 4 levels.
const BOOK_SNAPSHOT: usize = 0;
const BOOK_DELTA: usize = 2;

fn bench(c: &mut Criterion) {
    let mut group = c.benchmark_group("decode");
    for (name, line) in [
        ("book_snapshot_10", BOOK_SNAPSHOT),
        ("book_delta_4", BOOK_DELTA),
    ] {
        let text = FRAMES.lines().nth(line).unwrap();
        assert!(parse_frame(text).unwrap().is_some());
        group.bench_function(name, |b| b.iter(|| parse_frame(black_box(text)).unwrap()));
    }
    group.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
