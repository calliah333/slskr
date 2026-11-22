use std::sync::Mutex;
use std::time::Duration;

use tokio::sync::Notify;
use tokio::time::{Instant, sleep_until};

const NANOS_PER_SECOND: u128 = 1_000_000_000;

/// Shared token bucket with up to one second of burst capacity.
pub(crate) struct RateLimiter {
    bucket: Mutex<Bucket>,
    changed: Notify,
}

/// Holds credit until bytes are moved; any unused credit is returned on drop.
pub(crate) struct Reservation<'a> {
    limiter: Option<&'a RateLimiter>,
    remaining: usize,
}

impl Reservation<'_> {
    pub fn remaining(&self) -> usize {
        self.remaining
    }

    pub fn consume(&mut self, bytes: usize) {
        self.remaining -= bytes;
    }
}

impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        if let Some(limiter) = self.limiter
            && self.remaining != 0
        {
            limiter.refund(self.remaining);
        }
    }
}

struct Bucket {
    rate: u64,
    // One byte costs NANOS_PER_SECOND credits. Keeping the fraction prevents
    // frequent callers from discarding sub-byte progress during refill.
    credits: u128,
    updated_at: Instant,
}

impl Bucket {
    fn capacity(rate: u64) -> u128 {
        u128::from(rate) * NANOS_PER_SECOND
    }

    fn refill(&mut self, now: Instant) {
        let elapsed = now
            .duration_since(self.updated_at)
            .as_nanos()
            .min(NANOS_PER_SECOND);
        self.credits =
            (self.credits + elapsed * u128::from(self.rate)).min(Self::capacity(self.rate));
        self.updated_at = now;
    }
}

impl RateLimiter {
    pub fn new(bytes_per_second: u64) -> Self {
        Self {
            bucket: Mutex::new(Bucket {
                rate: bytes_per_second,
                credits: Bucket::capacity(bytes_per_second),
                updated_at: Instant::now(),
            }),
            changed: Notify::new(),
        }
    }

    pub fn set_rate(&self, bytes_per_second: u64) {
        {
            let mut bucket = self.bucket.lock().unwrap_or_else(|e| e.into_inner());
            if bucket.rate == bytes_per_second {
                return;
            }
            let now = Instant::now();
            let capacity = Bucket::capacity(bytes_per_second);
            if bucket.rate == 0 {
                bucket.credits = capacity;
                bucket.updated_at = now;
            } else {
                bucket.refill(now);
                bucket.credits = bucket.credits.min(capacity);
            }
            bucket.rate = bytes_per_second;
        }
        self.changed.notify_waiters();
    }

    pub async fn acquire(&self, wanted: usize) -> Reservation<'_> {
        if wanted == 0 {
            return Reservation {
                limiter: None,
                remaining: 0,
            };
        }
        loop {
            let (deadline, changed) = {
                let mut bucket = self.bucket.lock().unwrap_or_else(|e| e.into_inner());
                if bucket.rate == 0 {
                    return Reservation {
                        limiter: None,
                        remaining: wanted,
                    };
                }

                let now = Instant::now();
                bucket.refill(now);
                let granted = (bucket.credits / NANOS_PER_SECOND).min(wanted as u128);
                if granted > 0 {
                    bucket.credits -= granted * NANOS_PER_SECOND;
                    return Reservation {
                        limiter: Some(self),
                        remaining: granted as usize,
                    };
                }

                let nanos = (NANOS_PER_SECOND - bucket.credits).div_ceil(u128::from(bucket.rate));
                // Create the notification before unlocking so a rate change
                // cannot be missed even if this future has not been polled yet.
                (
                    now + Duration::from_nanos(nanos as u64),
                    self.changed.notified(),
                )
            };

            // A blocked caller has consumed nothing and can be dropped freely.
            tokio::select! {
                _ = sleep_until(deadline) => {},
                _ = changed => {},
            }
        }
    }

    fn refund(&self, bytes: usize) {
        {
            let mut bucket = self.bucket.lock().unwrap_or_else(|e| e.into_inner());
            if bucket.rate == 0 {
                return;
            }
            bucket.refill(Instant::now());
            bucket.credits = (bucket.credits + bytes as u128 * NANOS_PER_SECOND)
                .min(Bucket::capacity(bucket.rate));
        }
        self.changed.notify_waiters();
    }
}

#[cfg(test)]
mod tests {
    use std::pin::pin;

    use futures::poll;
    use tokio::time::advance;

    use super::*;

    async fn take(limiter: &RateLimiter, wanted: usize) -> usize {
        let mut reservation = limiter.acquire(wanted).await;
        let granted = reservation.remaining();
        reservation.consume(granted);
        granted
    }

    #[tokio::test(start_paused = true)]
    async fn unlimited_grants_everything_and_zero_consumes_nothing() {
        let limiter = RateLimiter::new(0);
        assert_eq!(take(&limiter, usize::MAX).await, usize::MAX);
        limiter.set_rate(5);
        let start = Instant::now();
        assert_eq!(take(&limiter, 0).await, 0);
        assert_eq!(take(&limiter, 5).await, 5);
        assert_eq!(take(&limiter, 0).await, 0);
        assert_eq!(start.elapsed(), Duration::ZERO);
    }

    #[tokio::test(start_paused = true)]
    async fn shares_partial_grants_and_caps_the_idle_burst() {
        let limiter = RateLimiter::new(4);
        assert_eq!(take(&limiter, 3).await, 3);
        assert_eq!(take(&limiter, 10).await, 1);
        let mut waiting = pin!(take(&limiter, 10));
        assert!(poll!(waiting.as_mut()).is_pending());
        advance(Duration::from_millis(125)).await;
        assert!(poll!(waiting.as_mut()).is_pending());
        advance(Duration::from_millis(125)).await;
        assert_eq!(waiting.await, 1);

        advance(Duration::from_secs(5)).await;
        assert_eq!(take(&limiter, 10).await, 4);
    }

    #[tokio::test(start_paused = true)]
    async fn a_live_rate_change_wakes_waiters_and_preserves_earned_credit() {
        let limiter = RateLimiter::new(2);
        assert_eq!(take(&limiter, 2).await, 2);
        let old_deadline = Instant::now() + Duration::from_millis(500);
        let mut waiting = pin!(take(&limiter, 10));
        assert!(poll!(waiting.as_mut()).is_pending());

        advance(Duration::from_millis(250)).await;
        limiter.set_rate(4);
        assert!(poll!(waiting.as_mut()).is_pending());
        advance(Duration::from_millis(125)).await;
        assert_eq!(waiting.await, 1);
        assert!(Instant::now() < old_deadline);
    }

    #[tokio::test(start_paused = true)]
    async fn lowering_the_rate_clamps_the_remaining_burst() {
        let limiter = RateLimiter::new(10);
        assert_eq!(take(&limiter, 1).await, 1);
        limiter.set_rate(2);
        assert_eq!(take(&limiter, 10).await, 2);
        let mut waiting = pin!(take(&limiter, 10));
        assert!(poll!(waiting.as_mut()).is_pending());
        advance(Duration::from_millis(250)).await;
        assert!(poll!(waiting.as_mut()).is_pending());
        advance(Duration::from_millis(250)).await;
        assert_eq!(waiting.await, 1);
    }

    #[tokio::test(start_paused = true)]
    async fn switching_to_unlimited_wakes_all_waiters() {
        let limiter = RateLimiter::new(1);
        assert_eq!(take(&limiter, 1).await, 1);
        let mut first = pin!(take(&limiter, 10));
        let mut second = pin!(take(&limiter, 20));
        assert!(poll!(first.as_mut()).is_pending());
        assert!(poll!(second.as_mut()).is_pending());

        limiter.set_rate(0);
        assert_eq!(poll!(first.as_mut()), std::task::Poll::Ready(10));
        assert_eq!(poll!(second.as_mut()), std::task::Poll::Ready(20));
    }

    #[tokio::test(start_paused = true)]
    async fn dropping_a_blocked_acquisition_does_not_reserve_credit() {
        let limiter = RateLimiter::new(4);
        assert_eq!(take(&limiter, 4).await, 4);
        {
            let mut abandoned = pin!(take(&limiter, 100));
            assert!(poll!(abandoned.as_mut()).is_pending());
        }

        advance(Duration::from_millis(250)).await;
        let now = Instant::now();
        assert_eq!(take(&limiter, 4).await, 1);
        assert_eq!(now.elapsed(), Duration::ZERO);
    }

    #[tokio::test(start_paused = true)]
    async fn refund_wakes_a_waiter_at_the_new_finite_rate() {
        let limiter = RateLimiter::new(4);
        let mut reservation = limiter.acquire(4).await;
        reservation.consume(1);
        limiter.set_rate(2);
        let mut waiting = pin!(take(&limiter, 4));
        assert!(poll!(waiting.as_mut()).is_pending());

        let now = Instant::now();
        drop(reservation);
        assert_eq!(poll!(waiting.as_mut()), std::task::Poll::Ready(2));
        assert_eq!(now.elapsed(), Duration::ZERO);
    }

    #[tokio::test(start_paused = true)]
    async fn unlimited_reservation_does_not_credit_a_finite_bucket() {
        let limiter = RateLimiter::new(0);
        let reservation = limiter.acquire(4).await;
        limiter.set_rate(2);
        assert_eq!(take(&limiter, 2).await, 2);
        drop(reservation);
        let mut waiting = pin!(take(&limiter, 4));
        assert!(poll!(waiting.as_mut()).is_pending());
    }
}
