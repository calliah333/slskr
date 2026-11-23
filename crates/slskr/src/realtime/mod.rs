//! Live updates: the internal event bus services publish to, and the `/api/events`
//! WebSocket that fans it out to browser connections by topic (see [`socket`]).
//!
//! Every published event is serialized once and stamped with a sequence number. A
//! connection subscribing to a topic reads the current sequence number before taking the
//! topic's snapshot and skips queued events of that topic up to it, because the snapshot
//! already includes them (publishers change their data before they publish).

pub mod socket;

#[cfg(test)]
mod tests;

use std::sync::{
    Arc, Mutex, PoisonError,
    atomic::{AtomicUsize, Ordering},
};

use axum::extract::ws::Utf8Bytes;
use slskr_api::realtime::{Event, Topic};
use tokio::sync::broadcast;

pub use socket::{Limits, Snapshots};

/// Events buffered for connections that have not yet read them. A connection that falls
/// this far behind is closed as too slow.
const BUS_CAPACITY: usize = 4096;

/// A published event, serialized once for every connection.
#[derive(Clone, Debug)]
pub struct Published {
    pub seq: u64,
    pub topic: Topic,
    pub frame: Utf8Bytes,
}

/// Internal publish/subscribe channel for live updates. Cheap to clone.
#[derive(Clone)]
pub struct EventBus(Arc<Inner>);

struct Inner {
    sender: broadcast::Sender<Published>,
    /// Last assigned sequence number; held while sending so queue order matches it.
    seq: Mutex<u64>,
    /// Connections subscribed to each topic, by [`Topic::index`].
    subscribers: [AtomicUsize; Topic::ALL.len()],
    connections: AtomicUsize,
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

impl EventBus {
    pub fn new() -> Self {
        Self(Arc::new(Inner {
            sender: broadcast::channel(BUS_CAPACITY).0,
            seq: Mutex::new(0),
            subscribers: Default::default(),
            connections: AtomicUsize::new(0),
        }))
    }

    /// True when a connection is subscribed to `topic`. Publishers use this to skip
    /// building events nobody receives; a new subscriber gets a snapshot instead.
    pub fn wants(&self, topic: Topic) -> bool {
        self.0.subscribers[topic.index()].load(Ordering::SeqCst) != 0
    }

    /// Publishes `event` to the connections subscribed to its topic.
    pub fn publish(&self, event: &Event) {
        let Some(topic) = event.topic() else { return };
        if self.wants(topic) {
            self.send(topic, event);
        }
    }

    /// Builds and publishes an event only when its topic has subscribers.
    pub fn publish_with(&self, topic: Topic, event: impl FnOnce() -> Event) {
        if self.wants(topic) {
            let event = event();
            debug_assert_eq!(event.topic(), Some(topic));
            self.send(topic, &event);
        }
    }

    fn send(&self, topic: Topic, event: &Event) {
        let frame = match serde_json::to_string(event) {
            Ok(frame) => Utf8Bytes::from(frame),
            Err(error) => {
                // No tracing here: the log buffer publishes from inside the tracing layer.
                eprintln!("slskr: could not encode {} event: {error}", topic.as_str());
                return;
            }
        };
        let mut seq = self.0.seq.lock().unwrap_or_else(PoisonError::into_inner);
        *seq += 1;
        // An error only means that no connection is listening right now.
        let _ = self.0.sender.send(Published {
            seq: *seq,
            topic,
            frame,
        });
    }

    /// The sequence number of the last published event. Every event up to it is reflected
    /// in data its publisher changed before publishing.
    pub fn seq(&self) -> u64 {
        *self.0.seq.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Number of open `/api/events` connections.
    pub fn connection_count(&self) -> usize {
        self.0.connections.load(Ordering::SeqCst)
    }

    /// Number of connections subscribed to `topic`.
    pub fn subscriber_count(&self, topic: Topic) -> usize {
        self.0.subscribers[topic.index()].load(Ordering::SeqCst)
    }

    pub(crate) fn receiver(&self) -> broadcast::Receiver<Published> {
        self.0.sender.subscribe()
    }

    pub(crate) fn connection(&self) -> Connection {
        self.0.connections.fetch_add(1, Ordering::SeqCst);
        Connection {
            bus: self.clone(),
            topics: [false; Topic::ALL.len()],
        }
    }
}

/// One connection's registration and topic subscriptions; released on drop.
pub(crate) struct Connection {
    bus: EventBus,
    topics: [bool; Topic::ALL.len()],
}

impl Connection {
    pub fn contains(&self, topic: Topic) -> bool {
        self.topics[topic.index()]
    }

    pub fn subscribe(&mut self, topic: Topic) {
        if !std::mem::replace(&mut self.topics[topic.index()], true) {
            self.bus.0.subscribers[topic.index()].fetch_add(1, Ordering::SeqCst);
        }
    }

    pub fn unsubscribe(&mut self, topic: Topic) {
        if std::mem::replace(&mut self.topics[topic.index()], false) {
            self.bus.0.subscribers[topic.index()].fetch_sub(1, Ordering::SeqCst);
        }
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        for topic in Topic::ALL {
            self.unsubscribe(topic);
        }
        self.bus.0.connections.fetch_sub(1, Ordering::SeqCst);
    }
}
