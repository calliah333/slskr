//! `GET {url_base}/api/events`: the WebSocket carrying [`slskr_api::realtime`] events.
//!
//! The upgrade is authenticated like any other route (session cookie or `X-API-Key`; a
//! cookie presented with a foreign `Origin` is refused). Each connection runs as one task:
//! a dispatcher that reads client commands, filters the bus by subscription and keeps the
//! connection alive, and a writer draining a bounded queue into the socket. A connection
//! whose queue overflows is closed with [`CLOSE_TOO_SLOW`]; the client reconnects and gets
//! fresh snapshots. Both end on close, on error, and on daemon shutdown.

use std::{sync::Arc, time::Duration};

use axum::{
    Router,
    body::Bytes,
    extract::{
        FromRequestParts, Request,
        ws::{CloseFrame, Message, Utf8Bytes, WebSocket, WebSocketUpgrade, close_code},
    },
    http::{HeaderMap, Method, Uri},
    response::{IntoResponse, Response},
    routing::get,
};
use futures::{
    SinkExt, StreamExt,
    stream::{SplitSink, SplitStream},
};
use slskr_api::realtime::{
    CLOSE_TIMEOUT, CLOSE_TOO_SLOW, CLOSE_UNAUTHORIZED, Command, Event, MAX_CLIENT_MESSAGE, PATH,
    Topic,
};
use std::net::SocketAddr;
use tokio::{
    sync::{broadcast::error::RecvError, mpsc, oneshot},
    time::{Instant, MissedTickBehavior},
};
use tokio_util::sync::CancellationToken;
use tracing::debug;

use super::EventBus;
use crate::auth::{Auth, AuthError, Principal, Role, remote_addr};

/// How long the writer may take to deliver the close frame before the socket is dropped.
const CLOSE_GRACE: Duration = Duration::from_secs(5);

/// The current value of a topic, for new subscribers.
pub trait Snapshots: Send + Sync + 'static {
    /// The snapshot of `topic` and the bus sequence number it includes: read
    /// [`EventBus::seq`] before reading the data, or atomically with it.
    fn snapshot(&self, topic: Topic, bus: &EventBus) -> (u64, Event);
}

/// Per-connection limits.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Outbound messages queued for a connection before it is closed as too slow.
    pub queue: usize,
    /// Interval of server pings and credential re-checks.
    pub ping_interval: Duration,
    /// A connection that sends nothing (pongs included) for this long is closed.
    pub idle_timeout: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            queue: 256,
            ping_interval: Duration::from_secs(20),
            idle_timeout: Duration::from_secs(60),
        }
    }
}

/// The `/api/events` endpoint. Cheap to clone.
#[derive(Clone)]
pub struct EventsEndpoint(Arc<Inner>);

struct Inner {
    auth: Auth,
    bus: EventBus,
    snapshots: Arc<dyn Snapshots>,
    shutdown: CancellationToken,
    limits: Limits,
}

/// What authenticated the upgrade, kept to re-check it while the socket is open.
struct Credentials {
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    remote: SocketAddr,
}

enum Close {
    /// The peer is gone or closed the connection.
    Silent,
    Frame(u16, &'static str),
}

impl EventsEndpoint {
    pub fn new(
        auth: Auth,
        bus: EventBus,
        snapshots: Arc<dyn Snapshots>,
        shutdown: CancellationToken,
        limits: Limits,
    ) -> Self {
        Self(Arc::new(Inner {
            auth,
            bus,
            snapshots,
            shutdown,
            limits,
        }))
    }

    /// Routes [`PATH`]; merge at the root of the (`url_base`-nested) endpoints.
    pub fn router<S: Clone + Send + Sync + 'static>(&self) -> Router<S> {
        let endpoint = self.clone();
        Router::new().route(
            PATH,
            get(move |request: Request| endpoint.clone().upgrade(request)),
        )
    }

    async fn upgrade(self, request: Request) -> Response {
        let (mut parts, _) = request.into_parts();
        let principal = match self.0.auth.authenticate_request(&parts) {
            Ok(principal) => principal,
            Err(error) => return error.into_response(),
        };
        let upgrade = match WebSocketUpgrade::from_request_parts(&mut parts, &()).await {
            Ok(upgrade) => upgrade
                .max_message_size(MAX_CLIENT_MESSAGE)
                .max_frame_size(MAX_CLIENT_MESSAGE),
            Err(rejection) => return rejection.into_response(),
        };
        let credentials = Credentials {
            remote: remote_addr(&parts),
            method: parts.method,
            uri: parts.uri,
            headers: parts.headers,
        };
        upgrade.on_upgrade(move |socket| self.run(socket, principal, credentials))
    }

    async fn run(self, socket: WebSocket, principal: Principal, credentials: Credentials) {
        let (sink, stream) = socket.split();
        let (queue, outbound) = mpsc::channel(self.0.limits.queue.max(1));
        let (close_tx, close_rx) = oneshot::channel();
        let writer = write(sink, outbound, close_rx);
        tokio::pin!(writer);
        debug!(user = %principal.name, "events connection opened");
        let close = tokio::select! {
            () = &mut writer => None,
            close = self.dispatch(stream, queue, principal, &credentials) => Some(close),
        };
        if let Some(close) = close {
            if let Close::Frame(code, reason) = close {
                debug!(code, reason, "closing events connection");
            }
            let _ = close_tx.send(close);
            let _ = tokio::time::timeout(CLOSE_GRACE, writer).await;
        }
        debug!("events connection closed");
    }

    /// Reads commands and forwards subscribed events until the connection should close.
    async fn dispatch(
        &self,
        mut stream: SplitStream<WebSocket>,
        queue: mpsc::Sender<Message>,
        mut principal: Principal,
        credentials: &Credentials,
    ) -> Close {
        let inner = &*self.0;
        let mut session = Session {
            endpoint: inner,
            connection: inner.bus.connection(),
            skip: [0; Topic::ALL.len()],
            queue,
        };
        let mut published = inner.bus.receiver();
        let period = inner.limits.ping_interval;
        let mut ping = tokio::time::interval_at(Instant::now() + period, period);
        ping.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut last_seen = Instant::now();

        loop {
            let result = tokio::select! {
                biased;
                () = inner.shutdown.cancelled() => Err(Close::Frame(close_code::AWAY, "server shutting down")),
                frame = stream.next() => {
                    last_seen = Instant::now();
                    match frame {
                        Some(Ok(Message::Text(text))) => session.command(&text, &principal),
                        Some(Ok(Message::Binary(_))) => session.error("expected a JSON text message", None),
                        Some(Ok(Message::Ping(_) | Message::Pong(_))) => Ok(()),
                        Some(Ok(Message::Close(_)) | Err(_)) | None => Err(Close::Silent),
                    }
                }
                event = published.recv() => match event {
                    Ok(event) => session.forward(event.topic, event.seq, event.frame),
                    Err(RecvError::Lagged(_)) => Err(Close::Frame(CLOSE_TOO_SLOW, "client too slow")),
                    Err(RecvError::Closed) => Err(Close::Frame(close_code::AWAY, "server shutting down")),
                },
                _ = ping.tick() => {
                    if last_seen.elapsed() >= inner.limits.idle_timeout {
                        Err(Close::Frame(CLOSE_TIMEOUT, "no response from client"))
                    } else {
                        session.keep_alive(&mut principal, credentials)
                    }
                }
            };
            if let Err(close) = result {
                return close;
            }
        }
    }
}

struct Session<'a> {
    endpoint: &'a Inner,
    connection: super::Connection,
    /// Per topic: events up to this sequence number are in the snapshot already sent.
    skip: [u64; Topic::ALL.len()],
    queue: mpsc::Sender<Message>,
}

impl Session<'_> {
    fn send(&self, message: Message) -> Result<(), Close> {
        self.queue.try_send(message).map_err(|error| match error {
            mpsc::error::TrySendError::Full(_) => Close::Frame(CLOSE_TOO_SLOW, "client too slow"),
            mpsc::error::TrySendError::Closed(_) => Close::Silent,
        })
    }

    fn event(&self, event: &Event) -> Result<(), Close> {
        match serde_json::to_string(event) {
            Ok(text) => self.send(Message::Text(text.into())),
            Err(error) => {
                tracing::error!(%error, "could not encode an event");
                Ok(())
            }
        }
    }

    fn error(&self, message: impl Into<String>, topic: Option<Topic>) -> Result<(), Close> {
        self.event(&Event::Error {
            message: message.into(),
            topic,
        })
    }

    fn forward(&self, topic: Topic, seq: u64, frame: Utf8Bytes) -> Result<(), Close> {
        if self.connection.contains(topic) && seq > self.skip[topic.index()] {
            self.send(Message::Text(frame))
        } else {
            Ok(())
        }
    }

    fn command(&mut self, text: &str, principal: &Principal) -> Result<(), Close> {
        let command = match serde_json::from_str::<Command>(text) {
            Ok(command) => command,
            Err(error) => return self.error(format!("invalid command: {error}"), None),
        };
        match command {
            Command::Subscribe { mut topics } => {
                topics.sort_unstable();
                topics.dedup();
                for topic in topics {
                    if !allowed(principal, topic) {
                        self.error(
                            format!(
                                "the {} topic requires the Administrator role",
                                topic.as_str()
                            ),
                            Some(topic),
                        )?;
                        continue;
                    }
                    // Register first so publishers send every change after the snapshot.
                    self.connection.subscribe(topic);
                    let (seq, snapshot) =
                        self.endpoint.snapshots.snapshot(topic, &self.endpoint.bus);
                    self.skip[topic.index()] = seq;
                    self.event(&snapshot)?;
                }
            }
            Command::Unsubscribe { topics } => {
                for topic in topics {
                    self.connection.unsubscribe(topic);
                }
            }
        }
        Ok(())
    }

    /// Re-checks the credentials the connection was opened with and sends a ping.
    fn keep_alive(
        &mut self,
        principal: &mut Principal,
        credentials: &Credentials,
    ) -> Result<(), Close> {
        let current = reauthenticate(&self.endpoint.auth, credentials)
            .map_err(|_| Close::Frame(CLOSE_UNAUTHORIZED, "unauthorized"))?;
        for topic in Topic::ALL {
            if self.connection.contains(topic) && !allowed(&current, topic) {
                self.connection.unsubscribe(topic);
                self.error(
                    format!(
                        "the {} topic requires the Administrator role",
                        topic.as_str()
                    ),
                    Some(topic),
                )?;
            }
        }
        *principal = current;
        self.send(Message::Ping(Bytes::new()))
    }
}

fn allowed(principal: &Principal, topic: Topic) -> bool {
    topic != Topic::Logs || principal.role.satisfies(Role::Administrator)
}

fn reauthenticate(auth: &Auth, credentials: &Credentials) -> Result<Principal, AuthError> {
    let mut request = Request::new(());
    *request.method_mut() = credentials.method.clone();
    *request.uri_mut() = credentials.uri.clone();
    *request.headers_mut() = credentials.headers.clone();
    let (parts, ()) = request.into_parts();
    auth.authenticate(&parts, credentials.remote)
}

/// Drains `outbound` into the socket. Once the dispatcher hands over its close reason, the
/// queue is abandoned and the close frame is sent instead.
async fn write(
    mut sink: SplitSink<WebSocket, Message>,
    mut outbound: mpsc::Receiver<Message>,
    mut close: oneshot::Receiver<Close>,
) {
    let reason = loop {
        let message = tokio::select! {
            biased;
            reason = &mut close => break reason.ok(),
            message = outbound.recv() => match message {
                Some(message) => message,
                None => break None,
            },
        };
        tokio::select! {
            biased;
            reason = &mut close => break reason.ok(),
            sent = sink.send(message) => if sent.is_err() { return },
        }
    };
    if let Some(Close::Frame(code, reason)) = reason {
        let _ = sink
            .send(Message::Close(Some(CloseFrame {
                code,
                reason: reason.into(),
            })))
            .await;
    }
    let _ = sink.close().await;
}
