//! Peer information, group policy, and `/users` endpoints.

use std::{
    collections::{HashMap, HashSet},
    net::IpAddr,
    sync::{Arc, RwLock},
    time::Duration,
};

use axum::{
    Json, Router,
    extract::{Path, State, rejection::JsonRejection},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use base64::Engine;
use slskr_api::users::{
    BrowseResponse, BrowseStatus, Directory, DirectoryContentsRequest, Endpoint, Info, Statistics,
    Status, UserPresence,
};
use tokio::{sync::broadcast, time::Instant};

use crate::{
    app::{AppState, Core},
    auth::{Authenticated, RequireReadWrite},
    http::{ApiError, ApiResult},
};

const DEFAULT: &str = "default";
const PRIVILEGED: &str = "privileged";
const LEECHERS: &str = "leechers";
const BLACKLISTED: &str = "blacklisted";
const BROWSE_RETENTION: Duration = Duration::from_secs(5);
const INFO_RETENTION: Duration = Duration::from_secs(60);

#[derive(Clone, Copy, Debug, Default)]
pub struct GroupUploadInfo {
    pub slots: u32,
    pub strategy: &'static str,
    pub speed_limit: u32,
    pub priority: u32,
}

#[derive(Default)]
struct Cache {
    session: u64,
    next_browse: u64,
    statistics: HashMap<String, Statistics>,
    status: HashMap<String, Status>,
    privileged: HashSet<String>,
    info: HashMap<String, (Instant, Arc<Info>)>,
    browses: HashMap<String, Browse>,
}

impl Cache {
    fn reset_session(&mut self) {
        self.session = self.session.wrapping_add(1);
        self.statistics.clear();
        self.status.clear();
        self.privileged.clear();
        self.info.clear();
        self.browses.clear();
    }

    fn expire(&mut self, now: Instant) {
        self.info.retain(|_, (expires, _)| now < *expires);
        self.browses.retain(|_, browse| {
            browse
                .result
                .as_ref()
                .is_none_or(|(expires, _)| now < *expires)
        });
    }
}

struct Browse {
    id: u64,
    progress: BrowseStatus,
    result: Option<(Instant, Arc<BrowseResponse>)>,
}

struct ActiveBrowse<'a> {
    cache: &'a RwLock<Cache>,
    username: &'a str,
    id: u64,
    completed: bool,
}

impl Drop for ActiveBrowse<'_> {
    fn drop(&mut self) {
        if self.completed {
            return;
        }
        let mut cache = self.cache.write().unwrap();
        if cache
            .browses
            .get(self.username)
            .is_some_and(|b| b.id == self.id && b.result.is_none())
        {
            cache.browses.remove(self.username);
        }
    }
}

#[derive(Clone)]
pub struct UserService {
    core: Core,
    cache: Arc<RwLock<Cache>>,
}

impl UserService {
    pub fn new(core: &Core) -> anyhow::Result<Self> {
        Ok(Self {
            core: core.clone(),
            cache: Arc::new(RwLock::new(Cache::default())),
        })
    }

    pub fn start(&self, _app: AppState) {
        let mut events = self.core.client.subscribe();
        let service = self.clone();
        tokio::spawn(async move {
            let mut expiry =
                tokio::time::interval_at(Instant::now() + BROWSE_RETENTION, BROWSE_RETENTION);
            expiry.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    _ = service.core.shutdown.cancelled() => break,
                    now = expiry.tick() => service.cache.write().unwrap().expire(now),
                    event = events.recv() => match event {
                        Ok(event) => service.client_event(event),
                        Err(broadcast::error::RecvError::Lagged(skipped)) => {
                            tracing::debug!(skipped, "user service missed Soulseek events");
                        }
                        Err(broadcast::error::RecvError::Closed) => break,
                    }
                }
            }
            service.cache.write().unwrap().reset_session();
        });
    }

    fn client_event(&self, event: soulseek::Event) {
        use soulseek::Event;
        match event {
            Event::Connected | Event::Disconnected { .. } => {
                self.cache.write().unwrap().reset_session()
            }
            Event::UserStatusChanged(status) => {
                let value = convert_status(&status);
                self.cache
                    .write()
                    .unwrap()
                    .status
                    .insert(status.username, value);
            }
            Event::UserStatisticsChanged(statistics) => {
                let value = convert_statistics(&statistics);
                self.cache
                    .write()
                    .unwrap()
                    .statistics
                    .insert(statistics.username, value);
            }
            Event::PrivilegedUserListReceived(usernames) => {
                self.cache.write().unwrap().privileged = usernames.into_iter().collect();
            }
            Event::BrowseProgressUpdated {
                username,
                bytes_transferred,
                size,
            } => {
                if let Some(browse) = self.cache.write().unwrap().browses.get_mut(&username) {
                    browse.progress.bytes_transferred = bytes_transferred;
                    browse.progress.bytes_remaining = size.saturating_sub(bytes_transferred);
                    browse.progress.size = size;
                    browse.progress.percent_complete = if size == 0 {
                        100.0
                    } else {
                        100.0 * bytes_transferred.min(size) as f64 / size as f64
                    };
                }
            }
            _ => {}
        }
    }

    pub fn group(&self, username: &str) -> String {
        let options = self.core.options.get();
        if self.is_blacklisted(username, None) {
            return BLACKLISTED.into();
        }
        let (privileged, stats) = {
            let cache = self.cache.read().unwrap();
            (
                cache.privileged.contains(username)
                    || cache.status.get(username).is_some_and(|s| s.is_privileged),
                cache.statistics.get(username).copied(),
            )
        };
        if privileged {
            return PRIVILEGED.into();
        }
        if let Some((name, _)) = options
            .groups
            .custom
            .iter()
            .find(|(_, g)| g.members.iter().any(|u| u.eq_ignore_ascii_case(username)))
        {
            return name.clone();
        }
        let leechers = &options.groups.leechers;
        let leecher = stats.is_some_and(|s| {
            s.file_count < leechers.min_shared_files.unwrap_or(1)
                || s.directory_count < leechers.min_shared_directories.unwrap_or(1)
        });
        if leecher {
            LEECHERS.into()
        } else {
            DEFAULT.into()
        }
    }
    pub fn is_blacklisted(&self, username: &str, ip: Option<IpAddr>) -> bool {
        let b = &self.core.options.get().blacklist;
        b.users.iter().any(|v| v.eq_ignore_ascii_case(username))
            || b.patterns.iter().any(|p| wildcard(p, username))
            || ip.is_some_and(|ip| {
                b.cidrs
                    .iter()
                    .filter_map(|c| c.parse::<crate::auth::ip::IpRange>().ok())
                    .any(|r| r.contains(ip))
            })
    }
    pub fn group_upload_options(&self, group: &str) -> GroupUploadInfo {
        let options = self.core.options.get();
        let g = crate::transfers::group_config(&options, group);
        GroupUploadInfo {
            slots: g.slots,
            strategy: g.strategy.as_str(),
            speed_limit: g.speed_limit,
            priority: g.priority,
        }
    }
    async fn status(&self, username: &str) -> soulseek::Result<Status> {
        let session = self.cache.read().unwrap().session;
        let remote = self.core.client.get_user_status(username).await?;
        let value = convert_status(&remote);
        let mut cache = self.cache.write().unwrap();
        if cache.session == session {
            cache.status.insert(remote.username, value);
        }
        Ok(value)
    }

    async fn statistics(&self, username: &str) -> soulseek::Result<Statistics> {
        let session = self.cache.read().unwrap().session;
        let remote = self.core.client.get_user_statistics(username).await?;
        let value = convert_statistics(&remote);
        let mut cache = self.cache.write().unwrap();
        if cache.session == session {
            cache.statistics.insert(remote.username, value);
        }
        Ok(value)
    }

    async fn info(&self, username: &str) -> soulseek::Result<Arc<Info>> {
        let session = {
            let cache = self.cache.read().unwrap();
            if let Some((expires, info)) = cache.info.get(username)
                && Instant::now() < *expires
            {
                return Ok(info.clone());
            }
            cache.session
        };
        let remote = self
            .core
            .client
            .get_user_info(username, self.core.shutdown.child_token())
            .await?;
        let has_picture = remote.has_picture();
        let value = Arc::new(Info {
            description: Some(remote.description),
            has_free_upload_slot: remote.has_free_upload_slot,
            has_picture,
            picture: remote
                .picture
                .map(|picture| base64::engine::general_purpose::STANDARD.encode(picture)),
            queue_length: remote.queue_length,
            upload_slots: remote.upload_slots,
        });
        let mut cache = self.cache.write().unwrap();
        if cache.session == session {
            cache.info.insert(
                username.into(),
                (Instant::now() + INFO_RETENTION, value.clone()),
            );
        }
        Ok(value)
    }

    async fn browse_peer(&self, username: &str) -> soulseek::Result<Arc<BrowseResponse>> {
        let id = {
            let mut cache = self.cache.write().unwrap();
            if let Some(browse) = cache.browses.get(username) {
                match &browse.result {
                    Some((expires, response)) if Instant::now() < *expires => {
                        return Ok(response.clone());
                    }
                    None => {
                        return Err(soulseek::Error::DuplicateToken(format!(
                            "A browse of {username} is already in progress"
                        )));
                    }
                    _ => {}
                }
            }
            let id = cache.next_browse;
            cache.next_browse = id.wrapping_add(1);
            cache.browses.insert(
                username.into(),
                Browse {
                    id,
                    progress: BrowseStatus {
                        username: username.into(),
                        ..BrowseStatus::default()
                    },
                    result: None,
                },
            );
            id
        };
        let mut active = ActiveBrowse {
            cache: &self.cache,
            username,
            id,
            completed: false,
        };
        let remote = self
            .core
            .client
            .browse(
                username,
                Default::default(),
                self.core.shutdown.child_token(),
            )
            .await?;
        let response = Arc::new(BrowseResponse::new(
            remote
                .directories
                .into_iter()
                .map(convert_directory)
                .collect(),
            remote
                .locked_directories
                .into_iter()
                .map(convert_directory)
                .collect(),
        ));
        let mut cache = self.cache.write().unwrap();
        if let Some(browse) = cache
            .browses
            .get_mut(username)
            .filter(|browse| browse.id == id)
        {
            // The final transport event may still be queued; its byte counts are
            // filled in by the event observer even after the result is available.
            browse.progress.percent_complete = 100.0;
            browse.result = Some((Instant::now() + BROWSE_RETENTION, response.clone()));
        }
        active.completed = true;
        Ok(response)
    }

    fn browse_status(&self, username: &str) -> Option<BrowseStatus> {
        self.cache
            .read()
            .unwrap()
            .browses
            .get(username)
            .filter(|browse| {
                browse
                    .result
                    .as_ref()
                    .is_none_or(|(expires, _)| Instant::now() < *expires)
            })
            .map(|browse| browse.progress.clone())
    }
}

fn convert_status(remote: &soulseek::UserStatus) -> Status {
    Status {
        is_privileged: remote.is_privileged,
        presence: match remote.presence {
            soulseek::UserPresence::Offline => UserPresence::Offline,
            soulseek::UserPresence::Away => UserPresence::Away,
            soulseek::UserPresence::Online => UserPresence::Online,
        },
    }
}

fn convert_statistics(remote: &soulseek::UserStatistics) -> Statistics {
    Statistics {
        average_speed: remote.average_speed,
        upload_count: remote.upload_count,
        file_count: remote.file_count,
        directory_count: remote.directory_count,
    }
}

fn wildcard(pattern: &str, value: &str) -> bool {
    let (p, v) = (pattern.to_lowercase(), value.to_lowercase());
    let (mut i, mut j, mut star, mut mark) = (0, 0, None, 0);
    let (p, v) = (p.as_bytes(), v.as_bytes());
    while j < v.len() {
        if i < p.len() && (p[i] == b'?' || p[i] == v[j]) {
            i += 1;
            j += 1;
        } else if i < p.len() && p[i] == b'*' {
            star = Some(i);
            i += 1;
            mark = j;
        } else if let Some(s) = star {
            i = s + 1;
            mark += 1;
            j = mark;
        } else {
            return false;
        }
    }
    while i < p.len() && p[i] == b'*' {
        i += 1;
    }
    i == p.len()
}
pub(crate) fn convert_directory(directory: soulseek::Directory) -> Directory {
    Directory::new(
        directory.name,
        directory.files.into_iter().map(convert_file).collect(),
    )
}

pub(crate) fn convert_file(file: soulseek::File) -> slskr_api::users::File {
    use slskr_api::users::{File, FileAttribute, FileAttributeType as ApiType};
    use soulseek::FileAttributeType as WireType;

    let attributes = file
        .attributes
        .into_iter()
        .filter_map(|attribute| {
            let kind = match attribute.kind {
                WireType::BitRate => ApiType::BitRate,
                WireType::Duration => ApiType::Length,
                WireType::VariableBitRate => ApiType::VariableBitRate,
                WireType::SampleRate => ApiType::SampleRate,
                WireType::BitDepth => ApiType::BitDepth,
                WireType::Encoder | WireType::Unknown(_) => return None,
            };
            Some(FileAttribute {
                kind,
                value: attribute.value as i32,
            })
        })
        .collect();
    File::new(
        i32::from(file.code),
        file.filename,
        file.size,
        file.extension,
        attributes,
    )
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/users/{username}/browse", get(browse))
        .route("/users/{username}/browse/status", get(browse_status))
        .route("/users/{username}/info", get(info))
        .route("/users/{username}/status", get(status))
        .route("/users/{username}/statistics", get(statistics))
        .route("/users/{username}/endpoint", get(endpoint))
        .route("/users/{username}/directory", post(directory))
}
/// Upstream hides blacklisted peers from network lookups.
fn ensure_lookup(app: &AppState, username: &str) -> ApiResult<()> {
    if app.services.users.is_blacklisted(username, None) {
        return Err(ApiError::NotFound(String::new()));
    }
    Ok(())
}

async fn browse(
    _: RequireReadWrite,
    State(s): State<AppState>,
    Path(u): Path<String>,
) -> ApiResult<Response> {
    ensure_lookup(&s, &u)?;
    let response = s.services.users.browse_peer(&u).await?;
    Ok(Json(response.as_ref()).into_response())
}

async fn browse_status(
    _: Authenticated,
    State(s): State<AppState>,
    Path(u): Path<String>,
) -> ApiResult<Json<BrowseStatus>> {
    s.services
        .users
        .browse_status(&u)
        .map(Json)
        .ok_or_else(|| ApiError::NotFound(String::new()))
}

async fn info(
    _: RequireReadWrite,
    State(s): State<AppState>,
    Path(u): Path<String>,
) -> ApiResult<Response> {
    ensure_lookup(&s, &u)?;
    let response = s.services.users.info(&u).await?;
    Ok(Json(response.as_ref()).into_response())
}

async fn status(
    _: RequireReadWrite,
    State(s): State<AppState>,
    Path(u): Path<String>,
) -> ApiResult<Json<Status>> {
    ensure_lookup(&s, &u)?;
    Ok(Json(s.services.users.status(&u).await?))
}

async fn statistics(
    _: Authenticated,
    State(s): State<AppState>,
    Path(u): Path<String>,
) -> ApiResult<Json<Statistics>> {
    ensure_lookup(&s, &u)?;
    Ok(Json(s.services.users.statistics(&u).await?))
}

async fn endpoint(
    _: RequireReadWrite,
    State(s): State<AppState>,
    Path(u): Path<String>,
) -> ApiResult<Json<Endpoint>> {
    ensure_lookup(&s, &u)?;
    Ok(Json(Endpoint::from_socket_addr(
        s.core.client.get_user_endpoint(&u).await?,
    )))
}

async fn directory(
    _: RequireReadWrite,
    State(s): State<AppState>,
    Path(u): Path<String>,
    body: Result<Json<DirectoryContentsRequest>, JsonRejection>,
) -> ApiResult<Json<Vec<Directory>>> {
    let directory = match body {
        Ok(Json(q)) if !q.directory.is_empty() => q.directory,
        _ => return Err(ApiError::BadRequest(String::new())),
    };
    ensure_lookup(&s, &u)?;
    let dirs = s
        .core
        .client
        .get_directory_contents(&u, &directory, None, s.core.shutdown.child_token())
        .await?;
    Ok(Json(dirs.into_iter().map(convert_directory).collect()))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn blacklist_glob_is_case_insensitive_and_anchored() {
        assert!(wildcard("bad*name?", "BadUserName1"));
        assert!(!wildcard("bad*name", "prefixBadUserName"));
    }

    #[test]
    fn dropped_browse_clears_only_the_matching_active_request() {
        let cache = RwLock::new(Cache::default());
        cache.write().unwrap().browses.insert(
            "peer".into(),
            Browse {
                id: 2,
                progress: BrowseStatus::default(),
                result: None,
            },
        );
        drop(ActiveBrowse {
            cache: &cache,
            username: "peer",
            id: 1,
            completed: false,
        });
        assert!(cache.read().unwrap().browses.contains_key("peer"));
        drop(ActiveBrowse {
            cache: &cache,
            username: "peer",
            id: 2,
            completed: false,
        });
        assert!(cache.read().unwrap().browses.is_empty());
    }

    #[test]
    fn wire_file_conversion_preserves_known_attributes() {
        let file = convert_file(soulseek::File {
            code: 1,
            filename: "track.flac".into(),
            size: 42,
            extension: "flac".into(),
            attributes: vec![
                soulseek::FileAttribute::new(soulseek::FileAttributeType::SampleRate, 48_000),
                soulseek::FileAttribute::new(soulseek::FileAttributeType::Duration, 180),
                soulseek::FileAttribute::new(soulseek::FileAttributeType::Unknown(99), 0),
            ],
        });
        assert_eq!(file.attribute_count, 2);
        assert_eq!(file.length, Some(180));
        assert_eq!(file.sample_rate, Some(48_000));
    }
}
