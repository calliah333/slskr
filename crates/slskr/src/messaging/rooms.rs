//! Joined chat rooms, session membership and the `/rooms` HTTP API.

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::{Arc, OnceLock, PoisonError, RwLock},
};

use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use chrono::Utc;
use slskr_types::{
    events::EventData,
    realtime::{Event as LiveEvent, Topic},
    rooms::{AddMember, JoinRoom, Room, RoomInfo, RoomMessage, RoomUser, Ticker},
    users::UserPresence,
};
use soulseek::{Event as ClientEvent, RoomData, RoomList, RoomTicker, SoulseekClient};
use tokio::{
    sync::{Mutex, broadcast::error::RecvError, mpsc, oneshot},
    task::JoinSet,
};

use crate::{
    app::{AppState, Core},
    auth::{Authenticated, RequireReadWrite},
    http::{ApiError, ApiResult, Json, Path, Routes},
    realtime::EventBus,
};

const MESSAGE_LIMIT: usize = 25;

type Reply<T> = oneshot::Sender<ApiResult<T>>;

#[derive(Clone)]
pub struct RoomService(Arc<Inner>);

struct Inner {
    client: SoulseekClient,
    /// Changed only together with publishing to the `rooms` topic, under the write lock,
    /// so a snapshot taken under the read lock is exact.
    rooms: RwLock<BTreeMap<String, JoinedRoom>>,
    bus: EventBus,
    commands: OnceLock<mpsc::Sender<Command>>,
    list_request: Mutex<()>,
}

impl RoomService {
    pub fn new(core: &Core) -> anyhow::Result<Self> {
        Ok(Self(Arc::new(Inner {
            client: core.client.clone(),
            rooms: RwLock::default(),
            bus: core.bus.clone(),
            commands: OnceLock::new(),
            list_request: Mutex::default(),
        })))
    }

    /// Every joined room for the `rooms` topic snapshot, with the bus sequence number it
    /// includes.
    pub(crate) fn snapshot(&self) -> (u64, Vec<Room>) {
        let rooms = self.0.rooms.read().unwrap_or_else(PoisonError::into_inner);
        let seq = self.0.bus.seq();
        if !self.0.client.state().is_logged_in() {
            return (seq, Vec::new());
        }
        let username = self.0.client.username();
        let rooms = rooms
            .values()
            .map(|room| room.response(username.as_deref()))
            .collect();
        (seq, rooms)
    }

    pub fn start(&self, app: AppState) {
        let (sender, commands) = mpsc::channel(32);
        if self.0.commands.set(sender).is_err() {
            return;
        }
        let events = self.0.client.subscribe();
        let session = Session {
            app,
            service: self.clone(),
            last_username: None,
            logged_in: false,
            rejoin: BTreeSet::new(),
            pending: BTreeMap::new(),
            operations: JoinSet::new(),
        };
        tokio::spawn(session.run(events, commands));
    }

    fn names(&self) -> Vec<String> {
        if !self.0.client.state().is_logged_in() {
            return Vec::new();
        }
        self.0
            .rooms
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .keys()
            .cloned()
            .collect()
    }

    fn with_room<T>(&self, name: &str, read: impl FnOnce(&JoinedRoom) -> T) -> ApiResult<T> {
        if self.0.client.state().is_logged_in() {
            let rooms = self.0.rooms.read().unwrap_or_else(PoisonError::into_inner);
            if let Some(room) = rooms.get(name) {
                return Ok(read(room));
            }
        }
        Err(ApiError::NotFound(String::new()))
    }

    async fn available(&self) -> ApiResult<Vec<RoomInfo>> {
        let _guard = self.0.list_request.lock().await;
        Ok(available_rooms(self.0.client.get_room_list().await?))
    }

    async fn request<T>(&self, command: impl FnOnce(Reply<T>) -> Command) -> ApiResult<T> {
        let sender = self.0.commands.get().ok_or_else(unavailable)?;
        let (reply, response) = oneshot::channel();
        sender
            .send(command(reply))
            .await
            .map_err(|_| unavailable())?;
        response.await.map_err(|_| unavailable())?
    }
}

fn unavailable() -> ApiError {
    ApiError::Status(
        StatusCode::SERVICE_UNAVAILABLE,
        "Room service unavailable".into(),
    )
}

struct JoinedRoom {
    data: RoomData,
    messages: VecDeque<RoomMessage>,
    tickers: BTreeMap<String, String>,
}

impl JoinedRoom {
    fn new(data: RoomData) -> Self {
        Self {
            data,
            messages: VecDeque::new(),
            tickers: BTreeMap::new(),
        }
    }

    fn users(&self, username: Option<&str>) -> Vec<RoomUser> {
        self.data
            .users
            .iter()
            .map(|user| user_response(user, username))
            .collect()
    }

    fn response(&self, username: Option<&str>) -> Room {
        Room {
            name: self.data.name.clone(),
            is_private: self.data.is_private,
            operators: if self.data.is_private {
                self.data.operators.clone()
            } else {
                Vec::new()
            },
            owner: self.data.owner.clone(),
            users: self.users(username),
            messages: self.messages.iter().cloned().collect(),
        }
    }

    fn can_add_member(&self, username: &str) -> bool {
        self.data.is_private
            && (self.data.owner.as_deref() == Some(username)
                || self
                    .data
                    .operators
                    .iter()
                    .any(|operator| operator == username))
    }
}

fn user_response(user: &soulseek::UserData, username: Option<&str>) -> RoomUser {
    RoomUser {
        username: user.username.clone(),
        is_self: username == Some(user.username.as_str()),
        average_speed: user.average_speed,
        country_code: user.country_code.clone(),
        directory_count: user.directory_count,
        file_count: user.file_count,
        slots_free: user.slots_free,
        upload_count: user.upload_count,
        status: match user.status {
            soulseek::UserPresence::Offline => UserPresence::Offline,
            soulseek::UserPresence::Away => UserPresence::Away,
            soulseek::UserPresence::Online => UserPresence::Online,
        },
    }
}

fn available_rooms(list: RoomList) -> Vec<RoomInfo> {
    let moderated: BTreeSet<_> = list
        .moderated_rooms
        .iter()
        .map(|room| room.name.as_str())
        .collect();
    let mut rooms = Vec::with_capacity(list.public.len() + list.private.len() + list.owned.len());
    for (entries, is_private, is_owned) in [
        (list.public, false, false),
        (list.private, true, false),
        (list.owned, true, true),
    ] {
        rooms.extend(entries.into_iter().map(|room| RoomInfo {
            is_moderated: moderated.contains(room.name.as_str()),
            name: room.name,
            user_count: room.user_count,
            is_private,
            is_owned,
        }));
    }
    rooms
}

enum Command {
    Join(String, Reply<Option<Room>>),
    Leave(String, Reply<bool>),
}

enum Action {
    Join(Option<Reply<Option<Room>>>),
    Leave(Reply<bool>),
}

impl Action {
    fn fail(self, error: ApiError) {
        match self {
            Self::Join(Some(reply)) => {
                let _ = reply.send(Err(error));
            }
            Self::Join(None) => {}
            Self::Leave(reply) => {
                let _ = reply.send(Err(error));
            }
        }
    }
}

enum Outcome {
    Joined(RoomData),
    Left,
}

struct Pending {
    action: Action,
    observed: bool,
    outcome: Option<Outcome>,
    // Soulseek emits our RoomJoined notification before delivering RoomData to
    // the join waiter. Preserve the subsequent wire-ordered updates if the
    // event loop reads them before the waiter has returned the full snapshot.
    updates: Vec<Update>,
}

enum Update {
    UserJoined(soulseek::UserData),
    UserLeft(String),
    Message(RoomMessage),
    TickerAdded(RoomTicker),
    TickerList(Vec<RoomTicker>),
    TickerRemoved(String),
    ModerationChanged { username: String, is_operator: bool },
}

impl Update {
    /// The `rooms` topic event of this update, if it has one.
    fn live_event(&self, room: &str, username: Option<&str>) -> Option<LiveEvent> {
        match self {
            Self::UserJoined(user) => Some(LiveEvent::RoomUserJoined {
                room: room.into(),
                user: user_response(user, username),
            }),
            Self::UserLeft(name) => Some(LiveEvent::RoomUserLeft {
                room: room.into(),
                username: name.clone(),
            }),
            Self::Message(message) => Some(LiveEvent::RoomMessage(message.clone())),
            _ => None,
        }
    }

    fn apply(self, room: &mut JoinedRoom) -> Option<RoomMessage> {
        match self {
            Self::UserJoined(user) => {
                if let Some(existing) = room
                    .data
                    .users
                    .iter_mut()
                    .find(|existing| existing.username == user.username)
                {
                    *existing = user;
                } else {
                    room.data.users.push(user);
                }
            }
            Self::UserLeft(username) => {
                room.data.users.retain(|user| user.username != username);
            }
            Self::Message(message) => {
                let event_message = message.clone();
                if room.messages.len() == MESSAGE_LIMIT {
                    room.messages.pop_front();
                }
                room.messages.push_back(message);
                return Some(event_message);
            }
            Self::TickerAdded(ticker) => {
                room.tickers.insert(ticker.username, ticker.message);
            }
            Self::TickerList(tickers) => {
                room.tickers = tickers
                    .into_iter()
                    .map(|ticker| (ticker.username, ticker.message))
                    .collect();
            }
            Self::TickerRemoved(username) => {
                room.tickers.remove(&username);
            }
            Self::ModerationChanged {
                username,
                is_operator,
            } => {
                if is_operator {
                    if !room.data.operators.contains(&username) {
                        room.data.operators.push(username);
                    }
                } else {
                    room.data.operators.retain(|operator| operator != &username);
                }
            }
        }
        None
    }
}

struct Session {
    app: AppState,
    service: RoomService,
    last_username: Option<String>,
    logged_in: bool,
    rejoin: BTreeSet<String>,
    pending: BTreeMap<String, Pending>,
    operations: JoinSet<(String, soulseek::Result<Outcome>)>,
}

impl Session {
    async fn run(
        mut self,
        mut events: tokio::sync::broadcast::Receiver<ClientEvent>,
        mut commands: mpsc::Receiver<Command>,
    ) {
        self.logged_in();
        loop {
            tokio::select! {
                biased;
                _ = self.app.shutdown.cancelled() => break,
                event = events.recv() => match event {
                    Ok(event) => self.event(event),
                    Err(RecvError::Lagged(skipped)) => self.lagged(skipped),
                    Err(RecvError::Closed) => break,
                },
                Some(operation) = self.operations.join_next(), if !self.operations.is_empty() => match operation {
                    Ok((name, result)) => self.completed(name, result),
                    Err(error) => tracing::error!(%error, "room membership task failed"),
                },
                Some(command) = commands.recv() => self.command(command),
            }
        }
        self.disconnected();
    }

    fn command(&mut self, command: Command) {
        match command {
            Command::Join(name, reply) => {
                if self.service.with_room(&name, |_| ()).is_ok() {
                    let _ = reply.send(Ok(None));
                } else if !self.logged_in {
                    Action::Join(Some(reply))
                        .fail(ApiError::Conflict("Not logged in to Soulseek".into()));
                } else {
                    self.begin(name, Action::Join(Some(reply)));
                }
            }
            Command::Leave(name, reply) => {
                if self.service.with_room(&name, |_| ()).is_err() {
                    let _ = reply.send(Ok(false));
                } else {
                    self.begin(name, Action::Leave(reply));
                }
            }
        }
    }

    fn begin(&mut self, name: String, action: Action) {
        if let Some(in_flight) = self.pending.get_mut(&name) {
            match (&mut in_flight.action, action) {
                (Action::Join(waiter @ None), Action::Join(Some(reply))) => *waiter = Some(reply),
                (_, action) => action.fail(ApiError::Conflict(
                    "A room membership request is already pending".into(),
                )),
            }
            return;
        }

        let joining = matches!(&action, Action::Join(_));
        let client = self.service.0.client.clone();
        let room = name.clone();
        self.operations.spawn(async move {
            let result = if joining {
                client.join_room(&room, false).await.map(Outcome::Joined)
            } else {
                client.leave_room(&room).await.map(|()| Outcome::Left)
            };
            (room, result)
        });
        self.pending.insert(
            name,
            Pending {
                action,
                outcome: None,
                observed: false,
                updates: Vec::new(),
            },
        );
    }

    fn completed(&mut self, name: String, result: soulseek::Result<Outcome>) {
        let Some(pending) = self.pending.get_mut(&name) else {
            return;
        };
        match result {
            Ok(outcome) => pending.outcome = Some(outcome),
            Err(error) => {
                tracing::warn!(room = %name, %error, "room membership request failed");
                let error = match error {
                    soulseek::Error::RoomJoinForbidden(_) => ApiError::Forbidden(format!(
                        "The server rejected your request to join {name}"
                    )),
                    other => other.into(),
                };
                self.pending.remove(&name).unwrap().action.fail(error);
                return;
            }
        }
        self.finish(name);
    }

    fn finish(&mut self, name: String) {
        if !self
            .pending
            .get(&name)
            .is_some_and(|pending| pending.observed && pending.outcome.is_some())
        {
            return;
        }
        let Pending {
            action,
            outcome,
            updates,
            ..
        } = self.pending.remove(&name).unwrap();
        match (action, outcome.unwrap()) {
            (Action::Join(reply), Outcome::Joined(data)) => {
                let mut room = JoinedRoom::new(data);
                let received: Vec<_> = updates
                    .into_iter()
                    .filter_map(|update| update.apply(&mut room))
                    .collect();
                let response = reply
                    .as_ref()
                    .map(|_| room.response(self.last_username.as_deref()));
                self.rejoin.insert(name.clone());
                {
                    let bus = &self.service.0.bus;
                    let mut rooms = self
                        .service
                        .0
                        .rooms
                        .write()
                        .unwrap_or_else(PoisonError::into_inner);
                    bus.publish_with(Topic::Rooms, || {
                        LiveEvent::RoomJoined(room.response(self.last_username.as_deref()))
                    });
                    rooms.insert(name, room);
                }
                self.publish_rooms();
                for message in received {
                    self.raise_message(message);
                }
                if let Some(reply) = reply {
                    let _ = reply.send(Ok(response));
                }
            }
            (Action::Leave(reply), Outcome::Left) => {
                self.remove_joined(&name);
                let _ = reply.send(Ok(true));
            }
            _ => unreachable!("room membership outcomes match their request"),
        }
    }

    fn event(&mut self, event: ClientEvent) {
        match event {
            ClientEvent::LoggedIn => self.logged_in(),
            ClientEvent::Disconnected { .. } => self.disconnected(),
            ClientEvent::RoomJoined {
                room,
                username,
                user_data,
            } => {
                if self.is_self(&username) {
                    if let Some(pending) = self.pending.get_mut(&room)
                        && matches!(pending.action, Action::Join(_))
                    {
                        pending.observed = true;
                        self.finish(room);
                    }
                } else if let Some(user) = user_data {
                    self.update(&room, Update::UserJoined(user));
                }
            }
            ClientEvent::RoomLeft { room, username } => {
                if self.is_self(&username) {
                    self.remove_joined(&room);
                    if let Some(pending) = self.pending.get_mut(&room)
                        && matches!(pending.action, Action::Leave(_))
                    {
                        pending.observed = true;
                        self.finish(room);
                    }
                } else {
                    self.update(&room, Update::UserLeft(username));
                }
            }
            ClientEvent::RoomMessageReceived(message) => {
                if !self
                    .app
                    .services
                    .users
                    .is_blacklisted(&message.username, None)
                {
                    let timestamp = Utc::now();
                    let is_self = self.is_self(&message.username);
                    let room = message.room;
                    self.update(
                        &room,
                        Update::Message(RoomMessage {
                            timestamp,
                            is_self,
                            username: message.username,
                            message: message.message,
                            room: room.clone(),
                        }),
                    );
                }
            }
            ClientEvent::RoomTickerListReceived { room, tickers } => {
                self.update(&room, Update::TickerList(tickers))
            }
            ClientEvent::RoomTickerAdded { room, ticker } => {
                self.update(&room, Update::TickerAdded(ticker))
            }
            ClientEvent::RoomTickerRemoved { room, username } => {
                self.update(&room, Update::TickerRemoved(username))
            }
            ClientEvent::PrivateRoomModerationAdded(room) => self.update_moderation(&room, true),
            ClientEvent::PrivateRoomModerationRemoved(room) => self.update_moderation(&room, false),
            _ => {}
        }
    }

    fn update_moderation(&mut self, room: &str, is_operator: bool) {
        if self.logged_in
            && let Some(username) = self.last_username.clone()
        {
            self.update(
                room,
                Update::ModerationChanged {
                    username,
                    is_operator,
                },
            );
        }
    }

    fn update(&mut self, name: &str, update: Update) {
        if let Some(pending) = self.pending.get_mut(name)
            && pending.observed
            && matches!(pending.action, Action::Join(_))
        {
            pending.updates.push(update);
            return;
        }
        let message = {
            let bus = &self.service.0.bus;
            let mut rooms = self
                .service
                .0
                .rooms
                .write()
                .unwrap_or_else(PoisonError::into_inner);
            rooms.get_mut(name).and_then(|room| {
                let live = bus
                    .wants(Topic::Rooms)
                    .then(|| update.live_event(name, self.last_username.as_deref()))
                    .flatten();
                let message = update.apply(room);
                if let Some(live) = live {
                    bus.publish(&live);
                }
                message
            })
        };
        if let Some(message) = message {
            self.raise_message(message);
        }
    }

    fn raise_message(&self, message: RoomMessage) {
        self.app
            .services
            .events
            .raise(EventData::RoomMessageReceived(message));
    }

    fn is_self(&self, username: &str) -> bool {
        self.logged_in && self.last_username.as_deref() == Some(username)
    }

    fn logged_in(&mut self) {
        if self.logged_in || !self.service.0.client.state().is_logged_in() {
            return;
        }
        let Some(username) = self.service.0.client.username() else {
            return;
        };
        if self.last_username.as_ref() != Some(&username) {
            self.rejoin.clear();
            self.last_username = Some(username);
        }
        self.logged_in = true;
        let options = self.app.options.get();
        let rooms: BTreeSet<_> = options
            .soulseek
            .rooms
            .iter()
            .chain(&self.rejoin)
            .cloned()
            .collect();
        for name in rooms {
            self.begin(name, Action::Join(None));
        }
    }

    fn disconnected(&mut self) {
        self.logged_in = false;
        self.operations = JoinSet::new();
        for (_, pending) in std::mem::take(&mut self.pending) {
            pending.action.fail(unavailable());
        }
        {
            let mut rooms = self
                .service
                .0
                .rooms
                .write()
                .unwrap_or_else(PoisonError::into_inner);
            for room in std::mem::take(&mut *rooms).into_keys() {
                self.service
                    .0
                    .bus
                    .publish_with(Topic::Rooms, || LiveEvent::RoomLeft { room });
            }
        }
        self.publish_rooms();
    }

    fn lagged(&mut self, skipped: u64) {
        tracing::warn!(skipped, "room event receiver lagged");
        if !self.service.0.client.state().is_logged_in() {
            self.disconnected();
            return;
        }
        // A self notification may have been dropped, but the correlated room
        // replies still establish membership and must unblock callers.
        let mut ready = Vec::new();
        for (name, pending) in &mut self.pending {
            pending.observed = true;
            if pending.outcome.is_some() {
                ready.push(name.clone());
            }
        }
        for name in ready {
            self.finish(name);
        }
        self.logged_in();
    }

    fn remove_joined(&mut self, name: &str) {
        self.rejoin.remove(name);
        let removed = {
            let mut rooms = self
                .service
                .0
                .rooms
                .write()
                .unwrap_or_else(PoisonError::into_inner);
            let removed = rooms.remove(name).is_some();
            if removed {
                self.service
                    .0
                    .bus
                    .publish_with(Topic::Rooms, || LiveEvent::RoomLeft { room: name.into() });
            }
            removed
        };
        if removed {
            self.publish_rooms();
        }
    }

    fn publish_rooms(&self) {
        let names = self.service.names();
        self.app.state.update(|state| state.rooms = names);
    }
}

pub fn routes() -> Routes {
    Routes::new()
        .get("/api/rooms", available)
        .get("/api/rooms/joined", joined)
        .post("/api/rooms/joined", join)
        .get("/api/rooms/joined/{room}", room)
        .delete("/api/rooms/joined/{room}", leave)
        .get("/api/rooms/joined/{room}/users", users)
        .post("/api/rooms/joined/{room}/members", add_member)
        .get("/api/rooms/joined/{room}/messages", messages)
        .post("/api/rooms/joined/{room}/messages", send_message)
        .get("/api/rooms/joined/{room}/ticker", ticker)
        .put("/api/rooms/joined/{room}/ticker", set_ticker)
}

async fn available(
    _: Authenticated,
    State(app): State<AppState>,
) -> ApiResult<Json<Vec<RoomInfo>>> {
    Ok(Json(app.services.messaging.rooms.available().await?))
}

async fn joined(_: Authenticated, State(app): State<AppState>) -> ApiResult<Json<Vec<String>>> {
    Ok(Json(app.services.messaging.rooms.names()))
}

fn room_response(app: &AppState, name: &str) -> ApiResult<Room> {
    let username = app.client.username();
    app.services
        .messaging
        .rooms
        .with_room(name, |room| room.response(username.as_deref()))
}

async fn room(
    _: Authenticated,
    State(app): State<AppState>,
    Path(name): Path<String>,
) -> ApiResult<Json<Room>> {
    room_response(&app, &name).map(Json)
}

async fn users(
    _: Authenticated,
    State(app): State<AppState>,
    Path(name): Path<String>,
) -> ApiResult<Json<Vec<RoomUser>>> {
    let username = app.client.username();
    app.services
        .messaging
        .rooms
        .with_room(&name, |room| Json(room.users(username.as_deref())))
}

async fn messages(
    _: Authenticated,
    State(app): State<AppState>,
    Path(name): Path<String>,
) -> ApiResult<Json<Vec<RoomMessage>>> {
    app.services
        .messaging
        .rooms
        .with_room(&name, |room| Json(room.messages.iter().cloned().collect()))
}

async fn ticker(
    _: Authenticated,
    State(app): State<AppState>,
    Path(name): Path<String>,
) -> ApiResult<Json<Ticker>> {
    let username = app.client.username();
    app.services.messaging.rooms.with_room(&name, |room| {
        Json(Ticker {
            ticker: username
                .as_ref()
                .and_then(|username| room.tickers.get(username))
                .cloned()
                .unwrap_or_default(),
        })
    })
}

/// Joins a room: 201 with the room, or 200 with it when already joined.
async fn join(
    _: RequireReadWrite,
    State(app): State<AppState>,
    Json(JoinRoom { name }): Json<JoinRoom>,
) -> ApiResult<Response> {
    if name.trim().is_empty() {
        return Err(ApiError::BadRequest(
            "The room name must not be blank".into(),
        ));
    }
    let joined = app
        .services
        .messaging
        .rooms
        .request(|reply| Command::Join(name.clone(), reply))
        .await?;
    Ok(match joined {
        Some(room) => (StatusCode::CREATED, Json(room)).into_response(),
        None => Json(room_response(&app, &name)?).into_response(),
    })
}

async fn leave(
    _: RequireReadWrite,
    State(app): State<AppState>,
    Path(name): Path<String>,
) -> ApiResult<StatusCode> {
    let left = app
        .services
        .messaging
        .rooms
        .request(|reply| Command::Leave(name, reply))
        .await?;
    if left {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::not_found())
    }
}

async fn send_message(
    _: RequireReadWrite,
    State(app): State<AppState>,
    Path(name): Path<String>,
    Json(request): Json<slskr_types::conversations::SendMessage>,
) -> ApiResult<StatusCode> {
    app.services.messaging.rooms.with_room(&name, |_| ())?;
    app.client
        .send_room_message(&name, &request.message)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn set_ticker(
    _: RequireReadWrite,
    State(app): State<AppState>,
    Path(name): Path<String>,
    Json(request): Json<Ticker>,
) -> ApiResult<StatusCode> {
    app.services.messaging.rooms.with_room(&name, |_| ())?;
    app.client.set_room_ticker(&name, &request.ticker).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn add_member(
    _: RequireReadWrite,
    State(app): State<AppState>,
    Path(name): Path<String>,
    Json(AddMember { username }): Json<AddMember>,
) -> ApiResult<StatusCode> {
    let self_username = app.client.username();
    let allowed = app.services.messaging.rooms.with_room(&name, |room| {
        self_username
            .as_deref()
            .is_some_and(|username| room.can_add_member(username))
    })?;
    if !allowed {
        return Err(ApiError::Forbidden(
            "Only private room owners and operators can add members".into(),
        ));
    }
    app.client.add_private_room_member(&name, &username).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(username: &str) -> soulseek::UserData {
        soulseek::UserData {
            username: username.into(),
            status: soulseek::UserPresence::Online,
            average_speed: 0,
            upload_count: 0,
            file_count: 0,
            directory_count: 0,
            country_code: None,
            slots_free: None,
        }
    }

    #[test]
    fn live_room_updates_apply_to_the_join_snapshot() {
        let mut room = JoinedRoom::new(RoomData {
            name: "private".into(),
            users: vec![user("owner"), user("guest")],
            is_private: true,
            owner: Some("owner".into()),
            operators: vec!["operator".into()],
        });
        assert!(room.can_add_member("owner"));
        assert!(room.can_add_member("operator"));
        assert!(!room.can_add_member("guest"));
        Update::UserJoined(user("owner")).apply(&mut room);
        Update::UserJoined(user("operator")).apply(&mut room);
        Update::TickerList(vec![RoomTicker {
            username: "guest".into(),
            message: "old".into(),
        }])
        .apply(&mut room);
        Update::TickerAdded(RoomTicker {
            username: "owner".into(),
            message: "new".into(),
        })
        .apply(&mut room);
        Update::TickerRemoved("owner".into()).apply(&mut room);
        Update::UserLeft("guest".into()).apply(&mut room);
        Update::TickerRemoved("guest".into()).apply(&mut room);
        assert!(room.tickers.is_empty());
        let now = Utc::now();
        for i in 0..=MESSAGE_LIMIT {
            Update::Message(RoomMessage {
                room: "private".into(),
                username: "owner".into(),
                timestamp: now,
                message: i.to_string(),
                is_self: true,
            })
            .apply(&mut room);
        }
        let response = room.response(Some("owner"));
        assert_eq!(response.operators.len(), 1);
        assert_eq!(response.users.len(), 2);
        assert!(response.users[0].is_self);
        assert!(!response.users[1].is_self);
        assert_eq!(response.messages.len(), MESSAGE_LIMIT);
        assert_eq!(response.messages[0].message, "1");
    }

    #[tokio::test]
    async fn membership_and_messages_are_published_to_the_rooms_topic() {
        let dir = tempfile::tempdir().unwrap();
        let app = crate::bootstrap::test_app(dir.path(), "[soulseek]\nconnect = false\n", &[])
            .await
            .unwrap();
        let service = app.services.messaging.rooms.clone();
        let mut session = Session {
            app: app.clone(),
            service: service.clone(),
            last_username: Some("me".into()),
            logged_in: true,
            rejoin: BTreeSet::new(),
            pending: BTreeMap::new(),
            operations: JoinSet::new(),
        };
        let mut connection = app.bus.connection();
        connection.subscribe(Topic::Rooms);
        let mut events = app.bus.receiver();
        let mut next = move || {
            let published = events.try_recv().expect("an event");
            assert_eq!(published.topic, Topic::Rooms);
            serde_json::from_str::<LiveEvent>(published.frame.as_str()).unwrap()
        };

        session.pending.insert(
            "room".into(),
            Pending {
                action: Action::Join(None),
                observed: true,
                outcome: Some(Outcome::Joined(RoomData {
                    name: "room".into(),
                    users: vec![user("me")],
                    is_private: false,
                    owner: None,
                    operators: Vec::new(),
                })),
                updates: Vec::new(),
            },
        );
        session.finish("room".into());
        let LiveEvent::RoomJoined(room) = next() else {
            panic!("expected room_joined")
        };
        assert_eq!(room.name, "room");
        assert!(room.users[0].is_self);

        session.update("room", Update::UserJoined(user("guest")));
        assert_eq!(
            next(),
            LiveEvent::RoomUserJoined {
                room: "room".into(),
                user: user_response(&user("guest"), Some("me")),
            }
        );
        let message = RoomMessage {
            timestamp: Utc::now(),
            room: "room".into(),
            username: "guest".into(),
            message: "hi".into(),
            is_self: false,
        };
        session.update("room", Update::Message(message.clone()));
        assert_eq!(next(), LiveEvent::RoomMessage(message));
        session.update("room", Update::UserLeft("guest".into()));
        assert_eq!(
            next(),
            LiveEvent::RoomUserLeft {
                room: "room".into(),
                username: "guest".into(),
            }
        );
        // Updates of rooms that are not joined are not published.
        session.update("elsewhere", Update::UserLeft("guest".into()));
        session.remove_joined("room");
        assert_eq!(
            next(),
            LiveEvent::RoomLeft {
                room: "room".into()
            }
        );
        // The snapshot is empty while not logged in.
        assert!(service.snapshot().1.is_empty());
    }

    #[test]
    fn available_list_distinguishes_public_private_owned_and_moderated_rooms() {
        let info = soulseek::RoomInfo::with_count;
        let rooms = available_rooms(RoomList {
            public: vec![info("public", 6)],
            private: vec![info("private", 2)],
            owned: vec![info("owned", 3)],
            moderated_rooms: vec![info("private", 0)],
        });
        assert_eq!(
            rooms[0],
            RoomInfo {
                name: "public".into(),
                user_count: 6,
                ..RoomInfo::default()
            }
        );
        assert!(rooms[1].is_private && rooms[1].is_moderated && !rooms[1].is_owned);
        assert!(rooms[2].is_private && rooms[2].is_owned && !rooms[2].is_moderated);
    }
}
