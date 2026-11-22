//! Outgoing search lifetimes and incoming search responses.

use std::collections::{HashMap, HashSet, hash_map::Entry};
use std::sync::{Mutex, MutexGuard, OnceLock};

use tokio::sync::{Semaphore, mpsc};
use tokio::time::{Instant, sleep};
use tokio_util::sync::CancellationToken;

use super::{SoulseekClient, require_non_blank};
use crate::error::{Error, Result};
use crate::events::Event;
use crate::model::{
    Search, SearchQuery, SearchResponse, SearchScope, SearchScopeType, SearchState,
};
use crate::options::SearchOptions;
use crate::protocol::server::{
    RoomSearchRequest, SearchRequest, UserSearchRequest, WishlistSearchRequest,
};

/// A bounded, session-local cache of recently received search requests. Keep two
/// generations so an interleaved duplicate near a rollover is still detected.
#[derive(Default)]
pub(super) struct RecentRequests {
    current: HashSet<(String, u32, String)>,
    previous: HashSet<(String, u32, String)>,
}

impl RecentRequests {
    pub(super) fn is_duplicate(&mut self, username: &str, token: u32, query: &str) -> bool {
        const GENERATION_SIZE: usize = 512;
        let key = (username.to_owned(), token, query.to_owned());
        if self.current.contains(&key) || self.previous.contains(&key) {
            return true;
        }
        if self.current.len() == GENERATION_SIZE {
            self.previous.clear();
            std::mem::swap(&mut self.previous, &mut self.current);
        }
        self.current.insert(key);
        false
    }
}

struct ActiveSearch {
    /// Queued searches own their token but cannot receive responses yet.
    responses: Option<mpsc::UnboundedSender<(Instant, SearchResponse)>>,
    explicitly_searches_self: bool,
}

#[derive(Default)]
struct Session {
    disconnected: CancellationToken,
    searches: HashMap<u32, ActiveSearch>,
    recent_requests: RecentRequests,
}

#[derive(Default)]
pub(crate) struct SearchManager {
    session: Mutex<Session>,
    slots: OnceLock<Semaphore>,
}

struct Registration<'a> {
    manager: &'a SearchManager,
    token: u32,
    disconnected: CancellationToken,
}

impl Drop for Registration<'_> {
    fn drop(&mut self) {
        let mut session = self.manager.session();
        // A disconnected session has already removed this entry. Its token may
        // now belong to a search started on a new server connection.
        if !self.disconnected.is_cancelled() {
            session.searches.remove(&self.token);
        }
    }
}

impl SearchManager {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    fn session(&self) -> MutexGuard<'_, Session> {
        self.session.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn register(&self, client: &SoulseekClient, search: &Search) -> Result<Registration<'_>> {
        let username = client.username();
        let mut session = self.session();
        client.inner().ensure_logged_in("search")?;
        match session.searches.entry(search.token) {
            Entry::Occupied(_) => Err(Error::DuplicateToken(format!(
                "An active search with token {} is already in progress",
                search.token
            ))),
            Entry::Vacant(entry) => {
                entry.insert(ActiveSearch {
                    responses: None,
                    explicitly_searches_self: search.scope.kind == SearchScopeType::User
                        && username.is_some_and(|username| {
                            search
                                .scope
                                .subjects
                                .iter()
                                .any(|subject| subject.eq_ignore_ascii_case(&username))
                        }),
                });
                Ok(Registration {
                    manager: self,
                    token: search.token,
                    disconnected: session.disconnected.clone(),
                })
            }
        }
    }

    pub(crate) fn on_disconnected(&self) {
        let mut session = self.session();
        session.disconnected.cancel();
        *session = Session::default();
    }

    pub(crate) fn handle_search_response(
        &self,
        _client: &SoulseekClient,
        response: SearchResponse,
    ) {
        if let Some(sender) = self
            .session()
            .searches
            .get(&response.token)
            .and_then(|search| search.responses.as_ref())
        {
            let _ = sender.send((Instant::now(), response));
        }
    }

    pub(super) fn explicitly_searches_self(&self, token: u32) -> bool {
        self.session()
            .searches
            .get(&token)
            .is_some_and(|search| search.responses.is_some() && search.explicitly_searches_self)
    }

    pub(crate) fn respond(
        &self,
        client: &SoulseekClient,
        username: impl Into<String>,
        token: u32,
        query: impl Into<String>,
    ) {
        let username = username.into();
        let query = query.into();
        let inner = client.inner();
        let deduplicate = inner
            .options
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .deduplicate_search_requests;
        let disconnected = {
            let mut session = self.session();
            if !inner.state().is_logged_in()
                || (deduplicate
                    && session
                        .recent_requests
                        .is_duplicate(&username, token, &query))
            {
                return;
            }
            session.disconnected.clone()
        };

        inner.emit(Event::SearchRequestReceived {
            username: username.clone(),
            token,
            query: query.clone(),
        });
        let client = client.clone();
        tokio::spawn(async move {
            let inner = client.inner();
            let response = tokio::select! {
                biased;
                _ = disconnected.cancelled() => return,
                response = inner.resolvers.search(username.clone(), token, SearchQuery::parse(&query)) => response,
            };
            let Some(mut response) =
                response.filter(|r| !r.files.is_empty() || !r.locked_files.is_empty())
            else {
                return;
            };
            let Some(local_username) = client.username() else {
                return;
            };
            response.username = local_username;
            response.token = token;
            let frame = response.to_bytes();
            let delivered = tokio::select! {
                biased;
                _ = disconnected.cancelled() => false,
                result = inner.peers.send_to(&client, &username, frame, &disconnected) => result.is_ok(),
            };
            inner.emit(if delivered {
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
        });
    }
}

impl SoulseekClient {
    /// Searches the network within `scope` and streams accepted responses to
    /// `responses` (also raised as [`Event::SearchResponseReceived`]).
    ///
    /// `token`: unique search token (generated when `None`); a token in use →
    /// [`Error::DuplicateToken`] (`"An active search with token {token} is already in progress"`).
    /// The query must contain at least one term after
    /// `remove_single_character_search_terms` ([`Error::Argument`]); an invalid
    /// scope → [`Error::Argument`]. At most
    /// [`crate::options::ClientOptions::maximum_concurrent_searches`] run at once; others wait in
    /// state `QUEUED`. States raised via [`Event::SearchStateChanged`] (and
    /// [`SearchOptions::state_changed`]): `REQUESTED` → `IN_PROGRESS` →
    /// `COMPLETED |` one of `TIMED_OUT` (no accepted response for `search_timeout`),
    /// `RESPONSE_LIMIT_REACHED`, `FILE_LIMIT_REACHED`, `CANCELLED`, `ERRORED`.
    /// Responses are filtered per [`SearchOptions`] (files by `file_filter`, then
    /// minimum file count, upload speed, queue length, `response_filter`).
    ///
    /// Returns the final [`Search`] on timeout or limits; cancellation of
    /// `cancel` → [`Error::Cancelled`] (after the `CANCELLED` state is raised).
    pub async fn search(
        &self,
        mut query: SearchQuery,
        scope: SearchScope,
        token: Option<u32>,
        options: SearchOptions,
        responses: mpsc::UnboundedSender<SearchResponse>,
        cancel: CancellationToken,
    ) -> Result<Search> {
        let inner = self.inner();
        inner.ensure_logged_in("search")?;
        require_non_blank(&query.search_text, "search text")?;
        scope.validate().map_err(Error::Argument)?;
        for subject in &scope.subjects {
            require_non_blank(subject, "search scope subject")?;
        }
        if options.remove_single_character_search_terms {
            query.terms.retain(|term| term.chars().nth(1).is_some());
            query = SearchQuery::new(query.terms, query.exclusions);
        }
        if !query.terms.iter().any(|term| !term.trim().is_empty()) {
            return Err(Error::Argument(
                "Search query must contain at least one non-exclusion term".into(),
            ));
        }
        let mut search = Search {
            query,
            scope,
            token: token.unwrap_or_else(|| self.get_next_token()),
            ..Default::default()
        };
        let manager = &inner.searches;
        let registration = manager.register(self, &search)?;
        update_state(self, &mut search, &options, SearchState::REQUESTED);

        let result = tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(Error::Cancelled),
            _ = registration.disconnected.cancelled() => Err(search_disconnected()),
            result = async {
                let slots = manager.slots.get_or_init(|| {
                    let limit = inner.options.read().unwrap_or_else(|e| e.into_inner())
                        .maximum_concurrent_searches;
                    Semaphore::new(limit.clamp(1, Semaphore::MAX_PERMITS))
                });
                let _permit = if let Ok(permit) = slots.try_acquire() {
                    permit
                } else {
                    update_state(self, &mut search, &options, SearchState::QUEUED);
                    slots.acquire().await.expect("search semaphore is never closed")
                };
                let (tx, rx) = mpsc::unbounded_channel();
                {
                    let mut session = manager.session();
                    if registration.disconnected.is_cancelled() {
                        return Err(search_disconnected());
                    }
                    session.searches.get_mut(&search.token)
                        .expect("live search registration").responses = Some(tx);
                }
                send_request(self, &search).await.map_err(|err| match err {
                    Error::Timeout(_) | Error::Cancelled | Error::Connection(_) => err,
                    _ => Error::Other(format!("Failed to search for {} ({}): {err}", search.search_text(), search.token)),
                })?;
                update_state(self, &mut search, &options, SearchState::IN_PROGRESS);
                Ok(collect_responses(self, &mut search, &options, &responses, rx).await)
            } => result,
        };
        let end_state = match &result {
            Ok(state) => *state,
            Err(Error::Cancelled) => SearchState::CANCELLED,
            Err(_) => SearchState::ERRORED,
        };
        update_state(
            self,
            &mut search,
            &options,
            SearchState::COMPLETED | end_state,
        );
        result.map(|_| search)
    }
}

fn search_disconnected() -> Error {
    Error::Connection("The server disconnected during the search".into())
}

fn update_state(
    client: &SoulseekClient,
    search: &mut Search,
    options: &SearchOptions,
    state: SearchState,
) {
    let previous = std::mem::replace(&mut search.state, state);
    if let Some(callback) = &options.state_changed {
        callback(previous, search);
    }
    let inner = client.inner();
    if inner.events.receiver_count() != 0 {
        inner.emit(Event::SearchStateChanged {
            previous,
            search: search.clone(),
        });
    }
}

async fn send_request(client: &SoulseekClient, search: &Search) -> Result<()> {
    let send = |frame| client.inner().send_server(frame);
    let token = search.token;
    let search_text = search.search_text();
    match search.scope.kind {
        SearchScopeType::Network => {
            send(
                SearchRequest {
                    token,
                    search_text: search_text.into(),
                }
                .to_bytes(),
            )
            .await
        }
        SearchScopeType::Wishlist => {
            send(
                WishlistSearchRequest {
                    token,
                    search_text: search_text.into(),
                }
                .to_bytes(),
            )
            .await
        }
        SearchScopeType::Room => {
            send(
                RoomSearchRequest {
                    token,
                    search_text: search_text.into(),
                    room_name: search.scope.subjects[0].clone(),
                }
                .to_bytes(),
            )
            .await
        }
        SearchScopeType::User => {
            for username in &search.scope.subjects {
                send(
                    UserSearchRequest {
                        token,
                        search_text: search_text.into(),
                        username: username.clone(),
                    }
                    .to_bytes(),
                )
                .await?;
            }
            Ok(())
        }
    }
}

async fn collect_responses(
    client: &SoulseekClient,
    search: &mut Search,
    options: &SearchOptions,
    responses: &mpsc::UnboundedSender<SearchResponse>,
    mut incoming: mpsc::UnboundedReceiver<(Instant, SearchResponse)>,
) -> SearchState {
    if options.response_limit == 0 {
        return SearchState::RESPONSE_LIMIT_REACHED;
    }
    if options.file_limit == 0 {
        return SearchState::FILE_LIMIT_REACHED;
    }
    let started = Instant::now();
    let deadline = sleep(options.search_timeout);
    tokio::pin!(deadline);
    loop {
        let (arrived, mut response) = tokio::select! {
            biased;
            Some(entry) = incoming.recv() => entry,
            _ = &mut deadline => return SearchState::TIMED_OUT,
        };
        if arrived.max(started) >= deadline.deadline() {
            return SearchState::TIMED_OUT;
        }
        if !filter_response(options, &mut response) {
            continue;
        }
        if let Some(next) = arrived.max(started).checked_add(options.search_timeout) {
            deadline.as_mut().reset(next);
        }
        search.response_count = search.response_count.saturating_add(1);
        search.file_count = search
            .file_count
            .saturating_add(count(response.file_count()));
        search.locked_file_count = search
            .locked_file_count
            .saturating_add(count(response.locked_file_count()));
        let inner = client.inner();
        if responses.is_closed() {
            inner.emit(Event::SearchResponseReceived {
                token: search.token,
                response,
            });
        } else {
            if inner.events.receiver_count() != 0 {
                inner.emit(Event::SearchResponseReceived {
                    token: search.token,
                    response: response.clone(),
                });
            }
            let _ = responses.send(response);
        }
        if search.response_count as usize >= options.response_limit {
            return SearchState::RESPONSE_LIMIT_REACHED;
        }
        if search.file_count as usize >= options.file_limit {
            return SearchState::FILE_LIMIT_REACHED;
        }
    }
}

fn count(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

fn filter_response(options: &SearchOptions, response: &mut SearchResponse) -> bool {
    if !options.filter_responses {
        return true;
    }
    if let Some(filter) = &options.file_filter {
        response.files.retain(|file| filter(file));
        response.locked_files.retain(|file| filter(file));
    }
    response.file_count() + response.locked_file_count() >= options.minimum_response_file_count
        && response.upload_speed >= options.minimum_peer_upload_speed
        && response.queue_length < options.maximum_peer_queue_length
        && options
            .response_filter
            .as_ref()
            .is_none_or(|filter| filter(response))
}
