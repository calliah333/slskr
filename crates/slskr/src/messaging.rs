//! Private conversations and live room messaging.

pub mod conversations;
pub mod rooms;

use chrono::Utc;
use slskr_api::events::{EventHeader, EventType};
use uuid::Uuid;

use crate::{
    app::{AppState, Core},
    events::EventService,
};
use conversations::ConversationService;
use rooms::RoomService;

#[derive(Clone)]
pub struct MessagingService {
    pub conversations: ConversationService,
    pub rooms: RoomService,
}

impl MessagingService {
    pub fn new(core: &Core, events: &EventService) -> anyhow::Result<Self> {
        Ok(Self {
            conversations: ConversationService::new(core, events)?,
            rooms: RoomService::new(core)?,
        })
    }

    pub fn start(&self, app: AppState) {
        self.conversations.start(app.clone());
        self.rooms.start(app);
    }
}

pub fn routes() -> axum::Router<AppState> {
    conversations::routes().merge(rooms::routes())
}

fn event_header(kind: EventType) -> EventHeader {
    EventHeader {
        id: Uuid::new_v4().to_string(),
        timestamp: Utc::now(),
        kind,
        version: 1,
    }
}
