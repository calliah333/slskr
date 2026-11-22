//! Server transport, login and session transitions.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant};

use bytes::{Bytes, BytesMut};
use tokio::net::{TcpListener, lookup_host};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::{ClientInner, SoulseekClient, require_non_blank};
use crate::error::{Error, Result};
use crate::events::Event;
use crate::model::{ClientState, DistributedNetworkInfo, ServerInfo, UserPresence};
use crate::net::connection::Connection;
use crate::net::listener::Listener;
use crate::net::message_connection::MessageConnection;
use crate::options::{ClientOptions, ClientOptionsPatch};
use crate::protocol::server::{
    AcceptChildrenCommand, BranchLevelCommand, BranchRootCommand, HaveNoParentsCommand,
    LoginRequest, LoginResponse, PrivateRoomToggle, ServerIncoming, SetListenPortCommand,
    SetOnlineStatusCommand,
};

#[derive(Default)]
pub(crate) struct ServerConnection {
    // Owns all resources of one session, including a reservation during DNS.
    // Transitions and synchronous dispatch share this lock; no I/O awaits under it.
    lifecycle: Mutex<Lifecycle>,
    // Serialize initial configuration and live patches, without blocking disconnect.
    configuration: tokio::sync::Mutex<()>,
}

#[derive(Default)]
struct Lifecycle {
    generation: u64,
    active: Option<Active>,
}

struct Active {
    cancel: CancellationToken,
    listener: Option<Listener>,
    transport: Option<MessageConnection>,
    endpoints: HashMap<String, (SocketAddr, Instant)>,
}

impl Drop for Active {
    fn drop(&mut self) {
        self.cancel.cancel();
        self.listener.take();
        if let Some(transport) = self.transport.take() {
            transport.close();
        }
    }
}

impl Lifecycle {
    fn current(&self, generation: u64) -> bool {
        self.generation == generation && self.active.is_some()
    }

    fn active_mut(&mut self, generation: u64) -> Result<&mut Active> {
        if self.generation != generation {
            return Err(Error::Cancelled);
        }
        self.active.as_mut().ok_or(Error::Cancelled)
    }
}

impl ServerConnection {
    fn lock(&self) -> MutexGuard<'_, Lifecycle> {
        self.lifecycle.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(super) fn logged_in_transport(&self, inner: &ClientInner) -> Result<MessageConnection> {
        self.with_logged_in_transport(inner, |transport| transport)
    }

    /// Captures the transport and registers any response wait under the same
    /// lock that disconnect uses to cancel waiters.
    pub(super) fn with_logged_in_transport<T>(
        &self,
        inner: &ClientInner,
        f: impl FnOnce(MessageConnection) -> T,
    ) -> Result<T> {
        let lifecycle = self.lock();
        let state = inner.state();
        if !state.is_logged_in() {
            return Err(Error::InvalidState(format!(
                "The server connection is not logged in (currently: {state})"
            )));
        }
        let transport = lifecycle
            .active
            .as_ref()
            .and_then(|active| active.transport.clone())
            .ok_or_else(server_closed)?;
        Ok(f(transport))
    }

    /// Dispatches an accepted socket only if its listener is still running.
    pub(crate) fn with_live_listener(
        &self,
        inner: &ClientInner,
        stop: &CancellationToken,
        dispatch: impl FnOnce(),
    ) {
        let _lifecycle = self.lock();
        if !stop.is_cancelled() && inner.state().is_logged_in() {
            dispatch();
        }
    }

    pub(super) fn cached_endpoint(&self, username: &str) -> Option<SocketAddr> {
        let lifecycle = self.lock();
        let (endpoint, expires) = lifecycle.active.as_ref()?.endpoints.get(username)?;
        (*expires > Instant::now()).then_some(*endpoint)
    }
}

/// Dropping `connect` at any await releases its reservation and closes partial setup.
struct ConnectAttempt<'a> {
    client: &'a SoulseekClient,
    generation: u64,
    armed: bool,
}

impl Drop for ConnectAttempt<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.client.disconnect_generation(
                Some(self.generation),
                "Cancelled connection setup",
                Some(Error::Cancelled),
            );
        }
    }
}

impl SoulseekClient {
    /// Connects to `address:port` and logs in, starting the peer listener if enabled.
    /// Emits [`Event::Connected`] when TCP is up and [`Event::LoggedIn`] on success.
    /// Login rejection returns [`Error::LoginRejected`]; a failed setup disconnects
    /// the session before returning its error.
    pub async fn connect(
        &self,
        address: &str,
        port: u16,
        username: &str,
        password: &str,
    ) -> Result<()> {
        require_non_blank(address, "address")?;
        require_non_blank(username, "username")?;
        require_non_blank(password, "password")?;
        if port == 0 {
            return Err(Error::Argument(
                "The port must be within the range 1-65535".into(),
            ));
        }

        let (generation, cancel) = {
            let mut lifecycle = self.inner.server.lock();
            if lifecycle.active.is_some() {
                let message = if self.state().is_logged_in() {
                    "The client is already connected"
                } else {
                    "A connection is already in the process of being established"
                };
                return Err(Error::InvalidState(message.into()));
            }
            lifecycle.generation = lifecycle.generation.wrapping_add(1);
            let cancel = CancellationToken::new();
            lifecycle.active = Some(Active {
                cancel: cancel.clone(),
                listener: None,
                transport: None,
                endpoints: HashMap::new(),
            });
            (lifecycle.generation, cancel)
        };
        let mut attempt = ConnectAttempt {
            client: self,
            generation,
            armed: true,
        };
        let result = tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(Error::Cancelled),
            result = self.establish(generation, &cancel, address, port, username, password) => result,
        };
        attempt.armed = false;
        if let Err(error) = result {
            let (error, event_error) = connect_errors(error);
            self.disconnect_generation(Some(generation), &error.to_string(), Some(event_error));
            return Err(error);
        }
        Ok(())
    }

    async fn establish(
        &self,
        generation: u64,
        cancel: &CancellationToken,
        address: &str,
        port: u16,
        username: &str,
        password: &str,
    ) -> Result<()> {
        let endpoint = resolve(address, port).await?;
        let _configuration = self.inner.server.configuration.lock().await;
        let options = self.options();
        options.validate()?;
        {
            let mut lifecycle = self.inner.server.lock();
            let active = lifecycle.active_mut(generation)?;
            self.inner
                .set_state(ClientState::CONNECTING, Some("Connecting".into()), None);
            if let Some(endpoint) = listen_endpoint(&options).filter(|_| options.enable_listener) {
                let socket = Listener::bind(endpoint, options.listen_backlog)?;
                active.listener = Some(self.spawn_listener(socket, &options));
            }
        }
        let mut conn = Connection::connect(endpoint, &options.server_connection_options).await?;
        {
            let lifecycle = self.inner.server.lock();
            if !lifecycle.current(generation) {
                return Err(Error::Cancelled);
            }
            {
                let mut session = self
                    .inner
                    .session
                    .write()
                    .unwrap_or_else(|e| e.into_inner());
                session.address = Some(address.into());
                session.ip_endpoint = Some(endpoint);
                session.server_info = ServerInfo::default();
            }
            self.inner.set_state(
                ClientState::CONNECTED,
                Some(format!("Connected to {endpoint}")),
                None,
            );
            self.inner.emit(Event::Connected);
            self.inner.set_state(
                ClientState::CONNECTED | ClientState::LOGGING_IN,
                Some("Logging in".into()),
                None,
            );
        }

        let login = LoginRequest::new(username, password, options.minor_version).to_bytes();
        let listen = SetListenPortCommand {
            port: advertised_port(&options),
        }
        .to_bytes();
        let mut request = BytesMut::with_capacity(login.len() + listen.len());
        request.extend_from_slice(&login);
        request.extend_from_slice(&listen);
        // MessageConnection accepts one frame per send. Write the login batch
        // before handing the socket to its framed reader and writer.
        conn.write_all(&request).await?;
        let (transport, mut incoming) = {
            let mut lifecycle = self.inner.server.lock();
            let active = lifecycle.active_mut(generation)?;
            let (transport, incoming) = MessageConnection::spawn(conn, None);
            active.transport = Some(transport.clone());
            (transport, incoming)
        };
        let response = tokio::time::timeout(
            options.message_timeout,
            self.wait_for_login(generation, &mut incoming),
        )
        .await??;
        if !response.succeeded {
            return Err(Error::LoginRejected(format!(
                "The server rejected login attempt: {}",
                response.message
            )));
        }

        {
            let lifecycle = self.inner.server.lock();
            if !lifecycle.current(generation) {
                return Err(Error::Cancelled);
            }
            let info = {
                let mut session = self
                    .inner
                    .session
                    .write()
                    .unwrap_or_else(|e| e.into_inner());
                session.server_info.is_supporter = Some(response.is_supporter);
                session.username = Some(username.into());
                session.server_info.clone()
            };
            self.inner.emit(Event::ServerInfoReceived(info));
            self.inner.set_state(
                ClientState::CONNECTED | ClientState::LOGGED_IN,
                Some("Logged in".into()),
                None,
            );
            self.inner.emit(Event::LoggedIn);
            self.inner.distributed.on_logged_in(self);
        }

        transport
            .send(
                PrivateRoomToggle {
                    accept_invitations: options.accept_private_room_invitations,
                }
                .to_bytes(),
            )
            .await?;
        transport
            .send(
                SetOnlineStatusCommand {
                    status: UserPresence::Online,
                }
                .to_bytes(),
            )
            .await?;
        let info = self.distributed_network();
        send_distributed(&transport, &options, info, username).await?;
        tokio::spawn(read_server(
            Arc::downgrade(self.inner()),
            generation,
            cancel.clone(),
            incoming,
        ));
        Ok(())
    }

    async fn wait_for_login(
        &self,
        generation: u64,
        incoming: &mut mpsc::Receiver<Bytes>,
    ) -> Result<LoginResponse> {
        while let Some(body) = incoming.recv().await {
            let Some(message) = decode(body) else {
                continue;
            };
            let mut lifecycle = self.inner.server.lock();
            let active = lifecycle.active_mut(generation)?;
            match message {
                ServerIncoming::Login(response) => return Ok(response),
                ServerIncoming::KickedFromServer => {
                    self.inner.emit(Event::KickedFromServer);
                    return Err(Error::KickedFromServer);
                }
                other => dispatch(self, active, other),
            }
        }
        Err(server_closed())
    }

    /// Disconnects the server and listener, closes peer and distributed connections,
    /// and fails pending waits, searches and transfers. Does nothing if disconnected.
    pub async fn disconnect(&self, message: &str, error: Option<Error>) {
        self.disconnect_generation(None, message, error);
    }

    fn disconnect_generation(&self, generation: Option<u64>, message: &str, error: Option<Error>) {
        let mut lifecycle = self.inner.server.lock();
        if generation.is_none_or(|generation| lifecycle.current(generation)) {
            self.disconnect_locked(&mut lifecycle, message, error);
        }
    }

    fn disconnect_locked(&self, lifecycle: &mut Lifecycle, message: &str, error: Option<Error>) {
        let Some(active) = lifecycle.active.take() else {
            return;
        };
        let error = error.map(Arc::new);
        self.inner.set_state(
            ClientState::DISCONNECTING,
            Some(message.into()),
            error.clone(),
        );
        drop(active);
        self.inner.waiter.cancel_all();
        self.inner.peers.on_disconnected();
        self.inner.distributed.on_disconnected();
        self.inner.searches.on_disconnected();
        self.inner.transfers.on_disconnected();
        self.inner.set_state(
            ClientState::DISCONNECTED,
            Some(message.into()),
            error.clone(),
        );
        self.inner.emit(Event::Disconnected {
            message: message.into(),
            error,
        });
    }

    /// Applies `patch`, returning whether a logged-in session must reconnect for
    /// all settings to take effect. A failed listener bind rejects the patch.
    /// Established connections keep their options; listener and rate limits update
    /// immediately.
    pub async fn reconfigure(&self, patch: ClientOptionsPatch) -> Result<bool> {
        let _configuration = self.inner.server.configuration.lock().await;
        let old = self.options();
        let options = old.with_patch(&patch);
        options.validate()?;
        let (live, reconnect) = {
            let mut lifecycle = self.inner.server.lock();
            if old.listener_changed_by(&patch) {
                self.reconfigure_listener(lifecycle.active.as_mut(), &old, &options)?;
            }
            let live = if self.state().is_logged_in() {
                lifecycle
                    .active
                    .as_ref()
                    .and_then(|active| Some((active.transport.clone()?, active.cancel.clone())))
            } else {
                None
            };
            if old.maximum_upload_speed != options.maximum_upload_speed {
                self.inner
                    .upload_limiter
                    .set_rate(u64::from(options.maximum_upload_speed) * 1024);
            }
            if old.maximum_download_speed != options.maximum_download_speed {
                self.inner
                    .download_limiter
                    .set_rate(u64::from(options.maximum_download_speed) * 1024);
            }
            *self
                .inner
                .options
                .write()
                .unwrap_or_else(|e| e.into_inner()) = options.clone();
            if live.is_some() {
                self.inner.distributed.on_reconfigured(self, &old, &options);
            }
            let reconnect = live.is_some() && old.reconnect_required_for(&patch);
            (live, reconnect)
        };

        if let Some((transport, cancel)) = live {
            let update = async {
                if advertised_port(&old) != advertised_port(&options) {
                    transport
                        .send(
                            SetListenPortCommand {
                                port: advertised_port(&options),
                            }
                            .to_bytes(),
                        )
                        .await?;
                }
                if old.accept_private_room_invitations != options.accept_private_room_invitations {
                    transport
                        .send(
                            PrivateRoomToggle {
                                accept_invitations: options.accept_private_room_invitations,
                            }
                            .to_bytes(),
                        )
                        .await?;
                }
                Ok(reconnect)
            };
            tokio::select! {
                biased;
                _ = cancel.cancelled() => Ok(false),
                result = update => result,
            }
        } else {
            Ok(false)
        }
    }

    fn reconfigure_listener(
        &self,
        active: Option<&mut Active>,
        old: &ClientOptions,
        options: &ClientOptions,
    ) -> Result<()> {
        let old_endpoint = listen_endpoint(old);
        let new_endpoint = listen_endpoint(options);
        let mut probed_socket = if let Some(endpoint) = new_endpoint
            && (old_endpoint != new_endpoint || (!old.enable_listener && options.enable_listener))
        {
            Some(Listener::bind(endpoint, options.listen_backlog)?)
        } else {
            None
        };
        let Some(active) = active else { return Ok(()) };
        let was_listening = active.listener.take().is_some();
        if let Some(endpoint) = new_endpoint.filter(|_| options.enable_listener) {
            let socket = match probed_socket.take() {
                Some(socket) => socket,
                None => match Listener::bind(endpoint, options.listen_backlog) {
                    Ok(socket) => socket,
                    Err(error) => {
                        if was_listening && let Some(old_endpoint) = old_endpoint {
                            match Listener::bind(old_endpoint, old.listen_backlog) {
                                Ok(socket) => {
                                    active.listener = Some(self.spawn_listener(socket, old))
                                }
                                Err(restore_error) => {
                                    tracing::warn!(%restore_error, "Could not restore Soulseek peer listener")
                                }
                            }
                        }
                        return Err(error);
                    }
                },
            };
            active.listener = Some(self.spawn_listener(socket, options));
        }
        Ok(())
    }

    fn spawn_listener(&self, socket: TcpListener, options: &ClientOptions) -> Listener {
        Listener::spawn(
            socket,
            options.incoming_connection_options.clone(),
            Arc::downgrade(self.inner()),
        )
    }
}

async fn resolve(address: &str, port: u16) -> Result<SocketAddr> {
    let mut endpoints = lookup_host((address, port)).await.map_err(|error| {
        Error::Address(format!("Failed to resolve address '{address}': {error}"))
    })?;
    endpoints.next().ok_or_else(|| {
        Error::Address(format!(
            "Failed to resolve address '{address}': no addresses returned"
        ))
    })
}

fn listen_endpoint(options: &ClientOptions) -> Option<SocketAddr> {
    options
        .listen_port
        .map(|port| SocketAddr::new(options.listen_ip_address, port))
}

fn advertised_port(options: &ClientOptions) -> u32 {
    if options.enable_listener {
        u32::from(options.listen_port.unwrap_or(0))
    } else {
        0
    }
}

async fn send_distributed(
    transport: &MessageConnection,
    options: &ClientOptions,
    info: DistributedNetworkInfo,
    username: &str,
) -> Result<()> {
    let has_parent = options.enable_distributed_network && info.has_parent;
    let accepted = options.enable_distributed_network
        && options.accept_distributed_children
        && (has_parent || info.is_branch_root)
        && info.children.len() < options.distributed_child_limit as usize;
    let root = if has_parent {
        info.branch_root
    } else {
        username.into()
    };
    let level = if has_parent { info.branch_level } else { 0 };
    transport
        .send(BranchLevelCommand { level }.to_bytes())
        .await?;
    transport
        .send(BranchRootCommand { username: root }.to_bytes())
        .await?;
    transport
        .send(AcceptChildrenCommand { accepted }.to_bytes())
        .await?;
    transport
        .send(
            HaveNoParentsCommand {
                have_no_parents: options.enable_distributed_network && !has_parent,
            }
            .to_bytes(),
        )
        .await
}

fn connect_errors(error: Error) -> (Error, Error) {
    let (make, message): (fn(String) -> Error, _) = match error {
        Error::Argument(message) => (Error::Argument, message),
        Error::Address(message) => (Error::Address, message),
        Error::Listen(message) => (Error::Listen, message),
        Error::Timeout(message) => (Error::Timeout, message),
        Error::LoginRejected(message) => (Error::LoginRejected, message),
        Error::KickedFromServer => return (Error::KickedFromServer, Error::KickedFromServer),
        Error::Cancelled => return (Error::Cancelled, Error::Cancelled),
        other => (Error::Other, format!("Failed to connect: {other}")),
    };
    (make(message.clone()), make(message))
}

fn server_closed() -> Error {
    Error::Connection("The server connection closed".into())
}

fn decode(body: Bytes) -> Option<ServerIncoming> {
    match ServerIncoming::decode(body) {
        Ok(message) => Some(message),
        Err(error) => {
            tracing::debug!(%error, "Invalid Soulseek server message");
            None
        }
    }
}

async fn read_server(
    client: Weak<ClientInner>,
    generation: u64,
    cancel: CancellationToken,
    mut incoming: mpsc::Receiver<Bytes>,
) {
    loop {
        let body = tokio::select! {
            biased;
            _ = cancel.cancelled() => return,
            body = incoming.recv() => body,
        };
        let Some(inner) = client.upgrade() else {
            return;
        };
        let client = SoulseekClient::from_inner(inner);
        let mut lifecycle = client.inner.server.lock();
        if !lifecycle.current(generation) {
            return;
        }
        let Some(body) = body else {
            client.disconnect_locked(
                &mut lifecycle,
                "The server connection closed",
                Some(server_closed()),
            );
            return;
        };
        match decode(body) {
            Some(ServerIncoming::KickedFromServer) => {
                client.inner.emit(Event::KickedFromServer);
                client.disconnect_locked(
                    &mut lifecycle,
                    "Kicked from server",
                    Some(Error::KickedFromServer),
                );
                return;
            }
            Some(ServerIncoming::Login(_)) | None => {}
            Some(message) => {
                if let Some(active) = lifecycle.active.as_mut() {
                    dispatch(&client, active, message);
                }
            }
        }
    }
}

fn dispatch(client: &SoulseekClient, active: &mut Active, message: ServerIncoming) {
    let inner = client.inner();
    match message {
        ServerIncoming::PeerAddress(response) => {
            let endpoint = SocketAddr::from(response.endpoint());
            if endpoint.ip().is_unspecified() && endpoint.port() == 0 {
                active.endpoints.remove(&response.username);
            } else {
                let now = Instant::now();
                if active.endpoints.len() >= 1024 {
                    active.endpoints.retain(|_, (_, expires)| *expires > now);
                    if active.endpoints.len() >= 1024 {
                        active.endpoints.clear();
                    }
                }
                active.endpoints.insert(
                    response.username.clone(),
                    (endpoint, now + Duration::from_secs(30)),
                );
            }
            let key = crate::net::waiter::WaitKey("peer_address", response.username);
            inner.waiter.complete(&key, endpoint);
        }
        ServerIncoming::ConnectToPeer(response) => {
            inner.peers.handle_connect_to_peer(client, response)
        }
        ServerIncoming::CannotConnect(message) => {
            inner
                .peers
                .handle_cannot_connect(client, message.token, message.username);
        }
        ServerIncoming::ParentMinSpeed(_)
        | ServerIncoming::ParentSpeedRatio(_)
        | ServerIncoming::ParentInactivityTimeout(_)
        | ServerIncoming::SearchInactivityTimeout(_)
        | ServerIncoming::MinimumParentsInCache(_)
        | ServerIncoming::DistributedAliveInterval(_)
        | ServerIncoming::WishlistInterval(_)
        | ServerIncoming::EmbeddedMessage(_)
        | ServerIncoming::NetInfo(_)
        | ServerIncoming::DistributedReset
        | ServerIncoming::FileSearch(_) => {
            let update = match message {
                ServerIncoming::ParentMinSpeed(value) => Some((Some(value), None, None)),
                ServerIncoming::ParentSpeedRatio(value) => Some((None, Some(value), None)),
                ServerIncoming::WishlistInterval(value) => {
                    Some((None, None, Some(Duration::from_secs(u64::from(value)))))
                }
                _ => None,
            };
            if let Some((min_speed, speed_ratio, interval)) = update {
                let info = {
                    let mut session = inner.session.write().unwrap_or_else(|e| e.into_inner());
                    let info = &mut session.server_info;
                    info.parent_min_speed = min_speed.or(info.parent_min_speed);
                    info.parent_speed_ratio = speed_ratio.or(info.parent_speed_ratio);
                    info.wishlist_interval = interval.or(info.wishlist_interval);
                    info.clone()
                };
                inner.emit(Event::ServerInfoReceived(info));
            }
            inner.distributed.handle_server_message(client, &message);
        }
        other => {
            if let Some(transport) = active.transport.as_ref() {
                super::server_ops::dispatch(client, transport, other);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use bytes::Buf;

    use super::*;
    use crate::net::test_support::{FakeServer, read_frame, write_frame};
    use crate::protocol::codec::MessageBuilder;
    use crate::protocol::codes::ServerCode;
    use crate::resolvers::DefaultResolvers;

    #[tokio::test]
    async fn login_batch_and_remote_disconnect() {
        let server = FakeServer::bind().await;
        let port = server.addr().port();
        let client = SoulseekClient::new(
            ClientOptions {
                enable_listener: false,
                enable_distributed_network: false,
                ..ClientOptions::default()
            },
            Arc::new(DefaultResolvers),
        );
        let mut events = client.subscribe();
        let server = async {
            let mut stream = server.accept().await;
            assert_eq!(
                read_frame(&mut stream).await.get_u32_le(),
                ServerCode::Login as u32
            );
            assert_eq!(
                read_frame(&mut stream).await.get_u32_le(),
                ServerCode::SetListenPort as u32
            );
            let login = MessageBuilder::new()
                .code_u32(ServerCode::Login as u32)
                .bool(true)
                .string("")
                .ip(Ipv4Addr::LOCALHOST)
                .string("")
                .bool(true)
                .build();
            write_frame(&mut stream, &login[4..]).await;
            while read_frame(&mut stream).await.get_u32_le() != ServerCode::Ping as u32 {}
            write_frame(&mut stream, &(ServerCode::Ping as u32).to_le_bytes()).await;
            assert_eq!(
                read_frame(&mut stream).await.get_u32_le(),
                ServerCode::Ping as u32
            );
            // Close without replying to the second ping.
        };
        let caller = async {
            client
                .connect("127.0.0.1", port, "user", "password")
                .await
                .unwrap();
            assert_eq!(client.server_info().is_supporter, Some(true));
            client.ping_server().await.unwrap();
            assert!(matches!(
                client.ping_server().await,
                Err(Error::Connection(_))
            ));
            while !matches!(events.recv().await.unwrap(), Event::Disconnected { .. }) {}
            assert_eq!(client.state(), ClientState::DISCONNECTED);
        };
        tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(server, caller)
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn disabled_auto_ack_leaves_received_message_unacknowledged() {
        let server = FakeServer::bind().await;
        let port = server.addr().port();
        let client = SoulseekClient::new(
            ClientOptions {
                enable_listener: false,
                enable_distributed_network: false,
                auto_acknowledge_private_messages: false,
                ..ClientOptions::default()
            },
            Arc::new(DefaultResolvers),
        );
        let mut events = client.subscribe();
        let protocol = async {
            let mut stream = server.accept().await;
            assert_eq!(
                read_frame(&mut stream).await.get_u32_le(),
                ServerCode::Login as u32
            );
            assert_eq!(
                read_frame(&mut stream).await.get_u32_le(),
                ServerCode::SetListenPort as u32
            );
            let login = MessageBuilder::new()
                .code_u32(ServerCode::Login as u32)
                .bool(true)
                .string("")
                .ip(Ipv4Addr::LOCALHOST)
                .string("")
                .bool(false)
                .build();
            write_frame(&mut stream, &login[4..]).await;
            let message = crate::PrivateMessage {
                id: 42,
                timestamp: std::time::UNIX_EPOCH,
                username: "peer".into(),
                message: "hello".into(),
                is_admin: false,
                replayed: false,
            };
            write_frame(&mut stream, &message.to_bytes()[4..]).await;
            loop {
                let code = read_frame(&mut stream).await.get_u32_le();
                if code == ServerCode::Ping as u32 {
                    break;
                }
                assert_ne!(code, ServerCode::AcknowledgePrivateMessage as u32);
            }
            write_frame(&mut stream, &(ServerCode::Ping as u32).to_le_bytes()).await;
            let mut ack = read_frame(&mut stream).await;
            assert_eq!(
                ack.get_u32_le(),
                ServerCode::AcknowledgePrivateMessage as u32
            );
            assert_eq!(ack.get_u32_le(), 43);
        };
        let caller = async {
            client
                .connect("127.0.0.1", port, "user", "password")
                .await
                .unwrap();
            loop {
                if let Event::PrivateMessageReceived(message) = events.recv().await.unwrap() {
                    assert_eq!(message.id, 42);
                    break;
                }
            }
            client.ping_server().await.unwrap();
            client.acknowledge_private_message(43).await.unwrap();
        };
        tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(protocol, caller)
        })
        .await
        .unwrap();
    }
}
