//! Dispatch of complete frames from an identified peer message connection.

use std::sync::Arc;

use bytes::Bytes;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::SoulseekClient;
use crate::events::Event;
use crate::model::{BrowseResponse, SearchQuery, UserInfo};
use crate::net::message_connection::MessageConnection;
use crate::net::waiter::WaitKey;
use crate::protocol::codes::PeerCode;
use crate::protocol::peer::{FolderContentsResponse, PeerIncoming};
use crate::resolvers::Resolvers;

pub(super) fn browse_key(id: u64) -> WaitKey {
    WaitKey("peer.browse", id.to_string())
}

pub(super) fn directory_key(id: u64, token: u32) -> WaitKey {
    WaitKey("peer.directory", format!("{id}:{token}"))
}

pub(super) fn info_key(id: u64) -> WaitKey {
    WaitKey("peer.info", id.to_string())
}

pub(super) fn spawn(
    client: &SoulseekClient,
    username: String,
    connection: MessageConnection,
    mut incoming: mpsc::Receiver<Bytes>,
    session: CancellationToken,
) {
    let weak = Arc::downgrade(client.inner());
    tokio::spawn(async move {
        let id = connection.id();
        let remote = connection.remote();
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
            let Some(inner) = weak.upgrade() else { break };
            let client = SoulseekClient::from_inner(inner);
            let inner = client.inner();
            let size = frame.len() as u64;
            let code = frame.first_chunk::<4>().copied().map(u32::from_le_bytes);
            let message = match PeerIncoming::decode(frame) {
                Ok(message) => message,
                Err(error) => {
                    tracing::debug!(%username, %remote, %error, "Invalid peer message");
                    let key = match code.and_then(|code| PeerCode::try_from(code).ok()) {
                        Some(PeerCode::BrowseResponse) => Some(browse_key(id)),
                        Some(PeerCode::InfoResponse) => Some(info_key(id)),
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
                    inner.waiter.complete(&browse_key(id), (response, size));
                }
                PeerIncoming::InfoResponse(response) => {
                    inner.waiter.complete(&info_key(id), response);
                }
                PeerIncoming::FolderContentsResponse(response) => {
                    inner
                        .waiter
                        .complete(&directory_key(id, response.token), response.directories);
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
                    tokio::select! {
                        biased;
                        _ = session.cancelled() => break,
                        _ = connection.closed() => {},
                        _ = answer(resolvers.as_ref(), &username, &connection, request, local_username, &events) => {},
                    }
                }
            }
        }
        connection.close();
        if let Some(inner) = weak.upgrade() {
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
        PeerIncoming::BrowseRequest(_) => match resolvers.browse(username.into(), remote).await {
            Ok(response) => response.to_bytes(),
            Err(error) => {
                tracing::warn!(%username, %remote, %error, "Browse resolver failed");
                BrowseResponse::default().to_bytes()
            }
        },
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
