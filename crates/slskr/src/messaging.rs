//! Private conversations and live room messaging.

pub mod conversations;
pub mod rooms;

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

pub fn routes() -> crate::http::Routes {
    conversations::routes().merge(rooms::routes())
}
