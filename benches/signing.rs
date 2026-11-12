//! Signing: MessagePack of an order action, connection id and one full agent signature.

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use nautilus_txflow::{
    common::enums::TxflowEnvironment,
    signing::{
        TxflowSigner,
        action::{Action, Grouping, OrderAction, OrderBody, OrderKind, Tif},
        connection_id,
    },
};

/// Key of the TxFlow docs' signing vectors: public test key, never funded.
const KEY: &str = "0xabababababababababababababababababababababababababababababababab";
const NONCE: u64 = 1_700_000_000_000;

fn bench(c: &mut Criterion) {
    // The `order` action of the docs' signing vector
    let action = Action::Order(OrderAction {
        grouping: Grouping::Na,
        orders: vec![OrderBody {
            a: 1,
            b: true,
            p: "50000.0".to_owned(),
            s: "0.01".to_owned(),
            r: false,
            t: OrderKind::Limit { tif: Tif::Gtc },
            c: None,
            m: None,
            expires_at: None,
        }],
    });
    let signer = TxflowSigner::new(KEY, TxflowEnvironment::Testnet).unwrap();
    let bytes = action.msgpack().unwrap();

    let mut group = c.benchmark_group("signing");
    group.bench_function("msgpack", |b| {
        b.iter(|| black_box(&action).msgpack().unwrap());
    });
    group.bench_function("connection_id", |b| {
        b.iter(|| connection_id(black_box(&bytes), NONCE, None));
    });
    group.bench_function("sign_action", |b| {
        b.iter(|| signer.sign_action(black_box(&action), NONCE, None).unwrap());
    });
    group.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
