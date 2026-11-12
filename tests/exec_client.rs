//! The execution client against a venue stand-in on localhost.
//!
//! Nothing leaves the machine. The agent key is made in the test and the account address is
//! the public Anvil account #0. The user-endpoint fixtures are hand-made in the documented
//! shapes: no signed or authenticated request was ever sent to TxFlow.

use std::{
    cell::RefCell,
    collections::{HashMap, VecDeque},
    rc::Rc,
    sync::Arc,
    time::Duration,
};

use alloy::signers::local::PrivateKeySigner;
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
    cache::Cache,
    clients::ExecutionClient,
    live::runner::replace_exec_event_sender,
    messages::{
        ExecutionEvent, ExecutionReport,
        execution::{
            CancelAllOrders, CancelOrder, GenerateFillReports, GenerateOrderStatusReport,
            GenerateOrderStatusReports, GeneratePositionStatusReports, ModifyOrder, SubmitOrder,
        },
    },
};
use nautilus_core::{UUID4, UnixNanos};
use nautilus_live::ExecutionClientCore;
use nautilus_model::{
    data::QuoteTick,
    enums::{
        AccountType, LiquiditySide, OmsType, OrderSide, OrderStatus, OrderType, PositionSide,
        TimeInForce, TriggerType,
    },
    events::OrderEventAny,
    identifiers::{ClientId, ClientOrderId, InstrumentId, TraderId, VenueOrderId},
    orders::{Order, OrderAny, OrderTestBuilder},
    types::{Money, Price, Quantity},
};
use nautilus_txflow::{
    common::{
        consts::TXFLOW_VENUE,
        enums::{TxflowEnvironment, TxflowMarginMode},
    },
    config::TxflowExecClientConfig,
    execution::TxflowExecutionClient,
    signing::{
        TxflowSigner,
        action::{
            Action, CancelAction, CancelBody, CancelByCloidAction, CancelByCloidBody, Grouping,
            ModifyAction, ModifyBody, OrderAction, OrderBody, OrderKind, Tif, UpdateLeverage,
        },
    },
};
use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio::sync::{Notify, broadcast, mpsc::UnboundedReceiver};

/// Public Anvil account #0 address, lower-cased the way the client sends it.
const USER: &str = "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266";
const BTC: &str = "BTC-USDC-PERP.TXFLOW";
const OID: u64 = 9001;

#[derive(Default)]
struct VenueState {
    info: Vec<Value>,
    exchange: Vec<Value>,
    /// Answers of `/exchange`, first in first out; "ok" when empty.
    replies: VecDeque<Value>,
    subscriptions: Vec<Value>,
    /// `/info` answers by request type that replace the fixture.
    overrides: HashMap<String, Value>,
}

#[derive(Clone)]
struct Venue {
    state: Arc<Mutex<VenueState>>,
    frames: broadcast::Sender<String>,
    drop_connection: Arc<Notify>,
}

fn ok_reply() -> Value {
    json!({"status": "ok", "code": 0, "data": {"type": "PlaceOrder", "data": {"statuses": ["success"]}}})
}

async fn info(State(venue): State<Venue>, Json(request): Json<Value>) -> String {
    let kind = request["type"].as_str().unwrap_or_default().to_string();
    let mut state = venue.state.lock();
    state.info.push(request);
    state.overrides.get(&kind).map_or_else(
        || {
            let path = format!("{}/tests/fixtures/{kind}.json", env!("CARGO_MANIFEST_DIR"));
            std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
        },
        Value::to_string,
    )
}

async fn exchange(State(venue): State<Venue>, Json(envelope): Json<Value>) -> Json<Value> {
    let mut state = venue.state.lock();
    state.exchange.push(envelope);
    Json(state.replies.pop_front().unwrap_or_else(ok_reply))
}

async fn upgrade(ws: WebSocketUpgrade, State(venue): State<Venue>) -> Response {
    ws.on_upgrade(|socket| venue.serve(socket))
}

impl Venue {
    async fn start() -> (Self, String) {
        let venue = Self {
            state: Arc::default(),
            frames: broadcast::channel(64).0,
            drop_connection: Arc::default(),
        };
        let app = Router::new()
            .route("/ws", get(upgrade))
            .route("/info", post(info))
            .route("/exchange", post(exchange))
            .with_state(venue.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (venue, format!("http://{address}"))
    }

    async fn serve(self, mut socket: WebSocket) {
        let mut frames = self.frames.subscribe();
        loop {
            tokio::select! {
                () = self.drop_connection.notified() => return,
                Ok(frame) = frames.recv() => {
                    if socket.send(Message::Text(frame.into())).await.is_err() {
                        return;
                    }
                }
                message = socket.recv() => {
                    let Some(Ok(message)) = message else { return };
                    let Message::Text(text) = message else { continue };
                    let frame: Value = serde_json::from_str(&text).unwrap();
                    if frame["method"] == "subscribe" {
                        self.state.lock().subscriptions.push(frame);
                    }
                }
            }
        }
    }

    fn push(&self, frame: &Value) {
        self.frames
            .send(frame.to_string())
            .expect("no WebSocket connection");
    }

    fn reply(&self, reply: Value) {
        self.state.lock().replies.push_back(reply);
    }

    fn exchange_posts(&self) -> Vec<Value> {
        self.state.lock().exchange.clone()
    }

    fn info_requests(&self, kind: &str) -> Vec<Value> {
        let state = self.state.lock();
        state
            .info
            .iter()
            .filter(|request| request["type"] == kind)
            .cloned()
            .collect()
    }

    async fn wait_exchange_posts(&self, count: usize) -> Vec<Value> {
        wait_until("an /exchange request", || {
            self.state.lock().exchange.len() >= count
        })
        .await;
        let posts = self.exchange_posts();
        assert_eq!(posts.len(), count, "unexpected /exchange requests");
        posts
    }

    async fn wait_subscriptions(&self, count: usize) {
        wait_until("subscriptions", || {
            self.state.lock().subscriptions.len() >= count
        })
        .await;
    }
}

async fn wait_until(what: &str, condition: impl Fn() -> bool) {
    for _ in 0..1000 {
        if condition() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for {what}");
}

async fn next_event(events: &mut UnboundedReceiver<ExecutionEvent>) -> ExecutionEvent {
    tokio::time::timeout(Duration::from_secs(10), events.recv())
        .await
        .expect("timed out waiting for an execution event")
        .expect("event channel closed")
}

fn client_id() -> ClientId {
    ClientId::from("TXFLOW")
}

fn trader_id() -> TraderId {
    TraderId::from("TESTER-001")
}

fn btc() -> InstrumentId {
    InstrumentId::from(BTC)
}

fn limit(client_order_id: &str) -> OrderTestBuilder {
    let mut builder = OrderTestBuilder::new(OrderType::Limit);
    builder
        .instrument_id(btc())
        .client_order_id(ClientOrderId::from(client_order_id))
        .side(OrderSide::Buy)
        .quantity(Quantity::from("0.0030"))
        .price(Price::from("65000.0"));
    builder
}

/// The wire form of [`limit`].
fn limit_body(client_order_id: &str) -> OrderBody {
    OrderBody {
        a: 1,
        b: true,
        p: "65000.0".to_string(),
        s: "0.0030".to_string(),
        r: false,
        t: OrderKind::Limit { tif: Tif::Gtc },
        c: Some(client_order_id.to_string()),
        m: Some(TxflowMarginMode::Cross),
        expires_at: None,
    }
}

/// One `orderUpdates` frame for an order made with [`limit`].
fn row(
    cloid: &str,
    oid: u64,
    status: &str,
    filled: &str,
    avg_px: Option<&str>,
    fee: &str,
) -> Value {
    let remaining = Quantity::from("0.0030") - Quantity::from(filled);
    json!({"channel": "orderUpdates", "data": [{
        "order": {
            "asset": 1, "coin": "BTC-USDC", "side": "B", "orderType": "Limit",
            "limitPx": "65000.0", "sz": remaining.to_string(), "origSz": "0.0030",
            "filledAmount": filled, "avgFillPrice": avg_px, "fee": fee, "oid": oid,
            "cloid": cloid, "timestamp": 1_791_893_561_993_u64, "reduceOnly": false,
            "isTrigger": false,
        },
        "status": status,
        "statusTimestamp": 1_791_893_562_000_u64,
    }]})
}

struct Harness {
    venue: Venue,
    client: TxflowExecutionClient,
    events: UnboundedReceiver<ExecutionEvent>,
    cache: Rc<RefCell<Cache>>,
    /// A second signer with the key of the client: signatures are deterministic.
    signer: TxflowSigner,
}

impl Harness {
    async fn connect() -> Self {
        Self::connect_with(|_| {}).await
    }

    async fn connect_with(configure: impl FnOnce(&mut TxflowExecClientConfig)) -> Self {
        let (venue, url) = Venue::start().await;
        let key = alloy_primitives::hex::encode_prefixed(PrivateKeySigner::random().to_bytes());
        let mut config = TxflowExecClientConfig {
            environment: TxflowEnvironment::Testnet,
            base_url_http: Some(url),
            // Mixed case on purpose: the venue wants the lower-case form
            account_address: Some("0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266".to_string()),
            agent_private_key: Some(key.clone()),
            ..Default::default()
        };
        configure(&mut config);

        let (sender, events) = tokio::sync::mpsc::unbounded_channel();
        replace_exec_event_sender(sender);
        let cache = Rc::new(RefCell::new(Cache::default()));
        let core = ExecutionClientCore::new(
            trader_id(),
            client_id(),
            *TXFLOW_VENUE,
            OmsType::Netting,
            config.account_id,
            AccountType::Margin,
            None,
            cache.clone(),
        );
        let mut client = TxflowExecutionClient::new(core, config).unwrap();
        client.start().unwrap();
        client.connect().await.unwrap();
        venue.wait_subscriptions(2).await;

        let mut harness = Self {
            venue,
            client,
            events,
            cache,
            signer: TxflowSigner::new(&key, TxflowEnvironment::Testnet).unwrap(),
        };
        let ExecutionEvent::Account(_) = next_event(&mut harness.events).await else {
            panic!("the first event is the account state");
        };
        harness
    }

    /// The posted envelope is exactly `action`, signed by the agent key with the posted nonce.
    fn assert_signed(&self, posted: &Value, action: &Action) {
        let nonce = posted["nonce"].as_u64().expect("nonce");
        assert_eq!(posted, &self.signer.envelope(action, nonce).unwrap());
    }

    fn submit(&self, order: &OrderAny) {
        self.cache
            .borrow_mut()
            .add_order(order.clone(), None, Some(client_id()), false)
            .unwrap();
        let cmd = SubmitOrder::from_order(
            order,
            trader_id(),
            Some(client_id()),
            None,
            UUID4::new(),
            UnixNanos::default(),
        );
        self.client.submit_order(cmd).unwrap();
    }

    /// The next event, which must be an order event. It is applied to the cached order, so an
    /// event the order state machine refuses fails the test.
    async fn order_event(&mut self) -> OrderEventAny {
        match next_event(&mut self.events).await {
            ExecutionEvent::Order(event) => {
                self.cache.borrow_mut().update_order(&event).unwrap();
                event
            }
            other => panic!("expected an order event, got {other:?}"),
        }
    }

    async fn order_report(&mut self) -> nautilus_model::reports::OrderStatusReport {
        match next_event(&mut self.events).await {
            ExecutionEvent::Report(ExecutionReport::Order(report)) => *report,
            other => panic!("expected an order report, got {other:?}"),
        }
    }

    /// Submits a limit order and lets the order stream show it as open.
    async fn submit_accepted(&mut self, client_order_id: &str, oid: u64) -> OrderAny {
        let order = limit(client_order_id).build();
        self.submit(&order);
        assert!(matches!(
            self.order_event().await,
            OrderEventAny::Submitted(_)
        ));
        // A real venue shows the order only after its request arrived; without this wait the
        // next request of the test can reach the mock first.
        wait_until("the order request", || {
            self.venue
                .exchange_posts()
                .iter()
                .any(|post| post["action"]["orders"][0]["c"] == client_order_id)
        })
        .await;
        self.venue
            .push(&row(client_order_id, oid, "Open", "0", None, "0"));
        let OrderEventAny::Accepted(accepted) = self.order_event().await else {
            panic!("expected OrderAccepted");
        };
        assert_eq!(accepted.venue_order_id, VenueOrderId::new(oid.to_string()));
        order
    }

    fn cancel(&self, order: &OrderAny) {
        let cmd = CancelOrder::new(
            trader_id(),
            Some(client_id()),
            order.strategy_id(),
            order.instrument_id(),
            order.client_order_id(),
            None,
            UUID4::new(),
            UnixNanos::default(),
            None,
            None,
        );
        self.client.cancel_order(cmd).unwrap();
    }

    fn modify(&self, order: &OrderAny, price: &str) {
        let cmd = ModifyOrder::new(
            trader_id(),
            Some(client_id()),
            order.strategy_id(),
            order.instrument_id(),
            order.client_order_id(),
            None,
            None,
            Some(Price::from(price)),
            None,
            UUID4::new(),
            UnixNanos::default(),
            None,
            None,
        );
        self.client.modify_order(cmd).unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn connect_reads_the_account_and_subscribes_the_user_streams() {
    let h = Harness::connect().await;

    let state = h.venue.info_requests("clearinghouseState");
    assert_eq!(
        state,
        vec![json!({"type": "clearinghouseState", "user": USER})]
    );
    let subscriptions = h.venue.state.lock().subscriptions.clone();
    assert_eq!(
        subscriptions,
        ["orderUpdates", "clearinghouseState"].map(|channel| json!({
            "method": "subscribe",
            "type": channel,
            "subscription": {"type": channel, "user": USER},
        }))
    );
    // No leverage in the config: connecting signs nothing
    assert!(h.venue.exchange_posts().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn connect_reports_the_balance_and_sets_configured_leverage() {
    let mut h = Harness::connect_with(|config| {
        config.leverage.insert(btc(), 5);
    })
    .await;

    let posts = h.venue.wait_exchange_posts(1).await;
    let action = Action::UpdateLeverage(UpdateLeverage {
        asset: 1,
        leverage: 5,
        margin_mode: None,
    });
    h.assert_signed(&posts[0], &action);
    assert_eq!(posts[0]["confirm"], "executed");

    // A `clearinghouseState` frame of the stream becomes an account state
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/clearinghouseState.json"
    );
    let mut frame: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    frame["data"]["marginSummary"]["accountValue"] = json!("1300.25");
    h.venue
        .push(&json!({"channel": "clearinghouseState", "data": frame["data"]}));
    let ExecutionEvent::Account(state) = next_event(&mut h.events).await else {
        panic!("expected an account state");
    };
    assert_eq!(state.balances[0].total, Money::from("1300.25 USDC"));
    assert_eq!(state.balances[0].locked, Money::from("19.50 USDC"));
    assert_eq!(state.balances[0].free, Money::from("1280.75 USDC"));
}

#[tokio::test(flavor = "multi_thread")]
async fn limit_order_is_signed_accepted_and_filled_once() {
    let mut h = Harness::connect().await;
    let order = limit("O-1").build();
    h.submit(&order);
    assert!(matches!(h.order_event().await, OrderEventAny::Submitted(_)));

    let posts = h.venue.wait_exchange_posts(1).await;
    assert_eq!(
        posts[0]["action"],
        json!({"type": "order", "grouping": "na", "orders": [{
            "a": 1, "b": true, "p": "65000.0", "s": "0.0030", "r": false,
            "t": {"limit": {"tif": "Gtc"}}, "c": "O-1", "m": "cross",
        }]})
    );
    let action = Action::Order(OrderAction {
        grouping: Grouping::Na,
        orders: vec![limit_body("O-1")],
    });
    h.assert_signed(&posts[0], &action);

    // The answer of `/exchange` has no order id: the order stream accepts the order
    h.venue.push(&row("O-1", OID, "Open", "0", None, "0"));
    let OrderEventAny::Accepted(accepted) = h.order_event().await else {
        panic!("expected OrderAccepted");
    };
    assert_eq!(accepted.venue_order_id, VenueOrderId::new("9001"));

    h.venue.push(&row(
        "O-1",
        OID,
        "PartialFilled",
        "0.0010",
        Some("64990.0"),
        "0.0195",
    ));
    let OrderEventAny::Filled(first) = h.order_event().await else {
        panic!("expected OrderFilled");
    };
    assert_eq!(first.last_qty, Quantity::from("0.0010"));
    assert_eq!(first.last_px, Price::from("64990.0"));
    assert_eq!(first.commission, Some(Money::from("0.0195 USDC")));
    assert_eq!(first.liquidity_side, LiquiditySide::NoLiquiditySide);

    // 0.0010 @ 64990.0 and 0.0020 @ 64990.6 average to 64990.4
    let filled = row("O-1", OID, "Filled", "0.0030", Some("64990.4"), "0.0585");
    h.venue.push(&filled);
    let OrderEventAny::Filled(second) = h.order_event().await else {
        panic!("expected OrderFilled");
    };
    assert_eq!(second.last_qty, Quantity::from("0.0020"));
    assert_eq!(second.last_px, Price::from("64990.6"));
    assert_eq!(second.commission, Some(Money::from("0.0390 USDC")));
    assert_ne!(first.trade_id, second.trade_id);
    assert_eq!(
        h.cache
            .borrow()
            .order(&order.client_order_id())
            .unwrap()
            .status(),
        OrderStatus::Filled
    );

    // The same row again gives nothing: the next event is the report of another order
    h.venue.push(&filled);
    h.venue
        .push(&row("other-session", 9002, "Open", "0", None, "0"));
    let report = h.order_report().await;
    assert_eq!(report.venue_order_id, VenueOrderId::new("9002"));
    assert_eq!(report.order_status, OrderStatus::Accepted);
}

#[tokio::test(flavor = "multi_thread")]
async fn venue_refusal_rejects_the_order() {
    let mut h = Harness::connect().await;

    h.venue
        .reply(json!({"status": "err", "code": 10000, "message": "Insufficient margin"}));
    h.submit(&limit("O-1").build());
    assert!(matches!(h.order_event().await, OrderEventAny::Submitted(_)));
    let OrderEventAny::Rejected(rejected) = h.order_event().await else {
        panic!("expected OrderRejected");
    };
    assert!(
        rejected.reason.contains("Insufficient margin"),
        "{}",
        rejected.reason
    );
    assert!(!rejected.due_post_only);

    // A failed execution comes as an error entry of `statuses`
    h.venue.reply(
        json!({"status": "ok", "code": 0, "data": {"type": "PlaceOrder", "data": {
            "statuses": [{"error": "Post only order would have matched"}],
        }}}),
    );
    h.submit(&limit("O-2").post_only(true).build());
    assert!(matches!(h.order_event().await, OrderEventAny::Submitted(_)));
    let OrderEventAny::Rejected(rejected) = h.order_event().await else {
        panic!("expected OrderRejected");
    };
    assert!(rejected.reason.contains("Post only"), "{}", rejected.reason);
    assert!(rejected.due_post_only);

    // A late `Rejected` row of the stream adds no second event
    h.venue.push(&row("O-2", OID, "Rejected", "0", None, "0"));
    h.venue
        .push(&row("other-session", 9002, "Open", "0", None, "0"));
    assert_eq!(
        h.order_report().await.venue_order_id,
        VenueOrderId::new("9002")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn cancel_goes_by_oid_once_known_and_by_cloid_before() {
    let mut h = Harness::connect().await;
    let order = h.submit_accepted("O-1", OID).await;

    h.cancel(&order);
    let posts = h.venue.wait_exchange_posts(2).await;
    let by_oid = Action::Cancel(CancelAction {
        cancels: vec![CancelBody {
            a: 1,
            o: OID,
            grouping: None,
        }],
    });
    h.assert_signed(&posts[1], &by_oid);
    assert_eq!(
        posts[1]["action"],
        json!({"type": "cancel", "cancels": [{"a": 1, "o": 9001}]})
    );
    assert!(posts[1]["nonce"].as_u64() > posts[0]["nonce"].as_u64());

    // The row of the stream closes the order, not the answer
    h.venue.push(&row("O-1", OID, "Canceled", "0", None, "0"));
    let OrderEventAny::Canceled(canceled) = h.order_event().await else {
        panic!("expected OrderCanceled");
    };
    assert_eq!(canceled.venue_order_id, Some(VenueOrderId::new("9001")));

    // No row yet, so no venue order id: the cancel names the client order id
    let pending = limit("O-2").build();
    h.submit(&pending);
    assert!(matches!(h.order_event().await, OrderEventAny::Submitted(_)));
    h.venue.wait_exchange_posts(3).await;
    h.venue
        .reply(json!({"status": "err", "code": 10000, "message": "Order not found"}));
    h.cancel(&pending);
    let posts = h.venue.wait_exchange_posts(4).await;
    let by_cloid = Action::CancelByCloid(CancelByCloidAction {
        cancels: vec![CancelByCloidBody {
            asset: 1,
            cloid: "O-2".to_string(),
            grouping: None,
        }],
    });
    h.assert_signed(&posts[3], &by_cloid);
    let OrderEventAny::CancelRejected(refused) = h.order_event().await else {
        panic!("expected OrderCancelRejected");
    };
    assert!(
        refused.reason.contains("Order not found"),
        "{}",
        refused.reason
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn cancel_all_sends_the_open_orders_of_the_cache_in_one_action() {
    let mut h = Harness::connect().await;
    let first = h.submit_accepted("O-1", OID).await;
    h.submit_accepted("O-2", 9002).await;

    let cmd = CancelAllOrders::new(
        trader_id(),
        Some(client_id()),
        first.strategy_id(),
        btc(),
        None,
        UUID4::new(),
        UnixNanos::default(),
        None,
        None,
    );
    h.client.cancel_all_orders(cmd).unwrap();

    let posts = h.venue.wait_exchange_posts(3).await;
    assert_eq!(posts[2]["action"]["type"], "cancel");
    let mut oids: Vec<u64> = posts[2]["action"]["cancels"]
        .as_array()
        .unwrap()
        .iter()
        .map(|cancel| cancel["o"].as_u64().unwrap())
        .collect();
    oids.sort_unstable();
    assert_eq!(oids, [OID, 9002]);
}

#[tokio::test(flavor = "multi_thread")]
async fn modify_amends_price_in_place() {
    let mut h = Harness::connect().await;

    // Before the stream showed the order there is no venue order id to amend
    let pending = limit("O-0").build();
    h.submit(&pending);
    assert!(matches!(h.order_event().await, OrderEventAny::Submitted(_)));
    h.venue.wait_exchange_posts(1).await;
    h.modify(&pending, "64000.0");
    let OrderEventAny::ModifyRejected(refused) = h.order_event().await else {
        panic!("expected OrderModifyRejected");
    };
    assert!(
        refused.reason.contains("no venue order id"),
        "{}",
        refused.reason
    );

    let order = h.submit_accepted("O-1", OID).await;
    h.modify(&order, "64000.0");
    let posts = h.venue.wait_exchange_posts(3).await;
    let action = Action::Modify2(ModifyAction {
        order: ModifyBody {
            o: OID,
            p: "64000.0".to_string(),
            s: "0.0030".to_string(),
        },
    });
    h.assert_signed(&posts[2], &action);
    assert_eq!(
        posts[2]["action"],
        json!({"type": "modify2", "order": {"o": 9001, "p": "64000.0", "s": "0.0030"}})
    );
    let OrderEventAny::Updated(updated) = h.order_event().await else {
        panic!("expected OrderUpdated");
    };
    assert_eq!(updated.price, Some(Price::from("64000.0")));
    assert_eq!(updated.quantity, Quantity::from("0.0030"));
    assert_eq!(updated.venue_order_id, Some(VenueOrderId::new("9001")));
}

#[tokio::test(flavor = "multi_thread")]
async fn orders_the_venue_cannot_take_are_denied_without_a_request() {
    let mut h = Harness::connect().await;

    let gtd = limit("O-1")
        .time_in_force(TimeInForce::Gtd)
        .expire_time(UnixNanos::from(1_900_000_000_000_000_000))
        .build();
    let off_tick = limit("O-2").price(Price::from("65000.05")).build();
    let mut market = OrderTestBuilder::new(OrderType::Market);
    market
        .instrument_id(btc())
        .side(OrderSide::Buy)
        .quantity(Quantity::from("0.0030"));
    // No quote in the cache: a market order has no price cap
    let unpriced = market.client_order_id(ClientOrderId::from("O-3")).build();
    for order in [gtd, off_tick, unpriced] {
        h.submit(&order);
        let event = h.order_event().await;
        assert!(matches!(event, OrderEventAny::Denied(_)), "{event:?}");
    }
    assert!(h.venue.exchange_posts().is_empty());

    let quote = QuoteTick::new(
        btc(),
        Price::from("64999.0"),
        Price::from("65000.0"),
        Quantity::from("1.0000"),
        Quantity::from("1.0000"),
        UnixNanos::default(),
        UnixNanos::default(),
    );
    h.cache.borrow_mut().add_quote(quote).unwrap();
    h.submit(&market.client_order_id(ClientOrderId::from("O-4")).build());
    assert!(matches!(h.order_event().await, OrderEventAny::Submitted(_)));
    let posts = h.venue.wait_exchange_posts(1).await;
    let sent = &posts[0]["action"]["orders"][0];
    assert_eq!(sent["t"], json!({"limit": {"tif": "FrontendMarket"}}));
    // 50 bps past the ask, on the tick
    assert_eq!(sent["p"], "65325.0");
}

#[tokio::test(flavor = "multi_thread")]
async fn trigger_order_is_accepted_from_the_trigger_list_and_canceled_by_the_answer() {
    let mut h = Harness::connect().await;
    let stop = OrderTestBuilder::new(OrderType::StopMarket)
        .instrument_id(btc())
        .client_order_id(ClientOrderId::from("stop-001"))
        .side(OrderSide::Sell)
        .quantity(Quantity::from("0.0030"))
        .trigger_price(Price::from("60000.0"))
        .trigger_type(TriggerType::MarkPrice)
        .reduce_only(true)
        .build();
    h.submit(&stop);
    assert!(matches!(h.order_event().await, OrderEventAny::Submitted(_)));

    let posts = h.venue.wait_exchange_posts(1).await;
    let action = &posts[0]["action"];
    assert_eq!(action["grouping"], "normalTrigger");
    let sent = &action["orders"][0];
    assert_eq!(sent["t"]["trigger"]["isMarket"], true);
    assert_eq!(sent["t"]["trigger"]["triggerPx"], "60000.0");
    assert_eq!(sent["t"]["trigger"]["tpsl"], "sl");
    assert_eq!(
        (&sent["b"], &sent["r"], &sent["c"]),
        (&json!(false), &json!(true), &json!("stop-001"))
    );

    // The order stream may not show trigger orders: the list gives the venue order id
    let OrderEventAny::Accepted(accepted) = h.order_event().await else {
        panic!("expected OrderAccepted");
    };
    assert_eq!(accepted.venue_order_id, VenueOrderId::new("211558908001"));
    assert_eq!(
        h.venue.info_requests("triggerOrders"),
        vec![json!({"type": "triggerOrders", "user": USER})]
    );

    h.cancel(&stop);
    let posts = h.venue.wait_exchange_posts(2).await;
    assert_eq!(
        posts[1]["action"],
        json!({"type": "cancel", "cancels": [{"a": 1, "o": 211_558_908_001_u64, "grouping": "trigger"}]})
    );
    assert!(matches!(h.order_event().await, OrderEventAny::Canceled(_)));
}

#[tokio::test(flavor = "multi_thread")]
async fn reports_come_from_the_user_endpoints() {
    let h = Harness::connect().await;
    let ts = UnixNanos::default();

    let open =
        GenerateOrderStatusReports::new(UUID4::new(), ts, true, None, None, None, None, None);
    let open = h.client.generate_order_status_reports(&open).await.unwrap();
    let ids = |reports: &[nautilus_model::reports::OrderStatusReport]| -> Vec<String> {
        reports
            .iter()
            .map(|report| report.venue_order_id.to_string())
            .collect()
    };
    assert_eq!(ids(&open), ["211558907950", "211558908001"]);
    assert_eq!(open[0].order_status, OrderStatus::Accepted);
    assert_eq!(
        open[0].client_order_id,
        Some(ClientOrderId::from("my-order-001"))
    );
    assert_eq!(open[0].price, Some(Price::from("66539.0")));
    // The trigger list has no readable state: a listed order is open
    assert_eq!(open[1].order_status, OrderStatus::Accepted);
    assert_eq!(open[1].order_type, OrderType::StopMarket);
    assert_eq!(open[1].order_side, Some(OrderSide::Sell));
    assert_eq!(open[1].trigger_price, Some(Price::from("60000.0")));
    assert!(open[1].reduce_only);

    // With history: the open order is in both lists and reported once
    let all =
        GenerateOrderStatusReports::new(UUID4::new(), ts, false, None, None, None, None, None);
    let all = h.client.generate_order_status_reports(&all).await.unwrap();
    assert_eq!(
        ids(&all),
        [
            "211558907950",
            "211558908001",
            "211558900001",
            "211558900002"
        ]
    );
    let filled = &all[2];
    assert_eq!(filled.order_status, OrderStatus::Filled);
    assert_eq!(filled.quantity, Quantity::from("0.0030"));
    assert_eq!(filled.filled_qty, Quantity::from("0.0030"));
    assert_eq!(filled.avg_px.unwrap().to_string(), "64990.5");
    assert_eq!(all[3].order_status, OrderStatus::Canceled);
    assert_eq!(all[3].client_order_id, None);

    let one = GenerateOrderStatusReport::new(
        UUID4::new(),
        ts,
        None,
        None,
        Some(VenueOrderId::new("211558900001")),
        None,
        None,
    );
    let one = h
        .client
        .generate_order_status_report(&one)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        one.client_order_id,
        Some(ClientOrderId::from("hist-filled-1"))
    );
    assert_eq!(one.order_status, OrderStatus::Filled);
    assert_eq!(
        h.venue.info_requests("orderDetails"),
        vec![json!({"type": "orderDetails", "orderId": 211_558_900_001_u64})]
    );

    let fills = GenerateFillReports::new(UUID4::new(), ts, None, None, None, None, None, None);
    let fills = h.client.generate_fill_reports(fills).await.unwrap();
    assert_eq!(fills.len(), 2);
    let total = fills[0].last_qty + fills[1].last_qty;
    assert_eq!(total, Quantity::from("0.0030"));
    assert_ne!(fills[0].trade_id, fills[1].trade_id);
    let taker = fills
        .iter()
        .find(|fill| fill.liquidity_side == LiquiditySide::Taker)
        .expect("one taker fill");
    assert_eq!(taker.last_px, Price::from("64990.0"));
    assert_eq!(taker.commission, Money::from("0.0195 USDC"));
    assert_eq!(taker.venue_order_id, VenueOrderId::new("211558900001"));

    let positions =
        GeneratePositionStatusReports::new(UUID4::new(), ts, None, None, None, None, None);
    let positions = h
        .client
        .generate_position_status_reports(&positions)
        .await
        .unwrap();
    assert_eq!(positions.len(), 1);
    assert_eq!(positions[0].instrument_id, btc());
    assert_eq!(positions[0].position_side, PositionSide::Long);
    assert_eq!(positions[0].quantity, Quantity::from("0.0030"));

    // The venue lists open positions only: an asked market without a row is flat
    let eth = InstrumentId::from("ETH-USDC-PERP.TXFLOW");
    let flat =
        GeneratePositionStatusReports::new(UUID4::new(), ts, Some(eth), None, None, None, None);
    let flat = h
        .client
        .generate_position_status_reports(&flat)
        .await
        .unwrap();
    assert_eq!(flat.len(), 1);
    assert_eq!(flat[0].position_side, PositionSide::Flat);

    let mass = h
        .client
        .generate_mass_status(Some(60))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(mass.order_reports().len(), 4);
    assert_eq!(mass.position_reports().len(), 1);

    // Reading reports signs nothing
    assert!(h.venue.exchange_posts().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn reconnect_resubscribes_and_catches_up_from_the_history() {
    let mut h = Harness::connect().await;
    // The order of the history fixture: it fills while the socket is down
    let order = OrderTestBuilder::new(OrderType::Limit)
        .instrument_id(btc())
        .client_order_id(ClientOrderId::from("hist-filled-1"))
        .side(OrderSide::Buy)
        .quantity(Quantity::from("0.0030"))
        .price(Price::from("65000.0"))
        .build();
    h.submit(&order);
    assert!(matches!(h.order_event().await, OrderEventAny::Submitted(_)));
    h.venue.wait_exchange_posts(1).await;

    h.venue.drop_connection.notify_one();
    h.venue.wait_subscriptions(4).await;

    let OrderEventAny::Accepted(accepted) = h.order_event().await else {
        panic!("expected OrderAccepted");
    };
    assert_eq!(accepted.venue_order_id, VenueOrderId::new("211558900001"));
    let OrderEventAny::Filled(filled) = h.order_event().await else {
        panic!("expected OrderFilled");
    };
    assert_eq!(filled.last_qty, Quantity::from("0.0030"));
    assert_eq!(filled.last_px, Price::from("64990.5"));
    assert_eq!(filled.commission, Some(Money::from("0.0585 USDC")));

    // Orders of other sessions go to the engine as reports
    assert_eq!(h.order_report().await.order_status, OrderStatus::Canceled);
    assert_eq!(h.order_report().await.order_status, OrderStatus::Accepted);
    assert!(matches!(
        next_event(&mut h.events).await,
        ExecutionEvent::Account(_)
    ));

    let history = h.venue.info_requests("historicalOrders");
    assert_eq!(history.len(), 1);
    assert_eq!(history[0]["user"], USER);
    assert!(history[0]["startTimeMs"].as_u64().is_some());
}
