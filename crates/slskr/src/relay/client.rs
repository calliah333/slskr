use std::{collections::VecDeque, time::Duration};

use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use futures::{SinkExt, StreamExt};
use reqwest::{
    Client, Method, RequestBuilder, Url, header,
    multipart::{Form, Part},
};
use serde::Deserialize;
use serde_json::{Value, json};
use slskr_api::{
    options::{Options, RelayControllerOptions},
    relay::{
        RelayClientState, RelayMode, headers, hub_client_methods as inbound,
        hub_server_methods as outbound,
    },
};
use tokio::{
    io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt},
    task::JoinSet,
};
use tokio_tungstenite::{
    WebSocketStream,
    tungstenite::{
        Message,
        handshake::{client::generate_key, derive_accept_key},
        protocol::{Role, WebSocketConfig},
    },
};
use tokio_util::io::ReaderStream;
use uuid::Uuid;

use super::{CredentialKey, RelayService, descend, download_root, open_shared, safe_relative};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(12);
const KEEPALIVE: Duration = Duration::from_secs(10);
const SILENCE_TIMEOUT: Duration = Duration::from_secs(40);
const MAX_SIGNALR_FRAME: usize = 128 * 1024;
const MAX_JOBS: usize = 16;
const LOGIN: &str = "login";
const SHARE: &str = "share";

pub(super) async fn supervise(relay: RelayService) {
    let mut desired = relay.0.desired.subscribe();
    let mut options = relay.0.core.options.subscribe();
    let mut attempt: usize = 0;
    loop {
        relay.0.ready.send_replace(false);
        if !*desired.borrow() {
            relay.set_client_state(RelayClientState::Disconnected);
            attempt = 0;
            tokio::select! {
                _ = relay.0.core.shutdown.cancelled() => break,
                result = desired.wait_for(|desired| *desired) => if result.is_err() { break; },
            }
        }
        let settings = Settings::from_options(&options.borrow_and_update());
        relay.set_client_state(if attempt == 0 {
            RelayClientState::Connecting
        } else {
            RelayClientState::Reconnecting
        });
        let result = tokio::select! {
            biased;
            _ = relay.0.core.shutdown.cancelled() => break,
            _ = desired.wait_for(|desired| !*desired) => { attempt = 0; continue; },
            _ = settings.changed(&mut options) => { attempt = 0; continue; },
            result = session(&relay, &settings) => result,
        };
        if relay.0.ready.send_replace(false) {
            attempt = 0;
        }
        relay.set_client_state(RelayClientState::Reconnecting);
        if let Err(error) = result {
            tracing::warn!(%error, "relay controller connection ended");
        }
        let seconds = [1, 3, 10, 30, 60][attempt.min(4)];
        attempt = attempt.saturating_add(1);
        tokio::select! {
            _ = relay.0.core.shutdown.cancelled() => break,
            _ = desired.wait_for(|desired| !*desired) => { attempt = 0; },
            _ = settings.changed(&mut options) => { attempt = 0; },
            _ = tokio::time::sleep(Duration::from_secs(seconds)) => (),
        }
    }
    relay.0.ready.send_replace(false);
    relay.set_client_state(RelayClientState::Disconnected);
}

#[derive(PartialEq)]
struct Settings {
    agent: String,
    controller: RelayControllerOptions,
}

impl Settings {
    fn from_options(options: &Options) -> Self {
        Self {
            agent: options.instance_name.clone(),
            controller: options.relay.controller.clone(),
        }
    }

    async fn changed(&self, options: &mut tokio::sync::watch::Receiver<std::sync::Arc<Options>>) {
        let _ = options
            .wait_for(|options| {
                options.instance_name != self.agent || options.relay.controller != self.controller
            })
            .await;
    }
}

struct AgentClient {
    http: Client,
    base: Url,
    agent: String,
    api_key: String,
    key: CredentialKey,
}

impl AgentClient {
    fn new(settings: &Settings) -> Result<Self> {
        let controller = &settings.controller;
        let mut base = Url::parse(
            controller
                .address
                .as_deref()
                .context("Relay controller address is not configured")?,
        )?;
        ensure!(
            matches!(base.scheme(), "http" | "https")
                && base.host().is_some()
                && base.username().is_empty()
                && base.password().is_none()
                && base.query().is_none()
                && base.fragment().is_none(),
            "Invalid relay controller address"
        );
        if !base.path().ends_with('/') {
            base.path_segments_mut().expect("HTTP URL").push("");
        }
        let api_key = controller
            .api_key
            .as_ref()
            .filter(|s| !s.is_empty())
            .context("Relay controller API key is not configured")?
            .clone();
        let secret = controller
            .secret
            .as_deref()
            .filter(|s| !s.is_empty())
            .context("Relay secret is not configured")?;
        ensure!(!settings.agent.is_empty(), "Relay instance name is empty");
        let http = Client::builder()
            .http1_only()
            .connect_timeout(HANDSHAKE_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .danger_accept_invalid_certs(controller.ignore_certificate_errors)
            .build()?;
        Ok(Self {
            http,
            base,
            agent: settings.agent.clone(),
            api_key,
            key: CredentialKey::new(&settings.agent, secret),
        })
    }

    fn request(&self, method: Method, kind: &str, token: Uuid) -> RequestBuilder {
        let url = self
            .base
            .join(&format!("api/v0/relay/controller/{kind}/{token}"))
            .expect("fixed relay route");
        self.http
            .request(method, url)
            .header("X-API-Key", &self.api_key)
            .header(headers::AGENT, &self.agent)
            .header(headers::CREDENTIAL, self.key.sign(&token.to_string()))
    }

    async fn connect(&self) -> Result<SignalR> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Negotiation {
            connection_token: String,
        }
        let negotiate = self.base.join("hub/relay/negotiate?negotiateVersion=1")?;
        let response = self
            .http
            .post(negotiate)
            .bearer_auth(&self.api_key)
            .header("X-API-Key", &self.api_key)
            .send()
            .await?
            .error_for_status()?;
        let mut bytes = Vec::new();
        let mut chunks = response.bytes_stream();
        while let Some(chunk) = chunks.next().await {
            let chunk = chunk?;
            ensure!(
                bytes.len() + chunk.len() <= MAX_SIGNALR_FRAME,
                "Relay negotiate response is too large"
            );
            bytes.extend_from_slice(&chunk);
        }
        let negotiation: Negotiation = serde_json::from_slice(&bytes)?;
        ensure!(
            !negotiation.connection_token.is_empty(),
            "Relay negotiation omitted the connection token"
        );
        let mut url = self.base.join("hub/relay")?;
        url.query_pairs_mut()
            .append_pair("id", &negotiation.connection_token);
        let ws_key = generate_key();
        let response = self
            .http
            .get(url)
            .bearer_auth(&self.api_key)
            .header("X-API-Key", &self.api_key)
            .header(header::CONNECTION, "Upgrade")
            .header(header::UPGRADE, "websocket")
            .header("Sec-WebSocket-Key", &ws_key)
            .header("Sec-WebSocket-Version", "13")
            .send()
            .await?;
        ensure!(
            response.status() == reqwest::StatusCode::SWITCHING_PROTOCOLS
                && response
                    .headers()
                    .get(header::UPGRADE)
                    .and_then(|value| value.to_str().ok())
                    .is_some_and(|value| value.eq_ignore_ascii_case("websocket"))
                && response
                    .headers()
                    .get(header::CONNECTION)
                    .and_then(|value| value.to_str().ok())
                    .is_some_and(|value| value
                        .split(',')
                        .any(|part| part.trim().eq_ignore_ascii_case("upgrade")))
                && response
                    .headers()
                    .get("Sec-WebSocket-Accept")
                    .and_then(|value| value.to_str().ok())
                    .is_some_and(|value| value == derive_accept_key(ws_key.as_bytes())),
            "Relay WebSocket upgrade was rejected"
        );
        let transport = response.upgrade().await?;
        let config = WebSocketConfig::default()
            .max_message_size(Some(MAX_SIGNALR_FRAME))
            .max_frame_size(Some(MAX_SIGNALR_FRAME));
        let socket = WebSocketStream::from_raw_socket(transport, Role::Client, Some(config)).await;
        let mut hub = SignalR {
            socket,
            frames: VecDeque::new(),
            tail: Vec::new(),
        };
        hub.send(json!({"protocol":"json","version":1})).await?;
        let ack = hub.next().await?;
        ensure!(
            ack.as_object().is_some_and(|ack| ack.is_empty()),
            "SignalR handshake was rejected"
        );
        let challenge = loop {
            let message = hub.next().await?;
            if message["type"] == 6 {
                continue;
            }
            let [challenge] = arguments(&message, inbound::CHALLENGE)? else {
                bail!("Invalid relay challenge")
            };
            let token = challenge.as_str().context("Invalid relay challenge")?;
            ensure!(
                (40..=43).contains(&token.len())
                    && token.bytes().all(|b| b.is_ascii_alphanumeric()),
                "Invalid relay challenge"
            );
            break self.key.sign(token);
        };
        hub.invoke(
            outbound::LOGIN,
            vec![json!(self.agent), json!(challenge)],
            Some(LOGIN),
        )
        .await?;
        let mut deferred = VecDeque::new();
        loop {
            let message = hub.next().await?;
            if message["type"] == 3 && message["invocationId"] == LOGIN {
                ensure!(
                    message.get("error").is_none(),
                    "Relay agent login was rejected"
                );
                break;
            }
            // An authenticated request may race with the login completion on the wire.
            if message["type"] == 1 {
                ensure!(
                    deferred.len() < MAX_JOBS,
                    "Too many relay requests during login"
                );
                deferred.push_back(message);
            }
        }
        deferred.append(&mut hub.frames);
        hub.frames = deferred;
        Ok(hub)
    }
}

struct SignalR {
    socket: WebSocketStream<reqwest::Upgraded>,
    frames: VecDeque<Value>,
    tail: Vec<u8>,
}

impl SignalR {
    async fn send(&mut self, message: Value) -> Result<()> {
        let mut text = message.to_string();
        text.push('\u{1e}');
        self.socket.send(Message::Text(text.into())).await?;
        Ok(())
    }

    async fn invoke(&mut self, target: &str, args: Vec<Value>, id: Option<&str>) -> Result<()> {
        let mut message = json!({"type":1,"target":target,"arguments":args});
        if let Some(id) = id {
            message["invocationId"] = json!(id);
        }
        self.send(message).await
    }

    async fn receive(&mut self) -> Result<()> {
        match self
            .socket
            .next()
            .await
            .context("Relay WebSocket closed")??
        {
            Message::Text(text) => {
                ensure!(
                    self.tail.len() + text.len() <= MAX_SIGNALR_FRAME,
                    "Oversized relay SignalR record"
                );
                self.tail.extend_from_slice(text.as_bytes());
                if let Some(last) = self.tail.iter().rposition(|&b| b == 0x1e) {
                    for record in self.tail[..last].split(|&b| b == 0x1e) {
                        self.frames.push_back(serde_json::from_slice(record)?);
                    }
                    self.tail.drain(..=last);
                }
            }
            Message::Ping(_) => self.socket.flush().await?,
            Message::Pong(_) => (),
            _ => bail!("Relay WebSocket closed or returned an unsupported message"),
        }
        Ok(())
    }

    async fn next(&mut self) -> Result<Value> {
        loop {
            if let Some(message) = self.frames.pop_front() {
                ensure!(
                    message["type"] != 7,
                    "Relay controller closed SignalR session"
                );
                return Ok(message);
            }
            self.receive().await?;
        }
    }
}

fn arguments<'a>(message: &'a Value, target: &str) -> Result<&'a [Value]> {
    ensure!(
        message["type"] == 1
            && message["target"]
                .as_str()
                .is_some_and(|name| name.eq_ignore_ascii_case(target)),
        "Unexpected relay invocation"
    );
    Ok(message["arguments"]
        .as_array()
        .context("Invalid relay arguments")?)
}

#[derive(Clone, Copy, PartialEq)]
enum ShareSync {
    Idle,
    WaitingForToken,
    Uploading,
}

enum Job {
    Info(Uuid, Option<u64>),
    File(Uuid, Result<()>),
    Download(Result<()>),
    Share(Result<()>),
}

async fn session(relay: &RelayService, settings: &Settings) -> Result<()> {
    let client = std::sync::Arc::new(AgentClient::new(settings)?);
    let mut hub = tokio::time::timeout(HANDSHAKE_TIMEOUT, client.connect())
        .await
        .context("Relay login timed out")??;
    relay.set_client_state(RelayClientState::Connected);
    relay.0.ready.send_replace(true);
    let mut sync = relay.0.sync_shares.subscribe();
    let mut sync_state = ShareSync::WaitingForToken;
    let mut sync_needed = false;
    hub.invoke(outbound::BEGIN_SHARE_UPLOAD, vec![], Some(SHARE))
        .await?;
    let mut jobs = JoinSet::new();
    let mut pulse = tokio::time::interval(KEEPALIVE);
    pulse.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let silence = tokio::time::sleep(SILENCE_TIMEOUT);
    tokio::pin!(silence);
    loop {
        if sync_needed && sync_state == ShareSync::Idle {
            sync_needed = false;
            sync_state = ShareSync::WaitingForToken;
            hub.invoke(outbound::BEGIN_SHARE_UPLOAD, vec![], Some(SHARE))
                .await?;
        }
        tokio::select! {
            _ = &mut silence => bail!("Relay controller stopped sending messages"),
            _ = pulse.tick() => hub.send(json!({"type":6})).await?,
            changed = sync.changed() => { changed?; sync_needed = true; },
            message = hub.next() => {
                let message = message?;
                silence.as_mut().reset(tokio::time::Instant::now() + SILENCE_TIMEOUT);
                if message["type"] == 3 && message["invocationId"] == SHARE && sync_state == ShareSync::WaitingForToken {
                    match token(&message["result"]) {
                        Ok(token) => {
                            sync_state = ShareSync::Uploading;
                            let (client, relay) = (client.clone(), relay.clone());
                            jobs.spawn(async move { Job::Share(upload_shares(&client, &relay, token).await) });
                        }
                        Err(error) => {
                            sync_state = ShareSync::Idle;
                            tracing::warn!(%error, "relay controller rejected share synchronization");
                        }
                    }
                    continue;
                }
                let Some(target) = message["target"].as_str().filter(|_| message["type"] == 1) else { continue; };
                let args = arguments(&message, target)?;
                match (target, args) {
                    (inbound::REQUEST_FILE_INFO, [filename, id]) => {
                        let (filename, id) = file_args(filename, id)?;
                        if jobs.len() >= MAX_JOBS {
                            hub.invoke(outbound::RETURN_FILE_INFO, vec![json!(id), json!(false), json!(0)], None).await?;
                            continue;
                        }
                        let (shares, options) = (relay.0.shares.clone(), relay.0.core.options.get());
                        jobs.spawn(async move {
                            let length = tokio::task::spawn_blocking(move || -> Result<u64> {
                                Ok(open_shared(&shares, &options.shares.directories, &filename)?.metadata()?.len())
                            }).await.ok().and_then(Result::ok);
                            Job::Info(id, length)
                        });
                    }
                    (inbound::REQUEST_FILE_UPLOAD, [filename, offset, id]) => {
                        let (filename, id) = file_args(filename, id)?;
                        let offset = offset.as_u64().filter(|&offset| offset <= i64::MAX as u64).context("Invalid relay file offset")?;
                        if jobs.len() >= MAX_JOBS {
                            hub.invoke(outbound::NOTIFY_FILE_UPLOAD_FAILED, vec![json!(id), json!("Agent is busy")], None).await?;
                            continue;
                        }
                        let (client, relay) = (client.clone(), relay.clone());
                        jobs.spawn(async move { Job::File(id, upload_file(&client, &relay, filename, offset, id).await) });
                    }
                    (inbound::NOTIFY_FILE_DOWNLOAD_COMPLETED, [filename, id]) if settings.controller.downloads => {
                        let (filename, id) = file_args(filename, id)?;
                        if jobs.len() >= MAX_JOBS { tracing::warn!(%id, "relay agent is busy; dropping download notification"); continue; }
                        let (client, relay) = (client.clone(), relay.clone());
                        jobs.spawn(async move { Job::Download(download(&client, &relay, &filename, id).await) });
                    }
                    _ => (),
                }
            }
            result = jobs.join_next(), if !jobs.is_empty() => match result.expect("active job")? {
                Job::Info(id, length) => hub.invoke(outbound::RETURN_FILE_INFO, vec![json!(id), json!(length.is_some()), json!(length.unwrap_or(0))], None).await?,
                Job::File(id, Err(error)) => {
                    tracing::warn!(%id, %error, "relay upload failed");
                    hub.invoke(outbound::NOTIFY_FILE_UPLOAD_FAILED, vec![json!(id), json!("Agent could not upload requested file")], None).await?;
                }
                Job::Download(Err(error)) => tracing::warn!(%error, "relay download failed"),
                Job::Share(result) => {
                    sync_state = ShareSync::Idle;
                    if let Err(error) = result { tracing::warn!(%error, "relay share synchronization failed"); }
                }
                _ => (),
            },
        }
    }
}

fn token(value: &Value) -> Result<Uuid> {
    Ok(Uuid::parse_str(
        value.as_str().context("Missing relay request ID")?,
    )?)
}

fn file_args(filename: &Value, id: &Value) -> Result<(String, Uuid)> {
    Ok((
        filename
            .as_str()
            .context("Invalid relay filename")?
            .to_owned(),
        token(id)?,
    ))
}

async fn upload_file(
    client: &AgentClient,
    relay: &RelayService,
    filename: String,
    offset: u64,
    token: Uuid,
) -> Result<()> {
    let (shares, options) = (relay.0.shares.clone(), relay.0.core.options.get());
    let (file, filename) = tokio::task::spawn_blocking(move || {
        Ok::<_, anyhow::Error>((
            open_shared(&shares, &options.shares.directories, &filename)?,
            filename,
        ))
    })
    .await??;
    let mut file = tokio::fs::File::from_std(file);
    let length = file
        .metadata()
        .await?
        .len()
        .checked_sub(offset)
        .context("Offset beyond end of relay file")?;
    file.seek(std::io::SeekFrom::Start(offset)).await?;
    // Keep protocol backslashes verbatim for slskd's Content-Disposition parser.
    // MIME encoding matches .NET's filename representation for non-ASCII.
    let encoded;
    let filename = if !filename.is_ascii() || filename.contains('"') || filename.starts_with("=?") {
        encoded = format!("=?utf-8?B?{}?=", STANDARD.encode(filename.as_bytes()));
        &encoded
    } else {
        &filename
    };
    let boundary = Uuid::new_v4();
    let prefix = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\n\r\n"
    );
    let suffix = format!("\r\n--{boundary}--\r\n");
    let total = prefix.len() as u64 + length + suffix.len() as u64;
    let stream = futures::stream::iter([Ok::<_, std::io::Error>(axum::body::Bytes::from(prefix))])
        .chain(ReaderStream::new(file.take(length)))
        .chain(futures::stream::iter([Ok(axum::body::Bytes::from(suffix))]));
    let response = client
        .request(Method::POST, "files", token)
        .header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary=\"{boundary}\""),
        )
        .header(header::CONTENT_LENGTH, total)
        .body(reqwest::Body::wrap_stream(stream))
        .send()
        .await?;
    ensure!(
        response.status().is_success(),
        "Relay file upload rejected: {}",
        response.status()
    );
    Ok(())
}

async fn upload_shares(client: &AgentClient, relay: &RelayService, token: Uuid) -> Result<()> {
    let shares = relay.0.shares.clone();
    let (dir, metadata) = tokio::task::spawn_blocking(move || {
        let dir = tempfile::tempdir()?;
        let metadata = shares.export_relay(&dir.path().join("shares.db"))?;
        Ok::<_, anyhow::Error>((dir, metadata))
    })
    .await??;
    let file = tokio::fs::File::open(dir.path().join("shares.db")).await?;
    let length = file.metadata().await?.len();
    let body = reqwest::Body::wrap_stream(ReaderStream::new(file));
    let multipart = Form::new()
        .part(
            "shares",
            Part::text(metadata.to_string()).mime_str("text/plain; charset=utf-8")?,
        )
        .part(
            "database",
            Part::stream_with_length(body, length).file_name("shares"),
        );
    let response = client
        .request(Method::POST, "shares", token)
        .multipart(multipart)
        .send()
        .await?;
    ensure!(
        response.status().is_success(),
        "Relay share upload rejected: {}",
        response.status()
    );
    Ok(())
}

struct TemporaryDownload<'a> {
    dir: &'a cap_std::fs::Dir,
    name: String,
}
impl Drop for TemporaryDownload<'_> {
    fn drop(&mut self) {
        let _ = self.dir.remove_file(&self.name);
    }
}

async fn download(
    client: &AgentClient,
    relay: &RelayService,
    filename: &str,
    token: Uuid,
) -> Result<()> {
    let mut destination = safe_relative(filename)?;
    if relay.0.mode == Some(RelayMode::Debug) {
        destination.as_mut_os_string().push(".relayed");
    }
    let response = client
        .request(Method::GET, "downloads", token)
        .header(headers::FILENAME_BASE64, STANDARD.encode(filename))
        .send()
        .await?;
    ensure!(
        response.status().is_success(),
        "Relay download rejected: {}",
        response.status()
    );
    let expected = response.content_length();
    let options = relay.0.core.options.get();
    let root = download_root(&options.directories.downloads)?;
    let dir = descend(
        &root,
        destination
            .parent()
            .context("Missing relay destination parent")?,
        true,
    )?;
    let name = destination
        .file_name()
        .context("Missing relay destination filename")?;
    let temp_name = format!(".slskr-relay-{}.part", Uuid::new_v4());
    let mut open = cap_std::fs::OpenOptions::new();
    open.create_new(true).write(true);
    let file = dir.open_with(&temp_name, &open)?;
    let temp = TemporaryDownload {
        dir: &dir,
        name: temp_name,
    };
    let mut file = tokio::fs::File::from_std(file.into_std());
    let mut chunks = response.bytes_stream();
    let mut received = 0u64;
    while let Some(chunk) = chunks.next().await {
        let chunk = chunk?;
        file.write_all(&chunk).await?;
        received += chunk.len() as u64;
    }
    ensure!(
        expected.is_none_or(|length| length == received),
        "Truncated relay download"
    );
    file.flush().await?;
    drop(file);
    dir.rename(&temp.name, &dir, name)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{relay::challenge, signalr::Hub};

    #[tokio::test]
    async fn agent_negotiates_and_logs_in_on_loopback() {
        let hub = Hub::new("relay");
        hub.set_authorizer(|request| {
            request
                .headers
                .get("x-api-key")
                .is_some_and(|key| key == "test-api-key")
        });
        let token = challenge();
        let answer = CredentialKey::new("test-agent", "test-secret").sign(&token);
        hub.on_connection(move |_, ip| {
            assert!(ip.is_loopback());
            Ok(vec![(inbound::CHALLENGE.to_owned(), vec![json!(token)])])
        });
        hub.on_connection_invoke(move |_, target, args| {
            let expected = vec![json!("test-agent"), json!(answer)];
            Box::pin(async move {
                if target == outbound::LOGIN && args == expected {
                    Ok(Value::Null)
                } else {
                    Err("wrong login credentials".into())
                }
            })
        });
        let router = axum::Router::new().nest("/hub/relay", hub.router());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .await
        });
        let client = AgentClient::new(&Settings {
            agent: "test-agent".into(),
            controller: RelayControllerOptions {
                address: Some(format!("http://{address}")),
                api_key: Some("test-api-key".into()),
                secret: Some("test-secret".into()),
                ..Default::default()
            },
        })
        .unwrap();
        assert!(
            tokio::time::timeout(HANDSHAKE_TIMEOUT, client.connect())
                .await
                .unwrap()
                .is_ok()
        );
        server.abort();
    }
}
