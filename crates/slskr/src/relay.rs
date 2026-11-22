//! Relay controller and agent. Peer filenames are protocol identifiers, not local paths.

mod api;
mod client;
mod controller;

use std::{
    path::{Path, PathBuf},
    sync::{Arc, Weak},
};

use aes::cipher::{BlockEncryptMut, KeyIvInit, block_padding::Pkcs7};
use anyhow::{Context, Result, bail, ensure};
use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
use cap_std::{ambient_authority, fs::Dir};
use slskr_api::{
    events::Event,
    relay::{RelayClientState, RelayMode},
};
use tokio::sync::{broadcast, watch};

use crate::{
    app::{AppState, Core},
    auth::Role,
    shares::ShareService,
    transfers::TransferService,
};

pub use api::routes;
pub use controller::RelayFile;

#[derive(Clone)]
pub struct RelayService(Arc<Inner>);

struct Inner {
    core: Core,
    shares: ShareService,
    mode: Option<RelayMode>,
    controller: controller::Controller,
    desired: watch::Sender<bool>,
    sync_shares: watch::Sender<()>,
    ready: watch::Sender<bool>,
}

impl RelayService {
    pub fn new(core: &Core, shares: &ShareService, _transfers: &TransferService) -> Result<Self> {
        let options = core.options.get();
        let mode = if options.relay.enabled {
            Some(RelayMode::parse(&options.relay.mode).context("invalid relay mode")?)
        } else {
            None
        };
        Ok(Self(Arc::new(Inner {
            core: core.clone(),
            shares: shares.clone(),
            mode,
            controller: controller::Controller::default(),
            desired: watch::channel(matches!(mode, Some(RelayMode::Agent | RelayMode::Debug))).0,
            sync_shares: watch::channel(()).0,
            ready: watch::channel(false).0,
        })))
    }

    /// Agent mode does not connect to Soulseek; debug mode also runs the controller.
    pub fn is_agent(&self) -> bool {
        self.0.mode == Some(RelayMode::Agent)
    }

    fn serves_agents(&self) -> bool {
        matches!(self.0.mode, Some(RelayMode::Controller | RelayMode::Debug))
    }

    fn connects_to_controller(&self) -> bool {
        matches!(self.0.mode, Some(RelayMode::Agent | RelayMode::Debug))
    }

    pub fn start(&self, app: AppState) {
        let hub = &self.0.core.hubs.relay;
        hub.set_authorizer(self.0.core.auth.hub_authorizer(Role::ReadOnly));

        // The service owns the hub; hub hooks must not own the service.
        let weak = Arc::downgrade(&self.0);
        let inner = weak.clone();
        hub.on_connection(move |id, ip| {
            upgrade(&inner)?
                .connected(id, ip)
                .map_err(|e| e.to_string())
        });
        let inner = weak.clone();
        hub.on_connection_invoke(move |id, target, args| {
            let inner = inner.clone();
            Box::pin(async move {
                upgrade(&inner)?
                    .invoke(&id, &target, args)
                    .await
                    .map_err(|e| e.to_string())
            })
        });
        hub.on_disconnection(move |id| {
            if let Ok(relay) = upgrade(&weak) {
                relay.disconnected(id);
            }
        });

        self.set_client_state(RelayClientState::Disconnected);
        if self.connects_to_controller() {
            tokio::spawn(client::supervise(self.clone()));
        }
        if self.serves_agents() {
            let relay = self.clone();
            let mut events = app.services.events.subscribe();
            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        _ = relay.0.core.shutdown.cancelled() => break,
                        event = events.recv() => match event {
                            // The event test endpoint uses an empty transfer object.
                            Ok(Event::DownloadFileComplete(event)) if event.transfer.get("id").and_then(serde_json::Value::as_str).is_some() => {
                                if let Err(error) = relay.notify_download(Path::new(&event.local_filename)) {
                                    tracing::warn!(%error, "unable to relay completed download");
                                }
                            }
                            Ok(_) => {},
                            Err(broadcast::error::RecvError::Lagged(count)) => {
                                tracing::warn!(count, "relay missed application events");
                            }
                            Err(broadcast::error::RecvError::Closed) => break,
                        },
                    }
                }
            });
        }
    }

    /// Republish the local share index after a completed scan.
    pub fn synchronize_shares(&self) {
        self.0.sync_shares.send_replace(());
    }

    fn set_client_state(&self, state: RelayClientState) {
        let options = self.0.core.options.get();
        let mode = self.0.mode.unwrap_or_default();
        let address = self
            .connects_to_controller()
            .then(|| options.relay.controller.address.clone())
            .flatten();
        self.0.core.state.update(|current| {
            current.relay.mode = mode;
            current.relay.controller.address = address;
            current.relay.controller.state = state;
        });
    }
}

fn upgrade(inner: &Weak<Inner>) -> Result<RelayService, String> {
    inner
        .upgrade()
        .map(RelayService)
        .ok_or_else(|| "Relay is shutting down".into())
}

#[derive(Clone)]
struct CredentialKey([u8; 48]);

impl CredentialKey {
    fn new(agent: &str, secret: &str) -> Self {
        let mut key = [0; 48];
        pbkdf2::pbkdf2_hmac::<sha2::Sha256>(secret.as_bytes(), agent.as_bytes(), 1_000, &mut key);
        Self(key)
    }

    // slskd uses PBKDF2-SHA256, AES-256-CBC and its case-sensitive Base62 alphabet.
    fn sign(&self, token: &str) -> String {
        let (key, iv) = self.0.split_at(32);
        let mut block = [0; 64];
        block[..token.len()].copy_from_slice(token.as_bytes());
        let bytes = cbc::Encryptor::<aes::Aes256>::new_from_slices(key, iv)
            .expect("fixed AES key and IV lengths")
            .encrypt_padded_mut::<Pkcs7>(&mut block, token.len())
            .expect("bounded relay token length");
        base62(bytes)
    }
}

fn base62(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 62] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
    let zeros = bytes.iter().take_while(|&&byte| byte == 0).count();
    let mut digits = [0u8; 89]; // enough for 64 base-256 digits
    let mut len = 0;
    for &byte in &bytes[zeros..] {
        let mut carry = u16::from(byte);
        for digit in &mut digits[..len] {
            carry += u16::from(*digit) * 256;
            *digit = (carry % 62) as u8;
            carry /= 62;
        }
        while carry > 0 {
            digits[len] = (carry % 62) as u8;
            len += 1;
            carry /= 62;
        }
    }
    let mut output = String::with_capacity(zeros + len);
    output.extend(std::iter::repeat_n('0', zeros));
    output.extend(
        digits[..len]
            .iter()
            .rev()
            .map(|&digit| char::from(ALPHABET[digit as usize])),
    );
    output
}

fn challenge() -> String {
    base62(&rand::random::<[u8; 32]>())
}

fn valid_name(filename: &str) -> bool {
    !filename.is_empty()
        && filename.split(['/', '\\']).all(|part| {
            !matches!(part, "" | "." | "..")
                && !part.contains(':')
                && !part.chars().any(char::is_control)
        })
}

fn safe_relative(filename: &str) -> Result<PathBuf> {
    ensure!(valid_name(filename), "invalid relay filename");
    Ok(filename.split(['/', '\\']).collect())
}

fn download_root(config: impl AsRef<Path>) -> Result<Dir> {
    let config = config.as_ref();
    ensure!(
        !config.as_os_str().is_empty(),
        "downloads directory is not configured"
    );
    Dir::open_ambient_dir(config, ambient_authority()).context("open downloads directory")
}

fn open_regular(root: &Dir, relative: &Path) -> Result<std::fs::File> {
    let parent = relative.parent().context("missing relay file parent")?;
    let directory = descend(root, parent, false)?;
    let name = relative.file_name().context("missing relay file name")?;
    ensure!(
        directory.symlink_metadata(name)?.is_file(),
        "relay path is not a regular file"
    );
    let mut options = cap_std::fs::OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    let file = directory.open_with(name, &options)?;
    ensure!(
        file.metadata()?.is_file(),
        "relay path is not a regular file"
    );
    Ok(file.into_std())
}

fn descend(root: &Dir, path: &Path, create: bool) -> Result<Dir> {
    let mut directory = root.try_clone()?;
    for component in path.components() {
        let std::path::Component::Normal(name) = component else {
            bail!("invalid relay directory");
        };
        if create {
            match directory.create_dir(name) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error.into()),
            }
        }
        directory = directory.open_dir_nofollow(name)?;
    }
    Ok(directory)
}

fn open_shared(shares: &ShareService, roots: &[String], filename: &str) -> Result<std::fs::File> {
    ensure!(valid_name(filename), "invalid relay filename");
    let (path, _) = shares.resolve_file(filename)?;
    let alias = filename
        .split(['/', '\\'])
        .next()
        .expect("validated filename");
    for spec in roots {
        let spec = spec.trim_end_matches(['/', '\\']);
        if spec.starts_with(['-', '!']) {
            continue;
        }
        let (configured_alias, local) = spec
            .strip_prefix('[')
            .and_then(|s| s.split_once(']'))
            .unwrap_or_else(|| (spec.rsplit(['/', '\\']).next().unwrap_or_default(), spec));
        if configured_alias != alias || !Path::new(local).is_absolute() {
            continue;
        }
        let root = Path::new(local).canonicalize()?;
        if let Ok(relative) = path.strip_prefix(&root) {
            let directory = Dir::open_ambient_dir(&root, ambient_authority())?;
            return open_regular(&directory, relative);
        }
    }
    bail!("file is outside configured relay shares")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interoperable_credential() {
        let key = CredentialKey::new("agent", "secret");
        assert_eq!(
            key.sign("0123456789abcdef0123456789abcdef0123456789a"),
            "1z0qpxb5mIPqkIPiEkETPIDurJw1DUG6l4H6GFBANI22WtZirT6DNjEzSfsnXaOtF"
        );
    }

    #[test]
    fn paths_are_relative_protocol_identifiers() {
        for bad in [
            "/root", "\\root", "a/../b", "a\\.\\b", "a//b", "C:/drive", "a\nb",
        ] {
            assert!(safe_relative(bad).is_err(), "{bad:?}");
        }
        assert_eq!(
            safe_relative("share\\album/song").unwrap(),
            Path::new("share").join("album").join("song")
        );
    }

    #[cfg(unix)]
    #[test]
    fn relay_rejects_symlinks_inside_capability_root() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir(temp.path().join("original")).unwrap();
        std::fs::write(temp.path().join("original/song"), b"music").unwrap();
        std::os::unix::fs::symlink("original", temp.path().join("alias")).unwrap();
        std::os::unix::fs::symlink("original/song", temp.path().join("shortcut")).unwrap();
        let root = download_root(temp.path()).unwrap();
        assert!(open_regular(&root, Path::new("original/song")).is_ok());
        assert!(open_regular(&root, Path::new("alias/song")).is_err());
        assert!(open_regular(&root, Path::new("shortcut")).is_err());
        assert!(descend(&root, Path::new("alias"), true).is_err());
    }
}
