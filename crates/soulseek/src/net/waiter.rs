use std::any::{Any, TypeId};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use tokio::sync::oneshot;
use tokio::time::{Instant, timeout_at};

use crate::error::{Error, Result};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct WaitKey(pub &'static str, pub String);

type Reply = Result<Box<dyn Any + Send>>;

struct Entry {
    id: u64,
    response_type: TypeId,
    sender: oneshot::Sender<Reply>,
}

#[derive(Default)]
struct Pending {
    next_id: u64,
    entries: HashMap<Arc<WaitKey>, VecDeque<Entry>>,
}

impl Pending {
    fn remove(&mut self, key: &WaitKey, matches: impl FnMut(&Entry) -> bool) -> Option<Entry> {
        let queue = self.entries.get_mut(key)?;
        let index = queue.iter().position(matches)?;
        let entry = queue.remove(index);
        if queue.is_empty() {
            self.entries.remove(key);
        }
        entry
    }
}

#[derive(Default)]
pub(crate) struct Waiter {
    pending: Arc<Mutex<Pending>>,
}

impl Waiter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers synchronously, before the caller sends its request.
    pub fn wait<T: Send + 'static>(
        &self,
        key: WaitKey,
        timeout: Option<Duration>,
    ) -> impl Future<Output = Result<T>> + Send + 'static + use<T> {
        let deadline = timeout.and_then(|timeout| Instant::now().checked_add(timeout));
        let (sender, receiver) = oneshot::channel();
        let (key, id) = {
            let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
            let id = pending.next_id;
            pending.next_id = id.wrapping_add(1);
            let key = match pending.entries.get_key_value(&key) {
                Some((existing, _)) => Arc::clone(existing),
                None => Arc::new(key),
            };
            pending
                .entries
                .entry(Arc::clone(&key))
                .or_default()
                .push_back(Entry {
                    id,
                    response_type: TypeId::of::<T>(),
                    sender,
                });
            (key, id)
        };
        Registration {
            pending: Arc::downgrade(&self.pending),
            key,
            id: Some(id),
            receiver,
        }
        .receive::<T>(deadline)
    }

    pub fn wait_indefinitely<T: Send + 'static>(
        &self,
        key: WaitKey,
    ) -> impl Future<Output = Result<T>> + Send + 'static + use<T> {
        self.wait(key, None)
    }

    /// Completes the oldest pending registration with this key and response type.
    pub fn complete<T: Send + 'static>(&self, key: &WaitKey, value: T) -> bool {
        self.take(key, |entry| entry.response_type == TypeId::of::<T>())
            .is_some_and(|entry| entry.sender.send(Ok(Box::new(value))).is_ok())
    }

    pub fn throw(&self, key: &WaitKey, error: Error) -> bool {
        self.take(key, |_| true)
            .is_some_and(|entry| entry.sender.send(Err(error)).is_ok())
    }

    pub fn cancel_all(&self) {
        let entries = {
            let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
            std::mem::take(&mut pending.entries)
        };
        // Closing the channels wakes the waiters. Do it outside the registry lock.
        drop(entries);
    }

    fn take(&self, key: &WaitKey, matches: impl FnMut(&Entry) -> bool) -> Option<Entry> {
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        pending.remove(key, matches)
    }
}

struct Registration {
    pending: Weak<Mutex<Pending>>,
    key: Arc<WaitKey>,
    id: Option<u64>,
    receiver: oneshot::Receiver<Reply>,
}

impl Registration {
    async fn receive<T: Send + 'static>(mut self, deadline: Option<Instant>) -> Result<T> {
        let response = match deadline {
            Some(deadline) => timeout_at(deadline, &mut self.receiver).await?,
            None => (&mut self.receiver).await,
        };
        // Completion, failure, and disconnect all remove the sender first.
        self.id = None;
        let value = response
            .map_err(|_| Error::Connection("Disconnected while waiting for a response".into()))??;
        Ok(*value
            .downcast::<T>()
            .unwrap_or_else(|_| unreachable!("waiter response type checked before delivery")))
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        if let Some(id) = self.id
            && let Some(pending) = self.pending.upgrade()
        {
            let entry = {
                let mut pending = pending.lock().unwrap_or_else(|e| e.into_inner());
                pending.remove(&self.key, |entry| entry.id == id)
            };
            drop(entry);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::pin::pin;

    use futures::poll;
    use tokio::time::advance;

    use super::*;

    fn key(discriminator: &str) -> WaitKey {
        WaitKey("response", discriminator.into())
    }

    #[tokio::test]
    async fn registers_before_polling_and_matches_in_fifo_order_by_type() {
        let waiter = Waiter::new();
        let key = key("alice");
        let first = waiter.wait_indefinitely::<u32>(key.clone());
        let text = waiter.wait_indefinitely::<&str>(key.clone());
        let second = waiter.wait_indefinitely::<u32>(key.clone());

        assert!(!waiter.complete(&key, false));
        assert!(waiter.complete(&key, "reply"));
        assert!(waiter.complete(&key, 10_u32));
        assert!(waiter.complete(&key, 20_u32));
        assert!(!waiter.complete(&key, 30_u32));
        drop(waiter);
        assert_eq!(first.await.unwrap(), 10);
        assert_eq!(text.await.unwrap(), "reply");
        assert_eq!(second.await.unwrap(), 20);
    }

    #[tokio::test]
    async fn dropping_unpolled_and_polled_futures_unregisters_them() {
        let waiter = Waiter::new();
        let key = key("alice");
        drop(waiter.wait_indefinitely::<u32>(key.clone()));
        {
            let mut waiting = pin!(waiter.wait_indefinitely::<u32>(key.clone()));
            assert!(poll!(waiting.as_mut()).is_pending());
        }
        assert!(!waiter.complete(&key, 1_u32));

        let cancelled = waiter.wait_indefinitely::<u32>(key.clone());
        let remaining = waiter.wait_indefinitely::<u32>(key.clone());
        drop(cancelled);
        assert!(waiter.complete(&key, 2_u32));
        assert_eq!(remaining.await.unwrap(), 2);
        assert!(!waiter.complete(&key, 3_u32));
    }

    #[tokio::test(start_paused = true)]
    async fn timeout_starts_at_registration_and_unregisters_the_waiter() {
        let waiter = Waiter::new();
        let key = key("alice");
        let start = Instant::now();
        let waiting = waiter.wait::<u32>(key.clone(), Some(Duration::from_secs(1)));
        advance(Duration::from_secs(2)).await;

        assert!(matches!(waiting.await, Err(Error::Timeout(_))));
        assert_eq!(start.elapsed(), Duration::from_secs(2));
        assert!(!waiter.complete(&key, 1_u32));
    }

    #[tokio::test]
    async fn throw_only_fails_the_oldest_waiter_for_its_key() {
        let waiter = Waiter::new();
        let key = key("alice");
        let first = waiter.wait_indefinitely::<bool>(key.clone());
        let second = waiter.wait_indefinitely::<u32>(key.clone());

        assert!(waiter.throw(&key, Error::UserOffline("offline".into())));
        assert!(matches!(first.await, Err(Error::UserOffline(message)) if message == "offline"));
        assert!(waiter.complete(&key, 1_u32));
        assert_eq!(second.await.unwrap(), 1);
        assert!(!waiter.throw(&key, Error::Cancelled));
    }

    #[tokio::test]
    async fn disconnect_cancels_all_waiters_and_allows_reuse() {
        let waiter = Waiter::new();
        let alice = key("alice");
        let bob = key("bob");
        let old_alice = waiter.wait_indefinitely::<u32>(alice.clone());
        let old_bob = waiter.wait_indefinitely::<bool>(bob.clone());
        let abandoned = waiter.wait_indefinitely::<u32>(alice.clone());

        waiter.cancel_all();
        assert!(!waiter.complete(&alice, 1_u32));
        assert!(!waiter.complete(&bob, true));
        let new_alice = waiter.wait_indefinitely::<u32>(alice.clone());
        drop(abandoned);
        assert!(matches!(old_alice.await, Err(Error::Connection(_))));
        assert!(matches!(old_bob.await, Err(Error::Connection(_))));
        assert!(waiter.complete(&alice, 2_u32));
        assert_eq!(new_alice.await.unwrap(), 2);
    }

    #[tokio::test]
    async fn dropping_the_registry_closes_pending_waits() {
        let waiter = Waiter::new();
        let waiting = waiter.wait_indefinitely::<u32>(key("alice"));
        drop(waiter);
        assert!(matches!(waiting.await, Err(Error::Connection(_))));
    }
}
