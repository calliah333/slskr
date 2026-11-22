//! Shared application state and debounced application-hub notifications.

use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use serde_json::to_value;
use slskr_api::application::{State, hub_methods};
use tokio::sync::watch;

use crate::signalr::Hub;

const BROADCAST_DELAY: Duration = Duration::from_millis(50);

#[derive(Clone)]
pub struct StateStore {
    sender: watch::Sender<State>,
    hub: Hub,
    revision: Arc<AtomicU64>,
}

impl StateStore {
    pub fn new(hub: Hub) -> Self {
        let (sender, _) = watch::channel(State::default());
        Self {
            sender,
            hub,
            revision: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn get(&self) -> State {
        self.sender.borrow().clone()
    }

    pub fn update(&self, f: impl FnOnce(&mut State)) {
        self.sender.send_modify(f);
        let revision = Arc::clone(&self.revision);
        let generation = revision.fetch_add(1, Ordering::Relaxed) + 1;
        let sender = self.sender.clone();
        let hub = self.hub.clone();
        tokio::spawn(async move {
            tokio::time::sleep(BROADCAST_DELAY).await;
            if revision.load(Ordering::Relaxed) != generation {
                return;
            }
            if let Ok(value) = to_value(sender.borrow().clone()) {
                hub.broadcast(hub_methods::STATE, vec![value]);
            }
        });
    }

    pub fn subscribe(&self) -> watch::Receiver<State> {
        self.sender.subscribe()
    }
}
