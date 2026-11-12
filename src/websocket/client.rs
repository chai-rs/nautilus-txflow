//! WebSocket client for the two TxFlow streams.

use std::{fmt::Debug, sync::Arc};

use nautilus_common::live::get_runtime;
use nautilus_network::{
    RECONNECTED,
    websocket::{WebSocketClient, WebSocketConfig, channel_message_handler},
};
use serde_json::{Value, json};
use tokio::{sync::mpsc, task::JoinHandle};
use tokio_tungstenite::tungstenite::Message;

use super::{
    error::TxflowWsError,
    messages::{WsMessage, parse_frame},
};
use crate::common::consts::{WS_PING, WS_PING_SECS};

const RECONNECT_DELAY_INITIAL_MS: u64 = 250;
const RECONNECT_DELAY_MAX_MS: u64 = 30_000;
const RECONNECT_BACKOFF_FACTOR: f64 = 2.0;
const RECONNECT_JITTER_MS: u64 = 200;

/// The subscription objects the caller wants, sent again after every reconnect: the venue
/// forgets them when a connection ends.
type Subscriptions = Arc<parking_lot::Mutex<Vec<Value>>>;

/// WebSocket client for `/ws` (node stream) and `/api-ws` (API-service stream).
///
/// No authentication: user channels take the account address in the subscription.
/// Subscribing before [`Self::connect`] only records the subscription. A subscribe whose send
/// fails stays recorded and is sent on the next reconnect.
pub struct TxflowWebSocketClient {
    url: String,
    /// `/ws` routes on a top-level `type` next to `subscription`; `/api-ws` does not use it.
    top_level_type: bool,
    subscriptions: Subscriptions,
    client: Option<Arc<WebSocketClient>>,
    task: Option<JoinHandle<()>>,
}

impl Debug for TxflowWebSocketClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct(stringify!(TxflowWebSocketClient))
            .field("url", &self.url)
            .field("active", &self.is_active())
            .finish_non_exhaustive()
    }
}

impl TxflowWebSocketClient {
    /// Client for the node stream, `wss://<host>/ws`.
    #[must_use]
    pub fn node(url: String) -> Self {
        Self::new(url, true)
    }

    /// Client for the API-service stream, `wss://<host>/api-ws`.
    #[must_use]
    pub fn api(url: String) -> Self {
        Self::new(url, false)
    }

    fn new(url: String, top_level_type: bool) -> Self {
        Self {
            url,
            top_level_type,
            subscriptions: Subscriptions::default(),
            client: None,
            task: None,
        }
    }

    /// Connects and returns the stream of inbound frames.
    ///
    /// After each automatic reconnect the stream yields [`WsMessage::Reconnected`] and every
    /// recorded subscription is sent again.
    ///
    /// # Errors
    ///
    /// Returns an error if the URL is invalid or the connection cannot be established.
    pub async fn connect(&mut self) -> Result<mpsc::UnboundedReceiver<WsMessage>, TxflowWsError> {
        self.disconnect().await;

        let config = WebSocketConfig::builder()
            .url(self.url.clone())
            .heartbeat_interval_secs(WS_PING_SECS)
            .heartbeat_payload(WS_PING.to_string())
            .reconnect_delay_initial_ms(RECONNECT_DELAY_INITIAL_MS)
            .reconnect_delay_max_ms(RECONNECT_DELAY_MAX_MS)
            .reconnect_backoff_factor(RECONNECT_BACKOFF_FACTOR)
            .reconnect_jitter_ms(RECONNECT_JITTER_MS)
            .build()
            .map_err(|e| TxflowWsError::Client(e.to_string()))?;
        let (handler, raw_rx) = channel_message_handler();
        let client = WebSocketClient::builder()
            .config(config)
            .message_handler(handler)
            .connect()
            .await
            .map_err(|e| TxflowWsError::Network(e.to_string()))?;
        let client = Arc::new(client);

        // Subscriptions recorded before connecting. The read task starts after them so a
        // reconnect cannot interleave with this first send.
        if let Err(e) = send_all(&client, &self.subscriptions, self.top_level_type).await {
            client.disconnect().await;
            return Err(e);
        }
        let (out_tx, out_rx) = mpsc::unbounded_channel();
        self.client = Some(client.clone());
        self.task = Some(get_runtime().spawn(read_loop(
            raw_rx,
            out_tx,
            client,
            self.subscriptions.clone(),
            self.top_level_type,
        )));

        Ok(out_rx)
    }

    /// Closes the connection. Recorded subscriptions are kept for the next [`Self::connect`].
    pub async fn disconnect(&mut self) {
        if let Some(client) = self.client.take() {
            client.disconnect().await;
        }
        // The read task holds the client, which holds the task's input sender, so the task
        // never ends on its own.
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }

    /// Forgets every recorded subscription.
    pub fn clear_subscriptions(&self) {
        self.subscriptions.lock().clear();
    }

    /// Whether the connection is up (false while reconnecting).
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.client.as_ref().is_some_and(|c| c.is_active())
    }

    /// Records `subscription` (for example `{"type":"bbo","coin":"1"}`) and sends it if
    /// connected.
    ///
    /// # Errors
    ///
    /// Returns an error if the frame cannot be written.
    pub async fn subscribe(&self, subscription: Value) -> Result<(), TxflowWsError> {
        let text = frame("subscribe", &subscription, self.top_level_type);
        {
            let mut subscriptions = self.subscriptions.lock();
            if !subscriptions.contains(&subscription) {
                subscriptions.push(subscription);
            }
        }
        self.send(text).await
    }

    /// Forgets `subscription` and sends the unsubscribe frame if connected.
    ///
    /// # Errors
    ///
    /// Returns an error if the frame cannot be written.
    pub async fn unsubscribe(&self, subscription: &Value) -> Result<(), TxflowWsError> {
        self.subscriptions.lock().retain(|s| s != subscription);
        self.send(frame("unsubscribe", subscription, self.top_level_type))
            .await
    }

    async fn send(&self, text: String) -> Result<(), TxflowWsError> {
        match &self.client {
            Some(client) => Ok(client.send_text(text, None).await?),
            None => Ok(()),
        }
    }
}

fn frame(method: &str, subscription: &Value, top_level_type: bool) -> String {
    let mut frame = json!({"method": method, "subscription": subscription});
    if top_level_type {
        frame["type"] = subscription["type"].clone();
    }
    frame.to_string()
}

async fn send_all(
    client: &WebSocketClient,
    subscriptions: &Subscriptions,
    top_level_type: bool,
) -> Result<(), TxflowWsError> {
    let wanted = subscriptions.lock().clone();
    for subscription in &wanted {
        client
            .send_text(frame("subscribe", subscription, top_level_type), None)
            .await?;
    }
    Ok(())
}

async fn read_loop(
    mut raw_rx: mpsc::UnboundedReceiver<Message>,
    out_tx: mpsc::UnboundedSender<WsMessage>,
    client: Arc<WebSocketClient>,
    subscriptions: Subscriptions,
    top_level_type: bool,
) {
    while let Some(message) = raw_rx.recv().await {
        let Message::Text(text) = message else {
            continue;
        };
        if text.as_str() == RECONNECTED {
            // Marker first: the consumer must drop its books before the new snapshots.
            if out_tx.send(WsMessage::Reconnected).is_err() {
                return;
            }
            // A failure here means the connection dropped again; the next reconnect retries.
            if let Err(e) = send_all(&client, &subscriptions, top_level_type).await {
                log::warn!("TxFlow resubscribe failed: {e}");
            }
            continue;
        }
        match parse_frame(&text) {
            Ok(Some(message)) => {
                if out_tx.send(message).is_err() {
                    return;
                }
            }
            Ok(None) => {}
            Err(e) => log::warn!("TxFlow frame not understood: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::atomic::{AtomicUsize, Ordering},
        time::Duration,
    };

    use axum::{
        Router,
        extract::{
            State,
            ws::{Message as AxumMessage, WebSocket, WebSocketUpgrade},
        },
        response::Response,
        routing::get,
    };
    use tokio::sync::Notify;

    use super::*;

    const BBO: &str = r#"{"channel":"bbo","data":{"coin":"1","time":1794336500000,"bbo":[{"px":"73381.2","sz":"0.6110","n":2},{"px":"73381.3","sz":"0.1","n":1}]}}"#;

    /// A venue stand-in: answers a `bbo` subscribe with one frame, a ping with a pong, and
    /// drops the connection on demand.
    #[derive(Clone, Default)]
    struct MockVenue {
        connections: Arc<AtomicUsize>,
        /// (connection number, frame) for every text frame received.
        received: Arc<parking_lot::Mutex<Vec<(usize, Value)>>>,
        drop_connection: Arc<Notify>,
    }

    impl MockVenue {
        async fn start() -> (Self, String) {
            let venue = Self::default();
            let app = Router::new()
                .route(
                    "/ws",
                    get(
                        |ws: WebSocketUpgrade, State(venue): State<Self>| async move {
                            let response: Response = ws.on_upgrade(|socket| venue.serve(socket));
                            response
                        },
                    ),
                )
                .with_state(venue.clone());
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("ws://{}/ws", listener.local_addr().unwrap());
            tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            (venue, url)
        }

        async fn serve(self, mut socket: WebSocket) {
            let connection = self.connections.fetch_add(1, Ordering::SeqCst) + 1;
            loop {
                let message = tokio::select! {
                    () = self.drop_connection.notified() => return,
                    message = socket.recv() => message,
                };
                match message {
                    Some(Ok(AxumMessage::Text(text))) => {
                        let frame: Value = serde_json::from_str(&text).unwrap();
                        self.received.lock().push((connection, frame.clone()));
                        let reply = if frame["method"] == "ping" {
                            r#"{"method":"PONG"}"#
                        } else if frame["method"] == "subscribe"
                            && frame["subscription"]["type"] == "bbo"
                        {
                            BBO
                        } else {
                            continue;
                        };
                        socket.send(AxumMessage::Text(reply.into())).await.unwrap();
                    }
                    Some(Ok(_)) => {}
                    _ => return,
                }
            }
        }

        /// Subscribe and unsubscribe frames received on one connection.
        fn requests(&self, connection: usize) -> Vec<Value> {
            self.received
                .lock()
                .iter()
                .filter(|(c, frame)| *c == connection && frame["method"] != "ping")
                .map(|(_, frame)| frame.clone())
                .collect()
        }
    }

    async fn next(rx: &mut mpsc::UnboundedReceiver<WsMessage>) -> WsMessage {
        tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("timed out waiting for a frame")
            .expect("stream closed")
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

    #[test]
    fn frame_has_top_level_type_on_the_node_stream_only() {
        let subscription = json!({"type": "l2BookDepth", "coin": "4"});
        let node: Value = serde_json::from_str(&frame("subscribe", &subscription, true)).unwrap();
        assert_eq!(
            node,
            json!({"method": "subscribe", "type": "l2BookDepth", "subscription": {"type": "l2BookDepth", "coin": "4"}})
        );
        let api: Value = serde_json::from_str(&frame("unsubscribe", &subscription, false)).unwrap();
        assert_eq!(
            api,
            json!({"method": "unsubscribe", "subscription": {"type": "l2BookDepth", "coin": "4"}})
        );
    }

    #[tokio::test]
    async fn subscribes_receives_and_unsubscribes() {
        let (venue, url) = MockVenue::start().await;
        let mut client = TxflowWebSocketClient::node(url);
        assert!(!client.is_active());
        // Recorded before the connection exists, sent by connect
        client
            .subscribe(json!({"type": "trades", "coin": "1"}))
            .await
            .unwrap();
        let mut rx = client.connect().await.unwrap();
        assert!(client.is_active());

        let bbo = json!({"type": "bbo", "coin": "1"});
        client.subscribe(bbo.clone()).await.unwrap();
        assert!(matches!(next(&mut rx).await, WsMessage::Bbo(_)));
        client.unsubscribe(&bbo).await.unwrap();

        wait_until("three requests", || venue.requests(1).len() == 3).await;
        let requests = venue.requests(1);
        assert_eq!(requests[0]["type"], "trades");
        assert_eq!(requests[1]["method"], "subscribe");
        assert_eq!(requests[2]["method"], "unsubscribe");
        assert_eq!(requests[2]["subscription"], bbo);

        client.disconnect().await;
        assert!(!client.is_active());
    }

    #[tokio::test]
    async fn reconnect_sends_the_marker_and_every_subscription_again() {
        let (venue, url) = MockVenue::start().await;
        let mut client = TxflowWebSocketClient::node(url);
        let mut rx = client.connect().await.unwrap();
        client
            .subscribe(json!({"type": "bbo", "coin": "1"}))
            .await
            .unwrap();
        client
            .subscribe(json!({"type": "trades", "coin": "1"}))
            .await
            .unwrap();
        client
            .unsubscribe(&json!({"type": "trades", "coin": "1"}))
            .await
            .unwrap();
        assert!(matches!(next(&mut rx).await, WsMessage::Bbo(_)));
        wait_until("requests on connection 1", || venue.requests(1).len() == 3).await;

        venue.drop_connection.notify_waiters();
        loop {
            if matches!(next(&mut rx).await, WsMessage::Reconnected) {
                break;
            }
        }
        // Only the subscription still wanted comes back
        assert!(matches!(next(&mut rx).await, WsMessage::Bbo(_)));
        let requests = venue.requests(2);
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0]["subscription"]["type"], "bbo");
        client.disconnect().await;
    }

    #[tokio::test]
    async fn connect_fails_without_a_server() {
        let mut client = TxflowWebSocketClient::api("ws://127.0.0.1:9/api-ws".to_string());
        assert!(client.connect().await.is_err());
    }
}
