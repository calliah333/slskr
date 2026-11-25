//! Always-on, low-cost profiling for long runs.
//!
//! Counts and times the work the daemon does over hours or days, so a slow or busy
//! instance can be explained from its own numbers:
//!
//! - every database call, per database and calling line: time waiting for the
//!   connection, and time running;
//! - every API request, per method and route;
//! - how transfers end, per direction and state, and uploads held back by the
//!   [backoff](crate::transfers);
//! - process CPU time and resident memory, and Tokio's live task count.
//!
//! The numbers are exported with the Prometheus metrics (`slskr_profile_*`), and every
//! [`REPORT_INTERVAL`] the busiest entries of that interval are logged at info under
//! `slskr::profiling`.

use std::{
    collections::HashMap,
    fmt::Write as _,
    panic::Location,
    sync::{
        Arc, LazyLock, Mutex,
        atomic::{AtomicI64, AtomicU64, Ordering::Relaxed},
    },
    time::{Duration, Instant},
};

use axum::{
    extract::{MatchedPath, Request},
    middleware::Next,
    response::Response,
};
use slskr_types::transfers::{TransferDirection, TransferState};
use tokio_util::sync::CancellationToken;

/// How often the busiest entries are logged.
pub const REPORT_INTERVAL: Duration = Duration::from_secs(15 * 60);
/// A database call that runs this long is logged when it ends.
const SLOW_DB_CALL: Duration = Duration::from_secs(1);
/// Entries listed per kind in a report.
const REPORT_TOP: usize = 8;

/// A monotonic counter.
pub struct Counter(AtomicU64);

impl Counter {
    const fn new() -> Self {
        Self(AtomicU64::new(0))
    }

    pub fn add(&self, value: u64) {
        self.0.fetch_add(value, Relaxed);
    }

    fn get(&self) -> u64 {
        self.0.load(Relaxed)
    }
}

/// Uploads held in the queue right now because their file kept failing for the user.
pub static UPLOADS_HOLDING: Gauge = Gauge(AtomicI64::new(0));

/// A value that goes up and down.
pub struct Gauge(AtomicI64);

impl Gauge {
    pub fn add(&self, value: i64) {
        self.0.fetch_add(value, Relaxed);
    }

    fn get(&self) -> i64 {
        self.0.load(Relaxed)
    }
}

/// Uploads that waited in the queue because their file kept failing for the user.
pub static UPLOADS_HELD: Counter = Counter::new();

/// Count, total and maximum of one kind of timed work.
#[derive(Default)]
pub struct Timing {
    count: AtomicU64,
    total_ns: AtomicU64,
    max_ns: AtomicU64,
}

impl Timing {
    pub fn record(&self, elapsed: Duration) {
        let ns = u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX);
        self.count.fetch_add(1, Relaxed);
        self.total_ns.fetch_add(ns, Relaxed);
        self.max_ns.fetch_max(ns, Relaxed);
    }

    fn read(&self) -> Sample {
        Sample {
            count: self.count.load(Relaxed),
            total_ns: self.total_ns.load(Relaxed),
            max_ns: self.max_ns.load(Relaxed),
        }
    }
}

#[derive(Clone, Copy, Default)]
struct Sample {
    count: u64,
    total_ns: u64,
    max_ns: u64,
}

impl Sample {
    fn since(self, before: Sample) -> Sample {
        Sample {
            count: self.count.saturating_sub(before.count),
            total_ns: self.total_ns.saturating_sub(before.total_ns),
            max_ns: self.max_ns,
        }
    }
}

/// Timings of one database call site.
#[derive(Default)]
pub struct DbSite {
    /// From the call until the connection was free (blocking-pool queue and lock).
    pub wait: Timing,
    pub run: Timing,
}

type DbKey = (Arc<str>, &'static Location<'static>);
/// Method and matched route.
type RouteKey = (String, String);
type TransferKey = (TransferDirection, TransferState);

static DB_SITES: LazyLock<Mutex<HashMap<DbKey, Arc<DbSite>>>> = LazyLock::new(Default::default);
static HTTP_ROUTES: LazyLock<Mutex<HashMap<RouteKey, Arc<Timing>>>> =
    LazyLock::new(Default::default);
/// Ended transfers per direction (download, upload) and state.
static TRANSFER_ENDS: LazyLock<Mutex<HashMap<TransferKey, u64>>> = LazyLock::new(Default::default);

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The timings of database calls on `database` from `site`.
pub fn db_site(database: &Arc<str>, site: &'static Location<'static>) -> Arc<DbSite> {
    lock(&DB_SITES)
        .entry((Arc::clone(database), site))
        .or_default()
        .clone()
}

/// Logs a database call that ran for [`SLOW_DB_CALL`] or longer.
pub fn db_call_ended(database: &str, site: &Location<'_>, wait: Duration, run: Duration) {
    if run >= SLOW_DB_CALL {
        tracing::info!(
            database,
            site = %site_name(site),
            wait_ms = wait.as_millis() as u64,
            run_ms = run.as_millis() as u64,
            "slow database call"
        );
    }
}

/// Records how a transfer ended.
pub fn transfer_ended(direction: TransferDirection, state: TransferState) {
    *lock(&TRANSFER_ENDS).entry((direction, state)).or_default() += 1;
}

/// Middleware timing each request by method and matched route.
pub async fn http_timing(request: Request, next: Next) -> Response {
    let route = request
        .extensions()
        .get::<MatchedPath>()
        .map_or_else(|| "unmatched".to_owned(), |path| path.as_str().to_owned());
    let method = request.method().as_str().to_owned();
    let started = Instant::now();
    let response = next.run(request).await;
    let timing = lock(&HTTP_ROUTES)
        .entry((method, route))
        .or_default()
        .clone();
    timing.record(started.elapsed());
    response
}

/// `crates/slskr/src/transfers/store.rs:42` → `transfers/store.rs:42`.
fn site_name(site: &Location<'_>) -> String {
    let file = site.file();
    let file = file
        .rsplit_once("/src/")
        .map_or(file, |(_, relative)| relative);
    format!("{file}:{}", site.line())
}

/// CPU seconds (user + system) and resident bytes of this process, where `/proc` has them.
fn process_usage() -> Option<(f64, u64)> {
    let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
    // Fields after the parenthesised command name; utime and stime are 14 and 15 overall.
    let fields: Vec<&str> = stat.rsplit_once(')')?.1.split_whitespace().collect();
    let ticks: u64 = fields.get(11)?.parse::<u64>().ok()? + fields.get(12)?.parse::<u64>().ok()?;
    let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
    let resident_pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
    // Linux reports both in fixed units on every platform this runs on.
    const TICKS_PER_SECOND: f64 = 100.0;
    const PAGE_SIZE: u64 = 4096;
    Some((ticks as f64 / TICKS_PER_SECOND, resident_pages * PAGE_SIZE))
}

fn alive_tasks() -> Option<usize> {
    tokio::runtime::Handle::try_current()
        .ok()
        .map(|handle| handle.metrics().num_alive_tasks())
}

fn escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Appends the profiling metrics in the Prometheus text format.
pub fn render(text: &mut String) {
    let mut family = |name: &str, kind: &str, help: &str| {
        let _ = writeln!(text, "# HELP {name} {help}\n# TYPE {name} {kind}");
    };
    family(
        "slskr_profile_db_calls",
        "counter",
        "Database calls, by database and call site",
    );
    family(
        "slskr_profile_db_wait_seconds",
        "counter",
        "Time database calls waited for the connection",
    );
    family(
        "slskr_profile_db_run_seconds",
        "counter",
        "Time database calls ran",
    );
    family(
        "slskr_profile_db_run_max_seconds",
        "gauge",
        "Longest run of a database call",
    );
    family(
        "slskr_profile_http_requests",
        "counter",
        "API requests, by method and route",
    );
    family(
        "slskr_profile_http_seconds",
        "counter",
        "Time spent answering API requests",
    );
    family(
        "slskr_profile_http_max_seconds",
        "gauge",
        "Longest API request",
    );
    family(
        "slskr_profile_transfers_ended",
        "counter",
        "Ended transfers, by direction and state",
    );
    family(
        "slskr_profile_uploads_held",
        "counter",
        "Uploads held back after repeated failures",
    );
    family(
        "slskr_profile_uploads_holding",
        "gauge",
        "Uploads held back right now",
    );
    family(
        "slskr_profile_cpu_seconds",
        "counter",
        "CPU time of the process",
    );
    family(
        "slskr_profile_resident_bytes",
        "gauge",
        "Resident memory of the process",
    );
    family("slskr_profile_tasks", "gauge", "Live Tokio tasks");

    let seconds = |ns: u64| ns as f64 / 1e9;
    for ((database, site), timing) in lock(&DB_SITES).iter() {
        let labels = format!(
            "database=\"{}\",site=\"{}\"",
            escape(database),
            escape(&site_name(site))
        );
        let (wait, run) = (timing.wait.read(), timing.run.read());
        let _ = writeln!(text, "slskr_profile_db_calls{{{labels}}} {}", run.count);
        let _ = writeln!(
            text,
            "slskr_profile_db_wait_seconds{{{labels}}} {}",
            seconds(wait.total_ns)
        );
        let _ = writeln!(
            text,
            "slskr_profile_db_run_seconds{{{labels}}} {}",
            seconds(run.total_ns)
        );
        let _ = writeln!(
            text,
            "slskr_profile_db_run_max_seconds{{{labels}}} {}",
            seconds(run.max_ns)
        );
    }
    for ((method, route), timing) in lock(&HTTP_ROUTES).iter() {
        let labels = format!("method=\"{method}\",route=\"{}\"", escape(route));
        let sample = timing.read();
        let _ = writeln!(
            text,
            "slskr_profile_http_requests{{{labels}}} {}",
            sample.count
        );
        let _ = writeln!(
            text,
            "slskr_profile_http_seconds{{{labels}}} {}",
            seconds(sample.total_ns)
        );
        let _ = writeln!(
            text,
            "slskr_profile_http_max_seconds{{{labels}}} {}",
            seconds(sample.max_ns)
        );
    }
    for ((direction, state), count) in lock(&TRANSFER_ENDS).iter() {
        let _ = writeln!(
            text,
            "slskr_profile_transfers_ended{{direction=\"{}\",state=\"{}\"}} {count}",
            direction.as_str(),
            state.as_str()
        );
    }
    let _ = writeln!(text, "slskr_profile_uploads_held {}", UPLOADS_HELD.get());
    let _ = writeln!(
        text,
        "slskr_profile_uploads_holding {}",
        UPLOADS_HOLDING.get()
    );
    if let Some((cpu, resident)) = process_usage() {
        let _ = writeln!(text, "slskr_profile_cpu_seconds {cpu}");
        let _ = writeln!(text, "slskr_profile_resident_bytes {resident}");
    }
    if let Some(tasks) = alive_tasks() {
        let _ = writeln!(text, "slskr_profile_tasks {tasks}");
    }
}

/// Everything a report compares with the previous one.
#[derive(Default)]
struct Snapshot {
    db: HashMap<DbKey, (Sample, Sample)>,
    http: HashMap<RouteKey, Sample>,
    transfers: HashMap<TransferKey, u64>,
    held: u64,
    cpu: f64,
}

impl Snapshot {
    fn take() -> Self {
        Self {
            db: lock(&DB_SITES)
                .iter()
                .map(|(key, site)| (key.clone(), (site.wait.read(), site.run.read())))
                .collect(),
            http: lock(&HTTP_ROUTES)
                .iter()
                .map(|(key, timing)| (key.clone(), timing.read()))
                .collect(),
            transfers: lock(&TRANSFER_ENDS).clone(),
            held: UPLOADS_HELD.get(),
            cpu: process_usage().map_or(0.0, |(cpu, _)| cpu),
        }
    }
}

fn ms(ns: u64) -> u64 {
    ns / 1_000_000
}

/// The report of what happened between `before` and `now`, over `elapsed`.
fn report(before: &Snapshot, now: &Snapshot, elapsed: Duration) -> String {
    let mut text = String::new();
    let cpu = now.cpu - before.cpu;
    let _ = write!(
        text,
        "cpu={cpu:.1}s ({:.1}%)",
        100.0 * cpu / elapsed.as_secs_f64().max(1.0)
    );
    if let Some((_, resident)) = process_usage() {
        let _ = write!(text, " rss={}MiB", resident >> 20);
    }
    if let Some(tasks) = alive_tasks() {
        let _ = write!(text, " tasks={tasks}");
    }
    let _ = write!(
        text,
        " uploads_held={} holding={}",
        now.held - before.held,
        UPLOADS_HOLDING.get()
    );

    let mut ends: Vec<_> = now
        .transfers
        .iter()
        .map(|(key, count)| (key, count - before.transfers.get(key).copied().unwrap_or(0)))
        .filter(|(_, count)| *count > 0)
        .collect();
    ends.sort_by_key(|(_, count)| std::cmp::Reverse(*count));
    text.push_str("\n  transfers:");
    for ((direction, state), count) in ends {
        let _ = write!(text, " {}/{}={count}", direction.as_str(), state.as_str());
    }

    let mut db: Vec<_> = now
        .db
        .iter()
        .map(|(key, (wait, run))| {
            let (wait0, run0) = before.db.get(key).copied().unwrap_or_default();
            (key, wait.since(wait0), run.since(run0))
        })
        .filter(|(_, _, run)| run.count > 0)
        .collect();
    db.sort_by_key(|(_, wait, run)| std::cmp::Reverse(wait.total_ns + run.total_ns));
    let calls: u64 = db.iter().map(|(_, _, run)| run.count).sum();
    let busy: u64 = db.iter().map(|(_, _, run)| run.total_ns).sum();
    let _ = write!(text, "\n  db: calls={calls} run={}ms", ms(busy));
    for ((database, site), wait, run) in db.into_iter().take(REPORT_TOP) {
        let _ = write!(
            text,
            "\n    {database} {}: calls={} run={}ms wait={}ms max_run={}ms",
            site_name(site),
            run.count,
            ms(run.total_ns),
            ms(wait.total_ns),
            ms(run.max_ns)
        );
    }

    let mut http: Vec<_> = now
        .http
        .iter()
        .map(|(key, sample)| {
            (
                key,
                sample.since(before.http.get(key).copied().unwrap_or_default()),
            )
        })
        .filter(|(_, sample)| sample.count > 0)
        .collect();
    http.sort_by_key(|(_, sample)| std::cmp::Reverse(sample.total_ns));
    let requests: u64 = http.iter().map(|(_, sample)| sample.count).sum();
    let _ = write!(text, "\n  http: requests={requests}");
    for ((method, route), sample) in http.into_iter().take(REPORT_TOP) {
        let _ = write!(
            text,
            "\n    {method} {route}: requests={} total={}ms max={}ms",
            sample.count,
            ms(sample.total_ns),
            ms(sample.max_ns)
        );
    }
    text
}

/// Logs a report every [`REPORT_INTERVAL`] until `shutdown`.
pub fn start(shutdown: CancellationToken) {
    tokio::spawn(async move {
        let mut before = Snapshot::take();
        let mut since = Instant::now();
        let mut ticker = tokio::time::interval_at(
            tokio::time::Instant::now() + REPORT_INTERVAL,
            REPORT_INTERVAL,
        );
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => break,
                _ = ticker.tick() => {
                    let now = Snapshot::take();
                    let elapsed = since.elapsed();
                    tracing::info!(
                        minutes = elapsed.as_secs() / 60,
                        "profile\n  {}",
                        report(&before, &now, elapsed)
                    );
                    before = now;
                    since = Instant::now();
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_and_exports_what_was_recorded() {
        let database: Arc<str> = "profiling-test".into();
        let before = Snapshot::take();
        let site = db_site(&database, Location::caller());
        site.wait.record(Duration::from_millis(2));
        site.run.record(Duration::from_millis(30));
        transfer_ended(TransferDirection::Upload, TransferState::Rejected);
        UPLOADS_HELD.add(1);
        let now = Snapshot::take();

        let report = report(&before, &now, Duration::from_secs(60));
        assert!(report.contains("upload/rejected="), "{report}");
        assert!(
            report.contains("profiling-test profiling.rs:")
                && report.contains("calls=1 run=30ms wait=2ms"),
            "{report}"
        );

        let mut text = String::new();
        render(&mut text);
        assert!(text.contains("# TYPE slskr_profile_db_run_seconds counter\n"));
        assert!(
            text.contains(
                "slskr_profile_db_calls{database=\"profiling-test\",site=\"profiling.rs:"
            )
        );
        assert!(
            text.contains("slskr_profile_transfers_ended{direction=\"upload\",state=\"rejected\"}")
        );
    }

    #[test]
    fn site_names_are_relative_to_the_crate_source() {
        assert!(site_name(Location::caller()).starts_with("profiling.rs:"));
    }
}
