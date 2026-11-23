use std::{
    net::{IpAddr, SocketAddr},
    path::Path,
};

use axum::{
    Router,
    body::Body,
    extract::{
        ConnectInfo, DefaultBodyLimit, Multipart, Path as RoutePath, State, multipart::Field,
    },
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post, put},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use futures::StreamExt;
use slskr_api::relay::headers;
use tokio::io::{AsyncWriteExt, duplex};
use tokio_util::io::ReaderStream;
use uuid::Uuid;

use crate::{
    app::AppState,
    auth::{RequireAdminJwt, RequireApiKey, RequireReadWriteApiKey},
};

use super::{
    RelayService,
    controller::{Grant, HttpKind, RelayFile},
    download_root, open_regular, valid_name,
};

const TIB: usize = 1 << 40;
const METADATA_LIMIT: usize = 1024 * 1024;
/// Mirrors `shares::relay::MAX_DATABASE_BYTES` (that module is private to `shares`);
/// the importer re-checks the staged file against its own constant.
const MAX_DATABASE_BYTES: u64 = 2 * 1024 * 1024 * 1024;
/// Database part + metadata part + slack for multipart boundaries and part headers.
const SHARES_BODY_LIMIT: usize = MAX_DATABASE_BYTES as usize + METADATA_LIMIT + 1024 * 1024;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/relay/agent", put(connect).delete(disconnect))
        .route(
            "/relay/controller/files/{token}",
            post(upload_file).layer(DefaultBodyLimit::max(10 * TIB)),
        )
        .route(
            "/relay/controller/shares/{token}",
            post(upload_shares).layer(DefaultBodyLimit::max(SHARES_BODY_LIMIT)),
        )
        .route(
            "/relay/controller/downloads/{token}",
            get(download).head(|| async { StatusCode::METHOD_NOT_ALLOWED }),
        )
}

async fn connect(_: RequireAdminJwt, State(app): State<AppState>) -> StatusCode {
    let relay = &app.services.relay;
    if !relay.connects_to_controller() {
        return StatusCode::FORBIDDEN;
    }
    relay.0.desired.send_replace(true);
    let mut ready = relay.0.ready.subscribe();
    let mut desired = relay.0.desired.subscribe();
    tokio::select! {
        result = tokio::time::timeout(std::time::Duration::from_secs(15), ready.wait_for(|ready| *ready)) => {
            if matches!(result, Ok(Ok(_))) { StatusCode::OK } else { StatusCode::SERVICE_UNAVAILABLE }
        }
        _ = desired.wait_for(|desired| !*desired) => StatusCode::SERVICE_UNAVAILABLE,
        _ = relay.0.core.shutdown.cancelled() => StatusCode::SERVICE_UNAVAILABLE,
    }
}

async fn disconnect(_: RequireAdminJwt, State(app): State<AppState>) -> StatusCode {
    let relay = &app.services.relay;
    if !relay.connects_to_controller() {
        return StatusCode::FORBIDDEN;
    }
    relay.0.desired.send_replace(false);
    let mut ready = relay.0.ready.subscribe();
    let _ = ready.wait_for(|ready| !*ready).await;
    StatusCode::NO_CONTENT
}

fn request_token(relay: &RelayService, token: &str) -> Result<Uuid, StatusCode> {
    if !relay.serves_agents() {
        return Err(StatusCode::FORBIDDEN);
    }
    Uuid::parse_str(token).map_err(|_| StatusCode::BAD_REQUEST)
}

fn relay_header<'a>(headers: &'a HeaderMap, name: &'static str) -> Option<&'a str> {
    headers
        .get(name)?
        .to_str()
        .ok()
        .filter(|value| !value.is_empty())
}

fn claim(
    relay: &RelayService,
    token: Uuid,
    ip: IpAddr,
    headers: &HeaderMap,
    kind: HttpKind<'_>,
) -> Result<Grant, StatusCode> {
    let agent = relay_header(headers, headers::AGENT).ok_or(StatusCode::UNAUTHORIZED)?;
    let credential = relay_header(headers, headers::CREDENTIAL).ok_or(StatusCode::UNAUTHORIZED)?;
    relay
        .claim(token, agent, ip, credential, kind)
        .ok_or(StatusCode::UNAUTHORIZED)
}

async fn upload_file(
    _: RequireReadWriteApiKey,
    State(app): State<AppState>,
    RoutePath(token): RoutePath<String>,
    ConnectInfo(remote): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    multipart: Result<Multipart, axum::extract::multipart::MultipartRejection>,
) -> Result<StatusCode, StatusCode> {
    let relay = &app.services.relay;
    let token = request_token(relay, &token)?;
    let mut multipart = multipart.map_err(|_| StatusCode::UNSUPPORTED_MEDIA_TYPE)?;
    let mut field = next_field(&mut multipart)
        .await?
        .ok_or(StatusCode::BAD_REQUEST)?;
    let filename = field
        .file_name()
        .and_then(parse_filename)
        .filter(|name| valid_name(name))
        .ok_or(StatusCode::BAD_REQUEST)?;
    if field.name() != Some("file") {
        return Err(StatusCode::BAD_REQUEST);
    }
    let grant = claim(
        relay,
        token,
        remote.ip(),
        &headers,
        HttpKind::Upload(&filename),
    )?;
    let (length, mut reply) = grant.upload.expect("upload grant");
    let (mut sink, reader) = duplex(64 * 1024);
    // Before returning a reader, keep HTTP processing tied to the pending request.
    let first = tokio::select! {
        _ = grant.closed.cancelled() => return Err(StatusCode::GONE),
        _ = reply.closed() => return Err(StatusCode::GONE),
        result = field.chunk() => result.map_err(|error| error.status())?,
    };
    if length == 0 {
        if first.is_some_and(|chunk| !chunk.is_empty()) {
            return Err(StatusCode::BAD_REQUEST);
        }
        drop(field);
        tokio::select! {
            _ = grant.closed.cancelled() => return Err(StatusCode::GONE),
            _ = reply.closed() => return Err(StatusCode::GONE),
            result = next_field(&mut multipart) => if result?.is_some() { return Err(StatusCode::BAD_REQUEST); },
        }
        reply
            .send(Ok(RelayFile::new(
                reader,
                length,
                grant.closed.child_token(),
            )))
            .map_err(|_| StatusCode::GONE)?;
        return Ok(StatusCode::OK);
    }
    let mut chunk = first.ok_or(StatusCode::BAD_REQUEST)?;
    let transfer = grant.closed.child_token();
    reply
        .send(Ok(RelayFile::new(reader, length, transfer.clone())))
        .map_err(|_| StatusCode::GONE)?;
    let last_byte = tokio::select! {
        _ = transfer.cancelled() => return Err(StatusCode::GONE),
        result = async {
            let mut remaining = length;
            let mut last_byte = None;
            loop {
                let amount = chunk.len() as u64;
                if amount > remaining { return Err(StatusCode::BAD_REQUEST); }
                remaining -= amount;
                if amount != 0 {
                    let bytes = if remaining == 0 {
                        let (last, prefix) = chunk.split_last().unwrap();
                        last_byte = Some(*last);
                        prefix
                    } else { &chunk };
                    sink.write_all(bytes).await.map_err(|_| StatusCode::GONE)?;
                }
                match field.chunk().await.map_err(|error| error.status())? {
                    Some(next) => chunk = next,
                    None => break,
                }
            }
            last_byte.ok_or(StatusCode::BAD_REQUEST).and_then(|last| {
                if remaining == 0 { Ok(last) } else { Err(StatusCode::BAD_REQUEST) }
            })
        } => result,
    }?;
    drop(field);
    tokio::select! {
        _ = transfer.cancelled() => return Err(StatusCode::GONE),
        result = next_field(&mut multipart) => if result?.is_some() { return Err(StatusCode::BAD_REQUEST); },
    }
    // Do not let the consumer see a complete file before validating the closing boundary.
    sink.write_all(&[last_byte])
        .await
        .map_err(|_| StatusCode::GONE)?;
    Ok(StatusCode::OK)
}

fn parse_filename(value: &str) -> Option<std::borrow::Cow<'_, str>> {
    if let Some(encoded) = value
        .strip_prefix("=?utf-8?B?")
        .or_else(|| value.strip_prefix("=?utf-8?b?"))
    {
        let bytes = STANDARD.decode(encoded.strip_suffix("?=")?).ok()?;
        Some(String::from_utf8(bytes).ok()?.into())
    } else {
        Some(value.into())
    }
}

async fn next_field(multipart: &mut Multipart) -> Result<Option<Field<'_>>, StatusCode> {
    multipart.next_field().await.map_err(|error| error.status())
}

async fn upload_shares(
    _: RequireReadWriteApiKey,
    State(app): State<AppState>,
    RoutePath(token): RoutePath<String>,
    ConnectInfo(remote): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    multipart: Result<Multipart, axum::extract::multipart::MultipartRejection>,
) -> Result<StatusCode, StatusCode> {
    let relay = &app.services.relay;
    let token = request_token(relay, &token)?;
    let mut multipart = multipart.map_err(|_| StatusCode::UNSUPPORTED_MEDIA_TYPE)?;
    let grant = claim(relay, token, remote.ip(), &headers, HttpKind::Share)?;
    let agent = relay_header(&headers, headers::AGENT).unwrap().to_owned();
    let directory = tempfile::tempdir().map_err(internal)?;
    let path = directory.path().join("shares.db");
    let metadata = tokio::select! {
        _ = grant.closed.cancelled() => return Err(StatusCode::GONE),
        result = receive_shares(&mut multipart, &path) => result?,
    };
    let shares = relay.0.shares.clone();
    tokio::task::spawn_blocking(move || {
        let _directory = directory;
        if grant.closed.is_cancelled() {
            return Err(StatusCode::GONE);
        }
        shares
            .import_relay(&agent, metadata, &path, &grant.closed)
            .map_err(|error| {
                if grant.closed.is_cancelled() {
                    return StatusCode::GONE;
                }
                tracing::warn!(%agent, %error, "rejected relay share index");
                StatusCode::BAD_REQUEST
            })
    })
    .await
    .map_err(internal)??;
    Ok(StatusCode::OK)
}

async fn receive_shares(
    multipart: &mut Multipart,
    path: &Path,
) -> Result<serde_json::Value, StatusCode> {
    let mut metadata = None;
    let mut database = false;
    while let Some(mut field) = next_field(multipart).await? {
        match field.name() {
            Some("shares") if metadata.is_none() && field.file_name().is_none() => {
                let mut bytes = Vec::new();
                while let Some(chunk) = field.chunk().await.map_err(|error| error.status())? {
                    if bytes.len() + chunk.len() > METADATA_LIMIT {
                        return Err(StatusCode::PAYLOAD_TOO_LARGE);
                    }
                    bytes.extend_from_slice(&chunk);
                }
                metadata =
                    Some(serde_json::from_slice(&bytes).map_err(|_| StatusCode::BAD_REQUEST)?);
            }
            Some("database") if !database && field.file_name() == Some("shares") => {
                let mut output = tokio::fs::File::create_new(path).await.map_err(internal)?;
                let mut written = 0u64;
                while let Some(chunk) = field.chunk().await.map_err(|error| error.status())? {
                    written += chunk.len() as u64;
                    if written > MAX_DATABASE_BYTES {
                        return Err(StatusCode::PAYLOAD_TOO_LARGE);
                    }
                    output.write_all(&chunk).await.map_err(internal)?;
                }
                output.flush().await.map_err(internal)?;
                database = true;
            }
            _ => return Err(StatusCode::BAD_REQUEST),
        }
    }
    metadata.filter(|_| database).ok_or(StatusCode::BAD_REQUEST)
}

async fn download(
    _: RequireApiKey,
    State(app): State<AppState>,
    RoutePath(token): RoutePath<String>,
    ConnectInfo(remote): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Result<Response, StatusCode> {
    let relay = &app.services.relay;
    let token = request_token(relay, &token)?;
    let name = relay_header(&headers, headers::FILENAME_BASE64).ok_or(StatusCode::BAD_REQUEST)?;
    let filename = String::from_utf8(STANDARD.decode(name).map_err(|_| StatusCode::BAD_REQUEST)?)
        .map_err(|_| StatusCode::BAD_REQUEST)?;
    if !valid_name(&filename) {
        return Err(StatusCode::BAD_REQUEST);
    }
    let grant = claim(
        relay,
        token,
        remote.ip(),
        &headers,
        HttpKind::Download(&filename),
    )?;
    let requested = grant.download.expect("download grant");
    let file = tokio::task::spawn_blocking(move || {
        open_regular(&download_root(&requested.root)?, &requested.relative)
    })
    .await
    .map_err(internal)?
    .map_err(|_| StatusCode::NOT_FOUND)?;
    let length = file.metadata().map_err(internal)?.len();
    let stream = ReaderStream::new(tokio::fs::File::from_std(file))
        .take_until(grant.closed.cancelled_owned());
    Ok((
        [
            (header::CONTENT_TYPE, "application/octet-stream".to_owned()),
            (header::CONTENT_LENGTH, length.to_string()),
        ],
        Body::from_stream(stream),
    )
        .into_response())
}

fn internal(error: impl std::fmt::Display) -> StatusCode {
    tracing::warn!(%error, "relay HTTP I/O failed");
    StatusCode::INTERNAL_SERVER_ERROR
}
