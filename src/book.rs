//! Order book stream: `/ws l2BookDepth` frames to Nautilus deltas.
//!
//! The channel sends a full snapshot (200 levels per side, `prevSeq` null) and then deltas
//! whose `prevSeq` is the `seq` of the frame before. Full snapshots come again every few
//! seconds. A delta that does not follow the last frame is a gap: the book is dropped until
//! the next snapshot.

use nautilus_core::UnixNanos;
use nautilus_model::{
    data::{BookOrder, OrderBookDelta, OrderBookDeltas},
    enums::{BookAction, OrderSide, RecordFlag},
    instruments::{Instrument, InstrumentAny},
};

use crate::{
    common::parse::{millis_to_nanos, price_from_decimal, quantity_from_decimal},
    http::models::{Level, Levels},
    websocket::messages::BookDepth,
};

#[derive(Debug, thiserror::Error)]
pub enum BookError {
    /// Reported once per gap. The caller should subscribe to the channel again.
    #[error("book sequence gap: expected prevSeq {expected}, got {got:?}")]
    Gap { expected: u64, got: Option<u64> },
    #[error("invalid book frame: {0}")]
    Invalid(#[from] anyhow::Error),
}

/// Sequence state of one market.
#[derive(Debug, Default)]
pub struct BookSync {
    /// `seq` of the last applied frame. `None` while waiting for a snapshot.
    seq: Option<u64>,
}

impl BookSync {
    /// Converts one frame. Returns `None` for a delta that arrives before a snapshot and for a
    /// delta without levels.
    ///
    /// # Errors
    ///
    /// Returns [`BookError::Gap`] when a delta does not follow the last frame, and
    /// [`BookError::Invalid`] when a level does not fit the instrument.
    pub fn apply(
        &mut self,
        frame: &BookDepth,
        instrument: &InstrumentAny,
        ts_init: UnixNanos,
    ) -> Result<Option<OrderBookDeltas>, BookError> {
        let ts_event = millis_to_nanos(frame.time);
        if frame.kind == "snapshot" {
            let deltas = snapshot_deltas(instrument, &frame.levels, frame.seq, ts_event, ts_init)?;
            self.seq = Some(frame.seq);
            return Ok(Some(deltas));
        }

        let Some(expected) = self.seq else {
            return Ok(None);
        };
        if frame.prev_seq != Some(expected) {
            self.seq = None;
            return Err(BookError::Gap {
                expected,
                got: frame.prev_seq,
            });
        }

        let mut deltas = Vec::with_capacity(frame.levels.0.len() + frame.levels.1.len());
        for (side, levels) in sides(&frame.levels) {
            for level in levels {
                let action = if level.sz.is_zero() {
                    BookAction::Delete
                } else {
                    BookAction::Update
                };
                let order = order(instrument, side, level);
                let delta = order.and_then(|order| {
                    OrderBookDelta::new_checked(
                        instrument.id(),
                        action,
                        order,
                        0,
                        frame.seq,
                        ts_event,
                        ts_init,
                    )
                });
                match delta {
                    Ok(delta) => deltas.push(delta),
                    Err(e) => {
                        // A half-applied delta would leave a wrong book
                        self.seq = None;
                        return Err(BookError::Invalid(e));
                    }
                }
            }
        }
        self.seq = Some(frame.seq);
        Ok(finish(instrument, deltas)?)
    }
}

/// Deltas that replace the whole book: a clear, then one add per level.
///
/// # Errors
///
/// Returns an error if a level does not fit the instrument.
pub fn snapshot_deltas(
    instrument: &InstrumentAny,
    levels: &Levels,
    sequence: u64,
    ts_event: UnixNanos,
    ts_init: UnixNanos,
) -> anyhow::Result<OrderBookDeltas> {
    let id = instrument.id();
    let flags = RecordFlag::F_SNAPSHOT as u8;
    let mut deltas = vec![OrderBookDelta::clear(id, sequence, ts_event, ts_init)];
    for (side, levels) in sides(levels) {
        for level in levels.iter().filter(|level| !level.sz.is_zero()) {
            deltas.push(OrderBookDelta::new_checked(
                id,
                BookAction::Add,
                order(instrument, side, level)?,
                flags,
                sequence,
                ts_event,
                ts_init,
            )?);
        }
    }
    finish(instrument, deltas)?.ok_or_else(|| anyhow::anyhow!("snapshot without deltas"))
}

fn sides(levels: &Levels) -> [(OrderSide, &Vec<Level>); 2] {
    [(OrderSide::Buy, &levels.0), (OrderSide::Sell, &levels.1)]
}

fn order(instrument: &InstrumentAny, side: OrderSide, level: &Level) -> anyhow::Result<BookOrder> {
    // Order id 0: an L2_MBP book keys its levels by price
    Ok(BookOrder::new(
        side,
        price_from_decimal(level.px, instrument.price_precision())?,
        quantity_from_decimal(level.sz, instrument.size_precision())?,
        0,
    ))
}

fn finish(
    instrument: &InstrumentAny,
    mut deltas: Vec<OrderBookDelta>,
) -> anyhow::Result<Option<OrderBookDeltas>> {
    let Some(last) = deltas.last_mut() else {
        return Ok(None);
    };
    last.flags |= RecordFlag::F_LAST as u8;
    Ok(Some(OrderBookDeltas::new_checked(instrument.id(), deltas)?))
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;
    use crate::http::{models::PerpMeta, parse::parse_instrument};

    fn instrument() -> InstrumentAny {
        let meta: PerpMeta = serde_json::from_str(
            r#"{"name":"BTC-USDC","baseCurrency":"BTC","quoteCurrency":"USDC","basePrecision":"0.0001","priceTick":"0.1","maxLeverage":75,"index":1}"#,
        )
        .unwrap();
        parse_instrument(&meta, UnixNanos::default()).unwrap()
    }

    fn frame(json: &str) -> BookDepth {
        serde_json::from_str(json).unwrap()
    }

    const SNAPSHOT: &str = r#"{"coin":"BTC-USDC","id":1,"time":1794336500000,"seq":10,"prevSeq":null,"type":"snapshot","levels":[[{"px":"73381.2","sz":"0.6110","n":2},{"px":"73381.0","sz":"1.0000","n":1}],[{"px":"73393.8","sz":"0.2000","n":1}]]}"#;
    const DELTA: &str = r#"{"coin":"BTC-USDC","id":1,"time":1794336500100,"seq":11,"prevSeq":10,"type":"delta","levels":[[{"px":"73381.2","sz":"0.5000","n":1}],[{"px":"73393.8","sz":"0.0000","n":0}]]}"#;

    #[rstest]
    fn snapshot_then_delta() {
        let instrument = instrument();
        let mut sync = BookSync::default();
        let ts = UnixNanos::from(5);

        let snapshot = sync
            .apply(&frame(SNAPSHOT), &instrument, ts)
            .unwrap()
            .unwrap();
        let actions: Vec<_> = snapshot.deltas.iter().map(|d| d.action).collect();
        assert_eq!(
            actions,
            [
                BookAction::Clear,
                BookAction::Add,
                BookAction::Add,
                BookAction::Add
            ]
        );
        assert_eq!(
            snapshot.deltas[3].flags,
            RecordFlag::F_SNAPSHOT as u8 | RecordFlag::F_LAST as u8
        );
        assert_eq!(snapshot.sequence, 10);
        assert_eq!(
            snapshot.ts_event,
            UnixNanos::from(1_794_336_500_000_000_000)
        );

        let delta = sync.apply(&frame(DELTA), &instrument, ts).unwrap().unwrap();
        assert_eq!(delta.deltas[0].action, BookAction::Update);
        assert_eq!(delta.deltas[0].order.side, Some(OrderSide::Buy));
        assert_eq!(delta.deltas[1].action, BookAction::Delete);
        assert_eq!(delta.deltas[1].order.side, Some(OrderSide::Sell));
        assert_eq!(delta.deltas[0].flags, 0);
        assert_eq!(delta.deltas[1].flags, RecordFlag::F_LAST as u8);
    }

    #[rstest]
    fn delta_before_snapshot_is_dropped() {
        let mut sync = BookSync::default();
        let result = sync.apply(&frame(DELTA), &instrument(), UnixNanos::default());
        assert!(matches!(result, Ok(None)));
    }

    #[rstest]
    fn gap_is_reported_once_and_a_snapshot_recovers() {
        let instrument = instrument();
        let mut sync = BookSync::default();
        let ts = UnixNanos::default();
        sync.apply(&frame(SNAPSHOT), &instrument, ts).unwrap();

        let late = DELTA.replace(r#""prevSeq":10"#, r#""prevSeq":12"#);
        let result = sync.apply(&frame(&late), &instrument, ts);
        assert!(matches!(
            result,
            Err(BookError::Gap {
                expected: 10,
                got: Some(12)
            })
        ));
        assert!(matches!(
            sync.apply(&frame(DELTA), &instrument, ts),
            Ok(None)
        ));

        assert!(
            sync.apply(&frame(SNAPSHOT), &instrument, ts)
                .unwrap()
                .is_some()
        );
        assert!(
            sync.apply(&frame(DELTA), &instrument, ts)
                .unwrap()
                .is_some()
        );
    }

    #[rstest]
    fn empty_snapshot_clears_the_book() {
        let empty = r#"{"coin":"BTC-USDC","id":1,"time":1,"seq":3,"prevSeq":null,"type":"snapshot","levels":[[],[]]}"#;
        let mut sync = BookSync::default();
        let deltas = sync
            .apply(&frame(empty), &instrument(), UnixNanos::default())
            .unwrap()
            .unwrap();
        assert_eq!(deltas.deltas.len(), 1);
        assert_eq!(deltas.deltas[0].action, BookAction::Clear);
    }
}
