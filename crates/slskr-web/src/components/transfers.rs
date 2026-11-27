//! Downloads and uploads (`/downloads`, `/uploads`).
//!
//! The list is fetched on load, on reconnect and every 30 s (every second while live
//! updates are unavailable); the `transfers` topic applies changes in between
//! (`live::LiveList`). Rendering stays cheap with thousands of transfers: each transfer
//! lives in its own signal (`Page::rows`) that is only written when that transfer changed,
//! so a progress tick re-renders one row. The grouped and filtered
//! layout (`Page::layout`) is a memo over a slim index (id, user, path, size, state) that
//! changes only when a transfer is added, removed or changes state; the keyed `For`
//! lists below it keep existing rows in place.

mod groups;
mod live;

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use chrono::{DateTime, Utc};
use gloo_net::http::Method;
use leptos::{prelude::*, task::spawn_local_scoped_with_cancellation};
use slskr_types::shares::Share;
use slskr_types::transfers::{
    CompletedFilter, DownloadFile, EnqueueDownloads, EnqueueDownloadsRequest, EnqueuedDownloads,
    Transfer, TransferDirection, TransferListSummary, TransferState as State,
};

use crate::{
    components::app::AppContext,
    config,
    services::{
        api,
        events::{Event, Status as EventsStatus, Topic, use_events},
        util,
    },
    ui::{
        Alert, Badge, Button, DropdownMenu, EmptyState, Icon, Loading, Modal, ModalAction, Pager,
    },
};

/// Local storage key: whether transfer folders start collapsed (downloads and uploads).
const COLLAPSED_KEY: &str = "slskr-transfer-folders-collapsed";
/// Local storage key: whether transfer lists use compact rows (downloads and uploads).
const COMPACT_KEY: &str = "slskr-transfers-compact";

/// Where a transfer is in its life, for counts and the state filter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Category {
    Active,
    Queued,
    Succeeded,
    Failed,
    Cancelled,
}

impl Category {
    fn of(state: State) -> Self {
        match state {
            State::Requested | State::Initializing | State::InProgress => Self::Active,
            State::QueuedLocally | State::QueuedRemotely => Self::Queued,
            State::Succeeded => Self::Succeeded,
            State::Cancelled => Self::Cancelled,
            State::TimedOut | State::Errored | State::Rejected | State::Aborted => Self::Failed,
        }
    }
}

/// The state filter (segmented control).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Filter {
    All,
    Active,
    Queued,
    Completed,
    Failed,
}

impl Filter {
    const ALL: [Self; 5] = [
        Self::All,
        Self::Active,
        Self::Queued,
        Self::Completed,
        Self::Failed,
    ];

    fn value(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Active => "active",
            Self::Queued => "queued",
            Self::Completed => "completed",
            Self::Failed => "failed",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::Active => "Active",
            Self::Queued => "Queued",
            Self::Completed => "Completed",
            Self::Failed => "Failed",
        }
    }

    /// Which completed transfers the daemon pages through for this filter.
    fn completed(self) -> CompletedFilter {
        match self {
            Self::All => CompletedFilter::All,
            Self::Active | Self::Queued => CompletedFilter::None,
            Self::Completed => CompletedFilter::Succeeded,
            Self::Failed => CompletedFilter::Failed,
        }
    }

    fn matches(self, category: Category) -> bool {
        match self {
            Self::All => true,
            Self::Active => category == Category::Active,
            Self::Queued => category == Category::Queued,
            Self::Completed => category == Category::Succeeded,
            Self::Failed => matches!(category, Category::Failed | Category::Cancelled),
        }
    }
}

/// The slim, rarely changing part of a transfer: what grouping, filtering, counts and
/// bulk actions need. Progress fields stay out so a progress tick leaves it unchanged.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Entry {
    id: String,
    username: String,
    filename: String,
    root: Option<String>,
    size: u64,
    state: State,
    removed: bool,
    /// Lower-case `username\nfilename` for the text filter.
    haystack: String,
}

impl Entry {
    fn new(file: &Transfer) -> Self {
        Self {
            id: file.id.clone(),
            username: file.username.clone(),
            filename: file.filename.clone(),
            root: file.root.clone(),
            size: file.size,
            state: file.state,
            removed: file.removed,
            haystack: format!("{}\n{}", file.username, file.filename).to_lowercase(),
        }
    }

    fn category(&self) -> Category {
        Category::of(self.state)
    }

    fn matches(&self, file: &Transfer) -> bool {
        self.id == file.id
            && self.username == file.username
            && self.filename == file.filename
            && self.root == file.root
            && self.size == file.size
            && self.state == file.state
            && self.removed == file.removed
    }
}

/// The page's counts: active transfers from the loaded rows (every one is loaded, and kept
/// current by events), completed ones from the daemon's summary of the whole list. Before
/// the summary arrives, the loaded rows alone.
fn count(index: &[Entry], summary: Option<&TransferListSummary>) -> Counts {
    let mut counts = Counts::default();
    for entry in index
        .iter()
        .filter(|entry| summary.is_none() || !entry.state.is_complete())
    {
        counts.add(entry);
    }
    for state in summary.into_iter().flat_map(|summary| &summary.states) {
        if !state.state.is_complete() {
            continue;
        }
        let files = usize::try_from(state.files).unwrap_or(usize::MAX);
        counts.files = counts.files.saturating_add(files);
        counts.bytes = counts.bytes.saturating_add(state.bytes);
        match Category::of(state.state) {
            Category::Succeeded => counts.succeeded += files,
            Category::Cancelled => counts.cancelled += files,
            _ => counts.failed += files,
        }
    }
    counts
}

/// Transfer counts by category (removed transfers excluded).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Counts {
    files: usize,
    bytes: u64,
    active: usize,
    queued: usize,
    succeeded: usize,
    failed: usize,
    cancelled: usize,
}

impl Counts {
    fn merge(&mut self, other: &Self) {
        self.files += other.files;
        self.bytes = self.bytes.saturating_add(other.bytes);
        self.active += other.active;
        self.queued += other.queued;
        self.succeeded += other.succeeded;
        self.failed += other.failed;
        self.cancelled += other.cancelled;
    }

    fn add(&mut self, entry: &Entry) {
        if entry.removed {
            return;
        }
        self.files += 1;
        self.bytes = self.bytes.saturating_add(entry.size);
        match entry.category() {
            Category::Active => self.active += 1,
            Category::Queued => self.queued += 1,
            Category::Succeeded => self.succeeded += 1,
            Category::Failed => self.failed += 1,
            Category::Cancelled => self.cancelled += 1,
        }
    }
}

/// One visible file of a directory group.
#[derive(Clone, Debug, PartialEq)]
struct FileRef {
    id: String,
    removed: bool,
}

#[derive(Clone, Debug, PartialEq)]
struct DirLayout {
    directory: String,
    files: Vec<FileRef>,
    counts: Counts,
}

/// One user's visible transfers, grouped by remote directory.
#[derive(Clone, Debug, PartialEq)]
struct UserLayout {
    username: String,
    directories: Vec<DirLayout>,
    counts: Counts,
}

fn directory_of(filename: &str) -> &str {
    filename
        .rsplit_once(['\\', '/'])
        .map_or("", |(directory, _)| directory)
}

/// Groups the matching entries by user, then by remote directory, in first-seen order;
/// files are sorted by name within a directory.
fn build_layout(index: &[Entry], filter: Filter, query: &str) -> Vec<UserLayout> {
    let query = query.trim().to_lowercase();
    let mut users = Vec::<UserLayout>::new();
    let mut user_positions = HashMap::<&str, usize>::new();
    let mut dir_positions = HashMap::<(usize, &str), usize>::new();
    let mut names = HashMap::<&str, &str>::new();
    for entry in index {
        if !filter.matches(entry.category())
            || (!query.is_empty() && !entry.haystack.contains(&query))
        {
            continue;
        }
        let user = *user_positions
            .entry(entry.username.as_str())
            .or_insert_with(|| {
                users.push(UserLayout {
                    username: entry.username.clone(),
                    directories: Vec::new(),
                    counts: Counts::default(),
                });
                users.len() - 1
            });
        let directory = directory_of(&entry.filename);
        let layout = &mut users[user];
        layout.counts.add(entry);
        let position = *dir_positions.entry((user, directory)).or_insert_with(|| {
            layout.directories.push(DirLayout {
                directory: directory.to_owned(),
                files: Vec::new(),
                counts: Counts::default(),
            });
            layout.directories.len() - 1
        });
        layout.directories[position].counts.add(entry);
        names.insert(&entry.id, util::get_file_name(&entry.filename));
        layout.directories[position].files.push(FileRef {
            id: entry.id.clone(),
            removed: entry.removed,
        });
    }
    for user in &mut users {
        for directory in &mut user.directories {
            directory.files.sort_unstable_by(|a, b| {
                names[a.id.as_str()]
                    .cmp(names[b.id.as_str()])
                    .then_with(|| a.id.cmp(&b.id))
            });
        }
    }
    users
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Operation {
    Retry,
    Cancel,
    Remove,
}

impl Operation {
    fn allows(self, direction: TransferDirection, entry: &Entry) -> bool {
        self.permits(direction, entry.state, entry.removed)
    }

    fn permits(self, direction: TransferDirection, state: State, removed: bool) -> bool {
        if removed {
            return false;
        }
        match self {
            Self::Retry => direction == TransferDirection::Download && state.is_failed(),
            Self::Cancel => !state.is_complete(),
            Self::Remove => state.is_complete(),
        }
    }
}

type Change = Callback<(Operation, Vec<Entry>)>;

/// Everything the group, directory and row components share. `Copy`, so it is passed by
/// value instead of a dozen props.
#[derive(Clone, Copy)]
struct Page {
    direction: TransferDirection,
    /// One signal per transfer, written only when that transfer changed.
    rows: StoredValue<HashMap<String, ArcRwSignal<Transfer>>>,
    index: RwSignal<Vec<Entry>>,
    layout: Memo<Vec<UserLayout>>,
    selected: RwSignal<HashSet<String>>,
    working: RwSignal<Option<Operation>>,
    connected: Signal<bool>,
    change: Change,
    /// The transfer shown in the details dialog.
    details: RwSignal<Option<String>>,
    /// Refreshed every 30 s for relative times ("2h ago").
    now: RwSignal<DateTime<Utc>>,
    /// Whether folders start collapsed (remembered; a folder's own toggle overrides it).
    collapse: RwSignal<bool>,
    /// Uploads: the name peers see for the one shared folder, when there is only one.
    /// Folder rows leave it out, since every upload is in it.
    share_root: RwSignal<Option<String>>,
}

impl Page {
    /// Entries of the given ids (in index order).
    fn entries(self, ids: &HashSet<String>) -> Vec<Entry> {
        self.index.with_untracked(|index| {
            index
                .iter()
                .filter(|entry| ids.contains(&entry.id))
                .cloned()
                .collect()
        })
    }

    /// Entries of one user (or all users) that the operation applies to and that match
    /// `keep`.
    fn targets(
        self,
        operation: Operation,
        username: Option<&str>,
        keep: impl Fn(&Entry) -> bool,
    ) -> Vec<Entry> {
        self.index.with_untracked(|index| {
            index
                .iter()
                .filter(|entry| username.is_none_or(|name| entry.username == name))
                .filter(|entry| operation.allows(self.direction, entry) && keep(entry))
                .cloned()
                .collect()
        })
    }

    fn base(self) -> String {
        config::url_base().trim_end_matches('/').to_owned()
    }
}

fn collection(direction: TransferDirection) -> String {
    format!("/transfers/{}", direction.collection())
}

fn transfer_url(direction: TransferDirection, id: &str) -> String {
    format!(
        "{}/{}",
        collection(direction),
        util::encode_uri_component(id)
    )
}

#[component]
pub fn Transfers(direction: TransferDirection) -> impl IntoView {
    let state = expect_context::<AppContext>().state;
    let connected = Signal::derive(move || state.with(|state| state.server.state.is_connected()));
    let rows = StoredValue::new(HashMap::<String, ArcRwSignal<Transfer>>::new());
    let index = RwSignal::new(Vec::<Entry>::new());
    let speed = RwSignal::new(0.0_f64);
    let loading = RwSignal::new(true);
    let load_error = RwSignal::new(None::<String>);
    let action_error = RwSignal::new(None::<String>);
    let working = RwSignal::new(None::<Operation>);
    let selected = RwSignal::new(HashSet::<String>::new());
    let details = RwSignal::new(None::<String>);
    let now = RwSignal::new(Utc::now());
    let filter = RwSignal::new(Filter::All);
    let query = RwSignal::new(String::new());
    let confirm_cancel = RwSignal::new(false);
    let is_download = direction == TransferDirection::Download;
    let noun = if is_download { "download" } else { "upload" };

    // The text filter applies to the loaded rows at once, and to the daemon's list (so
    // that pages hold only matches) once typing pauses.
    let server_query = RwSignal::new(String::new());
    let query_edits = StoredValue::new(0_u64);
    Effect::new(move |_| {
        let text = query.get().trim().to_lowercase();
        let edit = query_edits.get_value().wrapping_add(1);
        query_edits.set_value(edit);
        spawn_local_scoped_with_cancellation(async move {
            util::sleep(300).await;
            if query_edits.try_get_value() == Some(edit) && server_query.get_untracked() != text {
                server_query.set(text);
            }
        });
    });
    // The daemon's counts of the whole list; the loaded rows are one page of it.
    let summary = RwSignal::new(None::<TransferListSummary>);

    let layout = Memo::new(move |_| {
        index.with(|index| query.with(|query| build_layout(index, filter.get(), query)))
    });
    let counts = Memo::new(move |_| {
        index.with(|index| summary.with(|summary| count(index, summary.as_ref())))
    });

    // Selection follows what is visible: rows hidden by a filter (or gone) drop out.
    Effect::new(move |_| {
        layout.with(|layout| {
            let visible = layout
                .iter()
                .flat_map(|user| &user.directories)
                .flat_map(|directory| &directory.files)
                .filter(|file| !file.removed)
                .map(|file| file.id.as_str())
                .collect::<HashSet<_>>();
            if selected
                .with_untracked(|selected| selected.iter().any(|id| !visible.contains(id.as_str())))
            {
                selected.update(|selected| selected.retain(|id| visible.contains(id.as_str())));
            }
        });
    });

    // Every active transfer is listed, and one page of the completed ones (newest first):
    // retention keeps completed transfers forever by default.
    const PAGE: u32 = 200;
    let page_index = RwSignal::new(0_u32);
    let pages = Signal::derive(move || {
        let completed = counts.with(|c| match filter.get() {
            Filter::All => c.succeeded + c.failed + c.cancelled,
            Filter::Completed => c.succeeded,
            Filter::Failed => c.failed + c.cancelled,
            Filter::Active | Filter::Queued => 0,
        });
        u32::try_from(completed)
            .unwrap_or(u32::MAX)
            .div_ceil(PAGE)
            .max(1)
    });
    // Another view starts at its first page.
    Effect::new(move |previous: Option<()>| {
        filter.track();
        server_query.track();
        if previous.is_some() {
            page_index.set(0);
        }
    });

    // Actions run in the page's owner, not the clicked control's: a menu, the selection
    // bar or a row can unmount while the requests are still running.
    let owner = StoredValue::new(Owner::current().expect("the page has an owner"));
    let in_page = move |f: Box<dyn FnOnce()>| owner.with_value(|owner| owner.with(f));

    // The list is fetched on load, on reconnect and every 30 s; the `transfers` topic
    // keeps it current in between. A hidden page keeps the list but renders it when shown.
    let live = StoredValue::new(live::LiveList::new(direction));
    let stale = StoredValue::new(false);
    let render = move || {
        if document().hidden() {
            stale.set_value(true);
        } else {
            stale.set_value(false);
            live.with_value(|live| apply_snapshot(live.list(), rows, index, speed));
        }
    };
    let text_param = move || {
        let text = server_query.get_untracked();
        if text.is_empty() {
            String::new()
        } else {
            format!("&query={}", util::encode_uri_component(&text))
        }
    };
    // The summary follows the list: with each fetch, and soon after transfers complete.
    let summaries = StoredValue::new(0_u64);
    let summary_due = StoredValue::new(false);
    let fetch_summary = move || {
        summary_due.set_value(false);
        let ticket = summaries.get_value().wrapping_add(1);
        summaries.set_value(ticket);
        let url = format!(
            "{}/summary?include_removed=false{}",
            collection(direction),
            text_param()
        );
        in_page(Box::new(move || {
            spawn_local_scoped_with_cancellation(async move {
                let result = api::get::<TransferListSummary>(&url).await;
                if summaries.try_get_value() == Some(ticket)
                    && let Ok(result) = result
                    && summary.with_untracked(|current| current.as_ref() != Some(&result))
                {
                    summary.set(Some(result));
                }
            });
        }));
    };
    let validator = StoredValue::new(None::<(String, String)>);
    let fetches = StoredValue::new(0_u64);
    let fetch = move || {
        let ticket = fetches.get_value().wrapping_add(1);
        fetches.set_value(ticket);
        let page = page_index.get_untracked();
        let url = format!(
            "{}?include_removed=false&recent_completed={PAGE}&completed_offset={}&completed={}{}",
            collection(direction),
            page.saturating_mul(PAGE),
            filter.get_untracked().completed().as_str(),
            text_param()
        );
        let etag = validator.with_value(|validator| {
            validator
                .as_ref()
                .filter(|(path, _)| *path == url)
                .map(|(_, etag)| etag.clone())
        });
        live.update_value(|live| {
            live.set_first_page(page == 0);
            live.begin_fetch();
        });
        fetch_summary();
        in_page(Box::new(move || {
            spawn_local_scoped_with_cancellation(async move {
                let snapshot = api::get_if_changed::<Vec<Transfer>>(&url, etag.as_deref()).await;
                // A newer fetch (or a view change) supersedes this one.
                if fetches.try_get_value() != Some(ticket) {
                    return;
                }
                match snapshot {
                    Ok(Some((list, etag))) => {
                        live.update_value(|live| live.finish_fetch(list));
                        render();
                        validator.set_value(etag.map(|etag| (url, etag)));
                        if load_error.with_untracked(Option::is_some) {
                            load_error.set(None);
                        }
                    }
                    Ok(None) => {
                        live.update_value(live::LiveList::end_fetch);
                        if load_error.with_untracked(Option::is_some) {
                            load_error.set(None);
                        }
                    }
                    Err(error) => {
                        live.update_value(live::LiveList::end_fetch);
                        let message = error.to_string();
                        if load_error.with_untracked(|previous| previous.as_ref() != Some(&message))
                        {
                            load_error.set(Some(message));
                        }
                    }
                }
                if loading.get_untracked() {
                    loading.set(false);
                }
            });
        }));
    };
    // A new page starts at the top.
    Effect::new(move |previous: Option<u32>| {
        let page = page_index.get();
        if previous.is_some_and(|previous| previous != page) {
            window().scroll_to_with_x_and_y(0.0, 0.0);
        }
        page
    });
    // The first load, and again when the view changes.
    Effect::new(move |_| {
        page_index.track();
        filter.track();
        server_query.track();
        fetch();
    });
    let events = use_events();
    events.on(Topic::Transfers, move |event| match event {
        // (Re)subscribed: possibly after missed events, so fetch again too.
        Event::TransferList(files) => {
            live.update_value(|live| live.update(files));
            render();
            if !live.with_value(live::LiveList::is_fetching) {
                fetch();
            }
        }
        Event::TransferUpdated(files) => {
            live.update_value(|live| live.update(files));
            render();
            // A transfer of this list completed (or was added): its counts changed.
            if files.iter().any(|file| {
                file.direction == direction && (file.state.is_complete() || file.removed)
            }) {
                summary_due.set_value(true);
            }
        }
        Event::TransferRemoved { ids } => {
            live.update_value(|live| live.remove(ids));
            render();
            summary_due.set_value(true);
        }
        _ => {}
    });
    let socket = events.status();
    spawn_local_scoped_with_cancellation(async move {
        let mut ticks = 0_u32;
        let mut since_fetch = 0_u32;
        loop {
            util::sleep(1_000).await;
            if document().hidden() {
                continue;
            }
            ticks = ticks.wrapping_add(1);
            since_fetch += 1;
            if ticks.is_multiple_of(30) {
                now.set(Utc::now());
            }
            if stale.get_value() {
                render();
            }
            // Completions come in bursts; count them at most every 3 s.
            if summary_due.get_value() && ticks.is_multiple_of(3) {
                fetch_summary();
            }
            // Without live updates, poll every second as a fallback.
            if since_fetch >= 30 || socket.get_untracked() != EventsStatus::Open {
                since_fetch = 0;
                fetch();
            }
        }
    });

    let change = Callback::new(move |(operation, files): (Operation, Vec<Entry>)| {
        if working.with_untracked(Option::is_some)
            || (operation == Operation::Retry && !connected.get_untracked())
        {
            return;
        }
        let files = files
            .into_iter()
            .filter(|file| operation.allows(direction, file))
            .collect::<Vec<_>>();
        if files.is_empty() {
            return;
        }
        working.set(Some(operation));
        action_error.set(None);
        in_page(Box::new(move || {
            spawn_local_scoped_with_cancellation(async move {
                let errors = match operation {
                    Operation::Retry => retry(files).await,
                    Operation::Cancel | Operation::Remove => {
                        change_transfers(direction, operation, files).await
                    }
                };
                if !errors.is_empty() {
                    action_error.set(Some(errors.into_iter().collect::<Vec<_>>().join("; ")));
                }
                working.set(None);
            });
        }));
    });

    let page = Page {
        direction,
        rows,
        index,
        layout,
        selected,
        working,
        connected,
        change,
        details,
        now,
        collapse: util::stored_flag(COLLAPSED_KEY),
        share_root: RwSignal::new(None),
    };
    if !is_download {
        spawn_local_scoped_with_cancellation(async move {
            if let Ok(shares) = api::get::<Vec<Share>>("/shares").await {
                page.share_root.set(single_share_root(&shares));
            }
        });
    }

    let clear_completed = move || {
        if working.with_untracked(Option::is_some) {
            return;
        }
        working.set(Some(Operation::Remove));
        action_error.set(None);
        in_page(Box::new(move || {
            spawn_local_scoped_with_cancellation(async move {
                let url = format!("{}/completed", collection(direction));
                if let Err(error) = api::request_text(Method::DELETE, &url, None).await {
                    action_error.set(Some(error.to_string()));
                }
                working.set(None);
            });
        }));
    };

    let compact = util::stored_flag(COMPACT_KEY);
    let busy = Signal::derive(move || working.with(Option::is_some));
    let has_failed = Signal::derive(move || counts.with(|counts| counts.failed > 0));
    let has_open = Signal::derive(move || counts.with(|c| c.active + c.queued > 0));
    let has_completed =
        Signal::derive(move || counts.with(|c| c.succeeded + c.failed + c.cancelled > 0));
    let header_actions = move || {
        view! {
            {is_download.then(|| view! {
                <Button icon="rotate-cw" text="Retry failed"
                    disabled=Signal::derive(move || busy.get() || !connected.get() || !has_failed.get())
                    loading=Signal::derive(move || working.get() == Some(Operation::Retry))
                    on_click=Callback::new(move |_| change.run((Operation::Retry,
                        page.targets(Operation::Retry, None, |entry| entry.category() == Category::Failed))))/>
            })}
            <Button variant="danger-outline" icon="circle-x" text="Cancel all"
                disabled=Signal::derive(move || busy.get() || !has_open.get())
                loading=Signal::derive(move || working.get() == Some(Operation::Cancel))
                on_click=Callback::new(move |_| confirm_cancel.set(true))/>
            <DropdownMenu label="Clear finished transfers" text=Signal::stored("Clear".to_owned()) icon="trash-2" align="end">
                <button type="button" class="menu__item" role="menuitem"
                    disabled=move || busy.get() || counts.with(|c| c.succeeded == 0)
                    on:click=move |_| change.run((Operation::Remove,
                        page.targets(Operation::Remove, None, |entry| entry.state == State::Succeeded)))>
                    <Icon name="circle-check"/>"Clear shown succeeded"
                </button>
                <button type="button" class="menu__item" role="menuitem"
                    disabled=move || busy.get() || counts.with(|c| c.failed + c.cancelled == 0)
                    on:click=move |_| change.run((Operation::Remove,
                        page.targets(Operation::Remove, None, |entry| entry.state.is_failed())))>
                    <Icon name="circle-alert"/>"Clear shown failed and cancelled"
                </button>
                <div class="menu__separator"></div>
                <button type="button" class="menu__item" role="menuitem"
                    disabled=move || busy.get() || !has_completed.get()
                    on:click=move |_| clear_completed()>
                    <Icon name="trash-2"/>"Clear all completed"
                </button>
            </DropdownMenu>
        }
    };

    let title = if is_download { "Downloads" } else { "Uploads" };
    let filter_value = Signal::derive(move || filter.get().value().to_owned());
    let empty = Signal::derive(move || index.with(Vec::is_empty));
    let no_matches = Signal::derive(move || layout.with(Vec::is_empty));
    let base = page.base();
    let (empty_title, empty_href, empty_icon, empty_action) = if is_download {
        (
            "No downloads yet",
            format!("{base}/searches"),
            "search",
            "Go to Search",
        )
    } else {
        (
            "No uploads yet",
            format!("{base}/system/shares"),
            "share-2",
            "Manage shares",
        )
    };

    view! {
        <div class="transfers stack" class:transfers--busy=busy class:transfers--compact=compact>
            <div class="page-header transfers-header">
                <div class="stack stack--sm">
                    <h1 class="sr-only">{title}</h1>
                    <div class="cluster transfers-summary" aria-live="polite">
                        {move || counts.with(|c| summary_badges(*c, speed.get(), is_download))}
                    </div>
                </div>
                <div class="page-header__actions">{header_actions}</div>
            </div>

            <div class="card card--compact transfers-toolbar">
                <div class="card__content toolbar">
                    <div class="input-icon grow">
                        <Icon name="search"/>
                        <input class="input" type="search" aria-label=format!("Filter {noun}s")
                            placeholder="Filter by user or file name"
                            prop:value=query on:input=move |event| query.set(event_target_value(&event))/>
                    </div>
                    <div class="tabs transfers-filter" role="group" aria-label="Filter by state">
                        {Filter::ALL.into_iter().map(|option| {
                            let selected = Memo::new(move |_| filter.get() == option);
                            let count = move || counts.with(|c| match option {
                                Filter::All => c.files,
                                Filter::Active => c.active,
                                Filter::Queued => c.queued,
                                Filter::Completed => c.succeeded,
                                Filter::Failed => c.failed + c.cancelled,
                            });
                            view! {
                                <button type="button" class="tabs__tab" class:is-active=selected
                                    data-filter=option.value()
                                    title=move || format!("{} {}", util::group_digits(count()), option.label().to_lowercase())
                                    aria-pressed=move || selected.get().to_string()
                                    on:click=move |_| filter.set(option)>
                                    {option.label()}
                                    <span class="transfers-tab-count tabular">{move || util::short_count(count())}</span>
                                </button>
                            }
                        }).collect_view()}
                    </div>
                    <div class="btn-group transfers-view">
                        <Button variant="ghost"
                            icon=Signal::derive(move || if page.collapse.get() { "chevrons-up-down".to_owned() } else { "chevrons-down-up".to_owned() })
                            label=Signal::derive(move || if page.collapse.get() { "Expand folders".to_owned() } else { "Collapse folders".to_owned() })
                            on_click=Callback::new(move |_| page.collapse.update(|value| *value = !*value))/>
                        <Button variant="ghost" icon="rows-4" label="Compact rows" pressed=compact
                            on_click=Callback::new(move |_| compact.update(|value| *value = !*value))/>
                    </div>
                </div>
            </div>

            {move || load_error.get().map(|error| view! {
                <Alert variant="danger" title=format!("Could not load {noun}s")>{error}</Alert>
            })}
            {move || action_error.get().map(|error| view! {
                <Alert variant="danger" title="Some actions failed"
                    on_dismiss=Callback::new(move |()| action_error.set(None))>{error}</Alert>
            })}

            {move || if loading.get() {
                view! { <div class="card"><Loading label=format!("Loading {noun}s") show_label=true/></div> }.into_any()
            } else if empty.get() && load_error.with(Option::is_none) {
                let href = empty_href.clone();
                view! {
                    <div class="card transfers-empty">
                        <EmptyState icon=if is_download { "download" } else { "upload" }
                            title=empty_title>
                            <Button variant="primary" icon=empty_icon text=empty_action href=href/>
                        </EmptyState>
                    </div>
                }.into_any()
            } else if no_matches.get() && !empty.get() {
                view! {
                    <div class="card transfers-empty">
                        <EmptyState icon="list-filter" title=format!("No {noun}s match")>
                            <Button text="Clear filters" icon="x" on_click=Callback::new(move |_| {
                                query.set(String::new());
                                filter.set(Filter::All);
                            })/>
                        </EmptyState>
                    </div>
                }.into_any()
            } else {
                ().into_any()
            }}

            <div class="transfers-groups stack" hidden=loading data-filter=filter_value>
                <For each=move || layout.with(|layout| layout.iter().map(|user| user.username.clone()).collect::<Vec<_>>())
                    key=String::clone children=move |username: String| view! {
                        <groups::TransferGroup username page/>
                    }/>
            </div>

            <Pager page=page_index pages label=format!("{} pages", title)/>
            <SelectionBar page/>
            <groups::DetailsDialog page/>
            <Modal open=confirm_cancel size="sm"
                on_close=Callback::new(move |()| confirm_cancel.set(false))
                title=format!("Cancel all {noun}s?")
                content=format!("Every queued and active {noun} stops. Finished transfers are not affected.")
                actions=vec![
                    ModalAction::new("Keep them"),
                    ModalAction::new("Cancel all").negative().on_click(move || {
                        change.run((Operation::Cancel, page.targets(Operation::Cancel, None, |_| true)));
                    }),
                ]/>
        </div>
    }
}

/// The name peers see for the one shared folder, or `None` with several (or none).
fn single_share_root(shares: &[Share]) -> Option<String> {
    let mut roots = shares
        .iter()
        .filter(|share| !share.is_excluded)
        .filter_map(|share| share.remote_path.clone());
    let root = roots.next()?;
    roots.next().is_none().then_some(root)
}

/// Writes a fresh snapshot into the per-row signals and the index, touching only what
/// changed.
fn apply_snapshot(
    list: &[Transfer],
    rows: StoredValue<HashMap<String, ArcRwSignal<Transfer>>>,
    index: RwSignal<Vec<Entry>>,
    speed: RwSignal<f64>,
) {
    let entries = index.with_untracked(|current| {
        let changed = current.len() != list.len()
            || current
                .iter()
                .zip(list)
                .any(|(entry, file)| !entry.matches(file));
        changed.then(|| list.iter().map(Entry::new).collect::<Vec<_>>())
    });
    let total_speed = list
        .iter()
        .filter(|file| file.state == State::InProgress && !file.removed)
        .map(|file| file.average_speed)
        .sum::<f64>();
    rows.update_value(|map| {
        if entries.is_some() && !map.is_empty() {
            let ids = list
                .iter()
                .map(|file| file.id.as_str())
                .collect::<HashSet<_>>();
            map.retain(|id, _| ids.contains(id.as_str()));
        }
        for file in list {
            if let Some(signal) = map.get(&file.id) {
                if signal.with_untracked(|old| old != file) {
                    signal.set(file.clone());
                }
            } else {
                map.insert(file.id.clone(), ArcRwSignal::new(file.clone()));
            }
        }
    });
    if let Some(entries) = entries {
        index.set(entries);
    }
    if speed.get_untracked() != total_speed {
        speed.set(total_speed);
    }
}

fn summary_badges(counts: Counts, speed: f64, is_download: bool) -> impl IntoView {
    let plural = |n: usize| if n == 1 { "file" } else { "files" };
    let total = format!(
        "{} {} · {}",
        util::group_digits(counts.files),
        plural(counts.files),
        util::format_bytes(counts.bytes as f64)
    );
    let mut badges = Vec::new();
    if counts.files > 0 {
        badges.push(view! { <Badge variant="outline" text=total/> }.into_any());
    }
    // The tabs below count each state.
    if speed > 0.0 {
        let icon = if is_download {
            "arrow-down"
        } else {
            "arrow-up"
        };
        badges.push(
            view! { <Badge variant="primary" icon=icon text=util::format_speed(speed) title="Current speed"/> }
                .into_any(),
        );
    }
    badges.collect_view()
}

/// The floating bar with actions for the selected rows.
#[component]
fn SelectionBar(page: Page) -> impl IntoView {
    let Page {
        direction,
        selected,
        working,
        connected,
        change,
        ..
    } = page;
    let info = Memo::new(move |_| {
        selected.with(|selected| {
            page.index.with(|index| {
                let mut count = 0;
                let (mut retry, mut cancel, mut remove) = (false, false, false);
                for entry in index.iter().filter(|entry| selected.contains(&entry.id)) {
                    count += 1;
                    retry |= Operation::Retry.allows(direction, entry);
                    cancel |= Operation::Cancel.allows(direction, entry);
                    remove |= Operation::Remove.allows(direction, entry);
                }
                (count, retry, cancel, remove)
            })
        })
    });
    let run = move |operation| {
        let files = selected.with_untracked(|selected| page.entries(selected));
        change.run((operation, files));
    };
    let busy = Signal::derive(move || working.with(Option::is_some));
    view! {
        <Show when=move || info.with(|(count, ..)| *count > 0)>
            <div class="transfers-selection" role="region" aria-label="Selected transfers">
                <span class="transfers-selection__count">
                    {move || info.with(|(count, ..)| format!("{count} selected"))}
                </span>
                <div class="transfers-selection__actions">
                    <Show when=move || info.with(|(_, retry, ..)| *retry)>
                        <Button size="sm" icon="rotate-cw" text="Retry"
                            disabled=Signal::derive(move || busy.get() || !connected.get())
                            on_click=Callback::new(move |_| run(Operation::Retry))/>
                    </Show>
                    <Show when=move || info.with(|(_, _, cancel, _)| *cancel)>
                        <Button size="sm" variant="danger-outline" icon="circle-x" text="Cancel"
                            disabled=busy on_click=Callback::new(move |_| run(Operation::Cancel))/>
                    </Show>
                    <Show when=move || info.with(|(.., remove)| *remove)>
                        <Button size="sm" icon="trash-2" text="Remove"
                            disabled=busy on_click=Callback::new(move |_| run(Operation::Remove))/>
                    </Show>
                    <Button size="sm" variant="ghost" icon="x" label="Clear selection"
                        on_click=Callback::new(move |_| selected.set(HashSet::new()))/>
                </div>
            </div>
        </Show>
    }
}

async fn retry(files: Vec<Entry>) -> BTreeSet<String> {
    let mut requests: BTreeMap<_, BTreeMap<_, _>> = BTreeMap::new();
    for file in files {
        requests
            .entry((file.username, file.root))
            .or_default()
            .insert(file.filename, file.size);
    }
    let mut errors = BTreeSet::new();
    for ((username, root), files) in requests {
        let body = EnqueueDownloadsRequest {
            downloads: EnqueueDownloads {
                username: username.clone(),
                files: files
                    .into_iter()
                    .map(|(filename, size)| DownloadFile { filename, size })
                    .collect(),
            },
            root,
        };
        match api::post::<_, EnqueuedDownloads>("/transfers/downloads", &body).await {
            Ok(response) => {
                for failure in response.failed {
                    errors.insert(if failure.error.is_empty() {
                        format!("Could not requeue a download from {username}")
                    } else {
                        failure.error
                    });
                }
            }
            Err(error) => {
                errors.insert(error.to_string());
            }
        }
    }
    errors
}

async fn change_transfers(
    direction: TransferDirection,
    operation: Operation,
    files: Vec<Entry>,
) -> BTreeSet<String> {
    let mut errors = BTreeSet::new();
    for file in files {
        let url = transfer_url(direction, &file.id);
        let result = if operation == Operation::Remove {
            api::request_text(Method::DELETE, &url, None).await
        } else {
            api::request_text(Method::POST, &format!("{url}/cancel"), None).await
        };
        if let Err(error) = result {
            errors.insert(error.to_string());
        }
    }
    errors
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, username: &str, filename: &str, state: State) -> Entry {
        Entry {
            root: None,
            id: id.into(),
            username: username.into(),
            filename: filename.into(),
            size: 10,
            state,
            removed: false,
            haystack: format!("{username}\n{filename}").to_lowercase(),
        }
    }

    type Row = (String, String, Vec<String>);

    fn rows(layout: &[UserLayout]) -> Vec<Row> {
        layout
            .iter()
            .flat_map(|user| {
                user.directories.iter().map(|dir| {
                    (
                        user.username.clone(),
                        dir.directory.clone(),
                        dir.files.iter().map(|file| file.id.clone()).collect(),
                    )
                })
            })
            .collect()
    }

    fn row(user: &str, dir: &str, ids: &[&str]) -> Row {
        (
            user.into(),
            dir.into(),
            ids.iter().map(|id| (*id).to_owned()).collect(),
        )
    }

    #[test]
    fn layout_groups_by_user_and_directory_and_sorts_files() {
        let index = [
            entry("1", "bob", "@@a\\Dir\\b.mp3", State::Succeeded),
            entry("2", "alice", "@@x\\y.flac", State::InProgress),
            entry("3", "bob", "@@a\\Dir\\a.mp3", State::Errored),
            entry("4", "bob", "@@a\\Other\\c.mp3", State::QueuedRemotely),
        ];
        let layout = build_layout(&index, Filter::All, "");
        assert_eq!(
            rows(&layout),
            vec![
                row("bob", "@@a\\Dir", &["3", "1"]),
                row("bob", "@@a\\Other", &["4"]),
                row("alice", "@@x", &["2"]),
            ]
        );
        assert_eq!(layout[0].counts.files, 3);
        assert_eq!(layout[0].counts.failed, 1);
    }

    #[test]
    fn layout_applies_state_and_text_filters() {
        let index = [
            entry("1", "bob", "@@a\\Dir\\b.mp3", State::Succeeded),
            entry("2", "alice", "@@x\\Song.flac", State::InProgress),
            entry("3", "bob", "@@a\\Dir\\a.mp3", State::Cancelled),
        ];
        assert_eq!(
            rows(&build_layout(&index, Filter::Failed, "")),
            vec![row("bob", "@@a\\Dir", &["3"])]
        );
        assert_eq!(
            rows(&build_layout(&index, Filter::All, " SONG ")),
            vec![row("alice", "@@x", &["2"])]
        );
        assert!(build_layout(&index, Filter::Queued, "").is_empty());
    }
}
