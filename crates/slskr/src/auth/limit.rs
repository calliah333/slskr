//! Per-address failure counting for guesses at secrets (web sign-in, the setup code).

use std::collections::HashMap;
use std::net::{IpAddr, Ipv6Addr};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

/// Counts failures per address: once an address has `max` failures within `window` of its
/// first one, it is refused until that window ends.
pub struct FailureLimiter {
    max: u32,
    window: Duration,
    /// Addresses tracked at once; when full of recent failures, new addresses are refused.
    tracked: usize,
    failures: Mutex<HashMap<IpAddr, Failures>>,
}

struct Failures {
    count: u32,
    since: Instant,
}

impl FailureLimiter {
    pub fn new(max: u32, window: Duration, tracked: usize) -> Self {
        Self {
            max,
            window,
            tracked,
            failures: Mutex::default(),
        }
    }

    fn failures(&self) -> std::sync::MutexGuard<'_, HashMap<IpAddr, Failures>> {
        self.failures.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// How long `ip` must wait before trying again, or `None` when it may try now.
    pub fn limited(&self, ip: IpAddr, now: Instant) -> Option<Duration> {
        let key = bucket(ip);
        let mut failures = self.failures();
        let elapsed = now.saturating_duration_since(failures.get(&key)?.since);
        if elapsed >= self.window {
            failures.remove(&key);
            return None;
        }
        (failures[&key].count >= self.max).then(|| self.window - elapsed)
    }

    /// Counts a failure against `ip`. Refuses (with the wait) a new address while the table
    /// is full of recent failures, so spraying addresses cannot evict an attacker's count.
    pub fn fail(&self, ip: IpAddr, now: Instant) -> Result<(), Duration> {
        let key = bucket(ip);
        let mut failures = self.failures();
        if !failures.contains_key(&key) && failures.len() >= self.tracked {
            failures.retain(|_, f| now.saturating_duration_since(f.since) < self.window);
            if failures.len() >= self.tracked {
                return Err(self.window);
            }
        }
        failures
            .entry(key)
            .or_insert(Failures {
                count: 0,
                since: now,
            })
            .count += 1;
        Ok(())
    }

    /// Forgets the failures of `ip` (after a success).
    pub fn clear(&self, ip: IpAddr) {
        self.failures().remove(&bucket(ip));
    }

    /// Forgets every failure.
    pub fn reset(&self) {
        self.failures().clear();
    }
}

/// The address failures are counted against: IPv4 addresses alone, IPv6 per /64.
pub fn bucket(ip: IpAddr) -> IpAddr {
    match ip.to_canonical() {
        IpAddr::V6(v6) => {
            let mut segments = v6.segments();
            segments[4..].fill(0);
            IpAddr::V6(Ipv6Addr::from(segments))
        }
        v4 => v4,
    }
}

/// Seconds to put in `Retry-After`, rounded up.
pub fn retry_seconds(wait: Duration) -> u64 {
    wait.as_secs() + u64::from(wait.subsec_nanos() > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_per_address_until_the_window_ends() {
        let limiter = FailureLimiter::new(2, Duration::from_secs(60), 16);
        let start = Instant::now();
        let attacker: IpAddr = "192.0.2.1".parse().unwrap();
        let neighbour: IpAddr = "2001:db8::1".parse().unwrap();
        let same_64: IpAddr = "2001:db8::2".parse().unwrap();
        assert_eq!(limiter.limited(attacker, start), None);
        limiter.fail(attacker, start).unwrap();
        limiter.fail(attacker, start).unwrap();
        let later = start + Duration::from_secs(20);
        assert_eq!(
            limiter.limited(attacker, later),
            Some(Duration::from_secs(40))
        );
        assert_eq!(limiter.limited(neighbour, later), None);
        limiter.fail(neighbour, later).unwrap();
        limiter.fail(neighbour, later).unwrap();
        assert!(limiter.limited(same_64, later).is_some());
        assert_eq!(
            limiter.limited(attacker, start + Duration::from_secs(60)),
            None
        );
        limiter.clear(same_64);
        assert_eq!(limiter.limited(neighbour, later), None);
    }

    #[test]
    fn full_table_refuses_new_addresses() {
        let limiter = FailureLimiter::new(5, Duration::from_secs(60), 1);
        let now = Instant::now();
        limiter.fail("192.0.2.1".parse().unwrap(), now).unwrap();
        let other = "192.0.2.2".parse().unwrap();
        assert_eq!(limiter.fail(other, now), Err(Duration::from_secs(60)));
        assert!(limiter.fail(other, now + Duration::from_secs(60)).is_ok());
    }
}
