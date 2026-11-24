//! Resolvers answering incoming peer requests (Soulseek.NET option delegates).

use std::net::SocketAddr;

use bytes::Bytes;
use futures::FutureExt;
use futures::future::{BoxFuture, ready};

use crate::error::{Error, Result};
use crate::model::{BrowseResponse, Directory, SearchQuery, SearchResponse, UserInfo};

/// Answers requests made by remote peers. Every method has a default
/// implementation that shares nothing; implement only what is needed.
///
/// Futures are `'static`, so implementations clone what they need from `&self`
/// (typically an `Arc` of their state) before the `async move` block.
pub trait Resolvers: Send + Sync + 'static {
    /// Share list for a browse request. On error the client logs a warning and
    /// sends an empty response. Default: empty response.
    fn browse(
        &self,
        username: String,
        endpoint: SocketAddr,
    ) -> BoxFuture<'static, Result<BrowseResponse>> {
        let _ = (username, endpoint);
        ready(Ok(BrowseResponse::default())).boxed()
    }

    /// The encoded SharesReply frame answering a browse request; on error the
    /// client logs a warning and sends an empty response. Override to serve a
    /// frame encoded once (e.g. with [`BrowseResponse::to_bytes_best`]) instead
    /// of building and compressing the share list for every request. Default:
    /// encodes [`Resolvers::browse`] off the async workers.
    fn browse_frame(
        &self,
        username: String,
        endpoint: SocketAddr,
    ) -> BoxFuture<'static, Result<Bytes>> {
        let response = self.browse(username, endpoint);
        async move {
            let response = response.await?;
            tokio::task::spawn_blocking(move || response.to_bytes())
                .await
                .map_err(|error| Error::Other(format!("Could not encode browse response: {error}")))
        }
        .boxed()
    }

    /// Contents of `directory` for a folder contents request with `token`. On
    /// error no response is sent. Default: error (nothing shared).
    fn directory_contents(
        &self,
        username: String,
        endpoint: SocketAddr,
        token: u32,
        directory: String,
    ) -> BoxFuture<'static, Result<Vec<Directory>>> {
        let _ = (username, endpoint, token);
        ready(Err(Error::Other(format!(
            "Directory {directory} is not shared"
        ))))
        .boxed()
    }

    /// User info for an info request. On error the client logs a warning and
    /// sends the default info. Default: empty info.
    fn user_info(
        &self,
        username: String,
        endpoint: SocketAddr,
    ) -> BoxFuture<'static, Result<UserInfo>> {
        let _ = (username, endpoint);
        ready(Ok(UserInfo::default())).boxed()
    }

    /// Response to a search request from `username` with `token`; `None` (or a
    /// response without files) means no response is sent. Default: `None`.
    fn search(
        &self,
        username: String,
        token: u32,
        query: SearchQuery,
    ) -> BoxFuture<'static, Option<SearchResponse>> {
        let _ = (username, token, query);
        ready(None).boxed()
    }

    /// Decides whether a peer may enqueue a download of `filename` from us.
    /// `Ok(())` accepts. [`Error::DownloadEnqueue`] or [`Error::TransferRejected`]
    /// rejects with that message passed to the peer; any other error rejects with
    /// [`crate::error::ENQUEUE_INTERNAL_ERROR_MESSAGE`]
    /// (see [`Error::enqueue_rejection_message`]). Default: reject, "File not shared.".
    fn enqueue_download(
        &self,
        username: String,
        endpoint: SocketAddr,
        filename: String,
    ) -> BoxFuture<'static, Result<()>> {
        let _ = (username, endpoint, filename);
        ready(Err(Error::DownloadEnqueue("File not shared.".to_string()))).boxed()
    }

    /// Place of `filename` in our upload queue for `username`; `None` sends no
    /// response. Default: `None`.
    fn place_in_queue(
        &self,
        username: String,
        endpoint: SocketAddr,
        filename: String,
    ) -> BoxFuture<'static, Option<u32>> {
        let _ = (username, endpoint, filename);
        ready(None).boxed()
    }
}

/// Resolvers using every default (shares nothing, rejects all enqueues).
#[derive(Debug, Clone, Copy, Default)]
pub struct DefaultResolvers;

impl Resolvers for DefaultResolvers {}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn defaults() {
        let r = DefaultResolvers;
        let ep: SocketAddr = "127.0.0.1:1".parse().unwrap();
        assert_eq!(
            r.browse("u".into(), ep).await.unwrap(),
            BrowseResponse::default()
        );
        assert!(
            r.directory_contents("u".into(), ep, 1, "d".into())
                .await
                .is_err()
        );
        assert_eq!(
            r.user_info("u".into(), ep).await.unwrap(),
            UserInfo::default()
        );
        assert_eq!(r.place_in_queue("u".into(), ep, "f".into()).await, None);
        let err = r
            .enqueue_download("u".into(), ep, "f".into())
            .await
            .unwrap_err();
        assert_eq!(err.enqueue_rejection_message(), "File not shared.");
    }
}
