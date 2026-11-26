use std::{
    net::{Ipv4Addr, SocketAddr},
    sync::Arc,
    time::Duration,
};

use futures::{SinkExt, StreamExt, future::BoxFuture};
use slskr_types::{
    logs::LogRecord,
    realtime::{CLOSE_TIMEOUT, CLOSE_TOO_SLOW, CLOSE_UNAUTHORIZED, Event, Topic},
};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async,
    tungstenite::{
        Error as ClientError, Message as ClientMessage, client::IntoClientRequest,
        protocol::frame::coding::CloseCode,
    },
};
use tokio_util::sync::CancellationToken;

use super::{EventBus, Limits, Snapshots, socket::EventsEndpoint};
use crate::{
    auth::{ALLOW_ALL_CIDR, ApiKey, Auth, AuthConfig, COOKIE_NAME, Role, SessionStore},
    logging::LogBuffer,
    state::StateStore,
};

type Client = WebSocketStream<MaybeTlsStream<TcpStream>>;

const READER_KEY: &str = "reader-key-0123456789";

struct Fixture {
    state: StateStore,
    logs: LogBuffer,
}

impl Snapshots for Fixture {
    fn snapshot<'a>(&'a self, topic: Topic, bus: &'a EventBus) -> BoxFuture<'a, (u64, Event)> {
        let seq = bus.seq();
        let snapshot = match topic {
            Topic::State => (seq, Event::State(Box::new(self.state.get()))),
            Topic::Settings => (seq, Event::Settings(Box::default())),
            Topic::Searches => (seq, Event::SearchList(Vec::new())),
            Topic::Metrics => (seq, Event::Metrics(Default::default())),
            Topic::Logs => {
                let (seq, records) = self.logs.snapshot_with_seq();
                (seq, Event::LogBuffer(records))
            }
            Topic::Transfers => (seq, Event::TransferList(Vec::new())),
            Topic::Conversations => (seq, Event::ConversationList(Vec::new())),
            Topic::Rooms => (seq, Event::RoomList(Vec::new())),
        };
        Box::pin(std::future::ready(snapshot))
    }
}

struct Server {
    address: SocketAddr,
    bus: EventBus,
    state: StateStore,
    auth: Auth,
    cookie: String,
    shutdown: CancellationToken,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.shutdown.cancel();
        self.task.abort();
    }
}

impl Server {
    async fn start(limits: Limits) -> Self {
        let auth = Auth::new(
            AuthConfig {
                username: "admin".into(),
                password: "secret".into(),
                api_keys: vec![ApiKey {
                    name: "reader".into(),
                    key: READER_KEY.into(),
                    role: Role::ReadOnly,
                    cidr: ALLOW_ALL_CIDR.into(),
                }],
                ..AuthConfig::default()
            },
            SessionStore::memory(Duration::from_secs(3600)),
        );
        let (token, _) = auth
            .login("admin", "secret", std::net::Ipv4Addr::LOCALHOST.into())
            .await
            .unwrap();
        let bus = EventBus::new();
        let state = StateStore::new();
        let shutdown = CancellationToken::new();
        state.spawn_publisher(bus.clone(), shutdown.clone());
        let fixture = Fixture {
            state: state.clone(),
            logs: LogBuffer::new(bus.clone()),
        };
        let endpoint = EventsEndpoint::new(
            auth.clone(),
            bus.clone(),
            Arc::new(fixture),
            shutdown.clone(),
            limits,
        );
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let router = endpoint.router::<()>();
        let task = tokio::spawn(async move {
            axum::serve(
                listener,
                router.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap();
        });
        Self {
            address,
            bus,
            state,
            auth,
            cookie: format!("{COOKIE_NAME}={token}"),
            shutdown,
            task,
        }
    }

    async fn connect(&self, headers: &[(&str, &str)]) -> Result<Client, ClientError> {
        let mut request = format!("ws://{}/api/events", self.address)
            .into_client_request()
            .unwrap();
        for (name, value) in headers {
            request.headers_mut().insert(
                axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                value.parse().unwrap(),
            );
        }
        within(connect_async(request))
            .await
            .map(|(socket, _)| socket)
    }

    async fn admin(&self) -> Client {
        self.connect(&[("cookie", &self.cookie)]).await.unwrap()
    }

    fn log(&self, message: String) {
        self.bus.publish(&Event::Log(LogRecord {
            message: Some(message),
            ..Default::default()
        }));
    }

    async fn until(&self, condition: impl Fn(&Self) -> bool) {
        within(async {
            while !condition(self) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
    }
}

fn fast() -> Limits {
    Limits::default()
}

async fn within<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(10), future)
        .await
        .expect("timed out")
}

async fn send(client: &mut Client, text: &str) {
    within(client.send(ClientMessage::Text(text.into())))
        .await
        .unwrap();
}

async fn subscribe(client: &mut Client, topics: &[&str]) {
    let command = serde_json::json!({"type": "subscribe", "topics": topics});
    send(client, &command.to_string()).await;
}

/// The next event, skipping pings.
async fn recv(client: &mut Client) -> Event {
    loop {
        match within(client.next()).await {
            Some(Ok(ClientMessage::Text(text))) => return serde_json::from_str(&text).unwrap(),
            Some(Ok(ClientMessage::Ping(_) | ClientMessage::Pong(_))) => continue,
            other => panic!("expected an event, got {other:?}"),
        }
    }
}

/// Reads until the server closes the connection; returns the close code.
async fn close_code(client: &mut Client) -> Option<u16> {
    loop {
        match within(client.next()).await {
            Some(Ok(ClientMessage::Close(frame))) => return frame.map(|f| u16::from(f.code)),
            Some(Ok(_)) => continue,
            Some(Err(_)) | None => return None,
        }
    }
}

fn state(event: Event) -> slskr_types::application::State {
    match event {
        Event::State(state) => *state,
        other => panic!("expected state, got {other:?}"),
    }
}

#[tokio::test]
async fn subscribe_sends_the_snapshot_then_changes() {
    let server = Server::start(fast()).await;
    server.state.update(|state| state.pending_restart = true);
    let mut client = server.admin().await;
    assert_eq!(server.bus.connection_count(), 1);

    // Invalid commands are answered, not fatal.
    send(&mut client, "not json").await;
    assert!(matches!(
        recv(&mut client).await,
        Event::Error { topic: None, .. }
    ));
    send(&mut client, r#"{"type":"subscribe","topics":["nope"]}"#).await;
    assert!(matches!(recv(&mut client).await, Event::Error { .. }));
    within(client.send(ClientMessage::Binary(vec![1].into())))
        .await
        .unwrap();
    assert!(matches!(recv(&mut client).await, Event::Error { .. }));

    subscribe(&mut client, &["state", "searches"]).await;
    assert!(state(recv(&mut client).await).pending_restart);
    assert_eq!(recv(&mut client).await, Event::SearchList(Vec::new()));
    assert_eq!(server.bus.subscriber_count(Topic::State), 1);

    server.state.update(|state| state.pending_reconnect = true);
    let changed = state(recv(&mut client).await);
    assert!(changed.pending_reconnect && changed.pending_restart);

    // Events of unsubscribed topics are not sent.
    send(&mut client, r#"{"type":"unsubscribe","topics":["state"]}"#).await;
    server
        .until(|s| s.bus.subscriber_count(Topic::State) == 0)
        .await;
    server.state.update(|state| state.pending_reconnect = false);
    server.bus.publish(&Event::SearchDeleted { id: "x".into() });
    assert_eq!(
        recv(&mut client).await,
        Event::SearchDeleted { id: "x".into() }
    );

    drop(client);
    server.until(|s| s.bus.connection_count() == 0).await;
    assert_eq!(server.bus.subscriber_count(Topic::Searches), 0);
}

#[tokio::test]
async fn transfer_conversation_and_room_topics_send_snapshots_then_changes() {
    let server = Server::start(fast()).await;
    // A read-only key may follow all three.
    let mut client = server.connect(&[("x-api-key", READER_KEY)]).await.unwrap();
    subscribe(&mut client, &["transfers", "conversations", "rooms"]).await;
    // Snapshots arrive in topic order.
    assert_eq!(recv(&mut client).await, Event::TransferList(Vec::new()));
    assert_eq!(recv(&mut client).await, Event::ConversationList(Vec::new()));
    assert_eq!(recv(&mut client).await, Event::RoomList(Vec::new()));
    for topic in [Topic::Transfers, Topic::Conversations, Topic::Rooms] {
        assert!(server.bus.wants(topic));
    }

    let removed = Event::TransferRemoved {
        ids: vec!["id".into()],
    };
    server.bus.publish(&removed);
    assert_eq!(recv(&mut client).await, removed);
    let left = Event::RoomLeft {
        room: "room".into(),
    };
    server.bus.publish(&left);
    assert_eq!(recv(&mut client).await, left);

    send(
        &mut client,
        r#"{"type":"unsubscribe","topics":["transfers"]}"#,
    )
    .await;
    server.until(|s| !s.bus.wants(Topic::Transfers)).await;
    drop(client);
    server.until(|s| s.bus.connection_count() == 0).await;
    assert!(!server.bus.wants(Topic::Rooms));
}

#[tokio::test]
async fn logs_require_an_administrator() {
    let server = Server::start(fast()).await;
    let mut admin = server.admin().await;
    subscribe(&mut admin, &["logs"]).await;
    assert_eq!(recv(&mut admin).await, Event::LogBuffer(Vec::new()));

    let mut reader = server.connect(&[("x-api-key", READER_KEY)]).await.unwrap();
    subscribe(&mut reader, &["logs", "state"]).await;
    state(recv(&mut reader).await);
    assert!(matches!(
        recv(&mut reader).await,
        Event::Error {
            topic: Some(Topic::Logs),
            ..
        }
    ));
    assert_eq!(server.bus.subscriber_count(Topic::Logs), 1);

    server.log("secret".into());
    assert!(matches!(recv(&mut admin).await, Event::Log(_)));
    server.state.update(|state| state.pending_restart = true);
    // The reader's next event is the state change, not the log record.
    assert!(state(recv(&mut reader).await).pending_restart);
}

#[tokio::test]
async fn upgrade_is_authenticated_and_same_origin() {
    let server = Server::start(fast()).await;
    let status = |result: Result<Client, ClientError>| match result {
        Err(ClientError::Http(response)) => response.status().as_u16(),
        Ok(_) => 101,
        Err(error) => panic!("{error}"),
    };
    assert_eq!(status(server.connect(&[]).await), 401);
    assert_eq!(
        status(server.connect(&[("cookie", "slskr_session=forged")]).await),
        401
    );
    let cookie = server.cookie.as_str();
    assert_eq!(
        status(
            server
                .connect(&[("cookie", cookie), ("origin", "http://evil.example")])
                .await
        ),
        403
    );
    let origin = format!("http://{}", server.address);
    assert_eq!(
        status(
            server
                .connect(&[("cookie", cookie), ("origin", &origin)])
                .await
        ),
        101
    );
    // API keys are not ambient credentials; another origin may use one.
    assert_eq!(
        status(
            server
                .connect(&[("x-api-key", READER_KEY), ("origin", "http://evil.example")])
                .await
        ),
        101
    );
}

#[tokio::test]
async fn slow_consumers_are_closed_without_affecting_others() {
    let server = Server::start(Limits { queue: 4, ..fast() }).await;
    let mut slow = server.admin().await;
    subscribe(&mut slow, &["logs"]).await;
    recv(&mut slow).await;
    let mut fast = server.admin().await;
    subscribe(&mut fast, &["logs"]).await;
    recv(&mut fast).await;
    let reader = tokio::spawn(async move {
        let mut received = 0usize;
        loop {
            if let Event::Log(record) = recv(&mut fast).await {
                received += 1;
                if record.message.as_deref() == Some("last") {
                    return (fast, received);
                }
            }
        }
    });

    // Large records fill the socket buffers of the client that does not read.
    let filler = "x".repeat(256 * 1024);
    let mut published = 0;
    within(async {
        while server.bus.subscriber_count(Topic::Logs) == 2 {
            server.log(filler.clone());
            published += 1;
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await;
    assert_eq!(server.bus.connection_count(), 1);
    server.log("last".into());
    let (mut fast, received) = within(reader).await.unwrap();
    assert_eq!(received, published + 1);
    assert_eq!(close_code(&mut slow).await, Some(CLOSE_TOO_SLOW));

    // The other connection is still served.
    server.log("after".into());
    assert!(matches!(recv(&mut fast).await, Event::Log(_)));
}

#[tokio::test]
async fn state_changes_are_coalesced_and_the_latest_is_delivered() {
    let server = Server::start(fast()).await;
    let mut client = server.admin().await;
    subscribe(&mut client, &["state"]).await;
    assert_eq!(state(recv(&mut client).await).user.statistics.file_count, 0);

    for count in 1..=100 {
        server
            .state
            .update(|state| state.user.statistics.file_count = count);
    }
    let mut events = 0;
    loop {
        events += 1;
        if state(recv(&mut client).await).user.statistics.file_count == 100 {
            break;
        }
    }
    assert!(events <= 2, "{events} state events for one burst");
    // Nothing else is pending.
    assert!(
        tokio::time::timeout(crate::state::STATE_EVENT_INTERVAL * 2, recv(&mut client))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn silent_clients_time_out() {
    let server = Server::start(Limits {
        ping_interval: Duration::from_millis(50),
        idle_timeout: Duration::from_millis(200),
        ..fast()
    })
    .await;
    let mut client = server.admin().await;
    // Not reading means not answering the server's pings.
    server.until(|s| s.bus.connection_count() == 0).await;
    let mut pings = 0;
    let code = loop {
        match within(client.next()).await {
            Some(Ok(ClientMessage::Ping(_))) => pings += 1,
            Some(Ok(ClientMessage::Close(frame))) => break frame.map(|f| u16::from(f.code)),
            // Answering a buffered ping can hit the already closed socket.
            Some(Err(ClientError::Io(_))) | None => break Some(CLOSE_TIMEOUT),
            other => panic!("{other:?}"),
        }
    };
    assert!(pings >= 1, "{pings} pings");
    assert_eq!(code, Some(CLOSE_TIMEOUT));
}

#[tokio::test]
async fn ended_sessions_and_shutdown_close_connections() {
    let server = Server::start(Limits {
        ping_interval: Duration::from_millis(50),
        ..fast()
    })
    .await;
    let mut signed_out = server.admin().await;
    let mut other = server.connect(&[("x-api-key", READER_KEY)]).await.unwrap();
    let token = server.cookie.split_once('=').unwrap().1;
    server.auth.sessions().remove(token).await.unwrap();
    assert_eq!(close_code(&mut signed_out).await, Some(CLOSE_UNAUTHORIZED));

    // The API key connection answers pings while it reads, so it stays open.
    subscribe(&mut other, &["metrics"]).await;
    recv(&mut other).await;
    server.shutdown.cancel();
    assert_eq!(
        close_code(&mut other).await,
        Some(u16::from(CloseCode::Away))
    );
    server.until(|s| s.bus.connection_count() == 0).await;
}
