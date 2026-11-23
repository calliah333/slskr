//! Shared application state and its coalesced `state` events.

use std::time::Duration;

use slskr_api::{
    application::State,
    realtime::{Event, Topic},
};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::realtime::EventBus;

/// Minimum time between two `state` events (at most four a second).
pub const STATE_EVENT_INTERVAL: Duration = Duration::from_millis(250);

#[derive(Clone)]
pub struct StateStore {
    sender: watch::Sender<State>,
}

impl Default for StateStore {
    fn default() -> Self {
        Self::new()
    }
}

impl StateStore {
    pub fn new() -> Self {
        Self {
            sender: watch::channel(State::default()).0,
        }
    }

    pub fn get(&self) -> State {
        self.sender.borrow().clone()
    }

    /// Changes the state; subscribers are notified only when it actually changed.
    pub fn update(&self, f: impl FnOnce(&mut State)) {
        self.sender.send_if_modified(|state| {
            let before = state.clone();
            f(state);
            *state != before
        });
    }

    pub fn subscribe(&self) -> watch::Receiver<State> {
        self.sender.subscribe()
    }

    /// Publishes the state on `bus` after every change, at most once per
    /// [`STATE_EVENT_INTERVAL`]: changes in between are coalesced, and the latest state is
    /// always published. Ends on `shutdown`.
    pub fn spawn_publisher(&self, bus: EventBus, shutdown: CancellationToken) {
        let mut changes = self.subscribe();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    () = shutdown.cancelled() => break,
                    changed = changes.changed() => if changed.is_err() { break },
                }
                // Without subscribers only mark the change seen: a new subscriber gets a
                // snapshot.
                let event = {
                    let state = changes.borrow_and_update();
                    bus.wants(Topic::State)
                        .then(|| Event::State(Box::new(state.clone())))
                };
                if let Some(event) = event {
                    bus.publish(&event);
                }
                tokio::select! {
                    () = shutdown.cancelled() => break,
                    () = tokio::time::sleep(STATE_EVENT_INTERVAL) => {}
                }
            }
        });
    }
}
