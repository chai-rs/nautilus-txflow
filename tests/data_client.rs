//! The data client against a venue stand-in on localhost.
//!
//! Nothing leaves the machine. The stand-in answers `/info` from `tests/fixtures/{type}.json`
//! and each stream subscription with the frames recorded from the public streams
//! (`tests/fixtures/ws_frames.jsonl`).

use std::{
    str::FromStr,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use axum::{
    Json, Router,
    extract::{
        State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    response::Response,
    routing::{get, post},
};
use nautilus_common::{
    clients::DataClient,
    live::runner::replace_data_event_sender,
    messages::{
        DataEvent, DataResponse,
        data::{
            RequestBars, RequestBookSnapshot, RequestTrades, SubscribeBars, SubscribeBookDeltas,
            SubscribeBookDepth, SubscribeFundingRates, SubscribeIndexPrices,
            SubscribeInstrumentStatus, SubscribeMarkPrices, SubscribeQuotes, UnsubscribeBookDeltas,
        },
    },
};
use nautilus_core::{UUID4, UnixNanos};
use nautilus_model::{
    data::{BarType, Data},
    enums::{BookAction, BookType, MarketStatusAction},
    identifiers::{ClientId, InstrumentId},
    instruments::Instrument,
    types::Price,
};
use nautilus_txflow::{config::TxflowDataClientConfig, data::TxflowDataClient};
use parking_lot::Mutex;
use rust_decimal::Decimal;
use serde_json::Value;
use tokio::sync::mpsc::UnboundedReceiver;

const BTC: &str = "BTC-USDC-PERP.TXFLOW";
const FRAMES: &str = include_str!("fixtures/ws_frames.jsonl");

/// Every frame the stand-in received, with the path of its socket.
type Received = Arc<Mutex<Vec<(&'static str, Value)>>>;

/// While set, the stand-in shows the BTC market of `perpMeta` as halted.
static HALT_BTC: AtomicBool = AtomicBool::new(false);

async fn info(Json(request): Json<Value>) -> String {
    let kind = request["type"].as_str().unwrap_or_default();
    let path = format!("{}/tests/fixtures/{kind}.json", env!("CARGO_MANIFEST_DIR"));
    let body = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
    if kind == "perpMeta" && HALT_BTC.load(Ordering::SeqCst) {
        let mut meta: Value = serde_json::from_str(&body).unwrap();
        for market in meta["data"]["universe"].as_array_mut().unwrap() {
            if market["name"] == "BTC-USDC" {
                market["haltTrading"] = true.into();
            }
        }
        return meta.to_string();
    }
    body
}

/// Lines of the recorded frames the venue sends after a subscription to `channel`.
fn frames_of(channel: &str) -> &'static [usize] {
    match channel {
        "l2BookDepth" => &[0, 2],
        "bbo" => &[1],
        "allAssetCtxs" => &[8],
        "candle" => &[7, 6],
        _ => &[],
    }
}

async fn serve(mut socket: WebSocket, path: &'static str, received: Received) {
    let frames: Vec<&str> = FRAMES.lines().collect();
    while let Some(Ok(message)) = socket.recv().await {
        let Message::Text(text) = message else {
            continue;
        };
        let frame: Value = serde_json::from_str(&text).unwrap();
        let method = frame["method"].as_str().unwrap_or_default().to_string();
        let channel = frame["subscription"]["type"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        received.lock().push((path, frame));
        if method != "subscribe" {
            continue;
        }
        for line in frames_of(&channel) {
            if socket
                .send(Message::Text(frames[*line].into()))
                .await
                .is_err()
            {
                return;
            }
        }
    }
}

async fn node(ws: WebSocketUpgrade, State(received): State<Received>) -> Response {
    ws.on_upgrade(|socket| serve(socket, "/ws", received))
}

async fn api(ws: WebSocketUpgrade, State(received): State<Received>) -> Response {
    ws.on_upgrade(|socket| serve(socket, "/api-ws", received))
}

async fn start_venue() -> (Received, String) {
    let received = Received::default();
    let app = Router::new()
        .route("/info", post(info))
        .route("/ws", get(node))
        .route("/api-ws", get(api))
        .with_state(received.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (received, format!("http://{address}"))
}

async fn next_event(events: &mut UnboundedReceiver<DataEvent>) -> DataEvent {
    tokio::time::timeout(Duration::from_secs(10), events.recv())
        .await
        .expect("timed out waiting for a data event")
        .expect("event channel closed")
}

async fn next_data(events: &mut UnboundedReceiver<DataEvent>) -> Data {
    match next_event(events).await {
        DataEvent::Data(data) => data,
        other => panic!("expected data, was {other:?}"),
    }
}

async fn next_response(events: &mut UnboundedReceiver<DataEvent>) -> DataResponse {
    match next_event(events).await {
        DataEvent::Response(response) => response,
        other => panic!("expected a response, was {other:?}"),
    }
}

fn client_id() -> ClientId {
    ClientId::from("TXFLOW")
}

fn btc() -> InstrumentId {
    InstrumentId::from(BTC)
}

/// A connected client, with the instrument events of the connect already taken.
async fn connect() -> (TxflowDataClient, UnboundedReceiver<DataEvent>, Received) {
    let (received, url) = start_venue().await;
    let (sender, mut events) = tokio::sync::mpsc::unbounded_channel();
    replace_data_event_sender(sender);
    let config = TxflowDataClientConfig {
        base_url_http: Some(url),
        instrument_status_poll_secs: 1,
        ..Default::default()
    };
    let mut client = TxflowDataClient::new(client_id(), config).unwrap();
    client.connect().await.unwrap();
    assert!(client.is_connected());

    let mut instruments = Vec::new();
    while let Ok(event) = events.try_recv() {
        match event {
            DataEvent::Data(Data::Instrument(instrument)) => instruments.push(instrument.id()),
            other => panic!("expected an instrument, was {other:?}"),
        }
    }
    assert!(instruments.contains(&btc()), "{instruments:?}");
    assert!(instruments.contains(&InstrumentId::from("ETH-USDC-PERP.TXFLOW")));
    (client, events, received)
}

async fn wait_received(received: &Received, count: usize) -> Vec<(&'static str, Value)> {
    for _ in 0..500 {
        if received.lock().len() >= count {
            return received.lock().clone();
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!(
        "timed out waiting for {count} frames: {:?}",
        received.lock()
    );
}

#[tokio::test]
async fn streams_become_book_deltas_quotes_prices_funding_and_bars() {
    let (mut client, mut events, received) = connect().await;
    let id = Some(client_id());
    let now = UnixNanos::default;

    // Book: the snapshot of the subscription, then the delta that follows it
    client
        .subscribe_book_deltas(SubscribeBookDeltas::new(
            btc(),
            BookType::L2_MBP,
            id,
            None,
            UUID4::new(),
            now(),
            None,
            false,
            None,
            None,
        ))
        .unwrap();
    let Data::BookDeltas(snapshot) = next_data(&mut events).await else {
        panic!("expected the book snapshot");
    };
    assert_eq!(snapshot.instrument_id, btc());
    assert_eq!(snapshot.deltas[0].action, BookAction::Clear);
    assert!(snapshot.deltas.len() > 2);
    let Data::BookDeltas(delta) = next_data(&mut events).await else {
        panic!("expected the book delta");
    };
    assert!(
        delta
            .deltas
            .iter()
            .all(|delta| delta.action != BookAction::Clear)
    );
    assert_eq!(delta.sequence, 674_412);

    // Quotes
    client
        .subscribe_quotes(SubscribeQuotes::new(
            btc(),
            id,
            None,
            UUID4::new(),
            now(),
            None,
            None,
        ))
        .unwrap();
    let Data::Quote(quote) = next_data(&mut events).await else {
        panic!("expected a quote");
    };
    assert_eq!(quote.bid_price, Price::from("73382.0"));
    assert_eq!(quote.ask_price, Price::from("73382.2"));
    assert_eq!(quote.ts_event, UnixNanos::from(1_794_336_454_803_000_000));

    // Mark price, index price and funding rate come from one channel
    client
        .subscribe_mark_prices(SubscribeMarkPrices::new(
            btc(),
            id,
            None,
            UUID4::new(),
            now(),
            None,
            None,
        ))
        .unwrap();
    client
        .subscribe_index_prices(SubscribeIndexPrices::new(
            btc(),
            id,
            None,
            UUID4::new(),
            now(),
            None,
            None,
        ))
        .unwrap();
    client
        .subscribe_funding_rates(SubscribeFundingRates::new(
            btc(),
            id,
            None,
            UUID4::new(),
            now(),
            None,
            None,
        ))
        .unwrap();
    let Data::MarkPrice(mark) = next_data(&mut events).await else {
        panic!("expected a mark price");
    };
    assert_eq!(mark.value, Price::from("73390.0"));
    let Data::IndexPrice(index) = next_data(&mut events).await else {
        panic!("expected an index price");
    };
    assert_eq!(index.value, Price::from("73423.6"));
    let Data::FundingRate(funding) = next_data(&mut events).await else {
        panic!("expected a funding rate");
    };
    // The venue gives the rate in percent
    assert_eq!(funding.rate, Decimal::from_str("0.0000078992").unwrap());

    // Bars: the recorded candle ended long ago, so the timer closes it
    let bar_type = BarType::from(format!("{BTC}-1-MINUTE-LAST-EXTERNAL").as_str());
    client
        .subscribe_bars(SubscribeBars::new(
            bar_type,
            id,
            None,
            UUID4::new(),
            now(),
            None,
            None,
        ))
        .unwrap();
    let Data::Bar(bar) = next_data(&mut events).await else {
        panic!("expected a bar");
    };
    assert_eq!(bar.bar_type, bar_type);
    assert_eq!(bar.open, Price::from("73406.2"));
    assert_eq!(bar.close, Price::from("73383.9"));

    // One subscription per venue channel, each on its own socket
    let subscribed: Vec<(&str, String)> = wait_received(&received, 4)
        .await
        .iter()
        .map(|(path, frame)| (*path, frame["subscription"].to_string()))
        .collect();
    assert_eq!(
        subscribed,
        [
            ("/ws", r#"{"coin":"1","type":"l2BookDepth"}"#.to_string()),
            ("/ws", r#"{"coin":"1","type":"bbo"}"#.to_string()),
            ("/ws", r#"{"type":"allAssetCtxs"}"#.to_string()),
            (
                "/api-ws",
                r#"{"coin":"1","interval":"1m","type":"candle"}"#.to_string()
            ),
        ]
    );

    client
        .unsubscribe_book_deltas(&UnsubscribeBookDeltas::new(
            btc(),
            id,
            None,
            UUID4::new(),
            now(),
            None,
            None,
        ))
        .unwrap();
    let (path, frame) = wait_received(&received, 5).await.pop().unwrap();
    assert_eq!(path, "/ws");
    assert_eq!(frame["method"], "unsubscribe");
    assert_eq!(frame["subscription"]["type"], "l2BookDepth");

    client.disconnect().await.unwrap();
    assert!(client.is_disconnected());
}

#[tokio::test]
async fn book_depth_is_cut_from_the_stream_and_status_follows_the_market_list() {
    let (mut client, mut events, received) = connect().await;
    let id = Some(client_id());
    let now = UnixNanos::default;

    // Depth alone: the book channel is subscribed once and no deltas go out
    client
        .subscribe_book_depth(SubscribeBookDepth::new(
            btc(),
            BookType::L2_MBP,
            id,
            None,
            UUID4::new(),
            now(),
            None,
            false,
            None,
            None,
        ))
        .unwrap();
    let Data::BookDepth(depth) = next_data(&mut events).await else {
        panic!("expected the depth of the snapshot");
    };
    assert_eq!(depth.instrument_id, btc());
    // The recorded snapshot is cut to five levels per side
    assert_eq!((depth.bids.len(), depth.asks.len()), (5, 5));
    assert_eq!(depth.bids[0].price, Price::from("73382.0"));
    assert_eq!(depth.asks[0].price, Price::from("73382.2"));
    assert_eq!(depth.bid_counts[0], 1);
    assert_eq!(depth.sequence, 674_411);
    // The recorded delta adds one bid and two asks
    let Data::BookDepth(after) = next_data(&mut events).await else {
        panic!("expected the depth after the delta");
    };
    assert_eq!(after.sequence, 674_412);
    assert_eq!((after.bids.len(), after.asks.len()), (6, 7));
    assert_eq!(after.bids[5].price, Price::from("73381.2"));
    assert_eq!(after.bid_counts[5], 2);
    assert_eq!(after.asks[6].price, Price::from("73476.3"));
    assert!(
        after
            .asks
            .windows(2)
            .all(|pair| pair[0].price < pair[1].price)
    );
    let subscribed = wait_received(&received, 1).await;
    assert_eq!(subscribed[0].1["subscription"]["type"], "l2BookDepth");

    // Status: the one of the market list now, then each change the poll sees
    client
        .subscribe_instrument_status(SubscribeInstrumentStatus::new(
            btc(),
            id,
            None,
            UUID4::new(),
            now(),
            None,
            None,
        ))
        .unwrap();
    let Data::InstrumentStatus(status) = next_data(&mut events).await else {
        panic!("expected the instrument status");
    };
    assert_eq!(status.instrument_id, btc());
    assert_eq!(status.action, MarketStatusAction::Trading);
    assert_eq!(status.is_trading, Some(true));

    HALT_BTC.store(true, Ordering::SeqCst);
    let Data::InstrumentStatus(status) = next_data(&mut events).await else {
        panic!("expected the halt");
    };
    HALT_BTC.store(false, Ordering::SeqCst);
    assert_eq!(status.action, MarketStatusAction::Halt);
    assert_eq!(status.is_trading, Some(false));

    client.disconnect().await.unwrap();
}

#[tokio::test]
async fn requests_are_answered_from_the_info_endpoint() {
    let (mut client, mut events, _received) = connect().await;
    let id = Some(client_id());

    client
        .request_book_snapshot(RequestBookSnapshot::new(
            btc(),
            None,
            id,
            UUID4::new(),
            UnixNanos::default(),
            None,
        ))
        .unwrap();
    let DataResponse::Book(book) = next_response(&mut events).await else {
        panic!("expected a book response");
    };
    assert_eq!(book.data.best_bid_price(), Some(Price::from("73418.6")));
    assert_eq!(book.data.best_ask_price(), Some(Price::from("73418.8")));

    client
        .request_trades(RequestTrades::new(
            btc(),
            None,
            None,
            None,
            id,
            UUID4::new(),
            UnixNanos::default(),
            None,
        ))
        .unwrap();
    let DataResponse::Trades(trades) = next_response(&mut events).await else {
        panic!("expected a trades response");
    };
    assert_eq!(trades.data.len(), 6);
    assert!(
        trades
            .data
            .windows(2)
            .all(|pair| pair[0].ts_event <= pair[1].ts_event),
        "trades must be oldest first"
    );

    let bar_type = BarType::from(format!("{BTC}-1-MINUTE-LAST-EXTERNAL").as_str());
    client
        .request_bars(RequestBars::new(
            bar_type,
            None,
            None,
            None,
            id,
            UUID4::new(),
            UnixNanos::default(),
            None,
        ))
        .unwrap();
    let DataResponse::Bars(bars) = next_response(&mut events).await else {
        panic!("expected a bars response");
    };
    assert_eq!(bars.data.len(), 10);
    assert_eq!(bars.data[0].open, Price::from("73497.2"));

    client.disconnect().await.unwrap();
    assert!(client.is_disconnected());
}
