//! Incoming Soulseek requests resolved by daemon services.

use std::{
    net::SocketAddr,
    path::Path,
    sync::{
        Arc, OnceLock, Weak,
        atomic::{AtomicUsize, Ordering},
    },
};

use futures::future::BoxFuture;
use soulseek::{
    BrowseResponse, Directory, Error, Resolvers, Result, SearchQuery, SearchResponse, UserInfo,
};
use tokio::{
    io::AsyncReadExt,
    sync::{OwnedSemaphorePermit, Semaphore, TryAcquireError},
};

use crate::app::{AppInner, AppState};

const MAX_PICTURE_BYTES: u64 = 8 * 1024 * 1024;

pub(crate) struct AppResolvers {
    app: OnceLock<Weak<AppInner>>,
    searches: Arc<Semaphore>,
    queued_searches: Arc<AtomicUsize>,
}

impl AppResolvers {
    pub(crate) fn new(search_concurrency: usize) -> Self {
        Self {
            app: OnceLock::new(),
            searches: Arc::new(Semaphore::new(search_concurrency)),
            queued_searches: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub(crate) fn bind(&self, app: &AppState) {
        assert!(
            self.app.set(Arc::downgrade(&app.0)).is_ok(),
            "resolver already bound"
        );
    }

    fn app(&self) -> Result<AppState> {
        self.app
            .get()
            .and_then(Weak::upgrade)
            .map(AppState)
            .ok_or_else(|| Error::InvalidState("Application is unavailable".into()))
    }
}

impl Resolvers for AppResolvers {
    fn browse(
        &self,
        username: String,
        endpoint: SocketAddr,
    ) -> BoxFuture<'static, Result<BrowseResponse>> {
        let app = self.app();
        Box::pin(async move {
            let app = app?;
            if app
                .services
                .users
                .is_blacklisted(&username, Some(endpoint.ip()))
            {
                return Ok(BrowseResponse::default());
            }
            let shares = app.services.shares.clone();
            tokio::task::spawn_blocking(move || shares.browse())
                .await
                .map_err(other)?
                .map_err(other)
        })
    }

    fn directory_contents(
        &self,
        username: String,
        endpoint: SocketAddr,
        _token: u32,
        directory: String,
    ) -> BoxFuture<'static, Result<Vec<Directory>>> {
        let app = self.app();
        Box::pin(async move {
            let app = app?;
            if app
                .services
                .users
                .is_blacklisted(&username, Some(endpoint.ip()))
            {
                return Ok(Vec::new());
            }
            app.services
                .shares
                .list_directory(&directory)
                .map_err(other)
        })
    }

    fn user_info(
        &self,
        username: String,
        endpoint: SocketAddr,
    ) -> BoxFuture<'static, Result<UserInfo>> {
        let app = self.app();
        Box::pin(async move {
            let app = app?;
            let options = app.options.get();
            let picture = match Some(options.soulseek.picture.as_str())
                .filter(|path| !path.trim().is_empty())
            {
                Some(path) => match read_picture(Path::new(path)).await {
                    Ok(bytes) => Some(bytes),
                    Err(error) => {
                        tracing::warn!(%error, "could not read configured Soulseek picture");
                        None
                    }
                },
                None => None,
            };
            let (slots, queue_length, has_free_upload_slot) = if app
                .services
                .users
                .is_blacklisted(&username, Some(endpoint.ip()))
            {
                (0, i32::MAX as u32, false)
            } else {
                app.services.transfers.upload_info_for(&username)
            };
            Ok(UserInfo {
                description: options.soulseek.description.clone(),
                picture,
                upload_slots: slots,
                queue_length,
                has_free_upload_slot,
                upload_permissions: None,
            })
        })
    }

    fn search(
        &self,
        username: String,
        token: u32,
        mut query: SearchQuery,
    ) -> BoxFuture<'static, Option<SearchResponse>> {
        let app = self.app();
        let searches = self.searches.clone();
        let queued = self.queued_searches.clone();
        Box::pin(async move {
            let app = app.ok()?;
            if app.services.users.is_blacklisted(&username, None)
                || query.terms.is_empty()
                || app
                    .search_filters
                    .read()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .iter()
                    .any(|filter| filter.is_match(&query.search_text))
            {
                return None;
            }
            let options = app.options.get();
            let incoming = &options.search;
            let permit = search_permit(
                searches,
                &queued,
                usize::try_from(incoming.incoming_circuit_breaker).ok()?,
            )
            .await?;
            {
                let exclusions = app
                    .excluded_search_phrases
                    .read()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                for phrase in exclusions.iter() {
                    if !query.exclusions.contains(phrase) {
                        query.exclusions.push(phrase.clone());
                    }
                }
            }
            let shares = app.services.shares.clone();
            let limit = usize::try_from(incoming.response_file_limit).ok()?;
            let (files, _permit) =
                tokio::task::spawn_blocking(move || (shares.search(&query, limit), permit))
                    .await
                    .ok()?;
            let files = files.ok()?;
            if files.is_empty() {
                return None;
            }
            if !options.blacklist.cidrs.is_empty() {
                let endpoint = app.client.get_user_endpoint(&username).await.ok()?;
                if app
                    .services
                    .users
                    .is_blacklisted(&username, Some(endpoint.ip()))
                {
                    return None;
                }
            }
            let (_, queue_length, has_free_upload_slot) =
                app.services.transfers.upload_info_for(&username);
            Some(SearchResponse {
                username: app.client.username()?,
                token,
                files,
                has_free_upload_slot,
                queue_length: u64::from(queue_length),
                upload_speed: app.upload_speed.load(Ordering::Relaxed),
                locked_files: Vec::new(),
            })
        })
    }

    fn enqueue_download(
        &self,
        username: String,
        endpoint: SocketAddr,
        filename: String,
    ) -> BoxFuture<'static, Result<()>> {
        let app = self.app();
        Box::pin(async move {
            let app = app?;
            if app
                .services
                .users
                .is_blacklisted(&username, Some(endpoint.ip()))
            {
                return Err(Error::DownloadEnqueue("File not shared.".into()));
            }
            let transfers = app.services.transfers.clone();
            tokio::task::spawn_blocking(move || {
                transfers.enqueue_upload(&username, endpoint, &filename)
            })
            .await
            .map_err(other)?
            .map_err(Error::DownloadEnqueue)
        })
    }

    fn place_in_queue(
        &self,
        username: String,
        endpoint: SocketAddr,
        filename: String,
    ) -> BoxFuture<'static, Option<u32>> {
        let app = self.app();
        Box::pin(async move {
            let app = app.ok()?;
            if app
                .services
                .users
                .is_blacklisted(&username, Some(endpoint.ip()))
            {
                return None;
            }
            app.services.transfers.place_in_queue(&username, &filename)
        })
    }
}

async fn search_permit(
    semaphore: Arc<Semaphore>,
    queued: &AtomicUsize,
    limit: usize,
) -> Option<OwnedSemaphorePermit> {
    match semaphore.clone().try_acquire_owned() {
        Ok(permit) => Some(permit),
        Err(TryAcquireError::Closed) => None,
        Err(TryAcquireError::NoPermits) => {
            let mut count = queued.load(Ordering::Relaxed);
            loop {
                if count >= limit {
                    return None;
                }
                match queued.compare_exchange_weak(
                    count,
                    count + 1,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => break,
                    Err(actual) => count = actual,
                }
            }
            struct Queued<'a>(&'a AtomicUsize);
            impl Drop for Queued<'_> {
                fn drop(&mut self) {
                    self.0.fetch_sub(1, Ordering::Relaxed);
                }
            }
            let _queued = Queued(queued);
            semaphore.acquire_owned().await.ok()
        }
    }
}

async fn read_picture(path: &Path) -> std::io::Result<Vec<u8>> {
    fn check(meta: &std::fs::Metadata) -> std::io::Result<()> {
        if meta.is_file() && meta.len() <= MAX_PICTURE_BYTES {
            return Ok(());
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "picture must be a regular file no larger than 8 MiB",
        ))
    }
    check(&tokio::fs::metadata(path).await?)?;
    let file = tokio::fs::File::open(path).await?;
    let meta = file.metadata().await?;
    check(&meta)?;
    let mut bytes = Vec::with_capacity(meta.len() as usize);
    file.take(MAX_PICTURE_BYTES + 1)
        .read_to_end(&mut bytes)
        .await?;
    if bytes.len() as u64 > MAX_PICTURE_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "picture exceeds 8 MiB",
        ));
    }
    Ok(bytes)
}

fn other(error: impl std::fmt::Display) -> Error {
    Error::Other(error.to_string())
}
