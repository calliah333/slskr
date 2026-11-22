//! Peer connection establishment, cache ownership and peer requests.

use std::collections::{HashMap, hash_map::Entry};
use std::future::Future;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use bytes::Bytes;
use tokio::sync::{oneshot, watch};
use tokio_util::sync::CancellationToken;

use super::{SoulseekClient, peer_handler, require_non_blank};
use crate::error::{Error, Result};
use crate::events::Event;
use crate::model::{BrowseResponse, Directory, UserInfo};
use crate::net::connection::Connection;
use crate::net::message_connection::MessageConnection;
use crate::net::waiter::WaitKey;
use crate::options::{BrowseOptions, ClientOptions, ConnectionOptions};
use crate::protocol::init::{PeerInit, PierceFirewall};
use crate::protocol::peer::{BrowseRequest, FolderContentsRequest, UserInfoRequest};
use crate::protocol::server::{CannotConnect, ConnectToPeerRequest, ConnectToPeerResponse};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Peer,
    File,
    Distributed,
}

impl Kind {
    fn code(self) -> &'static str {
        match self {
            Self::Peer => "P",
            Self::File => "F",
            Self::Distributed => "D",
        }
    }

    fn options(self, options: &ClientOptions) -> &ConnectionOptions {
        match self {
            Self::Peer => &options.peer_connection_options,
            Self::File => &options.transfer_connection_options,
            Self::Distributed => &options.distributed_connection_options,
        }
    }
}

struct Solicitation {
    id: u64,
    username: String,
    result: oneshot::Sender<Result<Connection>>,
}

/// Removes only its own entry on timeout, cancellation, or a direct win.
struct PendingSolicitation<'a> {
    manager: &'a PeerManager,
    token: u32,
    id: u64,
}

impl Drop for PendingSolicitation<'_> {
    fn drop(&mut self) {
        let mut state = self.manager.lock();
        if state
            .solicitations
            .get(&self.token)
            .is_some_and(|entry| entry.id == self.id)
        {
            state.solicitations.remove(&self.token);
        }
    }
}

type MessageDial = watch::Sender<Option<Result<MessageConnection>>>;

#[derive(Default)]
struct PeerSockets {
    cached: Option<MessageConnection>,
    // A replacement need not interrupt work already using the previous socket.
    uncached: Vec<MessageConnection>,
}

impl PeerSockets {
    fn cached(&self) -> Option<&MessageConnection> {
        self.cached.as_ref().filter(|conn| !conn.is_closed())
    }

    fn uncache(&mut self) {
        if let Some(conn) = self.cached.take().filter(|conn| !conn.is_closed()) {
            self.uncached.push(conn);
        }
    }
}

#[derive(Default)]
struct State {
    session: CancellationToken,
    sockets: HashMap<String, PeerSockets>,
    message_dials: HashMap<String, Arc<MessageDial>>,
    reverse_message_dials: HashMap<String, Arc<CancellationToken>>,
    solicitations: HashMap<u32, Solicitation>,
    next_solicitation_id: u64,
}

pub(crate) struct PeerManager {
    state: Mutex<State>,
}

impl PeerManager {
    pub(crate) fn new() -> Self {
        Self {
            state: Mutex::new(State::default()),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) async fn get_message_connection(
        &self,
        client: &SoulseekClient,
        username: &str,
        cancel: &CancellationToken,
    ) -> Result<MessageConnection> {
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let mut result = {
            let mut state = self.lock();
            if let Some(conn) = state.sockets.get(username).and_then(PeerSockets::cached) {
                return Ok(conn.clone());
            }
            let session = state.session.clone();
            match state.message_dials.entry(username.to_owned()) {
                Entry::Occupied(dial) if dial.get().receiver_count() != 0 => dial.get().subscribe(),
                entry => {
                    let (dial, result) = watch::channel(None);
                    let dial = Arc::new(dial);
                    let username = entry.key().clone();
                    match entry {
                        Entry::Occupied(mut entry) => {
                            entry.insert(dial.clone());
                        }
                        Entry::Vacant(entry) => {
                            entry.insert(dial.clone());
                        }
                    }
                    let client = client.clone();
                    tokio::spawn(async move {
                        client
                            .inner()
                            .peers
                            .dial_message(&client, &username, &session, &dial)
                            .await;
                    });
                    result
                }
            }
        };
        tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(Error::Cancelled),
            update = result.wait_for(Option::is_some) => {
                let update = update.map_err(|_| disconnected())?;
                match update.as_ref().expect("waited for a dial result") {
                    Ok(conn) => Ok(conn.clone()),
                    Err(error) => Err(copy_dial_error(error)),
                }
            }
        }
    }

    async fn dial_message(
        &self,
        client: &SoulseekClient,
        username: &str,
        session: &CancellationToken,
        dial: &Arc<MessageDial>,
    ) {
        let connect = async {
            let endpoint = client.get_user_endpoint(username).await?;
            self.connect(
                client,
                username,
                endpoint,
                Kind::Peer,
                client.get_next_token(),
                session,
            )
            .await
        };
        let result = tokio::select! {
            biased;
            _ = session.cancelled() => Err(disconnected()),
            _ = dial.closed() => Err(Error::Cancelled),
            result = connect => result,
        };
        let mut state = self.lock();
        if !state
            .message_dials
            .get(username)
            .is_some_and(|active| Arc::ptr_eq(active, dial))
        {
            return;
        }
        match result {
            Ok(conn) if !session.is_cancelled() && dial.receiver_count() != 0 => {
                self.install_message(client, &mut state, username.to_owned(), conn, false);
            }
            result => {
                state.message_dials.remove(username);
                let _ = dial.send(Some(Err(result.err().unwrap_or_else(disconnected))));
            }
        }
    }

    pub(crate) async fn get_transfer_connection(
        &self,
        client: &SoulseekClient,
        username: &str,
        token: u32,
        cancel: &CancellationToken,
    ) -> Result<Connection> {
        let session = self.lock().session.clone();
        tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(Error::Cancelled),
            _ = session.cancelled() => Err(disconnected()),
            result = async {
                let endpoint = client.get_user_endpoint(username).await?;
                self.connect(client, username, endpoint, Kind::File, token, &session).await
            } => result,
        }
    }

    pub(crate) async fn get_distributed_connection(
        &self,
        client: &SoulseekClient,
        username: &str,
        endpoint: SocketAddr,
        cancel: &CancellationToken,
    ) -> Result<Connection> {
        let session = self.lock().session.clone();
        tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(Error::Cancelled),
            _ = session.cancelled() => Err(disconnected()),
            result = self.connect(client, username, endpoint, Kind::Distributed, client.get_next_token(), &session) => result,
        }
    }

    async fn connect(
        &self,
        client: &SoulseekClient,
        username: &str,
        endpoint: SocketAddr,
        kind: Kind,
        init_token: u32,
        session: &CancellationToken,
    ) -> Result<Connection> {
        let options = kind
            .options(
                &client
                    .inner()
                    .options
                    .read()
                    .unwrap_or_else(|e| e.into_inner()),
            )
            .clone();
        let local_username = client.username().ok_or_else(disconnected)?;
        let direct = async {
            let mut conn = Connection::connect(endpoint, &options).await?;
            let init = PeerInit {
                username: local_username,
                connection_type: kind.code().into(),
                token: init_token,
            };
            conn.write_all(&init.to_bytes()).await?;
            Ok::<_, Error>(conn)
        };
        let indirect = self.solicit(
            client,
            username,
            kind,
            init_token,
            options.connect_timeout,
            session,
        );
        // One rejected or unreachable path must not cancel the other.
        tokio::pin!(direct, indirect);
        let (direct_error, indirect_error) = tokio::select! {
            result = &mut direct => match result {
                Ok(conn) => return Ok(conn),
                Err(error) => match indirect.await {
                    Ok(conn) => return Ok(conn),
                    Err(indirect_error) => (error, indirect_error),
                },
            },
            result = &mut indirect => match result {
                Ok(conn) => return Ok(conn),
                Err(error) => match direct.await {
                    Ok(conn) => return Ok(conn),
                    Err(direct_error) => (direct_error, error),
                },
            },
        };
        let message = format!(
            "Failed to establish a {} connection to {username} ({endpoint}): direct: {direct_error}; indirect: {indirect_error}",
            kind.code(),
        );
        if direct_error.is_timeout() || indirect_error.is_timeout() {
            Err(Error::Timeout(message))
        } else {
            Err(Error::Connection(message))
        }
    }

    async fn solicit(
        &self,
        client: &SoulseekClient,
        username: &str,
        kind: Kind,
        init_token: u32,
        timeout: Duration,
        session: &CancellationToken,
    ) -> Result<Connection> {
        let (pending, result) = {
            let mut state = self.lock();
            if session.is_cancelled() {
                return Err(disconnected());
            }
            let token = loop {
                let token = client.get_next_token();
                if token != init_token && !state.solicitations.contains_key(&token) {
                    break token;
                }
            };
            let id = state.next_solicitation_id;
            state.next_solicitation_id = id.wrapping_add(1);
            let (send, result) = oneshot::channel();
            state.solicitations.insert(
                token,
                Solicitation {
                    id,
                    username: username.to_owned(),
                    result: send,
                },
            );
            (
                PendingSolicitation {
                    manager: self,
                    token,
                    id,
                },
                result,
            )
        };
        tokio::time::timeout(timeout, async {
            client
                .inner()
                .send_server(
                    ConnectToPeerRequest {
                        token: pending.token,
                        username: username.to_owned(),
                        connection_type: kind.code().into(),
                    }
                    .to_bytes(),
                )
                .await?;
            result.await.map_err(|_| disconnected())?
        })
        .await?
    }

    pub(crate) async fn send_to(
        &self,
        client: &SoulseekClient,
        username: &str,
        frame: Bytes,
        cancel: &CancellationToken,
    ) -> Result<()> {
        let conn = self
            .get_message_connection(client, username, cancel)
            .await?;
        cancellable(cancel, conn.send(frame)).await
    }

    /// Looks up an existing source socket, not its user's replacement in the cache.
    /// Intended for transfer messages whose dispatch hook takes a remote endpoint.
    pub(crate) fn message_connection_at(
        &self,
        username: &str,
        remote: SocketAddr,
    ) -> Option<MessageConnection> {
        let state = self.lock();
        let sockets = state.sockets.get(username)?;
        let mut matching = sockets
            .cached
            .iter()
            .chain(&sockets.uncached)
            .filter(|conn| !conn.is_closed() && conn.remote() == remote);
        let conn = matching.next()?;
        matching.next().is_none().then(|| conn.clone())
    }

    pub(crate) fn handle_incoming_peer_init(
        &self,
        client: &SoulseekClient,
        username: String,
        _token: u32,
        conn: Connection,
    ) {
        // The init token is assigned by the peer, not one of our solicitations.
        let mut state = self.lock();
        if !client.state().is_logged_in() {
            return;
        }
        if let Some(reverse) = state.reverse_message_dials.remove(&username) {
            reverse.cancel();
        }
        self.install_message(client, &mut state, username, conn, true);
    }

    pub(crate) fn handle_pierce_firewall(
        &self,
        _client: &SoulseekClient,
        token: u32,
        conn: Connection,
    ) {
        if let Some(pending) = self.lock().solicitations.remove(&token) {
            // The initiating operation already knows whether this is a P, F or D socket.
            let _ = pending.result.send(Ok(conn));
        }
    }

    pub(crate) fn handle_cannot_connect(
        &self,
        client: &SoulseekClient,
        token: u32,
        username: Option<String>,
    ) {
        let username = username.filter(|username| !username.is_empty());
        let pending = {
            let mut state = self.lock();
            if state.solicitations.get(&token).is_some_and(|pending| {
                username
                    .as_ref()
                    .is_none_or(|username| pending.username == *username)
            }) {
                state.solicitations.remove(&token)
            } else {
                None
            }
        };
        if let Some(pending) = pending {
            let _ = pending.result.send(Err(Error::Connection(format!(
                "{} could not connect for solicitation {token}",
                pending.username,
            ))));
        }
        if let Some(username) = username {
            client
                .inner()
                .emit(Event::UserCannotConnect { username, token });
        }
    }

    pub(crate) fn handle_connect_to_peer(
        &self,
        client: &SoulseekClient,
        response: ConnectToPeerResponse,
    ) {
        let kind = match response.connection_type.as_str() {
            "P" => Kind::Peer,
            "F" => Kind::File,
            "D" => Kind::Distributed,
            _ => return,
        };
        if kind == Kind::File
            && !client
                .inner()
                .downloads
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .values()
                .any(|download| {
                    download.username == response.username && !download.state.is_completed()
                })
        {
            return;
        }
        let child_scope = if kind == Kind::Distributed {
            let Some(scope) = client.inner().distributed.child_scope() else {
                return;
            };
            Some(scope)
        } else {
            None
        };
        let (session, reverse) = {
            let mut state = self.lock();
            if !client.state().is_logged_in() {
                return;
            }
            let reverse = if kind == Kind::Peer {
                if state.reverse_message_dials.contains_key(&response.username) {
                    return;
                }
                let reverse = Arc::new(state.session.child_token());
                state
                    .reverse_message_dials
                    .insert(response.username.clone(), reverse.clone());
                Some(reverse)
            } else {
                None
            };
            (state.session.clone(), reverse)
        };
        let client = client.clone();
        tokio::spawn(async move {
            client
                .inner()
                .peers
                .connect_back(&client, response, kind, session, reverse, child_scope)
                .await;
        });
    }

    async fn connect_back(
        &self,
        client: &SoulseekClient,
        response: ConnectToPeerResponse,
        kind: Kind,
        session: CancellationToken,
        reverse: Option<Arc<CancellationToken>>,
        child_scope: Option<CancellationToken>,
    ) {
        let cancel = reverse.as_deref().unwrap_or(&session);
        let options = kind
            .options(
                &client
                    .inner()
                    .options
                    .read()
                    .unwrap_or_else(|e| e.into_inner()),
            )
            .clone();
        let conn = cancellable(cancel, async {
            let mut conn = Connection::connect(response.endpoint().into(), &options).await?;
            conn.write_all(
                &PierceFirewall {
                    token: response.token,
                }
                .to_bytes(),
            )
            .await?;
            Ok(conn)
        });
        let conn = if let Some(scope) = &child_scope {
            tokio::select! {
                biased;
                _ = scope.cancelled() => return,
                result = conn => result,
            }
        } else {
            conn.await
        };
        let conn = if let Some(reverse) = reverse {
            let mut state = self.lock();
            if !state
                .reverse_message_dials
                .get(&response.username)
                .is_some_and(|active| Arc::ptr_eq(active, &reverse))
            {
                return;
            }
            state.reverse_message_dials.remove(&response.username);
            match conn {
                Ok(conn) => {
                    self.install_message(client, &mut state, response.username, conn, false);
                    return;
                }
                Err(error) => Err(error),
            }
        } else {
            conn
        };
        if session.is_cancelled()
            || child_scope
                .as_ref()
                .is_some_and(CancellationToken::is_cancelled)
        {
            return;
        }
        match conn {
            Ok(conn) if kind == Kind::File => {
                // The first raw u32 is the transfer token, not response.token.
                client
                    .inner()
                    .transfers
                    .handle_incoming_transfer_connection(client, response.username, conn);
            }
            Ok(conn) if kind == Kind::Distributed => {
                let scope = child_scope.expect("captured reverse distributed scope");
                client.inner().distributed.handle_scoped_incoming_child(
                    client,
                    response.username,
                    conn,
                    &scope,
                );
            }
            Ok(_) => unreachable!("P connections are installed above"),
            Err(error) => {
                tracing::debug!(username = %response.username, token = response.token, %error, "Could not connect back to peer");
                let _ = cancellable(
                    &session,
                    client.inner().send_server(
                        CannotConnect {
                            token: response.token,
                            username: Some(response.username),
                        }
                        .to_bytes(),
                    ),
                )
                .await;
            }
        }
    }

    fn install_message(
        &self,
        client: &SoulseekClient,
        state: &mut State,
        username: String,
        conn: Connection,
        supersede: bool,
    ) {
        let (conn, incoming) = MessageConnection::spawn(conn, Some(username.clone()));
        let sockets = state.sockets.entry(username.clone()).or_default();
        if supersede || sockets.cached().is_none() {
            sockets.uncache();
            sockets.cached = Some(conn.clone());
        } else {
            sockets.uncached.push(conn.clone());
        }
        if let Some(dial) = state.message_dials.remove(&username) {
            let _ = dial.send(Some(Ok(sockets
                .cached
                .as_ref()
                .expect("installed peer socket")
                .clone())));
        }
        peer_handler::spawn(client, username, conn, incoming, state.session.clone());
    }

    pub(super) fn forget_message(&self, username: &str, id: u64) {
        let mut state = self.lock();
        if let Some(sockets) = state.sockets.get_mut(username) {
            if sockets.cached.as_ref().is_some_and(|conn| conn.id() == id) {
                sockets.cached = None;
            } else {
                sockets.uncached.retain(|conn| conn.id() != id);
            }
            if sockets.cached.is_none() && sockets.uncached.is_empty() {
                state.sockets.remove(username);
            }
        }
    }

    fn invalidate(&self, username: &str) {
        if let Some(sockets) = self.lock().sockets.get_mut(username) {
            sockets.uncache();
        }
    }

    pub(crate) fn on_disconnected(&self) {
        let sockets = {
            let mut state = self.lock();
            state.session.cancel();
            state.session = CancellationToken::new();
            for (_, dial) in state.message_dials.drain() {
                let _ = dial.send(Some(Err(disconnected())));
            }
            state.reverse_message_dials.clear();
            state.solicitations.clear();
            std::mem::take(&mut state.sockets)
        };
        for sockets in sockets.into_values() {
            for conn in sockets.cached.into_iter().chain(sockets.uncached) {
                conn.close();
            }
        }
    }
}

impl Drop for PeerManager {
    fn drop(&mut self) {
        self.on_disconnected();
    }
}

fn disconnected() -> Error {
    Error::Connection("Peer connections were disconnected".into())
}

fn copy_dial_error(error: &Error) -> Error {
    match error {
        Error::Cancelled => Error::Cancelled,
        Error::Timeout(message) => Error::Timeout(message.clone()),
        Error::Connection(message) => Error::Connection(message.clone()),
        Error::UserOffline(message) => Error::UserOffline(message.clone()),
        Error::UserEndPoint(message) => Error::UserEndPoint(message.clone()),
        Error::InvalidState(message) => Error::InvalidState(message.clone()),
        error => Error::Other(error.to_string()),
    }
}

async fn cancellable<T>(
    cancel: &CancellationToken,
    operation: impl Future<Output = Result<T>>,
) -> Result<T> {
    tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(Error::Cancelled),
        result = operation => result,
    }
}

async fn request<T: Send + 'static>(
    client: &SoulseekClient,
    username: &str,
    message: Bytes,
    key: impl FnOnce(u64) -> WaitKey,
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<T> {
    let conn = client
        .inner()
        .peers
        .get_message_connection(client, username, cancel)
        .await?;
    cancellable(cancel, async {
        let response = client.inner().waiter.wait_indefinitely(key(conn.id()));
        conn.send(message).await?;
        tokio::time::timeout(timeout, response).await?
    })
    .await
}

fn operation_error(error: Error, operation: impl FnOnce() -> String) -> Error {
    match error {
        Error::Timeout(_) | Error::Cancelled | Error::UserOffline(_) => error,
        error => Error::Other(format!("{}: {error}", operation())),
    }
}

impl SoulseekClient {
    /// Fetches the share list of `username`.
    ///
    /// Sends BrowseRequest over the peer message connection, then waits up to
    /// [`BrowseOptions::response_timeout`] for the complete response.
    /// The framed transport reports [`Event::BrowseProgressUpdated`] upon completion.
    pub async fn browse(
        &self,
        username: &str,
        options: BrowseOptions,
        cancel: CancellationToken,
    ) -> Result<BrowseResponse> {
        require_non_blank(username, "username")?;
        self.inner().ensure_logged_in("browse")?;
        let (response, size) = request(
            self,
            username,
            BrowseRequest.to_bytes(),
            peer_handler::browse_key,
            options.response_timeout,
            &cancel,
        )
        .await
        .map_err(|error| operation_error(error, || format!("Failed to browse user {username}")))?;
        self.inner().emit(Event::BrowseProgressUpdated {
            username: username.to_owned(),
            bytes_transferred: size,
            size,
        });
        Ok(response)
    }

    /// Fetches the contents of `directory` shared by `username` (FolderContentsRequest
    /// with `token`, generated when `None`); waits [`ClientOptions::message_timeout`]
    /// for the response keyed by connection and token.
    pub async fn get_directory_contents(
        &self,
        username: &str,
        directory: &str,
        token: Option<u32>,
        cancel: CancellationToken,
    ) -> Result<Vec<Directory>> {
        require_non_blank(username, "username")?;
        require_non_blank(directory, "directory name")?;
        self.inner().ensure_logged_in("get directory contents")?;
        let token = token.unwrap_or_else(|| self.get_next_token());
        let timeout = self
            .inner()
            .options
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .message_timeout;
        request(
            self,
            username,
            FolderContentsRequest {
                token,
                directory_name: directory.to_owned(),
            }
            .to_bytes(),
            |id| peer_handler::directory_key(id, token),
            timeout,
            &cancel,
        )
        .await
        .map_err(|error| {
            operation_error(error, || {
                format!("Failed to retrieve directory contents for {directory} from {username}")
            })
        })
    }

    /// Fetches the user info of `username` (InfoRequest → InfoResponse).
    pub async fn get_user_info(
        &self,
        username: &str,
        cancel: CancellationToken,
    ) -> Result<UserInfo> {
        require_non_blank(username, "username")?;
        self.inner().ensure_logged_in("retrieve user info")?;
        let timeout = self
            .inner()
            .options
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .message_timeout;
        request(
            self,
            username,
            UserInfoRequest.to_bytes(),
            peer_handler::info_key,
            timeout,
            &cancel,
        )
        .await
        .map_err(|error| {
            operation_error(error, || {
                format!("Failed to retrieve information for user {username}")
            })
        })
    }

    /// Establishes (or reuses) a peer message connection to `username`;
    /// `invalidate_cache` drops a cached connection first.
    pub async fn connect_to_user(
        &self,
        username: &str,
        invalidate_cache: bool,
        cancel: CancellationToken,
    ) -> Result<()> {
        require_non_blank(username, "username")?;
        self.inner().ensure_logged_in("connect to a user")?;
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let peers = &self.inner().peers;
        if invalidate_cache {
            peers.invalidate(username);
        }
        peers
            .get_message_connection(self, username, &cancel)
            .await
            .map(|_| ())
            .map_err(|error| {
                operation_error(error, || format!("Failed to connect to user {username}"))
            })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use futures::future::{BoxFuture, ready};
    use tokio::net::TcpStream;

    use super::*;
    use crate::model::ClientState;
    use crate::net::test_support::{FakeServer, read_frame, write_frame};
    use crate::protocol::peer::{FolderContentsResponse, PeerIncoming};
    use crate::resolvers::Resolvers;

    struct TestResolvers;

    impl Resolvers for TestResolvers {
        fn directory_contents(
            &self,
            username: String,
            endpoint: SocketAddr,
            token: u32,
            directory: String,
        ) -> BoxFuture<'static, Result<Vec<Directory>>> {
            assert_eq!(username, "peer");
            assert!(endpoint.ip().is_loopback() && endpoint.port() != 0);
            assert_eq!(token, 7);
            Box::pin(ready(Ok(vec![Directory {
                name: directory,
                files: vec![],
            }])))
        }
    }

    fn logged_in_client() -> SoulseekClient {
        let client = SoulseekClient::new(ClientOptions::default(), Arc::new(TestResolvers));
        client
            .inner()
            .set_state(ClientState::CONNECTED | ClientState::LOGGED_IN, None, None);
        client
    }

    async fn install_peer(client: &SoulseekClient) -> TcpStream {
        let listener = FakeServer::bind().await;
        let remote = TcpStream::connect(listener.addr()).await.unwrap();
        let accepted = listener.accept().await;
        let endpoint = accepted.peer_addr().unwrap();
        let conn = Connection::from_accepted(accepted, endpoint, &ConnectionOptions::default());
        client
            .inner()
            .peers
            .handle_incoming_peer_init(client, "peer".into(), 0, conn);
        remote
    }

    #[tokio::test]
    async fn browse_round_trip_reports_completed_frame() {
        let client = logged_in_client();
        let mut peer = install_peer(&client).await;
        let mut events = client.subscribe();
        let response = BrowseResponse {
            directories: vec![Directory {
                name: "shared".into(),
                files: vec![],
            }],
            ..BrowseResponse::default()
        };
        let body = response.to_bytes().slice(4..);
        let exchange = async {
            let request = async {
                assert!(matches!(
                    PeerIncoming::decode(read_frame(&mut peer).await).unwrap(),
                    PeerIncoming::BrowseRequest(_)
                ));
                write_frame(&mut peer, &body).await;
            };
            let browse = client.browse("peer", BrowseOptions::default(), CancellationToken::new());
            let (actual, ()) = tokio::join!(browse, request);
            assert_eq!(actual.unwrap(), response);
            assert!(
                matches!(events.recv().await.unwrap(), Event::BrowseProgressUpdated {
                username, bytes_transferred, size,
            } if username == "peer" && size == body.len() as u64 && bytes_transferred == size)
            );
        };
        tokio::time::timeout(Duration::from_secs(2), exchange)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn uncached_socket_gets_its_own_reply_and_cannot_complete_replacement_waiter() {
        let client = logged_in_client();
        let mut old = install_peer(&client).await;
        let mut current = install_peer(&client).await;
        let exchange = async {
            let request = FolderContentsRequest {
                token: 7,
                directory_name: "requested".into(),
            }
            .to_bytes();
            write_frame(&mut old, &request[4..]).await;
            let reply = PeerIncoming::decode(read_frame(&mut old).await).unwrap();
            assert!(
                matches!(reply, PeerIncoming::FolderContentsResponse(response)
                if response.token == 7 && response.directory_name == "requested"
                    && response.directories[0].name == "requested")
            );

            let cancel = CancellationToken::new();
            let conn = client
                .inner()
                .peers
                .get_message_connection(&client, "peer", &cancel)
                .await
                .unwrap();
            let wait = client
                .inner()
                .waiter
                .wait_indefinitely::<Vec<Directory>>(peer_handler::directory_key(conn.id(), 9));
            let response = |name: &str| {
                FolderContentsResponse {
                    token: 9,
                    directory_name: "requested".into(),
                    directories: vec![Directory {
                        name: name.into(),
                        files: vec![],
                    }],
                }
                .to_bytes()
            };
            write_frame(&mut old, &response("wrong")[4..]).await;
            let original = client
                .inner()
                .peers
                .message_connection_at("peer", old.local_addr().unwrap())
                .unwrap();
            let old_id = original.id();
            let drained = client
                .inner()
                .waiter
                .wait_indefinitely::<Vec<Directory>>(peer_handler::directory_key(old_id, 10));
            let marker = FolderContentsResponse {
                token: 10,
                directory_name: "marker".into(),
                directories: vec![],
            }
            .to_bytes();
            write_frame(&mut old, &marker[4..]).await;
            drained.await.unwrap();
            drop(old);
            original.closed().await;
            while client.inner().peers.lock().sockets["peer"]
                .uncached
                .iter()
                .any(|conn| conn.id() == old_id)
            {
                tokio::task::yield_now().await;
            }
            assert_eq!(
                client
                    .inner()
                    .peers
                    .get_message_connection(&client, "peer", &cancel)
                    .await
                    .unwrap()
                    .id(),
                conn.id()
            );
            write_frame(&mut current, &response("right")[4..]).await;
            assert_eq!(wait.await.unwrap()[0].name, "right");
        };
        tokio::time::timeout(Duration::from_secs(2), exchange)
            .await
            .unwrap();
    }
}
