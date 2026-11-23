//! Search lifecycle, persistence, HTTP API and search-hub events.

mod store;

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};

use axum::{
    Json, Router,
    extract::{Path, Query, State, rejection::JsonRejection},
    http::StatusCode,
    routing::get,
};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::json;
use slskr_api::search::{self as dto, Search, SearchRequest, SearchStates};
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
    http::{ApiError, ApiResult},
    signalr::Hub,
    users::convert_file,
};

const PROGRESS_INTERVAL: Duration = Duration::from_millis(250);
const RETENTION_INTERVAL: Duration = Duration::from_secs(5 * 60);

#[derive(Clone)]
pub struct SearchService(Arc<Inner>);

struct Inner {
    db: Database,
    client: soulseek::SoulseekClient,
    hub: Hub,
    shutdown: CancellationToken,
    create_slot: Semaphore,
    // Mutations and their broadcasts are ordered by Database::call. The hub's
    // synchronous initial snapshot never queries SQLite or waits for disk I/O.
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

    fn broadcast(&self, method: &str, payload: impl Serialize) {
        if self.hub.connection_count() > 0 {
            match serde_json::to_value(payload) {
                Ok(payload) => self.hub.broadcast(method, vec![payload]),
                Err(error) => tracing::error!(%error, %method, "could not encode search hub event"),
            }
        }
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
            hub: core.hubs.search.clone(),
            shutdown: core.shutdown.clone(),
            create_slot: Semaphore::new(1),
            searches: Mutex::new(searches),
        }));
        if let Some(age) = core.options.get().retention.searches {
            service.prune(age).await?;
        }
        let inner = Arc::downgrade(&service.0);
        service.0.hub.on_connected(move || {
            let Some(inner) = inner.upgrade() else {
                return Vec::new();
            };
            // Upstream lists searches with empty (not null) responses.
            let list: Vec<_> = inner.list().into_iter().map(without_responses).collect();
            vec![(dto::hub_methods::LIST.into(), vec![json!(list)])]
        });
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
                    inner.broadcast_delete(entry);
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
                "The server connection must be connected and logged in to perform a search (currently: {state})"
            )));
        }

        let search = Search {
            started_at: Utc::now(),
            ended_at: None,
            file_count: 0,
            id: id.to_string(),
            is_complete: false,
            locked_file_count: 0,
            response_count: 0,
            responses: None,
            search_text: query.search_text.clone(),
            state: SearchStates::REQUESTED,
            token: self.0.client.get_next_token(),
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
                service.0.broadcast(dto::hub_methods::CREATE, &search);
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
                let _ = state_tx.send(SearchStates::from_bits(search.state.bits()));
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
            let mut dirty = false;
            loop {
                let result = tokio::select! {
                    biased;
                    Some(state) = states.recv() => self.change_state(&active, state).await,
                    _ = progress.tick(), if dirty => {
                        dirty = false;
                        self.broadcast_progress(&active).await
                    }
                    response = responses.recv() => {
                        let Some(response) = response else { break };
                        self.append_response(&active, response).await.map(|stored| dirty |= stored)
                    }
                };
                if let Err(error) = result {
                    tracing::warn!(%error, search_id = %active.id, "could not persist a search update");
                }
            }
        };
        let (result, ()) = tokio::join!(search, collect);
        let state = match result {
            Ok(search) => SearchStates::from_bits(search.state.bits()),
            Err(soulseek::Error::Cancelled) => SearchStates::COMPLETED | SearchStates::CANCELLED,
            Err(error) => {
                tracing::debug!(%error, search_id = %active.id, "search failed");
                SearchStates::COMPLETED | SearchStates::ERRORED
            }
        };
        if let Err(error) = self.change_state(&active, state).await {
            tracing::warn!(%error, search_id = %active.id, "could not finalize search");
        }
    }

    async fn change_state(&self, active: &ActiveSearch, state: SearchStates) -> anyhow::Result<()> {
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
                search.set_state(state);
                if search.is_complete {
                    search.ended_at = Some(Utc::now());
                }
                store::update(db, &search)?;
                {
                    let mut searches = inner.searches();
                    let entry = searches
                        .get_mut(active.id.as_ref())
                        .expect("search mutations are serialized by the database");
                    if search.is_complete {
                        entry.cancel = None;
                    }
                    entry.search = search.clone();
                }
                inner.broadcast(dto::hub_methods::UPDATE, search);
                Ok(())
            })
            .await
    }

    async fn append_response(
        &self,
        active: &ActiveSearch,
        response: soulseek::SearchResponse,
    ) -> anyhow::Result<bool> {
        let inner = self.0.clone();
        let active = active.clone();
        self.0
            .db
            .call(move |db| {
                let Some(counts) = active.entry(&inner.searches()).map(|entry| {
                    [
                        entry.search.response_count.saturating_add(1),
                        entry
                            .search
                            .file_count
                            .saturating_add(count(response.file_count())),
                        entry
                            .search
                            .locked_file_count
                            .saturating_add(count(response.locked_file_count())),
                    ]
                }) else {
                    return Ok(false);
                };

                let response = map_response(response);
                store::append(db, &active.id, &response, counts)?;
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
                if inner.hub.connection_count() > 0 {
                    inner.broadcast(
                        dto::hub_methods::RESPONSE,
                        dto::SearchResponseEvent {
                            search_id: active.id.to_string(),
                            response,
                        },
                    );
                }
                Ok(true)
            })
            .await
    }

    async fn broadcast_progress(&self, active: &ActiveSearch) -> anyhow::Result<()> {
        if self.0.hub.connection_count() == 0 {
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
                    inner.broadcast(dto::hub_methods::UPDATE, search);
                }
                Ok(())
            })
            .await
    }

    pub(crate) async fn search_text(&self, id: &str) -> anyhow::Result<Option<String>> {
        let id = id.to_owned();
        self.0.db.call(move |db| store::search_text(db, &id)).await
    }

    async fn get(&self, id: &str, include_responses: bool) -> ApiResult<Search> {
        let id = id.to_owned();
        self.0
            .db
            .call(move |db| store::get(db, &id, include_responses))
            .await?
            .ok_or_else(not_found)
    }

    fn cancel(&self, id: &str) -> ApiResult<()> {
        let searches = self.0.searches();
        let entry = searches.get(id).ok_or_else(not_found)?;
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
                inner.broadcast_delete(entry);
                Ok(true)
            })
            .await?;
        if removed { Ok(()) } else { Err(not_found()) }
    }
}

impl Inner {
    fn broadcast_delete(&self, Entry { search, cancel }: Entry) {
        if let Some(cancel) = cancel {
            cancel.cancel();
        }
        self.broadcast(dto::hub_methods::DELETE, search);
    }
}

fn not_found() -> ApiError {
    ApiError::NotFound(String::new())
}

fn validate(
    request: SearchRequest,
) -> ApiResult<(Uuid, soulseek::SearchQuery, soulseek::SearchOptions)> {
    let id = match request.id {
        Some(id) => Uuid::parse_str(&id)
            .map_err(|_| ApiError::BadRequest("Search ID must be a UUID".into()))?,
        None => Uuid::new_v4(),
    };
    if request.search_text.trim().is_empty() {
        return Err(ApiError::BadRequest(
            "Search text must not be empty or consist only of whitespace".into(),
        ));
    }
    let query = soulseek::SearchQuery::from(request.search_text.as_str());
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
        ("SearchTimeout", request.search_timeout, 5),
        ("ResponseLimit", request.response_limit, 0),
        ("FileLimit", request.file_limit, 0),
        (
            "MinimumResponseFileCount",
            request.minimum_response_file_count,
            0,
        ),
        (
            "MinimumPeerUploadSpeed",
            request.minimum_peer_upload_speed,
            0,
        ),
        (
            "MaximumPeerQueueLength",
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
    if let Some(value) = request.search_timeout {
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

fn map_response(response: soulseek::SearchResponse) -> dto::SoulseekSearchResponse {
    dto::SoulseekSearchResponse {
        file_count: count(response.files.len()),
        files: response.files.into_iter().map(convert_file).collect(),
        has_free_upload_slot: response.has_free_upload_slot,
        locked_file_count: count(response.locked_files.len()),
        locked_files: response
            .locked_files
            .into_iter()
            .map(convert_file)
            .collect(),
        // Soulseek sends queue depth as a 32-bit value on the wire.
        queue_length: u32::try_from(response.queue_length).unwrap_or(u32::MAX),
        token: response.token,
        upload_speed: response.upload_speed,
        username: response.username,
    }
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/searches", get(list).post(create))
        .route("/searches/{id}", get(get_one).put(cancel).delete(remove))
        .route("/searches/{id}/responses", get(responses))
}

/// HTTP payloads carry `responses: []` when responses were not loaded, matching
/// upstream's `WithoutResponses()` projection.
fn without_responses(mut search: Search) -> Search {
    search.responses.get_or_insert_with(Vec::new);
    search
}

async fn list(_: Authenticated, State(state): State<AppState>) -> ApiResult<Json<Vec<Search>>> {
    Ok(Json(
        state
            .services
            .search
            .0
            .list()
            .into_iter()
            .map(without_responses)
            .collect(),
    ))
}

async fn create(
    _: RequireReadWrite,
    State(state): State<AppState>,
    body: Result<Json<SearchRequest>, JsonRejection>,
) -> ApiResult<Json<Search>> {
    let service = &state.services.search;
    let Json(body) = body.map_err(|error| {
        let status = error.status();
        let status = if status == StatusCode::UNPROCESSABLE_ENTITY {
            StatusCode::BAD_REQUEST
        } else {
            status
        };
        ApiError::Status(status, error.body_text())
    })?;
    Ok(Json(without_responses(service.create(body).await?)))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Include {
    #[serde(default)]
    include_responses: bool,
}

async fn get_one(
    _: Authenticated,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Query(query): Query<Include>,
) -> ApiResult<Json<Search>> {
    let search = state
        .services
        .search
        .get(&id.to_string(), query.include_responses)
        .await?;
    Ok(Json(without_responses(search)))
}

async fn responses(
    _: Authenticated,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Vec<dto::SearchResponse>>> {
    Ok(Json(
        state
            .services
            .search
            .get(&id.to_string(), true)
            .await?
            .responses
            .unwrap_or_default(),
    ))
}

async fn cancel(
    _: RequireReadWrite,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    state.services.search.cancel(&id.to_string())?;
    Ok(StatusCode::OK)
}

async fn remove(
    _: RequireReadWrite,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<StatusCode> {
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
            search_text: "example".into(),
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
            search_text: "example -exclude -exclude".into(),
            search_timeout: Some(5),
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
                    search_text: text.into(),
                    ..Default::default()
                }),
                Err(ApiError::BadRequest(_))
            ));
        }
        let valid = SearchRequest {
            search_text: "example".into(),
            ..Default::default()
        };
        for request in [
            SearchRequest {
                search_timeout: Some(4),
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
            token,
            search_text: "example".into(),
            state: SearchStates::REQUESTED,
            started_at: Utc::now(),
            ended_at: None,
            is_complete: false,
            response_count: 0,
            file_count: 0,
            locked_file_count: 0,
            responses: None,
        }
    }

    fn memory_service() -> SearchService {
        SearchService(Arc::new(Inner {
            db: Database::open(Path::new(":memory:"), store::MIGRATIONS).unwrap(),
            client: soulseek::SoulseekClient::new(
                Default::default(),
                Arc::new(soulseek::resolvers::DefaultResolvers),
            ),
            hub: Hub::new("search-test"),
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
            service
                .append_response(&first, one_response(first.token))
                .await
                .unwrap()
        );
        service.remove(id.clone()).await.unwrap();
        assert!(service.0.list().is_empty());
        assert!(service.search_text(&id).await.unwrap().is_none());
        service
            .change_state(&first, SearchStates::COMPLETED | SearchStates::CANCELLED)
            .await
            .unwrap();
        assert!(matches!(
            service.get(&id, false).await,
            Err(ApiError::NotFound(_))
        ));

        let second = insert_active(&service, sample).await;
        assert!(
            !service
                .append_response(&first, one_response(first.token))
                .await
                .unwrap()
        );
        service
            .change_state(&first, SearchStates::COMPLETED | SearchStates::CANCELLED)
            .await
            .unwrap();
        assert_eq!(
            service.get(&id, false).await.unwrap().state,
            SearchStates::REQUESTED
        );

        service
            .change_state(&second, SearchStates::IN_PROGRESS)
            .await
            .unwrap();
        assert!(
            service
                .append_response(&second, one_response(second.token))
                .await
                .unwrap()
        );
        let terminal = SearchStates::COMPLETED | SearchStates::RESPONSE_LIMIT_REACHED;
        service.change_state(&second, terminal).await.unwrap();
        let search = service.get(&id, true).await.unwrap();
        assert_eq!(search.state, terminal);
        assert!(search.is_complete);
        assert!(search.ended_at.is_some());
        assert_eq!(
            (
                search.response_count,
                search.file_count,
                search.locked_file_count
            ),
            (1, 1, 1)
        );
        let responses = search.responses.unwrap();
        assert_eq!(responses.len(), 1);
        assert_eq!(responses[0].files[0].filename, "music.flac");
        assert_eq!(responses[0].locked_files[0].filename, "locked.flac");
        let overview = &service.0.list()[0];
        assert_eq!(overview.state, terminal);
        assert_eq!(overview.response_count, 1);
        assert!(overview.responses.is_none());
        service.cancel(&id).unwrap();
    }

    #[tokio::test]
    async fn retention_removes_expired_results_but_keeps_active_and_recent_searches() {
        let service = memory_service();
        let expired = insert_active(&service, sample_search(&Uuid::new_v4().to_string(), 1)).await;
        let current = insert_active(&service, sample_search(&Uuid::new_v4().to_string(), 2)).await;
        let active = insert_active(&service, sample_search(&Uuid::new_v4().to_string(), 3)).await;
        assert!(
            service
                .append_response(&expired, one_response(expired.token))
                .await
                .unwrap()
        );
        service
            .change_state(&current, SearchStates::COMPLETED | SearchStates::TIMED_OUT)
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
                entry.search.set_state(SearchStates::COMPLETED);
                entry.search.ended_at = Some(Utc::now() - chrono::TimeDelta::minutes(10));
                entry.cancel = None;
                store::update(db, &entry.search)
            })
            .await
            .unwrap();

        service.prune(Duration::from_secs(5 * 60)).await.unwrap();
        assert_eq!(service.0.list().len(), 2);
        assert!(service.get(&current.id, false).await.unwrap().is_complete);
        assert!(!service.get(&active.id, false).await.unwrap().is_complete);
        assert!(matches!(
            service.get(&expired.id, false).await,
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
    async fn migrates_response_order_and_recovers_an_interrupted_search() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("search.db");
        let db = Database::open(&path, &store::MIGRATIONS[..1]).unwrap();
        let id = Uuid::new_v4().to_string();
        let mut search = sample_search(&id, 123);
        search.response_count = 2;
        search.file_count = 2;
        search.locked_file_count = 2;
        let first = map_response(one_response(search.token));
        let second = dto::SoulseekSearchResponse {
            username: "other".into(),
            ..first.clone()
        };
        let legacy = serde_json::to_string(&[first, second]).unwrap();
        let record = serde_json::to_string(&search).unwrap();
        db.call(move |db| {
            db.execute(
                "INSERT INTO searches VALUES (?1, ?2, ?3)",
                rusqlite::params![search.id, record, legacy],
            )
        })
        .await
        .unwrap();
        drop(db);

        let db = Database::open(&path, store::MIGRATIONS).unwrap();
        let loaded = db.call(store::load).await.unwrap();
        assert_eq!(
            loaded[0].state,
            SearchStates::COMPLETED | SearchStates::TIMED_OUT
        );
        assert_eq!(loaded[0].response_count, 2);
        assert!(loaded[0].ended_at.is_some());
        assert!(loaded[0].responses.is_none());
        let search = db
            .call(move |db| store::get(db, &id, true))
            .await
            .unwrap()
            .unwrap();
        let responses = search.responses.unwrap();
        assert_eq!(responses[0].username, "peer");
        assert_eq!(responses[1].username, "other");
    }
}
