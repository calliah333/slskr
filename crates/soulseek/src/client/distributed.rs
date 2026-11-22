//! Owns the distributed tree and the tasks belonging to its current incarnation.

use std::collections::{BTreeMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::Duration;

use bytes::{BufMut, Bytes, BytesMut};
use futures::{StreamExt, stream};
use tokio::sync::{mpsc, watch};
use tokio::time::{Instant, sleep_until, timeout};
use tokio_util::sync::CancellationToken;

use super::{ClientInner, SoulseekClient};
use crate::events::Event;
use crate::model::{DistributedNetworkInfo, DistributedPeer};
use crate::net::{connection::Connection, message_connection::MessageConnection};
use crate::options::ClientOptions;
use crate::protocol::distributed::{
    DistributedBranchLevel, DistributedBranchRoot, DistributedChildDepth, DistributedIncoming,
    DistributedPingRequest, DistributedPingResponse, DistributedSearchRequest,
};
use crate::protocol::server::{
    AcceptChildrenCommand, BranchLevelCommand, BranchRootCommand, ChildDepthCommand,
    HaveNoParentsCommand, NetInfoParent, ServerIncoming,
};

use crate::protocol::{MessageReader, codes::DistributedCode, distributed::EmbeddedMessage};
const CANDIDATE_CONCURRENCY: usize = 16;
const CHILD_QUEUE: usize = 32;

#[derive(Default)]
pub(crate) struct DistributedManager(Mutex<State>);

#[derive(Default)]
struct State {
    scope: Option<Scope>,
    // These notifications may precede the login response.
    candidates: Vec<NetInfoParent>,
    candidate_revision: u64,
    search_timeout: Option<Duration>,
    parent_idle: Option<Duration>,
    ping_interval: Option<Duration>,
}

struct Scope {
    client: Weak<ClientInner>,
    stop: CancellationToken,
    status: Arc<StatusFeed>,
    username: String,
    enabled: bool,
    accept_children: bool,
    child_limit: u32,
    deduplicate: bool,
    root: bool,
    selecting_parent: bool,
    parent: Option<Parent>,
    children: BTreeMap<String, Child>,
    last_search: Option<DistributedSearchRequest>,
    broadcast_latency: Option<f64>,
}

struct Parent {
    peer: DistributedPeer,
    conn: MessageConnection,
    level: i32,
    root: String,
    control: watch::Sender<ParentControl>,
}

struct Child {
    conn: MessageConnection,
    depth: i32,
    outgoing: mpsc::Sender<(Bytes, Option<Instant>)>,
}

// Serialize status writes for the entire login, including across tree resets.
struct StatusFeed {
    stop: CancellationToken,
    updates: watch::Sender<Option<Status>>,
}

impl Drop for StatusFeed {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

#[derive(Clone, PartialEq)]
struct Status {
    level: u32,
    root: String,
    accept: bool,
    no_parents: bool,
    depth: u32,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct ParentControl {
    depth: i32,
    idle: Option<Duration>,
    ping: Option<Duration>,
}

impl Drop for Scope {
    fn drop(&mut self) {
        self.stop.cancel();
        if let Some(parent) = &self.parent {
            parent.conn.close();
        }
        for child in self.children.values() {
            child.conn.close();
        }
    }
}

impl Scope {
    fn level(&self) -> i32 {
        self.parent
            .as_ref()
            .map_or(0, |p| p.level.saturating_add(1))
    }

    fn branch_root(&self) -> &str {
        self.parent.as_ref().map_or(&self.username, |p| &p.root)
    }

    fn can_accept(&self) -> bool {
        self.enabled
            && self.accept_children
            && (self.root || self.parent.is_some())
            && self.children.len() < self.child_limit as usize
    }

    fn depth(&self) -> i32 {
        self.children
            .values()
            .map(|c| c.depth.saturating_add(1))
            .max()
            .unwrap_or(0)
    }

    fn info(&self) -> DistributedNetworkInfo {
        DistributedNetworkInfo {
            average_broadcast_latency: self.broadcast_latency,
            branch_level: self.level() as u32,
            branch_root: self.branch_root().into(),
            is_branch_root: self.root,
            child_limit: self.child_limit,
            can_accept_children: self.can_accept(),
            children: self
                .children
                .iter()
                .map(|(username, child)| DistributedPeer {
                    username: username.clone(),
                    ip_endpoint: child.conn.remote(),
                })
                .collect(),
            parent: self.parent.as_ref().map(|parent| parent.peer.clone()),
            has_parent: self.parent.is_some(),
        }
    }

    fn publish(&self, client: &ClientInner) {
        client.emit(Event::DistributedNetworkStateChanged(self.info()));
        self.publish_status();
    }

    fn publish_status(&self) {
        let depth = self.depth();
        if let Some(parent) = &self.parent {
            parent.control.send_if_modified(|control| {
                if control.depth == depth {
                    return false;
                }
                control.depth = depth;
                true
            });
        }
        let level = self.level() as u32;
        let root = self.branch_root();
        let accept = self.can_accept();
        let no_parents = self.enabled && self.parent.is_none();
        let depth = depth as u32;
        self.status.updates.send_if_modified(|old| {
            if old.as_ref().is_some_and(|old| {
                old.level == level
                    && old.root == root
                    && old.accept == accept
                    && old.no_parents == no_parents
                    && old.depth == depth
            }) {
                return false;
            }
            *old = Some(Status {
                level,
                root: root.into(),
                accept,
                no_parents,
                depth,
            });
            true
        });
    }

    fn branch_frames(&self) -> [Bytes; 2] {
        [
            DistributedBranchLevel {
                level: self.level(),
            }
            .to_bytes(),
            DistributedBranchRoot {
                username: self.branch_root().into(),
            }
            .to_bytes(),
        ]
    }

    fn broadcast_branch(&self) {
        if !self.children.is_empty() {
            for frame in self.branch_frames() {
                self.broadcast(frame, false);
            }
        }
    }

    fn broadcast(&self, frame: Bytes, measure: bool) {
        let start = measure.then(Instant::now);
        for child in self.children.values() {
            if child.outgoing.try_send((frame.clone(), start)).is_err() {
                child.conn.close();
            }
        }
    }

    fn handle_search(
        &mut self,
        client: &SoulseekClient,
        search: DistributedSearchRequest,
        frame: SearchFrame,
    ) {
        if self.deduplicate {
            if self.last_search.as_ref() == Some(&search) {
                return;
            }
            self.last_search = Some(search.clone());
        }
        if !self.children.is_empty() {
            self.broadcast(frame.into_frame(), true);
        }
        if !search.username.eq_ignore_ascii_case(&self.username) {
            client
                .inner()
                .searches
                .respond(client, search.username, search.token, search.query);
        }
    }
}

impl DistributedManager {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) fn info(&self) -> DistributedNetworkInfo {
        self.lock()
            .scope
            .as_ref()
            .map_or_else(DistributedNetworkInfo::default, Scope::info)
    }

    pub(crate) fn on_logged_in(&self, client: &SoulseekClient) {
        let options = client.options();
        let mut state = self.lock();
        Self::start_scope(&mut state, client, &options, false);
    }

    pub(crate) fn on_disconnected(&self) {
        let mut state = self.lock();
        if let Some(old) = state.scope.take()
            && let Some(inner) = old.client.upgrade()
        {
            Self::emit_departures(&old, &inner);
            inner.emit(Event::DistributedNetworkStateChanged(
                DistributedNetworkInfo::default(),
            ));
        }
        *state = State::default();
    }

    pub(crate) fn on_reconfigured(
        &self,
        client: &SoulseekClient,
        old: &ClientOptions,
        options: &ClientOptions,
    ) {
        let mut state = self.lock();
        if old.enable_distributed_network != options.enable_distributed_network
            || (old.accept_distributed_children && !options.accept_distributed_children)
            || old.distributed_connection_options != options.distributed_connection_options
            || old.server_connection_options != options.server_connection_options
            || old.incoming_connection_options != options.incoming_connection_options
            || old.listen_backlog != options.listen_backlog
            || old.enable_listener != options.enable_listener
            || old.listen_port != options.listen_port
            || old.listen_ip_address != options.listen_ip_address
        {
            if !options.enable_distributed_network {
                state.candidates.clear();
            }
            Self::start_scope(&mut state, client, options, true);
        } else if let Some(scope) = state.scope.as_mut() {
            scope.deduplicate = options.deduplicate_search_requests;
            if !scope.deduplicate {
                scope.last_search = None;
            }
            if scope.child_limit != options.distributed_child_limit
                || scope.accept_children != options.accept_distributed_children
            {
                scope.child_limit = options.distributed_child_limit;
                scope.accept_children = options.accept_distributed_children;
                while scope.children.len() > scope.child_limit as usize {
                    let (username, child) =
                        scope.children.pop_last().expect("over the child limit");
                    child.conn.close();
                    client.inner().emit(Event::DistributedChildDisconnected {
                        username,
                        ip_endpoint: child.conn.remote(),
                    });
                }
                scope.publish(client.inner());
            }
        }
    }

    fn emit_departures(old: &Scope, client: &ClientInner) {
        if let Some(parent) = &old.parent {
            client.emit(parent_disconnected(parent));
        }
        if old.root {
            client.emit(Event::DemotedFromDistributedBranchRoot);
        }
        for (username, child) in &old.children {
            client.emit(Event::DistributedChildDisconnected {
                username: username.clone(),
                ip_endpoint: child.conn.remote(),
            });
        }
    }

    fn start_scope(
        state: &mut State,
        client: &SoulseekClient,
        options: &ClientOptions,
        announce: bool,
    ) {
        let weak = Arc::downgrade(client.inner());
        let status = if let Some(old) = state.scope.take() {
            Self::emit_departures(&old, client.inner());
            Arc::clone(&old.status)
        } else {
            let (updates, receiver) = watch::channel(None);
            let stop = CancellationToken::new();
            tokio::spawn(send_status(weak.clone(), stop.clone(), receiver));
            Arc::new(StatusFeed { stop, updates })
        };
        let scope = Scope {
            client: weak,
            stop: CancellationToken::new(),
            status,
            username: client.username().unwrap_or_default(),
            enabled: options.enable_distributed_network,
            accept_children: options.accept_distributed_children,
            child_limit: options.distributed_child_limit,
            deduplicate: options.deduplicate_search_requests,
            root: false,
            selecting_parent: false,
            parent: None,
            children: BTreeMap::new(),
            last_search: None,
            broadcast_latency: None,
        };
        if announce {
            scope.publish(client.inner());
        } else {
            client
                .inner()
                .emit(Event::DistributedNetworkStateChanged(scope.info()));
        }
        state.scope = Some(scope);
        Self::select_parent(state, client);
    }

    pub(crate) fn handle_server_message(&self, client: &SoulseekClient, message: &ServerIncoming) {
        // These searches are server-routed, rather than part of the distributed tree.
        if let ServerIncoming::FileSearch(search) = message {
            if client
                .username()
                .is_some_and(|username| username.eq_ignore_ascii_case(&search.username))
                && !client
                    .inner()
                    .searches
                    .explicitly_searches_self(search.token)
            {
                return;
            }
            client
                .inner()
                .searches
                .respond(client, &search.username, search.token, &search.query);
            return;
        }
        let mut state = self.lock();
        match message {
            ServerIncoming::NetInfo(net) => {
                if state.candidates != net.parents {
                    state.candidates.clone_from(&net.parents);
                    state.candidate_revision = state.candidate_revision.wrapping_add(1);
                }
                Self::select_parent(&mut state, client);
            }
            ServerIncoming::DistributedReset => {
                client.inner().emit(Event::DistributedNetworkReset);
                state.candidates.clear();
                if state.scope.is_some() {
                    Self::start_scope(&mut state, client, &client.options(), true);
                }
            }
            ServerIncoming::SearchInactivityTimeout(seconds) => {
                state.search_timeout = duration(*seconds)
            }
            ServerIncoming::ParentInactivityTimeout(seconds) => {
                state.parent_idle = duration(*seconds);
                Self::update_parent_timers(&state);
            }
            ServerIncoming::DistributedAliveInterval(seconds) => {
                state.ping_interval = duration(*seconds);
                Self::update_parent_timers(&state);
            }
            ServerIncoming::EmbeddedMessage(embedded) => {
                let Some(scope) = state.scope.as_mut().filter(|scope| scope.enabled) else {
                    return;
                };
                let embedded = EmbeddedMessage {
                    distributed_code: embedded.distributed_code,
                    payload: embedded.payload.clone(),
                };
                let Some(search) = embedded_search(&embedded) else {
                    return;
                };
                if scope.parent.is_none() && !scope.root {
                    scope.root = true;
                    client.inner().emit(Event::PromotedToDistributedBranchRoot);
                    scope.publish(client.inner());
                    scope.broadcast_branch();
                }
                scope.handle_search(client, search, SearchFrame::Embedded(embedded));
            }
            // Server-side eligibility/cache hints and the wishlist cadence do not
            // change the local child limit or the client-driven search schedule.
            _ => {}
        }
    }

    fn update_parent_timers(state: &State) {
        if let Some(scope) = &state.scope
            && let Some(parent) = &scope.parent
        {
            parent.control.send_replace(ParentControl {
                depth: scope.depth(),
                idle: state.parent_idle,
                ping: state.ping_interval,
            });
        }
    }

    /// Identifies the exact tree in which a reverse child dial was solicited.
    pub(crate) fn child_scope(&self) -> Option<CancellationToken> {
        self.lock()
            .scope
            .as_ref()
            .filter(|scope| {
                scope.enabled && scope.accept_children && (scope.root || scope.parent.is_some())
            })
            .map(|scope| scope.stop.clone())
    }

    pub(crate) fn handle_incoming_child(
        &self,
        client: &SoulseekClient,
        username: String,
        conn: Connection,
    ) {
        self.install_child(client, username, conn, None);
    }

    pub(crate) fn handle_scoped_incoming_child(
        &self,
        client: &SoulseekClient,
        username: String,
        conn: Connection,
        stop: &CancellationToken,
    ) {
        self.install_child(client, username, conn, Some(stop));
    }

    fn install_child(
        &self,
        client: &SoulseekClient,
        username: String,
        conn: Connection,
        stop: Option<&CancellationToken>,
    ) {
        let mut state = self.lock();
        let Some(scope) = state.scope.as_mut() else {
            return;
        };
        if stop.is_some_and(CancellationToken::is_cancelled)
            || !scope.enabled
            || !scope.accept_children
            || !(scope.root || scope.parent.is_some())
            || username == scope.username
            || scope
                .parent
                .as_ref()
                .is_some_and(|p| p.peer.username == username)
            || !(scope.can_accept() || scope.children.contains_key(&username))
        {
            return;
        }
        let (conn, incoming) = MessageConnection::spawn(conn, Some(username.clone()));
        let (outgoing, writes) = mpsc::channel(CHILD_QUEUE);
        for frame in scope.branch_frames() {
            let _ = outgoing.try_send((frame, None));
        }
        if let Some(old) = scope.children.insert(
            username.clone(),
            Child {
                conn: conn.clone(),
                depth: 0,
                outgoing,
            },
        ) {
            old.conn.close();
        } else {
            client.inner().emit(Event::DistributedChildAdded {
                username: username.clone(),
                ip_endpoint: conn.remote(),
            });
        }
        scope.publish(client.inner());
        tokio::spawn(run_child(
            scope.client.clone(),
            scope.stop.clone(),
            username,
            conn,
            incoming,
            writes,
        ));
    }

    fn select_parent(state: &mut State, client: &SoulseekClient) {
        let Some(scope) = state
            .scope
            .as_mut()
            .filter(|scope| scope.enabled && scope.parent.is_none() && !scope.selecting_parent)
        else {
            return;
        };
        let mut seen = HashSet::new();
        let candidates: Vec<_> = state
            .candidates
            .iter()
            .filter(|p| {
                p.username != scope.username
                    && !scope.children.contains_key(&p.username)
                    && seen.insert(&p.username)
            })
            .cloned()
            .collect();
        if candidates.is_empty() {
            return;
        }
        scope.selecting_parent = true;
        let wait = state
            .search_timeout
            .unwrap_or_else(|| client.options().message_timeout);
        tokio::spawn(select_parent(
            scope.client.clone(),
            scope.stop.clone(),
            candidates,
            state.candidate_revision,
            wait,
        ));
    }
}

fn duration(seconds: u32) -> Option<Duration> {
    (seconds != 0).then(|| Duration::from_secs(u64::from(seconds)))
}

async fn send_status(
    client: Weak<ClientInner>,
    stop: CancellationToken,
    mut updates: watch::Receiver<Option<Status>>,
) {
    // Capturing the connection once prevents writes crossing a server reconnect.
    let transport = {
        let Some(client) = client.upgrade() else {
            return;
        };
        let Ok(transport) = client.server.logged_in_transport(&client) else {
            return;
        };
        transport
    };
    while let Some(status) = tokio::select! {
        biased;
        _ = stop.cancelled() => None,
        result = updates.changed() => result.ok().and_then(|()| updates.borrow_and_update().clone()),
    } {
        for frame in [
            BranchLevelCommand {
                level: status.level,
            }
            .to_bytes(),
            BranchRootCommand {
                username: status.root,
            }
            .to_bytes(),
            AcceptChildrenCommand {
                accepted: status.accept,
            }
            .to_bytes(),
            HaveNoParentsCommand {
                have_no_parents: status.no_parents,
            }
            .to_bytes(),
            ChildDepthCommand {
                depth: status.depth,
            }
            .to_bytes(),
        ] {
            if !send_link(&stop, &transport, frame).await {
                return;
            }
        }
    }
}

struct Candidate {
    conn: MessageConnection,
    incoming: mpsc::Receiver<Bytes>,
    peer: DistributedPeer,
    level: i32,
    root: String,
    first_search: Search,
}

async fn select_parent(
    client: Weak<ClientInner>,
    stop: CancellationToken,
    candidates: Vec<NetInfoParent>,
    revision: u64,
    wait: Duration,
) {
    let mut connected = Vec::new();
    {
        let dials = stream::iter(candidates.into_iter().enumerate().map(|(rank, candidate)| {
            let client = client.clone();
            let stop = stop.clone();
            async move {
                (
                    rank,
                    connect_candidate(client, stop, &candidate, wait).await,
                )
            }
        }))
        .buffer_unordered(CANDIDATE_CONCURRENCY);
        tokio::pin!(dials);
        loop {
            tokio::select! {
                biased;
                _ = stop.cancelled() => return,
                next = dials.next() => match next {
                    Some((rank, Some(candidate))) => connected.push((rank, candidate)),
                    Some((_, None)) => {},
                    None => break,
                }
            }
        }
    }
    let Some(inner) = client.upgrade() else {
        return;
    };
    let client = SoulseekClient::from_inner(inner);
    let mut state = client.inner().distributed.lock();
    let (idle, ping) = (state.parent_idle, state.ping_interval);
    let Some(scope) = state.scope.as_mut().filter(|_| !stop.is_cancelled()) else {
        return;
    };
    scope.selecting_parent = false;
    connected.retain(|(_, candidate)| {
        !candidate.conn.is_closed()
            && candidate.root != scope.username
            && !scope.children.contains_key(&candidate.peer.username)
    });
    let best = connected
        .iter()
        .enumerate()
        .min_by_key(|(_, (rank, candidate))| (candidate.level, *rank));
    let Some((index, _)) = best else {
        if state.candidate_revision != revision {
            DistributedManager::select_parent(&mut state, &client);
        } else if let Some(scope) = &state.scope {
            scope.publish_status();
            scope.status.updates.send_modify(|_| {});
        }
        return;
    };
    let (
        _,
        Candidate {
            conn,
            incoming,
            peer,
            level,
            root,
            first_search,
        },
    ) = connected.swap_remove(index);
    let (control, controls) = watch::channel(ParentControl {
        depth: scope.depth(),
        idle,
        ping,
    });
    scope.parent = Some(Parent {
        peer: peer.clone(),
        conn: conn.clone(),
        level,
        root: root.clone(),
        control,
    });
    if std::mem::take(&mut scope.root) {
        client.inner().emit(Event::DemotedFromDistributedBranchRoot);
    }
    client.inner().emit(Event::DistributedParentAdopted {
        username: peer.username.clone(),
        ip_endpoint: peer.ip_endpoint,
        branch_level: level as u32,
        branch_root: root,
    });
    scope.publish(client.inner());
    scope.broadcast_branch();
    scope.handle_search(&client, first_search.0, first_search.1);
    tokio::spawn(run_parent(
        scope.client.clone(),
        stop,
        peer,
        conn,
        incoming,
        controls,
    ));
}

async fn connect_candidate(
    client: Weak<ClientInner>,
    stop: CancellationToken,
    candidate: &NetInfoParent,
    wait: Duration,
) -> Option<Candidate> {
    let client = SoulseekClient::from_inner(client.upgrade()?);
    let endpoint = candidate.endpoint().into();
    let conn = client
        .inner()
        .peers
        .get_distributed_connection(&client, &candidate.username, endpoint, &stop)
        .await
        .ok()?;
    drop(client);
    let (conn, mut incoming) = MessageConnection::spawn(conn, Some(candidate.username.clone()));
    // A TCP handshake and branch advertisement alone are not proof of a live
    // upstream: require the first distributed search, as Soulseek.NET does.
    let (level, root, first_search) =
        timeout(wait, candidate_info(&candidate.username, &mut incoming))
            .await
            .ok()??;
    let peer = DistributedPeer {
        username: candidate.username.clone(),
        ip_endpoint: conn.remote(),
    };
    Some(Candidate {
        conn,
        incoming,
        peer,
        level,
        root,
        first_search,
    })
}

type Search = (DistributedSearchRequest, SearchFrame);

async fn candidate_info(
    username: &str,
    incoming: &mut mpsc::Receiver<Bytes>,
) -> Option<(i32, String, Search)> {
    let (mut level, mut root, mut search) = (None, None, None);
    loop {
        let body = incoming.recv().await?;
        match DistributedIncoming::decode(body.clone()).ok()? {
            DistributedIncoming::BranchLevel(value) if value.level >= 0 => {
                level = Some(value.level)
            }
            DistributedIncoming::BranchLevel(_) => return None,
            DistributedIncoming::BranchRoot(value) if !value.username.is_empty() => {
                root = Some(value.username)
            }
            message if search.is_none() => search = unwrapped_search(message, body),
            _ => {}
        }
        if let (Some(level), Some(_)) = (level, &search)
            && (level == 0 || root.is_some())
        {
            return Some((
                level,
                if level == 0 { username.into() } else { root? },
                search?,
            ));
        }
    }
}

enum SearchFrame {
    Body(Bytes),
    Embedded(EmbeddedMessage),
}

impl SearchFrame {
    fn into_frame(self) -> Bytes {
        match self {
            Self::Body(body) => {
                let mut frame = BytesMut::with_capacity(4 + body.len());
                frame.put_u32_le(body.len() as u32);
                frame.extend_from_slice(&body);
                frame.freeze()
            }
            Self::Embedded(embedded) => embedded.distributed_frame(),
        }
    }
}

fn embedded_search(embedded: &EmbeddedMessage) -> Option<DistributedSearchRequest> {
    if embedded.distributed_code != DistributedCode::SearchRequest.as_u8() {
        return None;
    }
    DistributedSearchRequest::decode(&mut MessageReader::new(embedded.payload.clone())).ok()
}

fn unwrapped_search(message: DistributedIncoming, body: Bytes) -> Option<Search> {
    match message {
        DistributedIncoming::SearchRequest(search) => Some((search, SearchFrame::Body(body))),
        DistributedIncoming::EmbeddedMessage(embedded) => {
            Some((embedded_search(&embedded)?, SearchFrame::Embedded(embedded)))
        }
        _ => None,
    }
}

async fn deadline(at: Option<Instant>) {
    if let Some(at) = at {
        sleep_until(at).await;
    } else {
        std::future::pending::<()>().await;
    }
}

async fn run_parent(
    client: Weak<ClientInner>,
    stop: CancellationToken,
    peer: DistributedPeer,
    conn: MessageConnection,
    mut incoming: mpsc::Receiver<Bytes>,
    mut controls: watch::Receiver<ParentControl>,
) {
    let mut last_received = Instant::now();
    let mut last_ping = last_received;
    let mut sent_depth = None;
    loop {
        let control = *controls.borrow_and_update();
        if sent_depth != Some(control.depth) {
            if !send_link(
                &stop,
                &conn,
                DistributedChildDepth {
                    depth: control.depth,
                }
                .to_bytes(),
            )
            .await
            {
                break;
            }
            sent_depth = Some(control.depth);
        }
        tokio::select! {
            biased;
            _ = stop.cancelled() => break,
            _ = conn.closed() => break,
            _ = deadline(control.idle.map(|idle| last_received + idle)) => break,
            changed = controls.changed() => if changed.is_err() { break; },
            _ = deadline(control.ping.map(|ping| last_ping + ping)) => {
                if !send_link(&stop, &conn, DistributedPingRequest.to_bytes()).await { break; }
                last_ping = Instant::now();
            }
            body = incoming.recv() => {
                let Some(body) = body else { break };
                let Ok(message) = DistributedIncoming::decode(body.clone()) else { continue };
                last_received = Instant::now();
                if matches!(message, DistributedIncoming::Ping(_)) && body.len() == 1 {
                    let Some(inner) = client.upgrade() else { break };
                    let token = SoulseekClient::from_inner(inner).get_next_token();
                    if !send_link(&stop, &conn, DistributedPingResponse { token }.to_bytes()).await { break; }
                    continue;
                }
                let Some(inner) = client.upgrade() else { break };
                parent_message(&SoulseekClient::from_inner(inner), &stop, conn.id(), &peer.username, message, body);
            }
        }
    }
    conn.close();
    let Some(inner) = client.upgrade() else {
        return;
    };
    let client = SoulseekClient::from_inner(inner);
    let mut state = client.inner().distributed.lock();
    let Some(scope) = state.scope.as_mut().filter(|scope| {
        !stop.is_cancelled()
            && scope
                .parent
                .as_ref()
                .is_some_and(|parent| parent.conn.id() == conn.id())
    }) else {
        return;
    };
    let parent = scope.parent.take().expect("parent identity checked");
    client.inner().emit(parent_disconnected(&parent));
    scope.publish(client.inner());
    // Prefer alternative cached candidates when a parent is lost.
    state
        .candidates
        .retain(|candidate| candidate.username != parent.peer.username);
    DistributedManager::select_parent(&mut state, &client);
}

fn parent_disconnected(parent: &Parent) -> Event {
    Event::DistributedParentDisconnected {
        username: parent.peer.username.clone(),
        ip_endpoint: parent.peer.ip_endpoint,
        branch_level: parent.level as u32,
        branch_root: parent.root.clone(),
    }
}

fn parent_message(
    client: &SoulseekClient,
    stop: &CancellationToken,
    id: u64,
    username: &str,
    message: DistributedIncoming,
    body: Bytes,
) {
    let mut state = client.inner().distributed.lock();
    let Some(scope) = state.scope.as_mut().filter(|_| !stop.is_cancelled()) else {
        return;
    };
    let Some(parent) = scope
        .parent
        .as_mut()
        .filter(|parent| parent.conn.id() == id)
    else {
        return;
    };
    match message {
        DistributedIncoming::BranchLevel(value)
            if value.level >= 0 && parent.level != value.level =>
        {
            parent.level = value.level;
            if value.level == 0 {
                parent.root = username.into();
            }
        }
        DistributedIncoming::BranchRoot(value) if value.username == scope.username => {
            parent.conn.close();
            return;
        }
        DistributedIncoming::BranchRoot(value)
            if parent.level > 0 && !value.username.is_empty() && parent.root != value.username =>
        {
            parent.root = value.username;
        }
        message => {
            if let Some((search, frame)) = unwrapped_search(message, body) {
                scope.handle_search(client, search, frame);
            }
            return;
        }
    }
    scope.publish(client.inner());
    scope.broadcast_branch();
}

async fn send_link(stop: &CancellationToken, conn: &MessageConnection, frame: Bytes) -> bool {
    tokio::select! {
        biased;
        _ = stop.cancelled() => false,
        result = conn.send(frame) => result.is_ok(),
    }
}

async fn run_child(
    client: Weak<ClientInner>,
    stop: CancellationToken,
    username: String,
    conn: MessageConnection,
    mut incoming: mpsc::Receiver<Bytes>,
    mut writes: mpsc::Receiver<(Bytes, Option<Instant>)>,
) {
    loop {
        tokio::select! {
            _ = stop.cancelled() => break,
            _ = conn.closed() => break,
            write = writes.recv() => {
                let Some((frame, start)) = write else { break };
                if !send_link(&stop, &conn, frame).await { break; }
                if let Some(start) = start && let Some(client) = client.upgrade() {
                    let mut state = client.distributed.lock();
                    if let Some(scope) = child_owner(&mut state, &stop, &username, conn.id()) {
                        let millis = start.elapsed().as_secs_f64() * 1_000.0;
                        scope.broadcast_latency = Some(scope.broadcast_latency.map_or(millis, |avg| avg + (millis - avg) * 0.005));
                    }
                }
            }
            body = incoming.recv() => {
                let Some(body) = body else { break };
                let ping = body.len() == 1;
                match DistributedIncoming::decode(body) {
                    Ok(DistributedIncoming::ChildDepth(value)) if value.depth >= 0 => {
                        let Some(client) = client.upgrade() else { break };
                        let mut state = client.distributed.lock();
                        if let Some(scope) = child_owner(&mut state, &stop, &username, conn.id()) {
                            scope.children.get_mut(&username).expect("child identity checked").depth = value.depth;
                            scope.publish_status();
                        }
                    }
                    Ok(DistributedIncoming::Ping(_)) if ping => {
                        let Some(inner) = client.upgrade() else { break };
                        let token = SoulseekClient::from_inner(inner).get_next_token();
                        if !send_link(&stop, &conn, DistributedPingResponse { token }.to_bytes()).await { break; }
                    }
                    _ => {}
                }
            }
        }
    }
    conn.close();
    let Some(client) = client.upgrade() else {
        return;
    };
    let mut state = client.distributed.lock();
    if let Some(scope) = child_owner(&mut state, &stop, &username, conn.id()) {
        scope.children.remove(&username);
        client.emit(Event::DistributedChildDisconnected {
            username,
            ip_endpoint: conn.remote(),
        });
        scope.publish(&client);
    }
}

fn child_owner<'a>(
    state: &'a mut State,
    stop: &CancellationToken,
    username: &str,
    id: u64,
) -> Option<&'a mut Scope> {
    state.scope.as_mut().filter(|scope| {
        !stop.is_cancelled()
            && scope
                .children
                .get(username)
                .is_some_and(|child| child.conn.id() == id)
    })
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use bytes::Buf;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    use super::*;
    use crate::net::test_support::{FakeServer, read_frame};
    use crate::protocol::init::InitIncoming;
    use crate::protocol::server::NetInfoNotification;
    use crate::protocol::{MessageBuilder, codes::ServerCode};
    use crate::resolvers::DefaultResolvers;

    async fn logged_in(client: &SoulseekClient) -> TcpStream {
        let server = FakeServer::bind().await;
        let (login, stream) = tokio::join!(
            client.connect("127.0.0.1", server.addr().port(), "local", "password"),
            async {
                let mut stream = server.accept().await;
                read_frame(&mut stream).await;
                read_frame(&mut stream).await;
                let login = MessageBuilder::new()
                    .code_u32(ServerCode::Login as u32)
                    .bool(true)
                    .string("")
                    .ip(Ipv4Addr::LOCALHOST)
                    .string("")
                    .bool(false)
                    .build();
                stream.write_all(&login).await.unwrap();
                stream
            },
        );
        login.unwrap();
        stream
    }

    #[tokio::test]
    async fn root_forwards_unwrapped_search_reports_depth_and_resets_children() {
        timeout(Duration::from_secs(5), async {
            let options = ClientOptions {
                enable_listener: false,
                distributed_child_limit: 1,
                ..ClientOptions::default()
            };
            let client = SoulseekClient::new(options.clone(), Arc::new(DefaultResolvers));
            let mut events = client.subscribe();
            let mut server = logged_in(&client).await;
            let search = |token| {
                DistributedSearchRequest {
                    username: "local".into(),
                    token,
                    query: "some music".into(),
                }
                .to_bytes()
            };
            let embedded = |frame: Bytes| {
                EmbeddedMessage {
                    distributed_code: frame[4],
                    payload: frame.slice(5..),
                }
                .to_bytes()
            };
            server.write_all(&embedded(search(1))).await.unwrap();
            while !matches!(
                events.recv().await.unwrap(),
                Event::PromotedToDistributedBranchRoot
            ) {}
            assert!(client.distributed_network().can_accept_children);

            let child = FakeServer::bind().await;
            let (local, mut child) = tokio::join!(TcpStream::connect(child.addr()), child.accept());
            let local = local.unwrap();
            let endpoint = local.peer_addr().unwrap();
            let conn =
                Connection::from_accepted(local, endpoint, &options.distributed_connection_options);
            let manager = &client.inner().distributed;
            manager.handle_incoming_child(&client, "child".into(), conn);
            let scope = manager.child_scope().unwrap();
            assert!(!manager.info().can_accept_children);
            assert_eq!(
                DistributedIncoming::decode(read_frame(&mut child).await).unwrap(),
                DistributedIncoming::BranchLevel(DistributedBranchLevel { level: 0 })
            );
            assert_eq!(
                DistributedIncoming::decode(read_frame(&mut child).await).unwrap(),
                DistributedIncoming::BranchRoot(DistributedBranchRoot {
                    username: "local".into()
                })
            );

            let request = search(2);
            server.write_all(&embedded(request.clone())).await.unwrap();
            assert_eq!(read_frame(&mut child).await, request.slice(4..));
            child
                .write_all(&DistributedPingRequest.to_bytes())
                .await
                .unwrap();
            assert!(matches!(
                DistributedIncoming::decode(read_frame(&mut child).await).unwrap(),
                DistributedIncoming::Ping(_)
            ));
            child
                .write_all(&DistributedChildDepth { depth: 3 }.to_bytes())
                .await
                .unwrap();
            loop {
                let mut body = read_frame(&mut server).await;
                if body.get_u32_le() == ServerCode::ChildDepth as u32 && body.get_u32_le() == 4 {
                    break;
                }
            }

            server
                .write_all(
                    &MessageBuilder::new()
                        .code_u32(ServerCode::DistributedReset as u32)
                        .build(),
                )
                .await
                .unwrap();
            while !matches!(
                events.recv().await.unwrap(),
                Event::DistributedChildDisconnected { .. }
            ) {}
            assert!(scope.is_cancelled());
            assert_eq!(child.read(&mut [0u8; 1]).await.unwrap(), 0);
            assert!(!manager.info().is_branch_root);
            assert!(manager.info().children.is_empty());
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn selects_lowest_live_candidate_and_closes_the_other_link() {
        async fn parent(listener: &FakeServer, level: i32) -> TcpStream {
            let mut stream = listener.accept().await;
            let InitIncoming::PeerInit(init) =
                InitIncoming::decode(read_frame(&mut stream).await).unwrap()
            else {
                panic!("expected distributed PeerInit");
            };
            assert_eq!(
                (init.username.as_str(), init.connection_type.as_str()),
                ("local", "D")
            );
            stream
                .write_all(&DistributedBranchLevel { level }.to_bytes())
                .await
                .unwrap();
            if level > 0 {
                stream
                    .write_all(
                        &DistributedBranchRoot {
                            username: "upstream".into(),
                        }
                        .to_bytes(),
                    )
                    .await
                    .unwrap();
            }
            let search = DistributedSearchRequest {
                username: "local".into(),
                token: 1,
                query: "music".into(),
            };
            stream.write_all(&search.to_bytes()).await.unwrap();
            stream
        }

        timeout(Duration::from_secs(5), async {
            let client = SoulseekClient::new(
                ClientOptions {
                    enable_listener: false,
                    ..ClientOptions::default()
                },
                Arc::new(DefaultResolvers),
            );
            let mut events = client.subscribe();
            let mut server = logged_in(&client).await;
            let distant = FakeServer::bind().await;
            let root = FakeServer::bind().await;
            let candidates = NetInfoNotification {
                parents: [("distant", distant.addr()), ("root", root.addr())]
                    .into_iter()
                    .map(|(username, endpoint)| NetInfoParent {
                        username: username.into(),
                        ip_address: Ipv4Addr::LOCALHOST,
                        port: endpoint.port(),
                    })
                    .collect(),
            };
            server.write_all(&candidates.to_bytes()).await.unwrap();
            let (mut distant, mut root) = tokio::join!(parent(&distant, 2), parent(&root, 0));
            loop {
                if let Event::DistributedParentAdopted {
                    username,
                    branch_level,
                    ..
                } = events.recv().await.unwrap()
                {
                    assert_eq!((username.as_str(), branch_level), ("root", 0));
                    break;
                }
            }
            let info = client.distributed_network();
            assert_eq!((info.branch_root.as_str(), info.branch_level), ("root", 1));
            assert_eq!(
                DistributedIncoming::decode(read_frame(&mut root).await).unwrap(),
                DistributedIncoming::ChildDepth(DistributedChildDepth { depth: 0 })
            );
            assert_eq!(distant.read(&mut [0u8; 1]).await.unwrap(), 0);
            client.disconnect("test finished", None).await;
            assert_eq!(root.read(&mut [0u8; 1]).await.unwrap(), 0);
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn candidate_waits_for_search_and_uses_root_candidate_username() {
        let (frames, mut incoming) = mpsc::channel(4);
        let search = DistributedSearchRequest {
            username: "requester".into(),
            token: 15,
            query: "music".into(),
        };
        frames
            .send(DistributedBranchLevel { level: 0 }.to_bytes().slice(4..))
            .await
            .unwrap();
        frames.send(search.to_bytes().slice(4..)).await.unwrap();
        let (level, root, (request, _)) = candidate_info("parent", &mut incoming).await.unwrap();
        assert_eq!((level, root.as_str()), (0, "parent"));
        assert_eq!(request, search);
    }
}
