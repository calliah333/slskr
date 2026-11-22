//! Transfer negotiation on P connections and handoff to raw F connections.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Mutex, MutexGuard, OnceLock, RwLock};
use std::time::{Duration, SystemTime};

use tokio::sync::{Semaphore, SemaphorePermit, oneshot};
use tokio::time::{Instant, timeout};
use tokio_util::sync::CancellationToken;

use super::{DownloadOutput, SoulseekClient, TransferHandle, UploadInput, require_non_blank};
use crate::error::{Error, Result};
use crate::events::Event;
use crate::model::{Transfer, TransferDirection as Direction, TransferState as State};
use crate::net::connection::Connection;
use crate::net::message_connection::MessageConnection;
use crate::net::waiter::WaitKey;
use crate::options::TransferOptions;
use crate::protocol::peer::{
    PeerIncoming, PlaceInQueueRequest, PlaceInQueueResponse, TransferRequest, TransferResponse,
    UploadDenied, UploadFailed,
};

mod stream;
#[cfg(test)]
mod tests;

pub(crate) struct TransferManager {
    registry: Mutex<Registry>,
    download_slots: OnceLock<Semaphore>,
    upload_slots: OnceLock<Semaphore>,
}

#[derive(Default)]
struct Registry {
    session: CancellationToken,
    active: HashMap<u32, Pending>,
}

struct Pending {
    direction: Direction,
    username: String,
    filename: String,
    session: CancellationToken,
    response: Option<oneshot::Sender<TransferResponse>>,
    offer: Option<oneshot::Sender<TransferRequest>>,
    failure: Option<oneshot::Sender<Error>>,
    remote_token: Option<u32>,
    incoming: Option<oneshot::Sender<Connection>>,
}

impl TransferManager {
    pub(crate) fn new() -> Self {
        Self {
            registry: Mutex::new(Registry::default()),
            download_slots: OnceLock::new(),
            upload_slots: OnceLock::new(),
        }
    }

    fn registry(&self) -> MutexGuard<'_, Registry> {
        self.registry.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) fn on_disconnected(&self) {
        std::mem::take(&mut self.registry().session).cancel();
    }

    /// Handles both a direct F PeerInit and the reverse socket after PierceFirewall.
    /// The raw token following init is distinct from a firewall solicitation token.
    pub(crate) fn handle_incoming_transfer_connection(
        &self,
        client: &SoulseekClient,
        username: String,
        mut conn: Connection,
    ) {
        let session = {
            let registry = self.registry();
            if !registry.active.values().any(|entry| {
                entry.username == username
                    && entry.incoming.is_some()
                    && !entry.session.is_cancelled()
            }) {
                return;
            }
            registry.session.clone()
        };
        let read_timeout = client
            .inner
            .options
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .transfer_connection_options
            .inactivity_timeout;
        let client = client.clone();
        tokio::spawn(async move {
            let mut bytes = [0; 4];
            let read = async {
                match read_timeout {
                    Some(limit) => timeout(limit, conn.read_exact(&mut bytes)).await?,
                    None => conn.read_exact(&mut bytes).await,
                }
            };
            tokio::select! {
                biased;
                _ = session.cancelled() => return,
                result = read => if result.is_err() { return },
            }
            let token = u32::from_le_bytes(bytes);
            let mut registry = client.inner.transfers.registry();
            if session.is_cancelled() {
                return;
            }
            let incoming = registry.active.values_mut().find_map(|entry| {
                if entry.username == username
                    && entry.remote_token == Some(token)
                    && !entry.session.is_cancelled()
                {
                    entry.incoming.take()
                } else {
                    None
                }
            });
            drop(registry);
            if let Some(incoming) = incoming {
                let _ = incoming.send(conn);
            }
        });
    }

    pub(crate) fn handle_peer_message(
        &self,
        client: &SoulseekClient,
        username: &str,
        remote: SocketAddr,
        msg: PeerIncoming,
    ) {
        match msg {
            PeerIncoming::TransferResponse(response) => {
                let sender = self
                    .registry()
                    .active
                    .get_mut(&response.token)
                    .and_then(|entry| {
                        (entry.username == username && !entry.session.is_cancelled())
                            .then(|| entry.response.take())
                            .flatten()
                    });
                if let Some(sender) = sender {
                    let _ = sender.send(response);
                }
            }
            PeerIncoming::TransferRequest(request) if request.direction == Direction::Upload => {
                let mut registry = self.registry();
                let token = request.token;
                if let Some(entry) = registry.active.values().find(|entry| {
                    entry.username == username
                        && entry.remote_token == Some(token)
                        && !entry.session.is_cancelled()
                }) {
                    let duplicate = entry.filename == request.filename;
                    drop(registry);
                    if !duplicate {
                        self.respond(client, username, remote, Reply::RejectOffer(token));
                    }
                    return;
                }
                let entry = registry.active.values_mut().find(|entry| {
                    entry.direction == Direction::Download
                        && entry.username == username
                        && entry.filename == request.filename
                        && !entry.session.is_cancelled()
                });
                if let Some(entry) = entry {
                    if let Some(sender) = entry.offer.take()
                        && sender.send(request).is_ok()
                    {
                        // Deduplicate offers while the download is waiting for a local data slot.
                        entry.remote_token = Some(token);
                        return;
                    }
                }
                drop(registry);
                self.respond(client, username, remote, Reply::RejectOffer(token));
            }
            PeerIncoming::TransferRequest(request) => {
                self.respond(
                    client,
                    username,
                    remote,
                    Reply::Enqueue(request.filename, Some(request.token)),
                );
            }
            PeerIncoming::QueueDownload(request) => {
                self.respond(
                    client,
                    username,
                    remote,
                    Reply::Enqueue(request.filename, None),
                );
            }
            PeerIncoming::PlaceInQueueRequest(request) => {
                self.respond(client, username, remote, Reply::Place(request.filename));
            }
            PeerIncoming::PlaceInQueueResponse(response) => {
                client.inner.waiter.complete(
                    &place_key(username, &response.filename),
                    response.place_in_queue,
                );
            }
            PeerIncoming::UploadDenied(denial) => {
                self.fail_download(username, &denial.filename, rejected(&denial.message));
                client.inner.emit(Event::DownloadDenied {
                    username: username.into(),
                    filename: denial.filename,
                    message: denial.message,
                });
            }
            PeerIncoming::UploadFailed(failure) => {
                self.fail_download(
                    username,
                    &failure.filename,
                    Error::TransferReportedFailed(
                        "Download reported as failed by remote client".into(),
                    ),
                );
                client.inner.emit(Event::DownloadFailed {
                    username: username.into(),
                    filename: failure.filename,
                });
            }
            _ => {}
        }
    }

    fn fail_download(&self, username: &str, filename: &str, error: Error) {
        let sender = self.registry().active.values_mut().find_map(|entry| {
            if entry.direction == Direction::Download
                && entry.username == username
                && entry.filename == filename
                && !entry.session.is_cancelled()
            {
                entry.failure.take()
            } else {
                None
            }
        });
        if let Some(sender) = sender {
            let _ = sender.send(error);
        }
    }

    fn respond(&self, client: &SoulseekClient, username: &str, remote: SocketAddr, reply: Reply) {
        let session = self.registry().session.clone();
        let client = client.clone();
        let username = username.to_owned();
        tokio::spawn(async move {
            tokio::select! {
                biased;
                _ = session.cancelled() => {}
                result = reply.send(&client, &username, remote, &session) => {
                    if let Err(error) = result {
                        tracing::debug!(%username, %error, "Could not send peer transfer reply");
                    }
                }
            }
        });
    }

    async fn slot(
        &self,
        client: &SoulseekClient,
        direction: Direction,
    ) -> Option<SemaphorePermit<'_>> {
        let limit = {
            let options = client
                .inner
                .options
                .read()
                .unwrap_or_else(|e| e.into_inner());
            match direction {
                Direction::Download => options.maximum_concurrent_downloads,
                Direction::Upload => options.maximum_concurrent_uploads,
            }
        };
        // ClientOptions uses usize::MAX for unlimited; it isn't a valid semaphore size.
        if limit > Semaphore::MAX_PERMITS {
            return None;
        }
        let slots = match direction {
            Direction::Download => &self.download_slots,
            Direction::Upload => &self.upload_slots,
        };
        Some(
            slots
                .get_or_init(|| Semaphore::new(limit))
                .acquire()
                .await
                .expect("transfer slots are never closed"),
        )
    }
}

enum Reply {
    RejectOffer(u32),
    Enqueue(String, Option<u32>),
    Place(String),
}

impl Reply {
    async fn send(
        self,
        client: &SoulseekClient,
        username: &str,
        remote: SocketAddr,
        cancel: &CancellationToken,
    ) -> Result<()> {
        let peers = &client.inner.peers;
        let filename = match self {
            Self::RejectOffer(token) => {
                return peers
                    .send_to(
                        client,
                        username,
                        TransferResponse::rejected(token, "Cancelled").to_bytes(),
                        cancel,
                    )
                    .await;
            }
            Self::Enqueue(filename, legacy_token) => {
                let decision = client
                    .inner
                    .resolvers
                    .enqueue_download(username.into(), remote, filename.clone())
                    .await;
                if let Err(error) = decision {
                    let message = error.enqueue_rejection_message();
                    if let Some(token) = legacy_token {
                        peers
                            .send_to(
                                client,
                                username,
                                TransferResponse::rejected(token, &message).to_bytes(),
                                cancel,
                            )
                            .await?;
                    }
                    return peers
                        .send_to(
                            client,
                            username,
                            UploadDenied { filename, message }.to_bytes(),
                            cancel,
                        )
                        .await;
                }
                if let Some(token) = legacy_token {
                    peers
                        .send_to(
                            client,
                            username,
                            TransferResponse::rejected(token, "Queued").to_bytes(),
                            cancel,
                        )
                        .await?;
                }
                filename
            }
            Self::Place(filename) => filename,
        };
        if let Some(place_in_queue) = client
            .inner
            .resolvers
            .place_in_queue(username.into(), remote, filename.clone())
            .await
        {
            peers
                .send_to(
                    client,
                    username,
                    PlaceInQueueResponse {
                        filename,
                        place_in_queue,
                    }
                    .to_bytes(),
                    cancel,
                )
                .await?;
        }
        Ok(())
    }
}

struct ActiveTransfer {
    client: SoulseekClient,
    snapshot: Transfer,
    options: TransferOptions,
    session: CancellationToken,
    started: Option<Instant>,
}

impl ActiveTransfer {
    fn begin(
        client: &SoulseekClient,
        snapshot: Transfer,
        options: TransferOptions,
    ) -> Result<Self> {
        let mut registry = client.inner.transfers.registry();
        client.inner.ensure_logged_in(match snapshot.direction {
            Direction::Download => "download files",
            Direction::Upload => "upload files",
        })?;
        if registry.active.contains_key(&snapshot.token) {
            return Err(Error::DuplicateToken(format!(
                "Transfer token {} is already in use",
                snapshot.token
            )));
        }
        if registry.active.values().any(|entry| {
            entry.direction == snapshot.direction
                && entry.username == snapshot.username
                && entry.filename == snapshot.filename
        }) {
            return Err(Error::DuplicateTransfer(format!(
                "A {} of {} with user {} is already active",
                snapshot.direction, snapshot.filename, snapshot.username,
            )));
        }
        let session = registry.session.clone();
        registry.active.insert(
            snapshot.token,
            Pending {
                direction: snapshot.direction,
                username: snapshot.username.clone(),
                filename: snapshot.filename.clone(),
                session: session.clone(),
                response: None,
                offer: None,
                failure: None,
                remote_token: None,
                incoming: None,
            },
        );
        let active = Self {
            client: client.clone(),
            snapshot,
            options,
            session,
            started: None,
        };
        active
            .snapshots()
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(active.snapshot.token, active.snapshot.clone());
        Ok(active)
    }

    fn snapshots(&self) -> &RwLock<HashMap<u32, Transfer>> {
        match self.snapshot.direction {
            Direction::Download => &self.client.inner.downloads,
            Direction::Upload => &self.client.inner.uploads,
        }
    }

    fn state(&mut self, state: State) {
        let previous = std::mem::replace(&mut self.snapshot.state, state);
        if state == State::IN_PROGRESS {
            self.started = Some(Instant::now());
            self.snapshot.start_time = Some(SystemTime::now());
        }
        if state.is_completed() {
            self.snapshot.end_time = Some(SystemTime::now());
        }
        self.snapshots()
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(self.snapshot.token, self.snapshot.clone());
        if let Some(callback) = &self.options.state_changed {
            callback(previous, &self.snapshot);
        }
        self.client.inner.emit(Event::TransferStateChanged {
            previous,
            transfer: self.snapshot.clone(),
        });
    }

    fn progress(&mut self, bytes: u64) {
        let previous_bytes = std::mem::replace(&mut self.snapshot.bytes_transferred, bytes);
        let elapsed = self
            .started
            .map_or(0.0, |start| start.elapsed().as_secs_f64());
        if elapsed > 0.0 {
            self.snapshot.average_speed = (bytes - self.snapshot.start_offset) as f64 / elapsed;
        }
        if let Some(snapshot) = self
            .snapshots()
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .get_mut(&self.snapshot.token)
        {
            snapshot.bytes_transferred = bytes;
            snapshot.average_speed = self.snapshot.average_speed;
        }
        if let Some(callback) = &self.options.progress_updated {
            callback(previous_bytes, &self.snapshot);
        }
        if self.client.inner.events.receiver_count() != 0 {
            self.client.inner.emit(Event::TransferProgressUpdated {
                previous_bytes,
                transfer: self.snapshot.clone(),
            });
        }
    }

    fn finish(&mut self, result: &Result<()>) {
        let disposition = match result {
            Ok(()) => State::SUCCEEDED,
            Err(error) => {
                self.snapshot.exception = Some(error.to_string());
                match error {
                    Error::Cancelled => State::CANCELLED,
                    Error::Timeout(_) => State::TIMED_OUT,
                    Error::TransferRejected(_) => State::REJECTED,
                    Error::TransferSizeMismatch { .. } => State::ABORTED,
                    _ => State::ERRORED,
                }
            }
        };
        self.state(State::COMPLETED | disposition);
    }

    fn pending(&self, f: impl FnOnce(&mut Pending)) {
        if let Some(entry) = self
            .client
            .inner
            .transfers
            .registry()
            .active
            .get_mut(&self.snapshot.token)
        {
            f(entry);
        }
    }

    fn expect_incoming(&self) -> oneshot::Receiver<Connection> {
        let (tx, rx) = oneshot::channel();
        self.pending(|entry| {
            entry.incoming = Some(tx);
        });
        rx
    }

    async fn message_connection(&self, cancel: &CancellationToken) -> Result<MessageConnection> {
        self.client
            .inner
            .peers
            .get_message_connection(&self.client, &self.snapshot.username, cancel)
            .await
    }

    async fn outgoing_file(&self, token: u32, cancel: &CancellationToken) -> Result<Connection> {
        let mut conn = self
            .client
            .inner
            .peers
            .get_transfer_connection(&self.client, &self.snapshot.username, token, cancel)
            .await?;
        conn.write_all(&token.to_le_bytes()).await?;
        Ok(conn)
    }

    fn message_timeout(&self) -> Duration {
        self.client
            .inner
            .options
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .message_timeout
    }

    fn resolve_size(&mut self, expected: Option<u64>, reported: Option<u64>) -> Result<()> {
        let Some(remote_size) = reported else {
            return expected
                .map(|_| ())
                .ok_or_else(|| Error::Transfer("Peer did not report the download size".into()));
        };
        let local_size = expected.unwrap_or(self.snapshot.start_offset);
        let resolved = match expected {
            Some(size) if size != remote_size => match &self.options.size_mismatch_resolver {
                Some(resolve) => resolve(&self.snapshot, remote_size),
                None if remote_size == 0 && size >= i32::MAX as u64 => Some(size),
                None => None,
            },
            _ => Some(remote_size),
        };
        match resolved.filter(|&size| size >= self.snapshot.start_offset) {
            Some(size) => {
                self.snapshot.size = size;
                Ok(())
            }
            None => Err(Error::TransferSizeMismatch {
                message: format!(
                    "Transfer aborted: the remote size of {remote_size} is incompatible with expected size {local_size} and start offset {}",
                    self.snapshot.start_offset
                ),
                local_size,
                remote_size,
            }),
        }
    }

    async fn download(
        &mut self,
        output: DownloadOutput,
        expected: Option<u64>,
        accepted: oneshot::Sender<()>,
        cancel: &CancellationToken,
    ) -> Result<()> {
        let client = self.client.clone();
        let peer = self.message_connection(cancel).await?;
        self.snapshot.ip_endpoint = Some(peer.remote());
        let (response, acknowledged) = oneshot::channel();
        let (offer_sender, offer) = oneshot::channel();
        self.pending(|entry| {
            entry.response = Some(response);
            entry.offer = Some(offer_sender);
        });
        let request = TransferRequest {
            direction: Direction::Download,
            token: self.snapshot.token,
            filename: self.snapshot.filename.clone(),
            file_size: 0,
        };
        self.state(State::REQUESTED);
        let response = self.request(&peer, request, acknowledged).await?;
        let ready = response.is_allowed;
        if ready {
            self.pending(|entry| {
                entry.offer = None;
            });
            self.resolve_size(expected, response.file_size)?;
        } else {
            let reason = response.message.as_deref().unwrap_or_default();
            if !reason
                .trim()
                .trim_end_matches('.')
                .eq_ignore_ascii_case("Queued")
            {
                return Err(rejected(reason));
            }
        }
        self.state(State::QUEUED | State::REMOTELY);
        let _ = accepted.send(());

        let offer = if ready {
            None
        } else {
            Some(receive(offer).await?)
        };
        if let Some(offer) = &offer {
            self.snapshot.remote_token = Some(offer.token);
            if let Err(error) = self.resolve_size(expected, Some(offer.file_size)) {
                client.inner.transfers.respond(
                    &client,
                    &self.snapshot.username,
                    peer.remote(),
                    Reply::RejectOffer(offer.token),
                );
                return Err(error);
            }
        }
        // Remotely queued downloads do not consume a data slot.
        let _slot = client
            .inner
            .transfers
            .slot(&client, Direction::Download)
            .await;
        self.state(State::INITIALIZING);
        let conn = if let Some(offer) = offer {
            let peer = self.message_connection(cancel).await?;
            let incoming = self.expect_incoming();
            let limit = client
                .inner
                .options
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .transfer_connection_options
                .connect_timeout;
            let response =
                TransferResponse::allowed(offer.token, Some(self.snapshot.size)).to_bytes();
            timeout(self.message_timeout(), peer.send(response)).await??;
            match timeout(limit, receive(incoming)).await {
                Ok(result) => result?,
                Err(_) => {
                    self.pending(|entry| {
                        entry.incoming = None;
                    });
                    self.outgoing_file(offer.token, cancel).await?
                }
            }
        } else {
            self.outgoing_file(response.token, cancel).await?
        };
        self.snapshot.ip_endpoint = Some(conn.remote());
        stream::download(self, conn, output).await
    }

    async fn request(
        &self,
        peer: &MessageConnection,
        request: TransferRequest,
        acknowledged: oneshot::Receiver<TransferResponse>,
    ) -> Result<TransferResponse> {
        timeout(self.message_timeout(), async {
            peer.send(request.to_bytes()).await?;
            tokio::select! {
                biased;
                response = receive(acknowledged) => response,
                _ = peer.closed() => Err(Error::Connection("Peer closed the transfer negotiation connection".into())),
            }
        }).await?
    }

    async fn upload(
        &mut self,
        source: UploadInput,
        cancel: &CancellationToken,
        external_slot: &mut bool,
        peer: &mut Option<MessageConnection>,
    ) -> Result<()> {
        if let Some(wait) = &self.options.slot_awaiter {
            wait(&self.snapshot).await?;
        }
        *external_slot = true;
        let client = self.client.clone();
        let _slot = client
            .inner
            .transfers
            .slot(&client, Direction::Upload)
            .await;
        let message = peer.insert(self.message_connection(cancel).await?);
        self.snapshot.ip_endpoint = Some(message.remote());
        let (response, acknowledged) = oneshot::channel();
        self.pending(|entry| {
            entry.response = Some(response);
        });
        let request = TransferRequest {
            direction: Direction::Upload,
            token: self.snapshot.token,
            filename: self.snapshot.filename.clone(),
            file_size: self.snapshot.size,
        };
        self.state(State::REQUESTED);
        let response = self.request(message, request, acknowledged).await?;
        if !response.is_allowed {
            return Err(rejected(response.message.as_deref().unwrap_or_default()));
        }
        self.state(State::INITIALIZING);
        let conn = self.outgoing_file(self.snapshot.token, cancel).await?;
        self.snapshot.ip_endpoint = Some(conn.remote());
        stream::upload(self, conn, source).await
    }

    fn report_upload_failure(&self, peer: Option<MessageConnection>, error: &Error) {
        let filename = self.snapshot.filename.clone();
        let frame = match error {
            Error::Cancelled => UploadDenied {
                filename,
                message: "Cancelled".into(),
            }
            .to_bytes(),
            _ => UploadFailed { filename }.to_bytes(),
        };
        let client = self.client.clone();
        let username = self.snapshot.username.clone();
        let session = self.session.clone();
        let limit = self.message_timeout();
        tokio::spawn(async move {
            let send = async {
                if let Some(peer) = peer.filter(|peer| !peer.is_closed()) {
                    peer.send(frame).await
                } else {
                    client
                        .inner
                        .peers
                        .send_to(&client, &username, frame, &session)
                        .await
                }
            };
            tokio::select! {
                biased;
                _ = session.cancelled() => {}
                _ = timeout(limit, send) => {}
            }
        });
    }
}

impl Drop for ActiveTransfer {
    fn drop(&mut self) {
        let mut registry = self.client.inner.transfers.registry();
        self.snapshots()
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.snapshot.token);
        registry.active.remove(&self.snapshot.token);
    }
}

async fn receive<T>(receiver: oneshot::Receiver<T>) -> Result<T> {
    receiver
        .await
        .map_err(|_| Error::Connection("Transfer negotiation ended unexpectedly".into()))
}

fn disconnected() -> Error {
    Error::Connection("Disconnected during transfer".into())
}

fn rejected(reason: &str) -> Error {
    Error::TransferRejected(format!("Transfer rejected: {reason}"))
}

fn place_key(username: &str, filename: &str) -> WaitKey {
    WaitKey(
        "transfer_place_in_queue",
        format!("{}:{username}{filename}", username.len()),
    )
}

impl SoulseekClient {
    /// Asks `username` for our place in their upload queue for `filename`
    /// (PlaceInQueueRequest → PlaceInQueueResponse). The download must be active
    /// locally → otherwise [`Error::TransferNotFound`]
    /// (`"A download of {filename} from user {username} is not active"`).
    pub async fn get_download_place_in_queue(
        &self,
        username: &str,
        filename: &str,
        cancel: CancellationToken,
    ) -> Result<u32> {
        require_non_blank(username, "username")?;
        require_non_blank(filename, "filename")?;
        self.inner.ensure_logged_in("get download place in queue")?;
        let session = {
            let registry = self.inner.transfers.registry();
            registry
                .active
                .values()
                .find(|entry| {
                    entry.direction == Direction::Download
                        && entry.username == username
                        && entry.filename == filename
                        && !entry.session.is_cancelled()
                })
                .map(|entry| entry.session.clone())
        }
        .ok_or_else(|| {
            Error::TransferNotFound(format!(
                "A download of {filename} from user {username} is not active"
            ))
        })?;
        let limit = self
            .inner
            .options
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .message_timeout;
        tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(Error::Cancelled),
            _ = session.cancelled() => Err(disconnected()),
            result = async {
                let peer = self.inner.peers.get_message_connection(self, username, &cancel).await?;
                let response = self.inner.waiter.wait::<u32>(place_key(username, filename), Some(limit));
                timeout(limit, peer.send(PlaceInQueueRequest { filename: filename.into() }.to_bytes())).await??;
                response.await
            } => result,
        }
    }

    /// Enqueues a download of `filename` from `username`; resolves once the peer
    /// has accepted the request (transfer started) or queued it remotely, and the
    /// returned handle resolves when the transfer ends.
    ///
    /// * `size`: expected size; `None` accepts the size reported by the peer.
    ///   A differing remote size is handled per [`TransferOptions::size_mismatch_resolver`]
    ///   ([`Error::TransferSizeMismatch`]).
    /// * `start_offset`: resume offset (must be ≤ size). A file download preserves
    ///   the existing prefix and overwrites data starting at the offset. A custom
    ///   output stream is seeked when [`TransferOptions::seek_output_stream_automatically`].
    /// * `token`: unique local token, generated when `None`
    ///   ([`Error::DuplicateToken`]); an active download of the same file from the
    ///   same user → [`Error::DuplicateTransfer`].
    /// * At most [`crate::ClientOptions::maximum_concurrent_downloads`] transfer data at
    ///   once; throughput is limited by [`crate::ClientOptions::maximum_download_speed`]
    ///   and [`TransferOptions::governor`].
    ///
    /// States (raised as [`Event::TransferStateChanged`] and via
    /// `options.state_changed`): `QUEUED | LOCALLY` → `REQUESTED` →
    /// `QUEUED | REMOTELY` → `INITIALIZING` → `IN_PROGRESS` → `COMPLETED |` one of
    /// `SUCCEEDED`, `CANCELLED`, `TIMED_OUT`, `ERRORED`, `REJECTED`, `ABORTED`.
    /// Progress raises [`Event::TransferProgressUpdated`].
    /// Errors: [`Error::UserOffline`], [`Error::TransferRejected`]
    /// (`"Transfer rejected: {reason}"`), [`Error::TransferReportedFailed`]
    /// (UploadFailed), [`Error::Timeout`], [`Error::Cancelled`].
    #[allow(clippy::too_many_arguments)]
    pub async fn enqueue_download(
        &self,
        username: &str,
        filename: &str,
        output: DownloadOutput,
        size: Option<u64>,
        start_offset: u64,
        token: Option<u32>,
        options: TransferOptions,
        cancel: CancellationToken,
    ) -> Result<TransferHandle> {
        require_non_blank(username, "username")?;
        require_non_blank(filename, "remote filename")?;
        if size.is_some_and(|size| start_offset > size) {
            return Err(Error::Argument(
                "The start offset must be less than or equal to the size".into(),
            ));
        }
        if let DownloadOutput::File(path) = &output
            && path.as_os_str().is_empty()
        {
            return Err(Error::Argument(
                "The local filename must not be empty".into(),
            ));
        }
        self.inner.ensure_logged_in("download files")?;
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let token = token.unwrap_or_else(|| self.get_next_token());
        let snapshot = Transfer::new(
            Direction::Download,
            username,
            filename,
            token,
            State::NONE,
            size.unwrap_or_default(),
            start_offset,
        );
        let mut transfer = ActiveTransfer::begin(self, snapshot, options)?;
        let (accepted, ready) = oneshot::channel();
        let (complete, completion) = oneshot::channel();
        let (failure, failed) = oneshot::channel();
        transfer.pending(|entry| {
            entry.failure = Some(failure);
        });
        tokio::spawn(async move {
            transfer.state(State::QUEUED | State::LOCALLY);
            let session = transfer.session.clone();
            let result = tokio::select! {
                biased;
                _ = cancel.cancelled() => Err(Error::Cancelled),
                _ = session.cancelled() => Err(disconnected()),
                error = receive(failed) => Err(error.unwrap_or_else(|error| error)),
                result = transfer.download(output, size, accepted, &cancel) => result,
            };
            transfer.finish(&result);
            let result = result.map(|()| transfer.snapshot.clone());
            drop(transfer);
            let _ = complete.send(result);
        });
        let handle = TransferHandle::new(token, completion);
        if ready.await.is_err() {
            return Err(handle.wait().await.err().unwrap_or_else(|| {
                Error::Transfer("Download ended before it was accepted".into())
            }));
        }
        Ok(handle)
    }

    /// Enqueues an upload of `filename` (the name requested by the peer) to
    /// `username`; resolves once registered locally and the handle resolves when
    /// the transfer ends.
    ///
    /// `token`/duplicates as for downloads ([`Error::DuplicateToken`],
    /// [`Error::DuplicateTransfer`]). The upload waits in `QUEUED | LOCALLY` for
    /// [`TransferOptions::slot_awaiter`] and a global slot
    /// ([`crate::ClientOptions::maximum_concurrent_uploads`]), then sends TransferRequest
    /// (`REQUESTED`); the peer's rejection → [`Error::TransferRejected`]. Data is
    /// sent from the offset requested by the peer, rate-limited by
    /// [`crate::ClientOptions::maximum_upload_speed`] and the governor. After the last
    /// byte the client waits up to [`TransferOptions::maximum_linger_time`] for
    /// the peer to close the connection. [`TransferOptions::slot_released`] is
    /// invoked once the slot is freed, whatever the outcome. States and events as
    /// for downloads.
    pub async fn enqueue_upload(
        &self,
        username: &str,
        filename: &str,
        input: UploadInput,
        token: Option<u32>,
        options: TransferOptions,
        cancel: CancellationToken,
    ) -> Result<TransferHandle> {
        require_non_blank(username, "username")?;
        require_non_blank(filename, "remote filename")?;
        if let UploadInput::File(path) = &input
            && path.as_os_str().is_empty()
        {
            return Err(Error::Argument(
                "The local filename must not be empty".into(),
            ));
        }
        self.inner.ensure_logged_in("upload files")?;
        let session = self.inner.transfers.registry().session.clone();
        let (size, input) = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(Error::Cancelled),
            _ = session.cancelled() => return Err(disconnected()),
            input = async move {
                match input {
                    UploadInput::File(path) => {
                        let size = tokio::fs::metadata(&path).await?.len();
                        Ok::<_, Error>((size, UploadInput::File(path)))
                    }
                    UploadInput::Stream { size, factory } => Ok((size, UploadInput::Stream { size, factory })),
                }
            } => input?,
        };
        let token = token.unwrap_or_else(|| self.get_next_token());
        let snapshot = Transfer::new(
            Direction::Upload,
            username,
            filename,
            token,
            State::NONE,
            size,
            0,
        );
        let mut transfer = ActiveTransfer::begin(self, snapshot, options)?;
        let (complete, completion) = oneshot::channel();
        tokio::spawn(async move {
            transfer.state(State::QUEUED | State::LOCALLY);
            let session = transfer.session.clone();
            let mut external_slot = false;
            let mut peer = None;
            let result = tokio::select! {
                biased;
                _ = cancel.cancelled() => Err(Error::Cancelled),
                _ = session.cancelled() => Err(disconnected()),
                result = transfer.upload(input, &cancel, &mut external_slot, &mut peer) => result,
            };
            transfer.finish(&result);
            if external_slot && let Some(released) = &transfer.options.slot_released {
                released(&transfer.snapshot);
            }
            if let Err(error) = &result
                && !session.is_cancelled()
            {
                transfer.report_upload_failure(peer, error);
            }
            let result = result.map(|()| transfer.snapshot.clone());
            drop(transfer);
            let _ = complete.send(result);
        });
        Ok(TransferHandle::new(token, completion))
    }
}
