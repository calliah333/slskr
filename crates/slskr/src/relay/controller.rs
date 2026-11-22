use std::{
    collections::HashMap,
    io,
    net::IpAddr,
    path::{Component, Path, PathBuf},
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context as TaskContext, Poll},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use slskr_api::relay::{Agent, hub_client_methods as client, hub_server_methods as server};
use tokio::{
    io::{AsyncRead, DuplexStream, ReadBuf},
    sync::oneshot,
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::{CredentialKey, RelayService, challenge, download_root, open_regular, valid_name};
use crate::auth::parse_ranges;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Default)]
pub(super) struct Controller(Mutex<State>);

#[derive(Default)]
struct State {
    connections: HashMap<String, Connection>,
    requests: HashMap<Uuid, Request>,
}

struct Connection {
    ip: IpAddr,
    challenge: Option<(String, Instant)>,
    agent: Option<(String, CredentialKey)>,
    closed: CancellationToken,
}

struct Request {
    connection: String,
    expires: Instant,
    kind: Kind,
}

enum Kind {
    Share,
    Download(Arc<Download>),
    Info(oneshot::Sender<Option<u64>>),
    Upload {
        filename: String,
        length: u64,
        reply: oneshot::Sender<Result<RelayFile>>,
    },
}

pub(super) struct Download {
    pub root: PathBuf,
    pub relative: PathBuf,
    filename: String,
}

#[derive(Clone, Copy)]
pub(super) enum HttpKind<'a> {
    Share,
    Upload(&'a str),
    Download(&'a str),
}

pub(super) struct Grant {
    pub closed: CancellationToken,
    pub upload: Option<(u64, oneshot::Sender<Result<RelayFile>>)>,
    pub download: Option<Arc<Download>>,
}

/// The streamed payload contains exactly the remaining bytes requested by the caller.
pub struct RelayFile {
    reader: DuplexStream,
    remaining: u64,
    transfer: CancellationToken,
}

impl RelayFile {
    pub(super) fn new(reader: DuplexStream, remaining: u64, transfer: CancellationToken) -> Self {
        Self {
            reader,
            remaining,
            transfer,
        }
    }
}

impl Drop for RelayFile {
    fn drop(&mut self) {
        self.transfer.cancel();
    }
}

impl AsyncRead for RelayFile {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if self.remaining == 0 || buf.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        let before = buf.filled().len();
        if let Err(error) = std::task::ready!(Pin::new(&mut self.reader).poll_read(cx, buf)) {
            return Poll::Ready(Err(error));
        }
        let count = (buf.filled().len() - before) as u64;
        if count == 0 || count > self.remaining {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "incomplete relay upload",
            )));
        }
        self.remaining -= count;
        Poll::Ready(Ok(()))
    }
}

struct Pending<'a>(&'a Controller, Uuid);
impl Drop for Pending<'_> {
    fn drop(&mut self) {
        self.0.0.lock().unwrap().requests.remove(&self.1);
    }
}

impl State {
    fn insert(&mut self, connection: &str, kind: Kind, ttl: Duration) -> Uuid {
        let now = Instant::now();
        self.requests.retain(|_, entry| entry.expires > now);
        let id = Uuid::new_v4();
        self.requests.insert(
            id,
            Request {
                connection: connection.to_owned(),
                expires: now + ttl,
                kind,
            },
        );
        id
    }

    fn remove_connection(&mut self, id: &str) -> Option<String> {
        let connection = self.connections.remove(id)?;
        connection.closed.cancel();
        self.requests.retain(|_, entry| entry.connection != id);
        connection.agent.map(|(name, _)| name)
    }
}

impl RelayService {
    pub(super) fn connected(&self, id: &str, ip: IpAddr) -> Result<Vec<(String, Vec<Value>)>> {
        ensure!(self.serves_agents(), "Relay controller is disabled");
        let token = challenge();
        self.0.controller.0.lock().unwrap().connections.insert(
            id.to_owned(),
            Connection {
                ip: ip.to_canonical(),
                challenge: Some((token.clone(), Instant::now() + Duration::from_secs(60))),
                agent: None,
                closed: self.0.core.shutdown.child_token(),
            },
        );
        Ok(vec![(client::CHALLENGE.into(), vec![json!(token)])])
    }

    pub(super) fn disconnected(&self, id: &str) {
        let mut state = self.0.controller.0.lock().unwrap();
        if let Some(agent) = state.remove_connection(id) {
            // Keep login and deactivation serialized: a superseded socket no longer owns this host.
            self.0.shares.deactivate_relay(&agent);
            self.publish_agents(&state);
        }
    }

    fn publish_agents(&self, state: &State) {
        let mut agents: Vec<_> = state
            .connections
            .values()
            .filter_map(|connection| {
                let (name, _) = connection.agent.as_ref()?;
                Some(Agent {
                    name: Some(name.clone()),
                    ip_address: Some(connection.ip.to_string()),
                })
            })
            .collect();
        agents.sort_unstable_by(|a, b| a.name.cmp(&b.name));
        self.0
            .core
            .state
            .update(|state| state.relay.agents = agents);
    }

    pub(super) async fn invoke(&self, id: &str, target: &str, args: Vec<Value>) -> Result<Value> {
        let mut state = self.0.controller.0.lock().unwrap();
        let connection = state
            .connections
            .get_mut(id)
            .context("Unauthorized relay connection")?;
        if target.eq_ignore_ascii_case(server::LOGIN) {
            let (token, expiry) = connection
                .challenge
                .take()
                .context("Unauthorized relay login")?;
            let ip = connection.ip;
            let [name, answer] = args.as_slice() else {
                bail!("Invalid relay login arguments")
            };
            let name = name.as_str().context("Invalid relay agent")?;
            let answer = answer.as_str().context("Invalid relay credential")?;
            let options = self.0.core.options.get();
            let mut matches = options
                .relay
                .agents
                .values()
                .filter(|agent| agent.instance_name.as_deref() == Some(name));
            let agent = matches.next().context("Unauthorized relay login")?;
            ensure!(
                matches.next().is_none()
                    && expiry > Instant::now()
                    && parse_ranges(&agent.cidr)
                        .is_ok_and(|ranges| ranges.iter().any(|range| range.contains(ip))),
                "Unauthorized relay login"
            );
            let key = CredentialKey::new(
                name,
                agent
                    .secret
                    .as_deref()
                    .filter(|s| !s.is_empty())
                    .context("Unauthorized relay login")?,
            );
            ensure!(
                credential_matches(&key.sign(&token), answer),
                "Unauthorized relay login"
            );
            if let Some(previous) = state.connections.iter().find_map(|(id, connection)| {
                connection
                    .agent
                    .as_ref()
                    .filter(|(agent, _)| agent == name)
                    .map(|_| id.clone())
            }) {
                let _ = state.remove_connection(&previous);
            }
            state
                .connections
                .get_mut(id)
                .expect("active login connection")
                .agent = Some((name.to_owned(), key));
            self.publish_agents(&state);
            return Ok(Value::Null);
        }
        ensure!(connection.agent.is_some(), "Unauthorized relay connection");
        if target.eq_ignore_ascii_case(server::BEGIN_SHARE_UPLOAD) {
            ensure!(args.is_empty(), "Invalid share-upload arguments");
            state
                .requests
                .retain(|_, entry| entry.connection != id || !matches!(entry.kind, Kind::Share));
            return Ok(json!(state.insert(
                id,
                Kind::Share,
                Duration::from_secs(300)
            )));
        }
        let token = args
            .first()
            .and_then(Value::as_str)
            .and_then(|value| Uuid::parse_str(value).ok())
            .context("Invalid relay request ID")?;
        let Some(entry) = state
            .requests
            .get(&token)
            .filter(|entry| entry.connection == id && entry.expires > Instant::now())
        else {
            bail!("Unknown relay request");
        };
        if target.eq_ignore_ascii_case(server::RETURN_FILE_INFO)
            && matches!(entry.kind, Kind::Info(_))
        {
            let [_, exists, length] = args.as_slice() else {
                bail!("Invalid file information arguments")
            };
            let exists = exists.as_bool().context("Invalid file existence flag")?;
            let length = length
                .as_u64()
                .filter(|&n| n <= i64::MAX as u64)
                .context("Invalid file length")?;
            if let Kind::Info(reply) = state.requests.remove(&token).unwrap().kind {
                let _ = reply.send(exists.then_some(length));
            }
        } else if target.eq_ignore_ascii_case(server::NOTIFY_FILE_UPLOAD_FAILED)
            && matches!(entry.kind, Kind::Upload { .. })
        {
            let [_, message] = args.as_slice() else {
                bail!("Invalid upload failure arguments")
            };
            let message = message.as_str().context("Invalid upload failure message")?;
            if let Kind::Upload { reply, .. } = state.requests.remove(&token).unwrap().kind {
                let _ = reply.send(Err(anyhow::anyhow!("Relay upload failed: {message}")));
            }
        } else {
            bail!("Unknown relay method or mismatched request");
        }
        Ok(Value::Null)
    }

    pub(super) fn claim(
        &self,
        token: Uuid,
        agent: &str,
        ip: IpAddr,
        credential: &str,
        kind: HttpKind<'_>,
    ) -> Option<Grant> {
        let mut state = self.0.controller.0.lock().unwrap();
        let entry = state.requests.get(&token)?;
        let connection = state.connections.get(&entry.connection)?;
        let (name, key) = connection.agent.as_ref()?;
        if entry.expires <= Instant::now()
            || connection.ip != ip.to_canonical()
            || name != agent
            || connection.closed.is_cancelled()
        {
            return None;
        }
        match (&entry.kind, kind) {
            (Kind::Share, HttpKind::Share) => (),
            (Kind::Upload { filename, .. }, HttpKind::Upload(given)) if filename == given => (),
            (Kind::Download(download), HttpKind::Download(given)) if download.filename == given => {
                ()
            }
            _ => return None,
        }
        if !credential_matches(&key.sign(&token.to_string()), credential) {
            return None;
        }
        let closed = connection.closed.clone();
        let (upload, download) = match state.requests.remove(&token)?.kind {
            Kind::Upload { length, reply, .. } => (Some((length, reply)), None),
            Kind::Download(download) => (None, Some(download)),
            _ => (None, None),
        };
        Some(Grant {
            closed,
            upload,
            download,
        })
    }

    fn request(
        &self,
        agent: &str,
        kind: Kind,
        target: &str,
        mut args: Vec<Value>,
    ) -> Result<Pending<'_>> {
        let controller = &self.0.controller;
        let mut state = controller.0.lock().unwrap();
        let id = state
            .connections
            .iter()
            .find_map(|(id, connection)| {
                connection
                    .agent
                    .as_ref()
                    .filter(|(name, _)| name == agent)
                    .map(|_| id.clone())
            })
            .context("Relay agent is not connected")?;
        let token = state.insert(&id, kind, REQUEST_TIMEOUT);
        args.push(json!(token));
        if !self.0.core.hubs.relay.send_to(&id, target, args) {
            state.requests.remove(&token);
            bail!("Relay agent is unavailable");
        }
        Ok(Pending(controller, token))
    }

    /// Returns the active owner and indexed length of an agent's published file.
    pub fn published_file(&self, filename: &str) -> Option<(String, u64)> {
        if !self.serves_agents() || !valid_name(filename) {
            return None;
        }
        let (agent, size) = self.0.shares.resolve_relay(filename)?;
        self.0
            .controller
            .0
            .lock()
            .unwrap()
            .connections
            .values()
            .any(|connection| {
                connection
                    .agent
                    .as_ref()
                    .is_some_and(|(name, _)| *name == agent)
            })
            .then_some((agent, size))
    }

    pub async fn file_info(&self, agent: &str, filename: &str) -> Result<Option<u64>> {
        ensure!(valid_name(filename), "Invalid relay filename");
        let (reply, result) = oneshot::channel();
        let _pending = self.request(
            agent,
            Kind::Info(reply),
            client::REQUEST_FILE_INFO,
            vec![json!(filename)],
        )?;
        Ok(tokio::time::timeout(REQUEST_TIMEOUT, result)
            .await
            .context("Relay file information timed out")??)
    }

    pub async fn file_stream(
        &self,
        agent: &str,
        filename: &str,
        offset: u64,
        length: u64,
    ) -> Result<RelayFile> {
        ensure!(
            valid_name(filename)
                && offset
                    .checked_add(length)
                    .is_some_and(|n| n <= i64::MAX as u64),
            "Invalid relay upload request"
        );
        let (reply, result) = oneshot::channel();
        let _pending = self.request(
            agent,
            Kind::Upload {
                filename: filename.to_owned(),
                length,
                reply,
            },
            client::REQUEST_FILE_UPLOAD,
            vec![json!(filename), json!(offset)],
        )?;
        tokio::time::timeout(REQUEST_TIMEOUT, result)
            .await
            .context("Relay file upload timed out")?
            .context("Relay agent disconnected before sending the file")?
    }

    pub(super) fn notify_download(&self, path: &Path) -> Result<()> {
        if !self
            .0
            .controller
            .0
            .lock()
            .unwrap()
            .connections
            .values()
            .any(|entry| entry.agent.is_some())
        {
            return Ok(());
        }
        let options = self.0.core.options.get();
        ensure!(
            !options.directories.downloads.is_empty(),
            "downloads directory is not configured"
        );
        let relative = std::path::absolute(path)?
            .strip_prefix(std::path::absolute(&options.directories.downloads)?)?
            .to_owned();
        let mut filename = String::new();
        for component in relative.components() {
            let Component::Normal(component) = component else {
                bail!("Invalid download path")
            };
            let component = component.to_str().context("Invalid download name")?;
            ensure!(
                valid_name(component) && !component.contains(['/', '\\']),
                "Invalid download name"
            );
            if !filename.is_empty() {
                filename.push('/');
            }
            filename.push_str(component);
        }
        ensure!(!filename.is_empty(), "Empty download name");
        let root = std::path::absolute(&options.directories.downloads)?;
        open_regular(&download_root(&root)?, &relative)?;
        let download = Arc::new(Download {
            root,
            relative,
            filename,
        });
        let mut state = self.0.controller.0.lock().unwrap();
        let ids: Vec<_> = state
            .connections
            .iter()
            .filter(|(_, connection)| connection.agent.is_some())
            .map(|(id, _)| id.clone())
            .collect();
        for id in ids {
            let token = state.insert(
                &id,
                Kind::Download(download.clone()),
                Duration::from_secs(600),
            );
            if !self.0.core.hubs.relay.send_to(
                &id,
                client::NOTIFY_FILE_DOWNLOAD_COMPLETED,
                vec![json!(download.filename), json!(token)],
            ) {
                state.requests.remove(&token);
            }
        }
        Ok(())
    }
}

fn credential_matches(expected: &str, supplied: &str) -> bool {
    crate::auth::constant_time_eq(expected.as_bytes(), supplied.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt, duplex};

    #[tokio::test]
    async fn short_upload_is_an_error_and_drop_cancels_sender() {
        let (reader, mut writer) = duplex(16);
        let transfer = CancellationToken::new();
        let mut file = RelayFile::new(reader, 4, transfer.clone());
        writer.write_all(b"abc").await.unwrap();
        drop(writer);
        let mut output = Vec::new();
        assert_eq!(
            file.read_to_end(&mut output).await.unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
        assert_eq!(&output, b"abc");
        drop(file);
        assert!(transfer.is_cancelled());
    }
}
