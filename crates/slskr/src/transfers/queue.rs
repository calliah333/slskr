use std::{
    collections::{HashMap, HashSet},
    time::Duration,
};

use chrono::{DateTime, TimeDelta, Utc};
use slskr_types::{
    config::{Config, GroupConfig, QueueStrategy},
    transfers::{Transfer, TransferState},
};
use tokio::{sync::oneshot, time::Instant};

use crate::users::UserService;

/// Upload settings of `group`; unknown and privileged users use `groups.default`.
pub(crate) fn group_options<'a>(settings: &'a Config, group: &str) -> &'a GroupConfig {
    let groups = &settings.groups;
    match group {
        "default" | "privileged" => &groups.default,
        "leechers" => &groups.leechers,
        _ => groups.custom.get(group).unwrap_or(&groups.default),
    }
}

fn fifo(group: &GroupConfig) -> bool {
    group.strategy == QueueStrategy::FirstInFirstOut
}

/// Per-user limit: the group's own when set, else the global one; 0 is unlimited.
#[derive(Default, PartialEq)]
struct Limits {
    files: Option<u32>,
    megabytes: Option<u32>,
    failures: Option<u32>,
}

fn limit(own: u32, global: u32) -> Option<u32> {
    [own, global].into_iter().find(|value| *value != 0)
}

struct Pending {
    id: String,
    username: String,
    filename: String,
    start: oneshot::Sender<()>,
    ready: bool,
}

#[derive(Default)]
pub(super) struct UploadQueue {
    pending: Vec<Pending>,
    // Group membership is pinned for the life of a slot, even on a config reload.
    running: HashMap<String, (String, String)>,
    rotation: HashMap<String, u64>,
    turn: u64,
}

impl UploadQueue {
    pub(super) fn push(&mut self, transfer: &Transfer) -> oneshot::Receiver<()> {
        let (start, receiver) = oneshot::channel();
        self.pending.push(Pending {
            id: transfer.id.clone(),
            username: transfer.username.clone(),
            filename: transfer.filename.clone(),
            start,
            ready: false,
        });
        receiver
    }

    pub(super) fn ready(&mut self, id: &str) {
        if let Some(p) = self.pending.iter_mut().find(|p| p.id == id) {
            p.ready = true;
        }
    }

    pub(super) fn release(&mut self, id: &str) {
        let user = if let Some((user, _)) = self.running.remove(id) {
            user
        } else if let Some(index) = self.pending.iter().position(|p| p.id == id) {
            self.pending.remove(index).username
        } else {
            return;
        };
        if !self.pending.iter().any(|p| p.username == user)
            && !self.running.values().any(|(name, _)| *name == user)
        {
            self.rotation.remove(&user);
        }
    }

    pub(super) fn dispatch(&mut self, users: &UserService, cfg: &Config, slots: u32) {
        let slots = slots as usize;
        while self.running.len() < slots {
            let mut running_groups = HashMap::<&str, usize>::new();
            let mut running_users = HashSet::with_capacity(self.running.len());
            for (user, group) in self.running.values() {
                *running_groups.entry(group.as_str()).or_default() += 1;
                running_users.insert(user.as_str());
            }
            let best = self
                .pending
                .iter()
                .enumerate()
                .filter_map(|(i, p)| {
                    if !p.ready
                        || running_users.contains(p.username.as_str())
                        || p.start.is_closed()
                    {
                        return None;
                    }
                    let group = users.group(&p.username);
                    let options = group_options(cfg, &group);
                    let (priority, group_slots, fifo) = match group.as_str() {
                        "blacklisted" => return None,
                        "privileged" => (0, slots, true),
                        _ => (
                            options.priority.max(1),
                            match options.slots {
                                0 => slots,
                                group_slots => group_slots as usize,
                            },
                            fifo(options),
                        ),
                    };
                    if running_groups.get(group.as_str()).copied().unwrap_or(0) >= group_slots {
                        return None;
                    }
                    let turn = if fifo {
                        0
                    } else {
                        self.rotation.get(&p.username).copied().unwrap_or(0)
                    };
                    Some((priority, group, turn, i))
                })
                .min();
            let Some((_, group, _, i)) = best else { break };
            let p = self.pending.remove(i);
            if p.start.send(()).is_ok() {
                self.turn += 1;
                self.rotation.insert(p.username.clone(), self.turn);
                self.running.insert(p.id, (p.username, group));
            }
        }
    }

    pub(super) fn group(&self, id: &str) -> Option<&str> {
        self.running.get(id).map(|(_, group)| group.as_str())
    }

    pub(super) fn position(
        &self,
        users: &UserService,
        cfg: &Config,
        username: &str,
        filename: &str,
    ) -> Option<u32> {
        let target = self
            .pending
            .iter()
            .position(|p| p.username == username && p.filename == filename)?;
        let group = users.group(username);
        let settings = group_options(cfg, &group);
        let position = if group == "privileged" || fifo(settings) {
            self.pending[..target]
                .iter()
                .filter(|p| users.group(&p.username) == group)
                .count()
        } else {
            let own = self.pending[..target]
                .iter()
                .filter(|p| p.username == username)
                .count();
            let mut counts = HashMap::<&str, usize>::new();
            for p in &self.pending {
                if p.username != username && users.group(&p.username) == group {
                    *counts.entry(&p.username).or_default() += 1;
                }
            }
            own + counts
                .values()
                .map(|count| (*count).min(own + 1))
                .sum::<usize>()
        };
        Some(position.min(u32::MAX as usize) as u32)
    }

    pub(super) fn forecast(
        &self,
        users: &UserService,
        cfg: &Config,
        global: u32,
        username: &str,
    ) -> (u32, u32, bool) {
        let group = users.group(username);
        if group == "blacklisted" {
            return (0, 0, false);
        }
        let global = global as usize;
        let settings = group_options(cfg, &group);
        let slots = if group == "privileged" {
            global
        } else {
            match settings.slots {
                0 => global,
                group_slots => global.min(group_slots as usize),
            }
        };
        let used = self
            .running
            .values()
            .filter(|(_, name)| name == &group)
            .count();
        let free = self.running.len() < global
            && used < slots
            && !self.running.values().any(|(user, _)| user == username);
        if free {
            return (slots as u32, 0, true);
        }
        let waiting = self
            .pending
            .iter()
            .filter(|p| users.group(&p.username) == group);
        let position = if group == "privileged" || fifo(settings) {
            waiting.count()
        } else {
            waiting
                .map(|p| p.username.as_str())
                .collect::<HashSet<_>>()
                .len()
        };
        (slots as u32, position.min(u32::MAX as usize) as u32, false)
    }

    pub(super) fn info(&self, slots: u32) -> (u32, u32, bool) {
        (
            slots,
            self.pending.len().min(u32::MAX as usize) as u32,
            self.running.len() < slots as usize,
        )
    }
}

struct Completion {
    started: DateTime<Utc>,
    size: u64,
    success: bool,
}

#[derive(Default)]
pub(super) struct UploadHistory(HashMap<String, Vec<Completion>>);

impl UploadHistory {
    pub(super) fn record(&mut self, transfer: &Transfer) {
        if let Some(started) = transfer
            .started_at
            .filter(|start| *start >= Utc::now() - TimeDelta::days(7))
        {
            self.0
                .entry(transfer.username.clone())
                .or_default()
                .push(Completion {
                    started,
                    size: transfer.size,
                    success: transfer.state == TransferState::Succeeded,
                });
        }
    }

    pub(super) fn prune(&mut self) {
        let cutoff = Utc::now() - TimeDelta::days(7);
        self.0.retain(|_, entries| {
            entries.retain(|entry| entry.started >= cutoff);
            !entries.is_empty()
        });
    }

    pub(super) fn check<'a>(
        &self,
        active: impl Iterator<Item = &'a Transfer>,
        cfg: &Config,
        group: &str,
        username: &str,
        size: u64,
    ) -> Result<(), String> {
        if group == "privileged" {
            return Ok(());
        }
        let own = &group_options(cfg, group).limits;
        let global = &cfg.uploads.limits;
        let queued = Limits {
            files: limit(own.queued_files, global.queued_files),
            megabytes: limit(own.queued_megabytes, global.queued_megabytes),
            failures: None,
        };
        if queued.files.is_some() || queued.megabytes.is_some() {
            let mut files = 0;
            let mut bytes = 0_u64;
            for t in active.filter(|t| t.username == username) {
                files += 1;
                bytes = bytes.saturating_add(t.size);
            }
            let (files, bytes) = exceeds(&queued, files, bytes, size);
            if bytes {
                return Err("Too many megabytes".into());
            }
            if files {
                return Err("Too many files".into());
            }
        }
        let now = Utc::now();
        for (days, period, limit) in [
            (
                7,
                "this week",
                Limits {
                    files: limit(own.weekly_files, global.weekly_files),
                    megabytes: limit(own.weekly_megabytes, global.weekly_megabytes),
                    failures: limit(own.weekly_failures, global.weekly_failures),
                },
            ),
            (
                1,
                "today",
                Limits {
                    files: limit(own.daily_files, global.daily_files),
                    megabytes: limit(own.daily_megabytes, global.daily_megabytes),
                    failures: limit(own.daily_failures, global.daily_failures),
                },
            ),
        ] {
            if limit == Limits::default() {
                continue;
            }
            let cutoff = now - TimeDelta::days(days);
            let mut failures = 0;
            let mut files = 0;
            let mut bytes = 0_u64;
            for t in self
                .0
                .get(username)
                .into_iter()
                .flatten()
                .filter(|t| t.started >= cutoff)
            {
                if t.success {
                    files += 1;
                    bytes = bytes.saturating_add(t.size);
                } else {
                    failures += 1;
                }
            }
            if limit.failures.is_some_and(|max| failures >= i64::from(max)) {
                return Err(format!("Too many failed transfers {period}"));
            }
            let (files, bytes) = exceeds(&limit, files, bytes, size);
            if files || bytes {
                return Err(format!(
                    "Too many {} {period}",
                    if files { "files" } else { "megabytes" }
                ));
            }
        }
        Ok(())
    }
}

fn exceeds(limit: &Limits, files: i64, bytes: u64, size: u64) -> (bool, bool) {
    (
        limit.files.is_some_and(|max| files + 1 > i64::from(max)),
        limit
            .megabytes
            .is_some_and(|max| bytes.saturating_add(size) > u64::from(max) * 1_000_000),
    )
}

#[derive(Default)]
pub(super) struct Bandwidth {
    groups: HashMap<String, Instant>,
}

impl Bandwidth {
    // The client owns the global limit. This timeline serializes reservations
    // for transfers that share the same group limit.
    pub(super) fn grant(
        &mut self,
        cfg: &Config,
        group: Option<&str>,
        bytes: usize,
    ) -> (usize, Instant) {
        let now = Instant::now();
        let Some(group) = group.filter(|group| *group != "privileged") else {
            return (bytes, now);
        };
        let Some(rate) = rate(group_options(cfg, group).speed_limit) else {
            self.groups.remove(group);
            return (bytes, now);
        };
        let bytes = bytes.min((rate as usize / 10).max(1));
        let duration = Duration::from_secs_f64(bytes as f64 / rate as f64);
        let ready = if let Some(next) = self.groups.get_mut(group) {
            *next = now.max(*next) + duration;
            *next
        } else {
            let next = now + duration;
            self.groups.insert(group.into(), next);
            next
        };
        (bytes, ready)
    }
}

pub(super) fn has_group_speed_limit(cfg: &Config) -> bool {
    let groups = &cfg.groups;
    std::iter::once(&groups.default)
        .chain([&groups.leechers])
        .chain(groups.custom.values())
        .any(|group| rate(group.speed_limit).is_some())
}

fn rate(kib: u32) -> Option<u64> {
    (kib > 0).then(|| u64::from(kib) * 1024)
}

#[cfg(test)]
mod tests {
    use slskr_types::transfers::TransferDirection;

    use super::*;
    use crate::transfers::new_transfer;

    #[test]
    fn applies_group_and_global_quotas_except_to_privileged_users() {
        let mut cfg = Config::default();
        cfg.uploads.limits.daily_files = 1;
        cfg.groups.default.limits.queued_megabytes = 1;
        let mut upload = new_transfer(
            TransferDirection::Upload,
            "peer",
            "first".into(),
            500_000,
            None,
        );
        let mut history = UploadHistory::default();
        assert_eq!(
            history.check([&upload].into_iter(), &cfg, "default", "peer", 500_001),
            Err("Too many megabytes".into())
        );
        upload.started_at = Some(Utc::now());
        upload.state = TransferState::Succeeded;
        history.record(&upload);
        assert_eq!(
            history.check(std::iter::empty(), &cfg, "default", "peer", 1),
            Err("Too many files today".into())
        );
        assert!(
            history
                .check(std::iter::empty(), &cfg, "privileged", "peer", 1)
                .is_ok()
        );
    }

    #[test]
    fn serializes_group_bandwidth_reservations() {
        let mut cfg = Config::default();
        cfg.groups.default.speed_limit = 10;
        let mut bandwidth = Bandwidth::default();
        let (first, ready) = bandwidth.grant(&cfg, Some("default"), 8_192);
        assert_eq!(first, 1_024);
        let (second, next) = bandwidth.grant(&cfg, Some("default"), 8_192);
        assert_eq!(second, 1_024);
        assert!(next >= ready + Duration::from_millis(100));
        assert_eq!(bandwidth.grant(&cfg, Some("privileged"), 8_192).0, 8_192);
    }
}
