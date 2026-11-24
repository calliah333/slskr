//! Search lifecycle, persistence, HTTP API and `searches` events.

mod store;

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};

use axum::{
    extract::State,
    http::{HeaderValue, StatusCode, header},
    response::IntoResponse,
};
use chrono::Utc;
use slskr_types::{
    realtime::{Event, Topic},
    search::{Search, SearchRequest, SearchResponse, SearchState},
};
use tokio::{
    sync::{Semaphore, mpsc},
    time::{Instant, MissedTickBehavior},
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::{
    app::{AppState, Core},
    auth::{Authenticated, RequireReadWrite},
    db::Database,
    http::{ApiError, ApiResult, Json, Path, Routes},
    realtime::EventBus,
    users::convert_file,
};

/// Progress (counts and response batches) is published at most this often per search.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(500);
const RETENTION_INTERVAL: Duration = Duration::from_secs(5 * 60);
/// Most responses already waiting on the channel that are stored in one transaction.
const RESPONSE_BATCH: usize = 256;

#[derive(Clone)]
pub struct SearchService(Arc<Inner>);

struct Inner {
    db: Database,
    client: soulseek::SoulseekClient,
    bus: EventBus,
    shutdown: CancellationToken,
    create_slot: Semaphore,
    // Mutations and their events are ordered by Database::call. Snapshots for new
    // subscribers never query SQLite or wait for disk I/O.
    searches: Mutex<HashMap<Arc<str>, Entry>>,
}

struct Entry {
    search: Search,
    cancel: Option<CancellationToken>,
}

#[derive(Clone)]
struct ActiveSearch {
    id: Arc<str>,
    token: u32,
}

impl ActiveSearch {
    fn entry<'a>(&self, searches: &'a HashMap<Arc<str>, Entry>) -> Option<&'a Entry> {
        let (id, entry) = searches.get_key_value(self.id.as_ref())?;
        // The ID can be reused immediately after deletion; only this task's
        // allocation may change the corresponding record, even if tokens wrap.
        (Arc::ptr_eq(id, &self.id) && entry.cancel.is_some()).then_some(entry)
    }
}

impl Inner {
    fn searches(&self) -> MutexGuard<'_, HashMap<Arc<str>, Entry>> {
        self.searches
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn list(&self) -> Vec<Search> {
        let mut list: Vec<_> = self
            .searches()
            .values()
            .map(|entry| entry.search.clone())
            .collect();
        list.sort_unstable_by(|a, b| {
            b.started_at
                .cmp(&a.started_at)
                .then_with(|| a.id.cmp(&b.id))
        });
        list
    }

    fn publish(&self, event: impl FnOnce() -> Event) {
        self.bus.publish_with(Topic::Searches, event);
    }
}

impl SearchService {
    pub async fn new(core: &Core) -> anyhow::Result<Self> {
        let path = core.paths.db_dir.join("search.db");
        let db =
            tokio::task::spawn_blocking(move || Database::open(&path, store::MIGRATIONS)).await??;
        let searches = db
            .call(store::load)
            .await?
            .into_iter()
            .map(|search| {
                (
                    Arc::from(search.id.as_str()),
                    Entry {
                        search,
                        cancel: None,
                    },
                )
            })
            .collect();
        let service = Self(Arc::new(Inner {
            db,
            client: core.client.clone(),
            bus: core.bus.clone(),
            shutdown: core.shutdown.clone(),
            create_slot: Semaphore::new(1),
            searches: Mutex::new(searches),
        }));
        if let Some(age) = core.options.get().retention.searches {
            service.prune(age).await?;
        }
        Ok(service)
    }

    pub fn start(&self, app: AppState) {
        let service = self.clone();
        let options = app.options.clone();
        tokio::spawn(async move {
            let mut interval =
                tokio::time::interval_at(Instant::now() + RETENTION_INTERVAL, RETENTION_INTERVAL);
            interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    biased;
                    _ = service.0.shutdown.cancelled() => break,
                    _ = interval.tick() => {
                        let Some(age) = options.get().retention.searches else { continue };
                        if let Err(error) = service.prune(age).await {
                            tracing::warn!(%error, "could not apply search retention");
                        }
                    }
                }
            }
        });
    }

    async fn prune(&self, age: Duration) -> anyhow::Result<()> {
        let cutoff = Utc::now() - chrono::TimeDelta::from_std(age)?;
        let inner = self.0.clone();
        self.0
            .db
            .call(move |db| {
                let expired: Vec<_> = inner
                    .searches()
                    .values()
                    .filter(|entry| entry.search.ended_at.is_some_and(|ended| ended < cutoff))
                    .map(|entry| entry.search.id.clone())
                    .collect();
                if expired.is_empty() {
                    return Ok(());
                }
                let transaction = db.transaction()?;
                for id in &expired {
                    store::remove(&transaction, id)?;
                }
                transaction.commit()?;
                let removed: Vec<_> = {
                    let mut searches = inner.searches();
                    expired
                        .into_iter()
                        .filter_map(|id| searches.remove(id.as_str()))
                        .collect()
                };
                for entry in removed {
                    inner.publish_delete(entry);
                }
                Ok(())
            })
            .await
    }

    async fn create(&self, request: SearchRequest) -> ApiResult<Search> {
        let (id, query, options) = validate(request)?;
        let _permit = self.0.create_slot.try_acquire().map_err(|_| ApiError::Status(
            StatusCode::TOO_MANY_REQUESTS,
            "Only one concurrent operation is permitted. Wait until the previous request completes".into(),
        ))?;
        let state = self.0.client.state();
        if !state.is_logged_in() {
            return Err(ApiError::Conflict(format!(
                "The server connection must be logged in to perform a search (currently: {})",
                crate::app::connection_state(state).as_str()
            )));
        }

        let search = Search {
            id: id.to_string(),
            query: query.search_text.clone(),
            state: SearchState::Requested,
            token: self.0.client.get_next_token(),
            started_at: Utc::now(),
            ended_at: None,
            response_count: 0,
            file_count: 0,
            locked_file_count: 0,
        };
        let service = self.clone();
        let runtime = tokio::runtime::Handle::current();
        self.0
            .db
            .call(move |db| {
                if !store::insert(db, &search)? {
                    return Ok(None);
                }
                let active = ActiveSearch {
                    id: Arc::from(search.id.as_str()),
                    token: search.token,
                };
                let cancel = service.0.shutdown.child_token();
                service.0.searches().insert(
                    active.id.clone(),
                    Entry {
                        search: search.clone(),
                        cancel: Some(cancel.clone()),
                    },
                );
                service.0.publish(|| Event::SearchCreated(search.clone()));
                // Launch from the insertion callback so a disconnected HTTP caller can't leave a
                // permanent REQUESTED record after SQLite has committed it.
                runtime.spawn(service.run(active, query, options, cancel));
                Ok(Some(search))
            })
            .await?
            .ok_or_else(|| ApiError::Conflict(format!("Search '{id}' already exists")))
    }

    async fn run(
        self,
        active: ActiveSearch,
        query: soulseek::SearchQuery,
        mut options: soulseek::SearchOptions,
        cancel: CancellationToken,
    ) {
        let (state_tx, mut states) = mpsc::unbounded_channel();
        options.state_changed = Some(Arc::new(move |_, search| {
            // REQUESTED was sent by CREATE, and completion must wait for the last response
            // to be persisted. Intermediate states precede all responses.
            if !search
                .state
                .intersects(soulseek::SearchState::REQUESTED | soulseek::SearchState::COMPLETED)
            {
                let _ = state_tx.send(search_state(search.state));
            }
        }));
        let (response_tx, mut responses) = mpsc::unbounded_channel();
        let search = self.0.client.search(
            query,
            soulseek::SearchScope::network(),
            Some(active.token),
            options,
            response_tx,
            cancel,
        );
        let collect = async {
            let mut progress = tokio::time::interval(PROGRESS_INTERVAL);
            progress.set_missed_tick_behavior(MissedTickBehavior::Delay);
            // Responses stored since the last progress event, published with it.
            let mut pending = Vec::new();
            let mut dirty = false;
            loop {
                let result = tokio::select! {
                    biased;
                    Some(state) = states.recv() => self.change_state(&active, state).await,
                    _ = progress.tick(), if dirty => {
                        dirty = false;
                        self.publish_progress(&active, std::mem::take(&mut pending)).await
                    }
                    response = responses.recv() => {
                        let Some(response) = response else { break };
                        // Store whatever else already arrived with the same transaction.
                        let mut batch = vec![response];
                        while batch.len() < RESPONSE_BATCH
                            && let Ok(response) = responses.try_recv()
                        {
                            batch.push(response);
                        }
                        self.append_responses(&active, batch).await.map(|stored| {
                            if !stored.is_empty() {
                                dirty = true;
                                if self.0.bus.wants(Topic::Searches) {
                                    pending.extend(stored);
                                }
                            }
                        })
                    }
                };
                if let Err(error) = result {
                    tracing::warn!(%error, search_id = %active.id, "could not persist a search update");
                }
            }
            // The final state follows the last responses.
            if dirty && let Err(error) = self.publish_progress(&active, pending).await {
                tracing::warn!(%error, search_id = %active.id, "could not publish search progress");
            }
        };
        let (result, ()) = tokio::join!(search, collect);
        let state = match result {
            Ok(search) => search_state(search.state),
            Err(soulseek::Error::Cancelled) => SearchState::Cancelled,
            Err(error) => {
                tracing::debug!(%error, search_id = %active.id, "search failed");
                SearchState::Errored
            }
        };
        if let Err(error) = self.change_state(&active, state).await {
            tracing::warn!(%error, search_id = %active.id, "could not finalize search");
        }
    }

    async fn change_state(&self, active: &ActiveSearch, state: SearchState) -> anyhow::Result<()> {
        let inner = self.0.clone();
        let active = active.clone();
        self.0
            .db
            .call(move |db| {
                let Some(mut search) = active
                    .entry(&inner.searches())
                    .filter(|entry| entry.search.state != state)
                    .map(|entry| entry.search.clone())
                else {
                    return Ok(());
                };
                search.set_state(state, Utc::now());
                store::update(db, &search)?;
                {
                    let mut searches = inner.searches();
                    let entry = searches
                        .get_mut(active.id.as_ref())
                        .expect("search mutations are serialized by the database");
                    if search.state.is_complete() {
                        entry.cancel = None;
                    }
                    entry.search = search.clone();
                }
                inner.publish(|| Event::SearchUpdated(search));
                Ok(())
            })
            .await
    }

    /// Stores responses in arrival order with one transaction; returns those stored, none
    /// when the search is no longer this task's.
    async fn append_responses(
        &self,
        active: &ActiveSearch,
        responses: Vec<soulseek::SearchResponse>,
    ) -> anyhow::Result<Vec<SearchResponse>> {
        let inner = self.0.clone();
        let active = active.clone();
        self.0
            .db
            .call(move |db| {
                let Some(mut counts) = active.entry(&inner.searches()).map(|entry| {
                    [
                        entry.search.response_count,
                        entry.search.file_count,
                        entry.search.locked_file_count,
                    ]
                }) else {
                    return Ok(Vec::new());
                };
                for response in &responses {
                    let [total, files, locked_files] = &mut counts;
                    *total = total.saturating_add(1);
                    *files = files.saturating_add(count(response.file_count()));
                    *locked_files =
                        locked_files.saturating_add(count(response.locked_file_count()));
                }

                let responses: Vec<_> = responses.into_iter().map(map_response).collect();
                store::append_many(db, &active.id, &responses, counts)?;
                {
                    let mut searches = inner.searches();
                    let search = &mut searches
                        .get_mut(active.id.as_ref())
                        .expect("search mutations are serialized by the database")
                        .search;
                    [
                        search.response_count,
                        search.file_count,
                        search.locked_file_count,
                    ] = counts;
                }
                Ok(responses)
            })
            .await
    }

    /// Publishes the responses stored since the last call, then the search's counts.
    async fn publish_progress(
        &self,
        active: &ActiveSearch,
        responses: Vec<SearchResponse>,
    ) -> anyhow::Result<()> {
        if !self.0.bus.wants(Topic::Searches) {
            return Ok(());
        }
        let inner = self.0.clone();
        let active = active.clone();
        self.0
            .db
            .call(move |_| {
                let search = active
                    .entry(&inner.searches())
                    .map(|entry| entry.search.clone());
                if let Some(search) = search {
                    if !responses.is_empty() {
                        inner.publish(|| Event::SearchResponses {
                            search_id: search.id.clone(),
                            responses,
                        });
                    }
                    inner.publish(|| Event::SearchUpdated(search));
                }
                Ok(())
            })
            .await
    }

    /// Every search (without responses) and the bus sequence number the list includes.
    pub(crate) fn snapshot(&self) -> (u64, Vec<Search>) {
        let seq = self.0.bus.seq();
        (seq, self.0.list())
    }

    pub(crate) async fn search_text(&self, id: &str) -> anyhow::Result<Option<String>> {
        let id = id.to_owned();
        self.0.db.call(move |db| store::search_text(db, &id)).await
    }

    async fn get(&self, id: &str) -> ApiResult<Search> {
        let id = id.to_owned();
        self.0
            .db
            .call(move |db| store::get(db, &id))
            .await?
            .ok_or_else(ApiError::not_found)
    }

    /// The `[SearchResponse]` JSON array, as stored.
    async fn responses_json(&self, id: &str) -> ApiResult<String> {
        let id = id.to_owned();
        self.0
            .db
            .call(move |db| store::responses_json(db, &id))
            .await?
            .ok_or_else(ApiError::not_found)
    }

    fn cancel(&self, id: &str) -> ApiResult<()> {
        let searches = self.0.searches();
        let entry = searches.get(id).ok_or_else(ApiError::not_found)?;
        if let Some(cancel) = &entry.cancel {
            cancel.cancel();
        }
        Ok(())
    }

    async fn remove(&self, id: String) -> ApiResult<()> {
        let inner = self.0.clone();
        let removed = self
            .0
            .db
            .call(move |db| {
                if !inner.searches().contains_key(id.as_str()) {
                    return Ok(false);
                }
                store::remove(db, &id)?;
                let entry = inner
                    .searches()
                    .remove(id.as_str())
                    .expect("search mutations are serialized by the database");
                inner.publish_delete(entry);
                Ok(true)
            })
            .await?;
        if removed {
            Ok(())
        } else {
            Err(ApiError::not_found())
        }
    }
}

impl Inner {
    fn publish_delete(&self, Entry { search, cancel }: Entry) {
        if let Some(cancel) = cancel {
            cancel.cancel();
        }
        self.publish(|| Event::SearchDeleted { id: search.id });
    }
}

fn validate(
    request: SearchRequest,
) -> ApiResult<(Uuid, soulseek::SearchQuery, soulseek::SearchOptions)> {
    let id = match request.id {
        Some(id) => {
            Uuid::parse_str(&id).map_err(|_| ApiError::BadRequest("id must be a UUID".into()))?
        }
        None => Uuid::new_v4(),
    };
    if request.query.trim().is_empty() {
        return Err(ApiError::BadRequest(
            "query must not be empty or consist only of whitespace".into(),
        ));
    }
    let query = soulseek::SearchQuery::from(request.query.as_str());
    if !query
        .terms
        .iter()
        .any(|term| !term.trim().is_empty() && term.chars().nth(1).is_some())
    {
        return Err(ApiError::BadRequest(
            "Search query must contain at least one non-exclusion term with length greater than 1"
                .into(),
        ));
    }
    for (name, value, minimum) in [
        ("timeout_ms", request.timeout_ms, 5),
        ("response_limit", request.response_limit, 0),
        ("file_limit", request.file_limit, 0),
        (
            "minimum_response_file_count",
            request.minimum_response_file_count,
            0,
        ),
        (
            "minimum_peer_upload_speed",
            request.minimum_peer_upload_speed,
            0,
        ),
        (
            "maximum_peer_queue_length",
            request.maximum_peer_queue_length,
            0,
        ),
    ] {
        if value.is_some_and(|value| !(minimum..=i32::MAX as u32).contains(&value)) {
            return Err(ApiError::BadRequest(format!(
                "{name} must be between {minimum} and {}",
                i32::MAX
            )));
        }
    }

    let mut options = soulseek::SearchOptions::default();
    if let Some(value) = request.timeout_ms {
        options.search_timeout = Duration::from_millis(u64::from(value));
    }
    if let Some(value) = request.response_limit {
        options.response_limit = value as usize;
    }
    if let Some(value) = request.file_limit {
        options.file_limit = value as usize;
    }
    if let Some(value) = request.filter_responses {
        options.filter_responses = value;
    }
    if let Some(value) = request.minimum_response_file_count {
        options.minimum_response_file_count = value as usize;
    }
    if let Some(value) = request.minimum_peer_upload_speed {
        options.minimum_peer_upload_speed = value;
    }
    if let Some(value) = request.maximum_peer_queue_length {
        options.maximum_peer_queue_length = u64::from(value);
    }
    Ok((id, query, options))
}

fn count(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

fn map_response(response: soulseek::SearchResponse) -> SearchResponse {
    SearchResponse {
        username: response.username,
        token: response.token,
        has_free_upload_slot: response.has_free_upload_slot,
        upload_speed: response.upload_speed,
        // Soulseek sends queue depth as a 32-bit value on the wire.
        queue_length: u32::try_from(response.queue_length).unwrap_or(u32::MAX),
        files: response.files.into_iter().map(convert_file).collect(),
        locked_files: response
            .locked_files
            .into_iter()
            .map(convert_file)
            .collect(),
    }
}

/// The API state of the client's search state flags.
fn search_state(state: soulseek::SearchState) -> SearchState {
    use soulseek::SearchState as Wire;
    if state.contains(Wire::COMPLETED) {
        [
            (Wire::CANCELLED, SearchState::Cancelled),
            (Wire::ERRORED, SearchState::Errored),
            (
                Wire::RESPONSE_LIMIT_REACHED,
                SearchState::ResponseLimitReached,
            ),
            (Wire::FILE_LIMIT_REACHED, SearchState::FileLimitReached),
            (Wire::TIMED_OUT, SearchState::TimedOut),
        ]
        .into_iter()
        .find(|(flag, _)| state.contains(*flag))
        .map_or(SearchState::Completed, |(_, state)| state)
    } else if state.contains(Wire::IN_PROGRESS) {
        SearchState::InProgress
    } else if state.contains(Wire::QUEUED) {
        SearchState::Queued
    } else {
        SearchState::Requested
    }
}

/// Search ids are UUIDs; anything else names no search.
fn search_id(id: &str) -> ApiResult<Uuid> {
    Uuid::parse_str(id).map_err(|_| ApiError::not_found())
}

pub fn routes() -> Routes {
    Routes::new()
        .get("/api/searches", list)
        .post("/api/searches", create)
        .get("/api/searches/{id}", get_one)
        .delete("/api/searches/{id}", remove)
        .post("/api/searches/{id}/cancel", cancel)
        .get("/api/searches/{id}/responses", responses)
}

async fn list(_: Authenticated, State(state): State<AppState>) -> Json<Vec<Search>> {
    Json(state.services.search.0.list())
}

async fn create(
    _: RequireReadWrite,
    State(state): State<AppState>,
    Json(request): Json<SearchRequest>,
) -> ApiResult<impl IntoResponse> {
    let search = state.services.search.create(request).await?;
    Ok((StatusCode::CREATED, Json(search)))
}

async fn get_one(
    _: Authenticated,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Search>> {
    let id = search_id(&id)?;
    state.services.search.get(&id.to_string()).await.map(Json)
}

async fn responses(
    _: Authenticated,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<impl IntoResponse> {
    let id = search_id(&id)?;
    let json = state
        .services
        .search
        .responses_json(&id.to_string())
        .await?;
    Ok((
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        )],
        json,
    ))
}

async fn cancel(
    _: RequireReadWrite,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    let id = search_id(&id)?;
    state.services.search.cancel(&id.to_string())?;
    Ok(StatusCode::NO_CONTENT)
}

async fn remove(
    _: RequireReadWrite,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    let id = search_id(&id)?;
    state.services.search.remove(id.to_string()).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn request_uses_reference_defaults_and_millisecond_overrides() {
        let (_, _, defaults) = validate(SearchRequest {
            query: "example".into(),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(defaults.search_timeout, Duration::from_millis(15_000));
        assert_eq!(defaults.response_limit, 250);
        assert_eq!(defaults.file_limit, 25_000);
        assert!(defaults.filter_responses);
        assert_eq!(defaults.minimum_response_file_count, 1);
        assert_eq!(defaults.minimum_peer_upload_speed, 0);
        assert_eq!(defaults.maximum_peer_queue_length, i32::MAX as u64);

        let (id, query, overrides) = validate(SearchRequest {
            id: Some("ABCDEFAB-CDEF-ABCD-EFAB-CDEFABCDEFAB".into()),
            query: "example -exclude -exclude".into(),
            timeout_ms: Some(5),
            response_limit: Some(9),
            file_limit: Some(0),
            filter_responses: Some(false),
            minimum_response_file_count: Some(2),
            minimum_peer_upload_speed: Some(3),
            maximum_peer_queue_length: Some(4),
        })
        .unwrap();
        assert_eq!(id.to_string(), "abcdefab-cdef-abcd-efab-cdefabcdefab");
        assert_eq!(query.search_text, "example -exclude");
        assert_eq!(overrides.search_timeout, Duration::from_millis(5));
        assert_eq!(overrides.response_limit, 9);
        assert_eq!(overrides.file_limit, 0);
        assert!(!overrides.filter_responses);
        assert_eq!(overrides.minimum_response_file_count, 2);
        assert_eq!(overrides.minimum_peer_upload_speed, 3);
        assert_eq!(overrides.maximum_peer_queue_length, 4);
    }

    #[test]
    fn request_rejects_empty_queries_and_values_outside_signed_ranges() {
        for text in ["", " \t ", "-words", "i x -words"] {
            assert!(matches!(
                validate(SearchRequest {
                    query: text.into(),
                    ..Default::default()
                }),
                Err(ApiError::BadRequest(_))
            ));
        }
        let valid = SearchRequest {
            query: "example".into(),
            ..Default::default()
        };
        for request in [
            SearchRequest {
                timeout_ms: Some(4),
                ..valid.clone()
            },
            SearchRequest {
                response_limit: Some(i32::MAX as u32 + 1),
                ..valid.clone()
            },
            SearchRequest {
                id: Some("bad".into()),
                ..valid
            },
        ] {
            assert!(matches!(validate(request), Err(ApiError::BadRequest(_))));
        }
    }

    fn sample_search(id: &str, token: u32) -> Search {
        Search {
            id: id.into(),
            query: "example".into(),
            state: SearchState::Requested,
            token,
            started_at: Utc::now(),
            ended_at: None,
            response_count: 0,
            file_count: 0,
            locked_file_count: 0,
        }
    }

    fn memory_service() -> SearchService {
        SearchService(Arc::new(Inner {
            db: Database::open(Path::new(":memory:"), store::MIGRATIONS).unwrap(),
            client: soulseek::SoulseekClient::new(
                Default::default(),
                Arc::new(soulseek::resolvers::DefaultResolvers),
            ),
            bus: EventBus::new(),
            shutdown: CancellationToken::new(),
            create_slot: Semaphore::new(1),
            searches: Mutex::new(HashMap::new()),
        }))
    }

    async fn insert_active(service: &SearchService, search: Search) -> ActiveSearch {
        let active = ActiveSearch {
            id: Arc::from(search.id.as_str()),
            token: search.token,
        };
        let key = active.id.clone();
        let inner = service.0.clone();
        service
            .0
            .db
            .call(move |db| {
                assert!(store::insert(db, &search)?);
                assert!(!store::insert(db, &search)?);
                inner.searches().insert(
                    key,
                    Entry {
                        search,
                        cancel: Some(CancellationToken::new()),
                    },
                );
                Ok(())
            })
            .await
            .unwrap();
        active
    }

    fn one_response(token: u32) -> soulseek::SearchResponse {
        soulseek::SearchResponse {
            token,
            username: "peer".into(),
            files: vec![soulseek::File {
                filename: "music.flac".into(),
                ..Default::default()
            }],
            locked_files: vec![soulseek::File {
                filename: "locked.flac".into(),
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn late_response_and_completion_cannot_resurrect_or_modify_a_reused_id() {
        let service = memory_service();
        let id = Uuid::new_v4().to_string();
        let sample = sample_search(&id, 7);
        let first = insert_active(&service, sample.clone()).await;
        assert!(
            !service
                .append_responses(&first, vec![one_response(first.token)])
                .await
                .unwrap()
                .is_empty()
        );
        service.remove(id.clone()).await.unwrap();
        assert!(service.0.list().is_empty());
        assert!(service.search_text(&id).await.unwrap().is_none());
        service
            .change_state(&first, SearchState::Cancelled)
            .await
            .unwrap();
        assert!(matches!(service.get(&id).await, Err(ApiError::NotFound(_))));

        let second = insert_active(&service, sample).await;
        assert!(
            service
                .append_responses(&first, vec![one_response(first.token)])
                .await
                .unwrap()
                .is_empty()
        );
        service
            .change_state(&first, SearchState::Cancelled)
            .await
            .unwrap();
        assert_eq!(
            service.get(&id).await.unwrap().state,
            SearchState::Requested
        );

        service
            .change_state(&second, SearchState::InProgress)
            .await
            .unwrap();
        assert!(
            !service
                .append_responses(&second, vec![one_response(second.token)])
                .await
                .unwrap()
                .is_empty()
        );
        let terminal = SearchState::ResponseLimitReached;
        service.change_state(&second, terminal).await.unwrap();
        let search = service.get(&id).await.unwrap();
        assert_eq!(search.state, terminal);
        assert!(search.state.is_complete());
        assert!(search.ended_at.is_some());
        assert_eq!(
            (
                search.response_count,
                search.file_count,
                search.locked_file_count
            ),
            (1, 1, 1)
        );
        let responses = service.responses(&id).await;
        assert_eq!(responses.len(), 1);
        assert_eq!(responses[0].files[0].filename, "music.flac");
        assert_eq!(responses[0].locked_files[0].filename, "locked.flac");
        let overview = &service.0.list()[0];
        assert_eq!(overview.state, terminal);
        assert_eq!(overview.response_count, 1);
        service.cancel(&id).unwrap();
    }

    impl SearchService {
        async fn responses(&self, id: &str) -> Vec<SearchResponse> {
            serde_json::from_str(&self.responses_json(id).await.unwrap()).unwrap()
        }
    }

    #[tokio::test]
    async fn a_batch_of_responses_is_stored_in_order_with_its_counts() {
        let service = memory_service();
        let id = Uuid::new_v4().to_string();
        let active = insert_active(&service, sample_search(&id, 9)).await;
        assert_eq!(service.responses_json(&id).await.unwrap(), "[]");
        assert!(matches!(
            service.responses_json(&Uuid::new_v4().to_string()).await,
            Err(ApiError::NotFound(_))
        ));

        let batch = |names: &[&str]| {
            names
                .iter()
                .map(|name| soulseek::SearchResponse {
                    username: (*name).into(),
                    ..one_response(active.token)
                })
                .collect::<Vec<_>>()
        };
        let stored = service
            .append_responses(&active, batch(&["a", "b", "c"]))
            .await
            .unwrap();
        assert_eq!(stored.len(), 3);
        let stored = service
            .append_responses(&active, batch(&["d"]))
            .await
            .unwrap();
        assert_eq!(stored[0].username, "d");

        let search = service.get(&id).await.unwrap();
        assert_eq!(
            (
                search.response_count,
                search.file_count,
                search.locked_file_count
            ),
            (4, 4, 4)
        );
        let overview = &service.0.list()[0];
        assert_eq!(overview.response_count, 4);
        let json = service.responses_json(&id).await.unwrap();
        let responses: Vec<SearchResponse> = serde_json::from_str(&json).unwrap();
        let names: Vec<_> = responses.iter().map(|r| r.username.as_str()).collect();
        assert_eq!(names, ["a", "b", "c", "d"]);
        // The stored JSON is served as is: the same document the types would encode.
        assert_eq!(json, serde_json::to_string(&responses).unwrap());
    }

    #[tokio::test]
    async fn retention_removes_expired_results_but_keeps_active_and_recent_searches() {
        let service = memory_service();
        let expired = insert_active(&service, sample_search(&Uuid::new_v4().to_string(), 1)).await;
        let current = insert_active(&service, sample_search(&Uuid::new_v4().to_string(), 2)).await;
        let active = insert_active(&service, sample_search(&Uuid::new_v4().to_string(), 3)).await;
        assert!(
            !service
                .append_responses(&expired, vec![one_response(expired.token)])
                .await
                .unwrap()
                .is_empty()
        );
        service
            .change_state(&current, SearchState::TimedOut)
            .await
            .unwrap();
        let inner = service.0.clone();
        let id = expired.id.clone();
        service
            .0
            .db
            .call(move |db| {
                let mut searches = inner.searches();
                let entry = searches.get_mut(id.as_ref()).unwrap();
                entry.search.state = SearchState::Completed;
                entry.search.ended_at = Some(Utc::now() - chrono::TimeDelta::minutes(10));
                entry.cancel = None;
                store::update(db, &entry.search)
            })
            .await
            .unwrap();

        service.prune(Duration::from_secs(5 * 60)).await.unwrap();
        assert_eq!(service.0.list().len(), 2);
        assert!(service.get(&current.id).await.unwrap().state.is_complete());
        assert!(!service.get(&active.id).await.unwrap().state.is_complete());
        assert!(matches!(
            service.get(&expired.id).await,
            Err(ApiError::NotFound(_))
        ));
        let response_rows: i64 = service
            .0
            .db
            .call(|db| {
                db.query_row("SELECT count(*) FROM search_responses", [], |row| {
                    row.get(0)
                })
            })
            .await
            .unwrap();
        assert_eq!(response_rows, 0);
    }

    #[tokio::test]
    async fn migrates_legacy_records_and_recovers_an_interrupted_search() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("search.db");
        let db = Database::open(&path, &store::MIGRATIONS[..1]).unwrap();
        let id = Uuid::new_v4().to_string();
        // Records as written before the API used snake_case.
        let record = serde_json::json!({
            "id": id, "searchText": "example", "state": "InProgress", "token": 123,
            "startedAt": "2024-05-01T12:00:00Z", "isComplete": false,
            "responseCount": 2, "fileCount": 2, "lockedFileCount": 2, "responses": null
        })
        .to_string();
        let file = |name: &str| {
            serde_json::json!({
                "code": 1, "filename": name, "size": 5, "extension": "flac",
                "attributeCount": 1, "attributes": [{"type": "Length", "value": 200}],
                "length": 200, "isVariableBitRate": false
            })
        };
        let legacy = serde_json::json!([
            {"username": "peer", "token": 123, "hasFreeUploadSlot": true, "uploadSpeed": 9,
             "queueLength": 3, "fileCount": 1, "files": [file("music.flac")],
             "lockedFileCount": 1, "lockedFiles": [file("locked.flac")]},
            {"username": "other", "token": 123, "hasFreeUploadSlot": false, "uploadSpeed": 0,
             "queueLength": 0, "fileCount": 0, "files": [], "lockedFileCount": 0, "lockedFiles": []}
        ])
        .to_string();
        let row_id = id.clone();
        db.call(move |db| {
            db.execute(
                "INSERT INTO searches VALUES (?1, ?2, ?3)",
                rusqlite::params![row_id, record, legacy],
            )
        })
        .await
        .unwrap();
        drop(db);

        let db = Database::open(&path, store::MIGRATIONS).unwrap();
        let loaded = db.call(store::load).await.unwrap();
        assert_eq!(loaded[0].state, SearchState::TimedOut);
        assert_eq!(loaded[0].query, "example");
        assert_eq!(loaded[0].response_count, 2);
        assert!(loaded[0].ended_at.is_some());
        let text_id = id.clone();
        assert_eq!(
            db.call(move |db| store::search_text(db, &text_id))
                .await
                .unwrap()
                .as_deref(),
            Some("example")
        );
        let json = db
            .call(move |db| store::responses_json(db, &id))
            .await
            .unwrap()
            .unwrap();
        let responses: Vec<SearchResponse> = serde_json::from_str(&json).unwrap();
        assert_eq!(responses[0].username, "peer");
        assert!(responses[0].has_free_upload_slot);
        assert_eq!(responses[0].queue_length, 3);
        assert_eq!(responses[0].files[0].duration_ms, Some(200_000));
        assert_eq!(responses[0].files[0].is_variable_bit_rate, Some(false));
        assert_eq!(responses[0].locked_files[0].filename, "locked.flac");
        assert_eq!(responses[1].username, "other");
        assert!(!responses[1].has_free_upload_slot);
        assert!(responses[1].files.is_empty());
    }
}
