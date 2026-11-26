//! Dispatch of complete frames from an identified peer message connection.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use tokio::sync::{Semaphore, mpsc};
use tokio_util::sync::CancellationToken;

use super::SoulseekClient;
use crate::error::Error;
use crate::events::Event;
use crate::model::{BrowseResponse, SearchQuery, UserInfo};
use crate::net::connection::{FrameObserver, FrameProgress};
use crate::net::message_connection::MessageConnection;
use crate::net::waiter::WaitKey;
use crate::protocol::ProtocolError;
use crate::protocol::codes::PeerCode;
use crate::protocol::peer::{FolderContentsResponse, PeerIncoming};
use crate::resolvers::Resolvers;

/// Requests answered concurrently per connection; further requests wait for a slot.
const MAX_CONCURRENT_ANSWERS: usize = 4;
/// Compressed responses inflated at once across all connections, bounding the memory a
/// crowd of peers can make the client hold (each may inflate to tens of MiB).
const MAX_CONCURRENT_INFLATES: usize = 8;
static INFLATE_SLOTS: Semaphore = Semaphore::const_new(MAX_CONCURRENT_INFLATES);
/// Frames larger than this (compressed) are decoded off the async workers.
pub(super) const BLOCKING_DECODE_LEN: u64 = 256 * 1024;

// Responses are keyed by user, not socket: a peer may answer on another connection.

pub(super) fn browse_key(username: &str) -> WaitKey {
    WaitKey("peer.browse", username.to_owned())
}

/// Completed with the body length once a browse response starts arriving.
pub(super) fn browse_start_key(username: &str) -> WaitKey {
    WaitKey("peer.browse.start", username.to_owned())
}

pub(super) fn directory_key(username: &str, token: u32) -> WaitKey {
    WaitKey("peer.directory", format!("{token}:{username}"))
}

pub(super) fn info_key(username: &str) -> WaitKey {
    WaitKey("peer.info", username.to_owned())
}

/// Browse responses read (or being read) on one connection, in frame order;
/// `true` when the frame's start was claimed by a waiting [`SoulseekClient::browse`].
#[derive(Default)]
pub(super) struct BrowseTracking {
    frames: VecDeque<bool>,
    current: bool,
}

pub(super) type Tracking = Arc<Mutex<BrowseTracking>>;

fn lock(tracking: &Tracking) -> std::sync::MutexGuard<'_, BrowseTracking> {
    tracking.lock().unwrap_or_else(|e| e.into_inner())
}

/// Observes frames as the reader receives them, so a browse can stop its response
/// timeout as soon as the response starts and report progress while it streams.
pub(super) fn observer(client: &SoulseekClient, username: &str) -> (FrameObserver, Tracking) {
    let weak = Arc::downgrade(client.inner());
    let username = username.to_owned();
    let tracking = Tracking::default();
    let state = tracking.clone();
    let observer: FrameObserver = Arc::new(move |progress: &FrameProgress| {
        if progress.code != PeerCode::BrowseResponse as u32 {
            return;
        }
        let Some(inner) = weak.upgrade() else { return };
        let mut state = lock(&state);
        if progress.first {
            let claimed = inner
                .waiter
                .complete(&browse_start_key(&username), progress.total as u64);
            state.frames.push_back(claimed);
            state.current = claimed;
        }
        let report = state.current && progress.received < progress.total;
        drop(state);
        if report {
            // The completed size is reported by browse() once the response is decoded.
            inner.emit(Event::BrowseProgressUpdated {
                username: username.clone(),
                bytes_transferred: progress.received as u64,
                size: progress.total as u64,
            });
        }
    });
    (observer, tracking)
}

pub(super) fn spawn(
    client: &SoulseekClient,
    username: String,
    connection: MessageConnection,
    mut incoming: mpsc::Receiver<Bytes>,
    session: CancellationToken,
    tracking: Tracking,
) {
    let weak = Arc::downgrade(client.inner());
    tokio::spawn(async move {
        let id = connection.id();
        let remote = connection.remote();
        // Answers run beside the read loop and end with this connection or the session.
        let answers = session.child_token();
        let slots = Arc::new(Semaphore::new(MAX_CONCURRENT_ANSWERS));
        loop {
            // Drain frames already read before EOF, but not after a session disconnect.
            let frame = tokio::select! {
                biased;
                _ = session.cancelled() => break,
                frame = incoming.recv() => match frame {
                    Some(frame) => frame,
                    None => break,
                },
            };
            let size = frame.len() as u64;
            let code = frame.first_chunk::<4>().copied().map(u32::from_le_bytes);
            let code = code.and_then(|code| PeerCode::try_from(code).ok());
            // Compressed responses can inflate a thousandfold, so ones nobody asked for are
            // dropped before inflating them.
            let wanted = match code {
                Some(PeerCode::BrowseResponse) => lock(&tracking).frames.pop_front() == Some(true),
                Some(PeerCode::SearchResponse) => weak
                    .upgrade()
                    .is_some_and(|inner| inner.searches.any_active()),
                _ => true,
            };
            if !wanted {
                tracing::debug!(%username, %remote, ?code, "Dropping an unrequested response");
                continue;
            }
            let inflating = match code {
                Some(
                    PeerCode::BrowseResponse
                    | PeerCode::SearchResponse
                    | PeerCode::FolderContentsResponse,
                ) => tokio::select! {
                    biased;
                    _ = session.cancelled() => break,
                    slot = INFLATE_SLOTS.acquire() => slot.ok(),
                },
                _ => None,
            };
            let decoded = if size > BLOCKING_DECODE_LEN {
                // Inflating and parsing a large share listing would stall this worker.
                tokio::task::spawn_blocking(move || PeerIncoming::decode(frame))
                    .await
                    .unwrap_or_else(|error| {
                        Err(ProtocolError::Invalid(format!("decoding stopped: {error}")))
                    })
            } else {
                PeerIncoming::decode(frame)
            };
            drop(inflating);
            let Some(inner) = weak.upgrade() else { break };
            let client = SoulseekClient::from_inner(inner);
            let inner = client.inner();
            let message = match decoded {
                Ok(message) => message,
                Err(error) => {
                    tracing::debug!(%username, %remote, %error, "Invalid peer message");
                    let key = match code {
                        Some(PeerCode::BrowseResponse) => Some(browse_key(&username)),
                        Some(PeerCode::InfoResponse) => Some(info_key(&username)),
                        _ => None,
                    };
                    if let Some(key) = key {
                        inner.waiter.throw(&key, error.into());
                    }
                    continue;
                }
            };
            match message {
                PeerIncoming::BrowseResponse(response) => {
                    inner
                        .waiter
                        .complete(&browse_key(&username), (response, size));
                }
                PeerIncoming::InfoResponse(response) => {
                    inner.waiter.complete(&info_key(&username), response);
                }
                PeerIncoming::FolderContentsResponse(response) => {
                    inner.waiter.complete(
                        &directory_key(&username, response.token),
                        response.directories,
                    );
                }
                PeerIncoming::SearchResponse(response) => {
                    inner.searches.handle_search_response(&client, response);
                }
                message @ (PeerIncoming::TransferRequest(_)
                | PeerIncoming::TransferResponse(_)
                | PeerIncoming::QueueDownload(_)
                | PeerIncoming::PlaceInQueueRequest(_)
                | PeerIncoming::PlaceInQueueResponse(_)
                | PeerIncoming::UploadFailed(_)
                | PeerIncoming::UploadDenied(_)) => {
                    inner
                        .transfers
                        .handle_peer_message(&client, &username, remote, message);
                }
                PeerIncoming::Unknown { code, .. } => {
                    tracing::trace!(%username, %remote, code, "Ignoring unknown peer message");
                }
                request @ (PeerIncoming::BrowseRequest(_)
                | PeerIncoming::InfoRequest(_)
                | PeerIncoming::FolderContentsRequest(_)
                | PeerIncoming::SearchRequest(_)) => {
                    let resolvers = inner.resolvers.clone();
                    let local_username = if matches!(&request, PeerIncoming::SearchRequest(_)) {
                        client.username()
                    } else {
                        None
                    };
                    let events = inner.events.clone();
                    drop(client);
                    let slot = tokio::select! {
                        biased;
                        _ = session.cancelled() => break,
                        // Nothing can be sent; keep draining what was already read.
                        _ = connection.closed() => continue,
                        slot = slots.clone().acquire_owned() => slot.expect("answer slots are never closed"),
                    };
                    let connection = connection.clone();
                    let username = username.clone();
                    let cancel = answers.clone();
                    tokio::spawn(async move {
                        let _slot = slot;
                        tokio::select! {
                            biased;
                            _ = cancel.cancelled() => {},
                            _ = connection.closed() => {},
                            _ = answer(resolvers.as_ref(), &username, &connection, request, local_username, &events) => {},
                        }
                    });
                }
            }
        }
        answers.cancel();
        connection.close();
        if let Some(inner) = weak.upgrade() {
            // A browse response cut off mid-frame must not leave its caller waiting.
            let interrupted = lock(&tracking)
                .frames
                .drain(..)
                .filter(|&claimed| claimed)
                .count();
            for _ in 0..interrupted {
                inner.waiter.throw(
                    &browse_key(&username),
                    Error::Connection(format!(
                        "Peer connection to {username} ({remote}) closed while receiving a browse response"
                    )),
                );
            }
            inner.peers.forget_message(&username, id);
        }
    });
}

async fn answer(
    resolvers: &dyn Resolvers,
    username: &str,
    connection: &MessageConnection,
    request: PeerIncoming,
    local_username: Option<String>,
    events: &tokio::sync::broadcast::Sender<Event>,
) {
    let remote = connection.remote();
    let frame = match request {
        PeerIncoming::BrowseRequest(_) => {
            match resolvers.browse_frame(username.into(), remote).await {
                Ok(frame) => frame,
                Err(error) => {
                    tracing::warn!(%username, %remote, %error, "Browse resolver failed");
                    BrowseResponse::default().to_bytes()
                }
            }
        }
        PeerIncoming::InfoRequest(_) => match resolvers.user_info(username.into(), remote).await {
            Ok(response) => response.to_bytes(),
            Err(error) => {
                tracing::warn!(%username, %remote, %error, "User info resolver failed");
                UserInfo::default().to_bytes()
            }
        },
        PeerIncoming::FolderContentsRequest(request) => match resolvers
            .directory_contents(
                username.into(),
                remote,
                request.token,
                request.directory_name.clone(),
            )
            .await
        {
            Ok(directories) => FolderContentsResponse {
                token: request.token,
                directory_name: request.directory_name,
                directories,
            }
            .to_bytes(),
            Err(error) => {
                tracing::debug!(%username, %remote, %error, "Directory contents resolver declined request");
                return;
            }
        },
        PeerIncoming::SearchRequest(request) => {
            let Some(local_username) = local_username else {
                return;
            };
            let token = request.token;
            let query = request.query;
            let _ = events.send(Event::SearchRequestReceived {
                username: username.into(),
                token,
                query: query.clone(),
            });
            let Some(mut response) = resolvers
                .search(username.into(), token, SearchQuery::parse(&query))
                .await
                .filter(|response| !response.files.is_empty() || !response.locked_files.is_empty())
            else {
                return;
            };
            response.username = local_username;
            response.token = token;
            let result = connection.send(response.to_bytes()).await;
            let username = username.into();
            let _ = events.send(if result.is_ok() {
                Event::SearchResponseDelivered {
                    username,
                    token,
                    query,
                    response,
                }
            } else {
                Event::SearchResponseDeliveryFailed {
                    username,
                    token,
                    query,
                    response,
                }
            });
            return;
        }
        _ => unreachable!("only peer requests require resolver replies"),
    };
    if let Err(error) = connection.send(frame).await {
        tracing::debug!(%username, %remote, %error, "Could not send peer reply");
    }
}
