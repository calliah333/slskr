//! Server side of the ASP.NET Core SignalR JSON hub protocol (negotiate + WebSockets).
//!
//! Compatible with stock `@microsoft/signalr` JavaScript clients:
//! `POST {base}/negotiate?negotiateVersion=1`, then a WebSocket at `{base}?id=<connectionToken>`.

pub mod protocol;

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, RwLock};
use std::time::Duration;

use axum::Router;
use axum::extract::ws::{Message, Utf8Bytes, WebSocket, WebSocketUpgrade};
use axum::extract::{FromRequestParts, Query, Request};
use axum::http::StatusCode;
use axum::http::request::Parts;
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use futures::future::BoxFuture;
use futures::stream::{FuturesUnordered, SplitSink, SplitStream};
use futures::{SinkExt, StreamExt};
use serde::Serialize;
use serde_json::Value;
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;
use tokio::time::{Instant, MissedTickBehavior};
use tracing::{debug, warn};

use protocol::{HubMessage, RecordBuffer};

/// Interval between server keep-alive pings.
pub const KEEP_ALIVE_INTERVAL: Duration = Duration::from_secs(15);
/// A client that sends nothing for this long is disconnected.
pub const CLIENT_TIMEOUT: Duration = Duration::from_secs(30);
/// Time allowed for the client to complete the handshake.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);
/// Per-connection broadcast queue length; clients that fall behind are dropped.
pub const CONNECTION_BUFFER: usize = 1024;
/// How long a negotiated connection token stays valid without a WebSocket connect.
const PENDING_TTL: Duration = Duration::from_secs(120);
/// Largest client WebSocket message or frame; see `protocol::MAX_BUFFERED` for sizing.
const MAX_WEBSOCKET_MESSAGE: usize = 2 * protocol::MAX_BUFFERED;

type InitialMessages = Vec<(String, Vec<Value>)>;
type OnConnected = dyn Fn() -> InitialMessages + Send + Sync;
type OnInvoke =
    dyn Fn(String, Vec<Value>) -> BoxFuture<'static, Result<Value, String>> + Send + Sync;
type Authorizer = dyn Fn(&Parts) -> bool + Send + Sync;

/// A SignalR hub: tracks connected clients and broadcasts invocations to them.
#[derive(Clone)]
pub struct Hub {
    inner: Arc<Inner>,
}

struct Inner {
    name: &'static str,
    connections: Mutex<HashMap<Arc<str>, mpsc::Sender<Utf8Bytes>>>,
    registry: Mutex<Registry>,
    on_connected: RwLock<Option<Arc<OnConnected>>>,
    on_invoke: RwLock<Option<Arc<OnInvoke>>>,
    authorizer: RwLock<Option<Arc<Authorizer>>>,
}

#[derive(Default)]
struct Registry {
    // Both negotiated tokens and connection IDs stay reserved through upgrade and teardown.
    reserved: HashSet<Arc<str>>,
    pending: HashMap<Arc<str>, Pending>,
}

impl Registry {
    fn reserve(&mut self, mut next: impl FnMut() -> Arc<str>) -> Arc<str> {
        loop {
            let id = next();
            if self.reserved.insert(id.clone()) {
                return id;
            }
        }
    }

    fn prune_pending(&mut self) {
        self.pending.retain(|token, pending| {
            if pending.created.elapsed() < PENDING_TTL {
                return true;
            }
            self.reserved.remove(token);
            self.reserved.remove(&pending.connection_id);
            false
        });
    }
}

struct Pending {
    connection_id: Arc<str>,
    created: Instant,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn read<T: Clone>(lock: &RwLock<Option<T>>) -> Option<T> {
    lock.read().unwrap_or_else(PoisonError::into_inner).clone()
}

fn write<T>(lock: &RwLock<Option<T>>, value: T) {
    let previous = lock
        .write()
        .unwrap_or_else(PoisonError::into_inner)
        .replace(value);
    drop(previous);
}

fn random_id() -> Arc<str> {
    let mut encoded = [0; 22];
    let len = URL_SAFE_NO_PAD
        .encode_slice(rand::random::<[u8; 16]>(), &mut encoded)
        .expect("room for an unpadded 128-bit ID");
    Arc::from(std::str::from_utf8(&encoded[..len]).expect("ASCII base64"))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct NegotiateResponse<'a> {
    negotiate_version: u32,
    connection_id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    connection_token: Option<&'a str>,
    available_transports: [Transport; 1],
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Transport {
    transport: &'static str,
    transfer_formats: [&'static str; 2],
}

/// Reserves an ID even before the socket completes its handshake.
struct Reservation {
    hub: Arc<Inner>,
    connection_id: Arc<str>,
    token: Option<Arc<str>>,
}

impl Drop for Reservation {
    fn drop(&mut self) {
        let mut registry = lock(&self.hub.registry);
        registry.reserved.remove(&self.connection_id);
        if let Some(token) = &self.token {
            registry.reserved.remove(token);
        }
    }
}

/// Removes the live route when the connection ends.
struct Registration {
    reservation: Reservation,
}

impl Drop for Registration {
    fn drop(&mut self) {
        lock(&self.reservation.hub.connections).remove(&self.reservation.connection_id);
    }
}

type Sink = SplitSink<WebSocket, Message>;

enum Close {
    Silent,
    Error(String),
    SlowClient,
}

impl Hub {
    /// Creates a hub; `name` is used for logging only.
    pub fn new(name: &'static str) -> Self {
        Self {
            inner: Arc::new(Inner {
                name,
                connections: Mutex::new(HashMap::new()),
                registry: Mutex::new(Registry::default()),
                on_connected: RwLock::new(None),
                on_invoke: RwLock::new(None),
                authorizer: RwLock::new(None),
            }),
        }
    }

    /// Hub name.
    pub fn name(&self) -> &'static str {
        self.inner.name
    }

    /// Sends `target(args...)` to every connected client without blocking.
    /// Clients whose queue is full are disconnected.
    pub fn broadcast(&self, target: &str, args: Vec<Value>) {
        if self.connection_count() == 0 {
            return;
        }

        let frame = Utf8Bytes::from(protocol::encode_invocation(target, &args));
        let name = self.inner.name;

        lock(&self.inner.connections).retain(|id, tx| match tx.try_send(frame.clone()) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) => {
                warn!(hub = name, connection = %id, "dropping slow SignalR client");
                false
            }
            Err(TrySendError::Closed(_)) => false,
        });
    }

    /// Registers messages sent only to a newly connected client right after the handshake.
    pub fn on_connected(&self, f: impl Fn() -> Vec<(String, Vec<Value>)> + Send + Sync + 'static) {
        write(&self.inner.on_connected, Arc::new(f) as Arc<OnConnected>);
    }

    /// Registers the handler for client-to-server invocations.
    pub fn on_invoke(
        &self,
        f: impl Fn(String, Vec<Value>) -> BoxFuture<'static, Result<Value, String>>
        + Send
        + Sync
        + 'static,
    ) {
        write(&self.inner.on_invoke, Arc::new(f) as Arc<OnInvoke>);
    }

    /// Registers the authorizer checked on negotiate and WebSocket upgrade (401 when it returns false).
    pub fn set_authorizer(&self, f: impl Fn(&Parts) -> bool + Send + Sync + 'static) {
        write(&self.inner.authorizer, Arc::new(f) as Arc<Authorizer>);
    }

    /// Number of connected clients that completed the handshake.
    pub fn connection_count(&self) -> usize {
        lock(&self.inner.connections).len()
    }

    /// Routes `/negotiate` and the WebSocket endpoint at `/`; nest under the hub path.
    pub fn router<S: Clone + Send + Sync + 'static>(&self) -> Router<S> {
        let negotiate = self.clone();
        let connect = self.clone();

        Router::<S>::new()
            .route(
                "/negotiate",
                post(move |req: Request| {
                    let hub = negotiate.clone();
                    async move { hub.negotiate(req) }
                }),
            )
            .route(
                "/",
                get(move |req: Request| {
                    let hub = connect.clone();
                    async move { hub.connect(req).await }
                }),
            )
    }

    fn authorized(&self, parts: &Parts) -> bool {
        read(&self.inner.authorizer).is_none_or(|authorize| (*authorize)(parts))
    }

    fn query(parts: &Parts) -> HashMap<String, String> {
        Query::<HashMap<String, String>>::try_from_uri(&parts.uri)
            .map(|q| q.0)
            .unwrap_or_default()
    }

    fn negotiate(&self, req: Request) -> Response {
        let (parts, _) = req.into_parts();

        if !self.authorized(&parts) {
            return StatusCode::UNAUTHORIZED.into_response();
        }

        let version = Self::query(&parts)
            .get("negotiateVersion")
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or(0)
            .min(1);

        let (connection_id, token) = {
            let mut registry = lock(&self.inner.registry);
            registry.prune_pending();
            let connection_id = registry.reserve(random_id);
            let token = if version >= 1 {
                registry.reserve(random_id)
            } else {
                connection_id.clone()
            };
            registry.pending.insert(
                token.clone(),
                Pending {
                    connection_id: connection_id.clone(),
                    created: Instant::now(),
                },
            );
            (connection_id, token)
        };

        Json(NegotiateResponse {
            negotiate_version: version,
            connection_id: &connection_id,
            connection_token: (version >= 1).then_some(token.as_ref()),
            available_transports: [Transport {
                transport: "WebSockets",
                transfer_formats: ["Text", "Binary"],
            }],
        })
        .into_response()
    }

    fn take_pending(&self, token: &str) -> Option<Reservation> {
        let (token, pending) = lock(&self.inner.registry).pending.remove_entry(token)?;
        let reservation = Reservation {
            hub: self.inner.clone(),
            token: (token != pending.connection_id).then_some(token),
            connection_id: pending.connection_id,
        };
        (pending.created.elapsed() < PENDING_TTL).then_some(reservation)
    }

    async fn connect(&self, req: Request) -> Response {
        let (mut parts, _) = req.into_parts();

        if !self.authorized(&parts) {
            return StatusCode::UNAUTHORIZED.into_response();
        }

        let reservation = match Self::query(&parts).get("id") {
            Some(token) => match self.take_pending(token) {
                Some(id) => id,
                None => {
                    return (StatusCode::NOT_FOUND, "No Connection with that ID").into_response();
                }
            },
            None => Reservation {
                hub: self.inner.clone(),
                connection_id: lock(&self.inner.registry).reserve(random_id),
                token: None,
            },
        };

        let upgrade = match WebSocketUpgrade::from_request_parts(&mut parts, &()).await {
            // axum's 64 MiB default would let one message far exceed the record cap; allow one
            // maximal record plus room for a few small records batched into the same message.
            Ok(upgrade) => upgrade
                .max_message_size(MAX_WEBSOCKET_MESSAGE)
                .max_frame_size(MAX_WEBSOCKET_MESSAGE),
            Err(rejection) => return rejection.into_response(),
        };

        let hub = self.clone();
        upgrade.on_upgrade(move |socket| hub.run(socket, reservation))
    }

    async fn run(self, socket: WebSocket, reservation: Reservation) {
        let connection_id = reservation.connection_id.clone();
        let name = self.inner.name;
        let (mut sink, mut stream) = socket.split();
        let mut buffer = RecordBuffer::new();

        let records =
            match tokio::time::timeout(HANDSHAKE_TIMEOUT, read_records(&mut stream, &mut buffer))
                .await
            {
                Ok(Ok(records)) => records,
                Ok(Err(Some(error))) => {
                    let _ =
                        send(&mut sink, protocol::encode_handshake_response(Some(&error))).await;
                    let _ = sink.close().await;
                    return;
                }
                Ok(Err(None)) => return,
                Err(_) => {
                    debug!(hub = name, connection = %connection_id, "SignalR handshake timed out");
                    let _ = sink.close().await;
                    return;
                }
            };

        let mut records = records.into_iter();
        let handshake = records
            .next()
            .ok_or_else(|| "Missing handshake request.".to_string())
            .and_then(|record| protocol::parse_handshake(&record))
            .and_then(|request| protocol::validate_handshake(&request));

        if let Err(error) = handshake {
            debug!(hub = name, connection = %connection_id, %error, "SignalR handshake rejected");
            let _ = send(&mut sink, protocol::encode_handshake_response(Some(&error))).await;
            let _ = sink.close().await;
            return;
        }

        if send(&mut sink, protocol::encode_handshake_response(None))
            .await
            .is_err()
        {
            return;
        }

        let (tx, mut rx) = mpsc::channel::<Utf8Bytes>(CONNECTION_BUFFER);
        lock(&self.inner.connections).insert(connection_id.clone(), tx);
        let registration = Registration { reservation };
        debug!(hub = name, connection = %connection_id, "SignalR client connected");

        let close = async {
            if let Some(on_connected) = read(&self.inner.on_connected) {
                if send_initial(&mut sink, on_connected()).await.is_err() {
                    return Close::Silent;
                }
            }

            // The invocation futures belong to this socket. Dropping them before the
            // registration prevents callbacks from continuing after disconnection cleanup.
            let mut in_flight = FuturesUnordered::new();
            let mut invoke = |id, target, args| self.invoke(id, target, args);
            if let Err(close) = self.handle_records(&mut sink, records, &mut in_flight, &mut invoke).await {
                return close;
            }

            let mut ping = tokio::time::interval_at(Instant::now() + KEEP_ALIVE_INTERVAL, KEEP_ALIVE_INTERVAL);
            ping.set_missed_tick_behavior(MissedTickBehavior::Delay);
            let mut last_received = Instant::now();

            loop {
                tokio::select! {
                    frame = stream.next() => {
                        let text = match &frame {
                            Some(Ok(Message::Text(text))) => text.as_str(),
                            Some(Ok(Message::Binary(bytes))) => match std::str::from_utf8(bytes) {
                                Ok(text) => text,
                                Err(_) => break Close::Error("Invalid UTF-8 payload.".into()),
                            },
                            Some(Ok(Message::Ping(_) | Message::Pong(_))) => {
                                last_received = Instant::now();
                                continue;
                            }
                            Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break Close::Silent,
                        };
                        last_received = Instant::now();

                        let records = match buffer.push(text) {
                            Ok(records) => records,
                            Err(error) => break Close::Error(error),
                        };
                        if let Err(close) = self.handle_records(&mut sink, records, &mut in_flight, &mut invoke).await {
                            break close;
                        }
                    }
                    message = rx.recv() => match message {
                        Some(frame) => {
                            if sink.send(Message::Text(frame)).await.is_err() {
                                break Close::Silent;
                            }
                        }
                        None => break Close::SlowClient,
                    },
                    Some(completion) = in_flight.next(), if !in_flight.is_empty() => {
                        if let Some(frame) = completion {
                            if send(&mut sink, frame).await.is_err() {
                                break Close::Silent;
                            }
                        }
                    }
                    _ = ping.tick() => {
                        if last_received.elapsed() > CLIENT_TIMEOUT {
                            debug!(hub = name, connection = %connection_id, "SignalR client timed out");
                            break Close::Silent;
                        }
                        if send(&mut sink, protocol::encode_ping()).await.is_err() {
                            break Close::Silent;
                        }
                    }
                }
            }
        }
        .await;

        drop(registration);
        let close_record = match close {
            Close::Silent => None,
            Close::Error(error) => Some(protocol::encode_close(Some(&error), false)),
            Close::SlowClient => Some(protocol::encode_close(
                Some("Server closed the connection: client too slow."),
                true,
            )),
        };
        if let Some(record) = close_record {
            let _ = send(&mut sink, record).await;
        }
        let _ = sink.close().await;
        debug!(hub = name, connection = %connection_id, "SignalR client disconnected");
    }

    async fn handle_records<F: Future<Output = Option<String>>>(
        &self,
        sink: &mut Sink,
        records: impl IntoIterator<Item = String>,
        in_flight: &mut FuturesUnordered<F>,
        mut invoke: impl FnMut(Option<String>, String, Vec<Value>) -> F,
    ) -> Result<(), Close> {
        for record in records {
            match protocol::parse_message(&record) {
                Ok(HubMessage::Invocation {
                    invocation_id,
                    target,
                    arguments,
                }) => {
                    in_flight.push(invoke(invocation_id, target, arguments));
                }
                Ok(HubMessage::StreamInvocation {
                    invocation_id,
                    target,
                    ..
                }) => {
                    let error = format!(
                        "Failed to invoke '{target}': streaming methods are not supported."
                    );
                    send(
                        sink,
                        protocol::encode_completion_error(&invocation_id, &error),
                    )
                    .await
                    .map_err(|_| Close::Silent)?;
                }
                Ok(HubMessage::Close { .. }) => return Err(Close::Silent),
                Ok(_) => {}
                Err(error) => {
                    debug!(hub = self.inner.name, %error, "invalid SignalR message");
                    return Err(Close::Error(format!(
                        "Connection closed with an error. {error}"
                    )));
                }
            }
        }
        Ok(())
    }

    fn invoke(
        &self,
        invocation_id: Option<String>,
        target: String,
        arguments: Vec<Value>,
    ) -> impl Future<Output = Option<String>> + Send + use<> {
        let handler = read(&self.inner.on_invoke);
        let name = self.inner.name;

        async move {
            let log_target = invocation_id.is_none().then(|| target.clone());
            let result = match handler {
                Some(handler) => handler(target, arguments).await,
                None => Err(format!(
                    "Failed to invoke '{target}' due to an error on the server. HubException: Method does not exist."
                )),
            };

            match (invocation_id, result) {
                (Some(id), Ok(value)) => Some(protocol::encode_completion_result(&id, &value)),
                (Some(id), Err(error)) => Some(protocol::encode_completion_error(&id, &error)),
                (None, Err(error)) => {
                    debug!(hub = name, target = %log_target.as_deref().unwrap_or_default(), %error, "SignalR invocation failed");
                    None
                }
                (None, Ok(_)) => None,
            }
        }
    }
}

async fn send_initial(sink: &mut Sink, messages: InitialMessages) -> Result<(), axum::Error> {
    for (target, args) in messages {
        send(sink, protocol::encode_invocation(&target, &args)).await?;
    }
    Ok(())
}

async fn send(sink: &mut Sink, record: String) -> Result<(), axum::Error> {
    sink.send(Message::Text(record.into())).await
}

/// Reads frames until at least one complete record arrives. `Err(Some)` carries a protocol
/// error to report to the client, `Err(None)` means the socket closed.
async fn read_records(
    stream: &mut SplitStream<WebSocket>,
    buffer: &mut RecordBuffer,
) -> Result<Vec<String>, Option<String>> {
    loop {
        let frame = stream.next().await;
        let text = match &frame {
            Some(Ok(Message::Text(text))) => text.as_str(),
            Some(Ok(Message::Binary(bytes))) => std::str::from_utf8(bytes)
                .map_err(|_| Some("Invalid UTF-8 payload.".to_string()))?,
            Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
            Some(Ok(Message::Close(_))) | Some(Err(_)) | None => return Err(None),
        };

        let records = buffer.push(text).map_err(Some)?;
        if !records.is_empty() {
            return Ok(records);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::pending;
    use std::net::{Ipv4Addr, SocketAddr};
    use std::sync::atomic::{AtomicBool, Ordering};

    use axum::body::Body;
    use http_body_util::BodyExt;
    use serde_json::json;
    use tokio::net::{TcpListener, TcpStream};
    use tokio_tungstenite::tungstenite::{Error as ClientError, Message as ClientMessage};
    use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};
    use tower::ServiceExt;

    async fn call(router: Router, req: axum::http::Request<Body>) -> (StatusCode, Vec<u8>) {
        let res = router.oneshot(req).await.unwrap();
        let status = res.status();
        let body = res.into_body().collect().await.unwrap().to_bytes().to_vec();
        (status, body)
    }

    fn post_negotiate(uri: &str) -> axum::http::Request<Body> {
        axum::http::Request::post(uri).body(Body::empty()).unwrap()
    }

    const HANDSHAKE: &str = "{\"protocol\":\"json\",\"version\":1}\u{1e}";
    type Client = WebSocketStream<MaybeTlsStream<TcpStream>>;

    struct Server {
        address: SocketAddr,
        task: tokio::task::JoinHandle<()>,
    }

    impl Drop for Server {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    impl Server {
        async fn start(hub: &Hub) -> Self {
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
            let address = listener.local_addr().unwrap();
            let router = hub.router::<()>();
            let task = tokio::spawn(async move {
                axum::serve(
                    listener,
                    router.into_make_service_with_connect_info::<SocketAddr>(),
                )
                .await
                .unwrap();
            });
            Self { address, task }
        }

        async fn connect(&self, token: Option<&str>) -> Result<Client, ClientError> {
            let mut url = format!("ws://{}/", self.address);
            if let Some(token) = token {
                url.push_str("?id=");
                url.push_str(token);
            }
            within(connect_async(url)).await.map(|(socket, _)| socket)
        }
    }

    async fn within<T>(future: impl Future<Output = T>) -> T {
        tokio::time::timeout(Duration::from_secs(5), future)
            .await
            .unwrap()
    }

    async fn client_send(socket: &mut Client, record: impl Into<String>) {
        within(socket.send(ClientMessage::Text(record.into().into())))
            .await
            .unwrap();
    }

    async fn client_recv(socket: &mut Client) -> Value {
        let frame = within(socket.next()).await.unwrap().unwrap();
        let ClientMessage::Text(text) = frame else {
            panic!("expected a SignalR text frame, got {frame:?}");
        };
        serde_json::from_str(
            text.as_str()
                .strip_suffix(protocol::RECORD_SEPARATOR)
                .unwrap(),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn negotiate_v1_returns_token_and_transports() {
        let hub = Hub::new("test");
        let router = Router::new().nest("/hub/test", hub.router());
        let (status, body) = call(
            router,
            post_negotiate("/hub/test/negotiate?negotiateVersion=1"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let json: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["negotiateVersion"], 1);
        assert!(json["connectionId"].as_str().is_some_and(|s| s.len() == 22));
        let token = json["connectionToken"].as_str().unwrap().to_string();
        assert_ne!(json["connectionId"], json["connectionToken"]);
        assert_eq!(
            json["availableTransports"],
            serde_json::json!([{"transport":"WebSockets","transferFormats":["Text","Binary"]}])
        );

        let reservation = hub.take_pending(&token).unwrap();
        let id = json["connectionId"].as_str().unwrap();
        assert_eq!(reservation.connection_id.as_ref(), id);
        assert!(hub.take_pending(&token).is_none());

        // Force a candidate collision with both the taken token and its live ID.
        let mut candidates = [token.as_str(), id, "unused"].into_iter();
        assert_eq!(
            lock(&hub.inner.registry)
                .reserve(|| candidates.next().unwrap().into())
                .as_ref(),
            "unused"
        );
        assert!(candidates.next().is_none());
        drop(reservation);
        let registry = lock(&hub.inner.registry);
        assert!(!registry.reserved.contains(token.as_str()));
        assert!(!registry.reserved.contains(id));
    }

    #[tokio::test]
    async fn negotiate_v0_omits_token() {
        let hub = Hub::new("test");
        let (status, body) = call(hub.router(), post_negotiate("/negotiate")).await;
        assert_eq!(status, StatusCode::OK);
        let json: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["negotiateVersion"], 0);
        assert!(json.get("connectionToken").is_none());
        let id = json["connectionId"].as_str().unwrap();
        assert_eq!(hub.take_pending(id).unwrap().connection_id.as_ref(), id);
    }

    #[tokio::test]
    async fn authorizer_rejects_with_401() {
        let hub = Hub::new("test");
        hub.set_authorizer(|parts| {
            parts
                .uri
                .query()
                .is_some_and(|q| q.contains("access_token=ok"))
        });

        let (status, _) = call(
            hub.router(),
            post_negotiate("/negotiate?negotiateVersion=1"),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        let get = axum::http::Request::get("/?id=x")
            .body(Body::empty())
            .unwrap();
        let (status, _) = call(hub.router(), get).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        let (status, _) = call(
            hub.router(),
            post_negotiate("/negotiate?negotiateVersion=1&access_token=ok"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn unknown_connection_id_is_404() {
        let hub = Hub::new("test");
        let get = axum::http::Request::get("/?id=missing")
            .body(Body::empty())
            .unwrap();
        let (status, _) = call(hub.router(), get).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn broadcast_delivers_and_drops_slow_clients() {
        let hub = Hub::new("test");
        hub.broadcast("noop", vec![]);

        let (fast_tx, mut fast_rx) = mpsc::channel(CONNECTION_BUFFER + 1);
        let (slow_tx, _slow_rx) = mpsc::channel(1);
        lock(&hub.inner.connections).insert("fast".into(), fast_tx);
        lock(&hub.inner.connections).insert("slow".into(), slow_tx);
        assert_eq!(hub.connection_count(), 2);

        hub.broadcast("state", vec![serde_json::json!(1)]);
        assert_eq!(hub.connection_count(), 2);
        assert_eq!(
            fast_rx.recv().await.unwrap().as_str(),
            "{\"type\":1,\"target\":\"state\",\"arguments\":[1]}\u{1e}"
        );

        hub.broadcast("state", vec![serde_json::json!(2)]);
        assert_eq!(hub.connection_count(), 1);
        assert!(lock(&hub.inner.connections).contains_key("fast"));
    }

    #[tokio::test]
    async fn negotiated_token_is_single_use_and_invocations_complete_in_order() {
        let hub = Hub::new("invocations");
        hub.on_connected(|| vec![("Welcome".into(), vec![])]);
        hub.on_invoke(|target, args| Box::pin(async move { Ok(json!([target, args])) }));
        let server = Server::start(&hub).await;

        let mut other = server.connect(None).await.unwrap();
        client_send(&mut other, HANDSHAKE).await;
        assert_eq!(client_recv(&mut other).await, json!({}));
        assert_eq!(client_recv(&mut other).await["target"], "Welcome");

        let (_, body) = call(
            hub.router(),
            post_negotiate("/negotiate?negotiateVersion=1"),
        )
        .await;
        let negotiated: Value = serde_json::from_slice(&body).unwrap();
        let token = negotiated["connectionToken"].as_str().unwrap();
        let mut socket = server.connect(Some(token)).await.unwrap();
        match server.connect(Some(token)).await {
            Err(ClientError::Http(response)) => {
                assert_eq!(response.status(), StatusCode::NOT_FOUND)
            }
            _ => panic!("a negotiated token must be single-use, even before its socket handshakes"),
        }

        // A pipelined invocation cannot overtake the initial messages.
        client_send(
            &mut socket,
            format!(
                "{HANDSHAKE}{}\u{1e}",
                json!({"type": 1, "target": "Echo", "invocationId": "1", "arguments": [1]}),
            ),
        )
        .await;
        assert_eq!(client_recv(&mut socket).await, json!({}));
        assert_eq!(client_recv(&mut socket).await["target"], "Welcome");
        assert_eq!(
            client_recv(&mut socket).await,
            json!({
                "type": 3, "invocationId": "1", "result": ["Echo", [1]],
            })
        );

        hub.broadcast("Public", vec![]);
        assert_eq!(client_recv(&mut socket).await["target"], "Public");
        assert_eq!(client_recv(&mut other).await["target"], "Public");
    }

    #[tokio::test]
    async fn pending_invocation_is_dropped_on_disconnection() {
        struct MarkDropped(Arc<AtomicBool>);
        impl Drop for MarkDropped {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Relaxed);
            }
        }

        let hub = Hub::new("cancellation");
        let (started, mut running) = mpsc::unbounded_channel();
        let dropped = Arc::new(AtomicBool::new(false));
        let mark = dropped.clone();
        hub.on_invoke(move |_, _| {
            let started = started.clone();
            let guard = MarkDropped(mark.clone());
            Box::pin(async move {
                let _guard = guard;
                started.send(()).unwrap();
                pending().await
            })
        });
        let server = Server::start(&hub).await;
        let mut socket = server.connect(None).await.unwrap();

        client_send(
            &mut socket,
            format!("{HANDSHAKE}{{\"type\":1,\"target\":\"Block\",\"invocationId\":\"1\"}}\u{1e}"),
        )
        .await;
        assert_eq!(client_recv(&mut socket).await, json!({}));
        within(running.recv()).await.unwrap();
        within(socket.send(ClientMessage::Close(None)))
            .await
            .unwrap();
        // The invocation is dropped before the connection is unregistered.
        within(async {
            while hub.connection_count() > 0 {
                tokio::task::yield_now().await;
            }
        })
        .await;
        assert!(dropped.load(Ordering::Relaxed));
    }

    #[tokio::test]
    async fn generic_invocation_returns_a_completion() {
        let hub = Hub::new("generic");
        hub.on_invoke(|target, args| Box::pin(async move { Ok(json!([target, args])) }));
        let completion = hub
            .invoke(Some("1".into()), "Echo".into(), vec![json!(true)])
            .await;
        assert_eq!(
            completion,
            Some(protocol::encode_completion_result(
                "1",
                &json!(["Echo", [true]])
            ))
        );
    }
}
