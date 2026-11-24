//! Search results, folder first: one card per (user, folder) with a split "Download
//! folder" button, a file list selected by click / Ctrl-click / Shift-click and the
//! keyboard (no checkboxes), a context menu, and a sticky selection bar.

use std::{
    cmp::Reverse,
    collections::{BTreeMap, BTreeSet, HashMap},
    sync::Arc,
};

use leptos::{ev::MouseEvent, prelude::*, task::spawn_local_scoped_with_cancellation};
use leptos_router::hooks::use_navigate;
use slskr_types::{
    search::SearchResponse,
    transfers::{DownloadFile, EnqueueDownloads, EnqueueDownloadsRequest, EnqueuedDownloads},
    users::{self, Directory},
};
use wasm_bindgen::JsCast;

use crate::{
    services::{api, util},
    ui::{
        Alert, Badge, Button, ContextMenu, ContextMenuState, DropdownMenu, EmptyState, Icon,
        ListAction, ListSelection, MenuEntry, StatusDot, Switch,
    },
};

use super::LiveResponses;

/// Folder cards shown per "Show more".
const PAGE_SIZE: usize = 10;

/// A result row: a file of a response, and whether it can not be downloaded.
#[derive(Clone, Debug, PartialEq, Eq)]
struct File {
    filename: String,
    size: u64,
    bit_rate: Option<u32>,
    bit_depth: Option<u32>,
    sample_rate: Option<u32>,
    /// Seconds.
    length: Option<u32>,
    is_variable_bit_rate: Option<bool>,
    is_locked: bool,
}

impl File {
    fn new(file: users::File, is_locked: bool) -> Self {
        Self {
            length: file
                .duration_ms
                .map(|ms| u32::try_from(ms / 1000).unwrap_or(u32::MAX)),
            filename: file.filename,
            size: file.size,
            bit_rate: file.bit_rate,
            bit_depth: file.bit_depth,
            sample_rate: file.sample_rate,
            is_variable_bit_rate: file.is_variable_bit_rate,
            is_locked,
        }
    }
}

#[derive(Clone, Default, PartialEq, Eq)]
struct Filter {
    include: Vec<String>,
    exclude: Vec<String>,
    cbr: bool,
    vbr: bool,
    lossless: bool,
    lossy: bool,
    bitrate: u32,
    depth: u32,
    size: u64,
    length: u32,
    folder_files: usize,
}

impl Filter {
    fn parse(text: &str) -> Self {
        let mut filter = Self::default();
        for term in text.to_lowercase().split_whitespace() {
            if let Some((key, value)) = term.split_once(':') {
                match key {
                    "minbr" | "minbitrate" => filter.bitrate = value.parse().unwrap_or_default(),
                    "minbd" | "minbitdepth" => filter.depth = value.parse().unwrap_or_default(),
                    "minfs" | "minfilesize" => filter.size = value.parse().unwrap_or_default(),
                    "minlen" | "minlength" => filter.length = value.parse().unwrap_or_default(),
                    "minfif" | "minfilesinfolder" => {
                        filter.folder_files = value.parse().unwrap_or_default()
                    }
                    _ => {}
                }
                continue;
            }
            match term {
                "iscbr" => filter.cbr = true,
                "isvbr" => filter.vbr = true,
                "islossless" => filter.lossless = true,
                "islossy" => filter.lossy = true,
                "-" => {}
                _ => {
                    if let Some(exclude) = term.strip_prefix('-') {
                        filter.exclude.push(exclude.into());
                    } else {
                        filter.include.push(term.into());
                    }
                }
            }
        }
        filter
    }

    fn matches(&self, file: &File) -> bool {
        let sample_rate = file.sample_rate.unwrap_or_default();
        let bit_depth = file.bit_depth.unwrap_or_default();
        if (self.cbr && file.is_variable_bit_rate != Some(false))
            || (self.vbr && file.is_variable_bit_rate != Some(true))
            || (self.lossless && (sample_rate == 0 || bit_depth == 0))
            || (self.lossy && (sample_rate != 0 || bit_depth != 0))
            || file.bit_rate.is_some_and(|rate| rate < self.bitrate)
            || file.bit_depth.is_some_and(|depth| depth < self.depth)
            || file.length.is_some_and(|length| length < self.length)
            || file.size < self.size
        {
            return false;
        }
        if self.include.is_empty() && self.exclude.is_empty() {
            return true;
        }
        let filename = file.filename.to_lowercase();
        self.include.iter().all(|term| filename.contains(term))
            && !self.exclude.iter().any(|term| filename.contains(term))
    }
}

/// One result card: the files one user offers from one folder.
#[derive(PartialEq, Eq)]
struct Folder {
    username: String,
    speed: u32,
    free_slot: bool,
    queue: u64,
    /// The peer's folder path ("" for files at the share root).
    directory: String,
    /// Sorted by filename.
    files: Vec<File>,
}

impl Folder {
    fn key(&self) -> String {
        format!("{}\u{1f}{}", self.username, self.directory)
    }
}

fn separator_of(path: &str) -> char {
    if path.contains('/') && !path.contains('\\') {
        '/'
    } else {
        '\\'
    }
}

/// The last component of a folder path (the album, usually).
fn folder_name(directory: &str) -> String {
    directory
        .rsplit(['\\', '/'])
        .find(|part| !part.is_empty())
        .unwrap_or("(root)")
        .to_owned()
}

/// By username: the response the cards were grouped from, and the cards.
type Grouped = HashMap<String, (Arc<SearchResponse>, Arc<[Arc<Folder>]>)>;

/// The folder cards of each response, for one filter and `hide_locked`.
#[derive(Default)]
struct FolderCache {
    filter: Filter,
    hide_locked: bool,
    entries: Grouped,
}

impl FolderCache {
    /// Starts a pass: returns the entries still valid for `filter` and `hide_locked`. Those
    /// not used again by [`Self::folders`] in this pass are dropped.
    fn begin(&mut self, filter: &Filter, hide_locked: bool) -> Grouped {
        let previous = std::mem::take(&mut self.entries);
        if self.filter == *filter && self.hide_locked == hide_locked {
            previous
        } else {
            self.filter = filter.clone();
            self.hide_locked = hide_locked;
            HashMap::new()
        }
    }

    /// The cards of `response`, reused when this exact response was grouped before.
    fn folders(
        &mut self,
        previous: &mut Grouped,
        response: &Arc<SearchResponse>,
        filter: &Filter,
        hide_locked: bool,
    ) -> Arc<[Arc<Folder>]> {
        let cards = match previous.remove(&response.username) {
            Some((grouped, cards)) if Arc::ptr_eq(&grouped, response) => cards,
            _ => folders(response, filter, hide_locked)
                .into_iter()
                .map(Arc::new)
                .collect(),
        };
        self.entries.insert(
            response.username.clone(),
            (Arc::clone(response), Arc::clone(&cards)),
        );
        cards
    }
}

/// The folders of one response that have files left after filtering.
fn folders(response: &SearchResponse, filter: &Filter, hide_locked: bool) -> Vec<Folder> {
    let count = response.files.len()
        + if hide_locked {
            0
        } else {
            response.locked_files.len()
        };
    if count < filter.folder_files {
        return Vec::new();
    }
    let mut dirs: BTreeMap<String, Vec<File>> = BTreeMap::new();
    for (rows, locked) in [(&response.files, false), (&response.locked_files, true)] {
        for file in rows {
            let file = File::new(file.clone(), locked);
            if (hide_locked && file.is_locked) || !filter.matches(&file) {
                continue;
            }
            let directory = file
                .filename
                .rsplit_once(['\\', '/'])
                .map_or("", |(path, _)| path);
            dirs.entry(directory.to_owned()).or_default().push(file);
        }
    }
    dirs.into_iter()
        .map(|(directory, mut files)| {
            files.sort_unstable_by(|a, b| a.filename.cmp(&b.filename));
            Folder {
                username: response.username.clone(),
                speed: response.upload_speed,
                free_slot: response.has_free_upload_slot,
                queue: response.queue_length.into(),
                directory,
                files,
            }
        })
        .collect()
}

/// "FLAC 16/44.1", "MP3 320", "MP3 192–320", or just the extension.
fn quality_summary(files: &[File]) -> Option<String> {
    let mut extensions: BTreeMap<String, usize> = BTreeMap::new();
    for file in files {
        if let Some((_, extension)) = util::get_file_name(&file.filename).rsplit_once('.') {
            *extensions.entry(extension.to_uppercase()).or_default() += 1;
        }
    }
    let extension = extensions
        .into_iter()
        .max_by_key(|(_, count)| *count)
        .map(|(extension, _)| extension)
        .unwrap_or_default();
    let detail = if let Some(file) = files
        .iter()
        .find(|file| file.bit_depth.is_some() && file.sample_rate.is_some())
    {
        let rate = file.sample_rate.unwrap_or_default() as f64 / 1000.0;
        let rate = format!("{rate:.1}");
        let rate = rate.trim_end_matches(".0");
        format!("{}/{rate}", file.bit_depth.unwrap_or_default())
    } else {
        let rates = files
            .iter()
            .filter_map(|file| file.bit_rate)
            .collect::<Vec<_>>();
        match (rates.iter().min(), rates.iter().max()) {
            (Some(min), Some(max)) if min == max => format!("{min}"),
            (Some(min), Some(max)) => format!("{min}–{max}"),
            _ => String::new(),
        }
    };
    let text = format!("{extension} {detail}").trim().to_owned();
    (!text.is_empty()).then_some(text)
}

fn plural(count: usize, one: &str, many: &str) -> String {
    format!("{count} {}", if count == 1 { one } else { many })
}

#[component]
pub(super) fn SearchResults(
    id: StoredValue<String>,
    live: RwSignal<LiveResponses>,
    saved: RwSignal<Vec<Arc<SearchResponse>>>,
    connected: Signal<bool>,
    done: Memo<bool>,
    ready: Signal<bool>,
) -> impl IntoView {
    let text = RwSignal::new(String::new());
    let filter = Memo::new(move |_| text.with(|text| Filter::parse(text)));
    let hide_locked = RwSignal::new(true);
    let free_only = RwSignal::new(false);
    let collapsed = RwSignal::new(false);
    let sort_by_queue = RwSignal::new(false);
    let hidden = RwSignal::new(BTreeSet::<String>::new());
    let limit = RwSignal::new(PAGE_SIZE);
    // Folder cards per response, kept while the response, the filter and `hide_locked` stay
    // the same: a live batch then only groups the responses it added or replaced.
    let grouped = StoredValue::new(FolderCache::default());
    // (responses, users with results left, folder cards)
    let results = Memo::new(move |_| {
        let hide_locked = hide_locked.get();
        let free_only = free_only.get();
        let by_queue = sort_by_queue.get();
        let complete = done.get();
        saved.with(|saved| {
            live.with(|live| {
                hidden.with(|hidden| {
                    filter.with(|filter| {
                        let mut responses: BTreeMap<&str, &Arc<SearchResponse>> = saved
                            .iter()
                            .map(|row| (row.username.as_str(), row))
                            .collect();
                        if id.with_value(|id| id == &live.id) {
                            for (username, row) in &live.responses {
                                if complete {
                                    responses.entry(username).or_insert(row);
                                } else {
                                    responses.insert(username, row);
                                }
                            }
                        }
                        let count = responses.len();
                        let mut users = 0;
                        let mut cards = Vec::new();
                        grouped.update_value(|cache| {
                            let mut previous = cache.begin(filter, hide_locked);
                            for row in responses.into_values().filter(|row| {
                                !hidden.contains(&row.username)
                                    && (!free_only || row.has_free_upload_slot)
                            }) {
                                let found = cache.folders(&mut previous, row, filter, hide_locked);
                                if !found.is_empty() {
                                    users += 1;
                                }
                                cards.extend(found.iter().cloned());
                            }
                        });
                        if by_queue {
                            cards.sort_by(|a, b| {
                                (a.queue, &a.username, &a.directory).cmp(&(
                                    b.queue,
                                    &b.username,
                                    &b.directory,
                                ))
                            });
                        } else {
                            cards.sort_by(|a, b| {
                                (Reverse(a.speed), &a.username, &a.directory).cmp(&(
                                    Reverse(b.speed),
                                    &b.username,
                                    &b.directory,
                                ))
                            });
                        }
                        (count, users, cards)
                    })
                })
            })
        })
    });
    let visible_cards = Memo::new(move |_| {
        results.with(|(_, _, cards)| {
            let mut keys = Vec::new();
            let mut by_key = HashMap::new();
            for card in cards.iter().take(limit.get()) {
                let key = card.key();
                keys.push(key.clone());
                by_key.insert(key, card.clone());
            }
            (keys, by_key)
        })
    });
    let hide = Callback::new(move |username: String| {
        hidden.update(|hidden| {
            hidden.insert(username);
        });
    });
    let help_open = RwSignal::new(false);
    let help_root = NodeRef::<leptos::html::Div>::new();
    let outside = window_event_listener(leptos::ev::mousedown, move |event| {
        if !help_open.get_untracked() {
            return;
        }
        let target = event
            .target()
            .and_then(|target| target.dyn_into::<web_sys::Node>().ok());
        if let (Some(root), Some(target)) = (help_root.get_untracked(), target)
            && !root.contains(Some(&target))
        {
            help_open.set(false);
        }
    });
    let escape = window_event_listener(leptos::ev::keydown, move |event| {
        if event.key() == "Escape" && help_open.get_untracked() {
            help_open.set(false);
        }
    });
    on_cleanup(move || {
        outside.remove();
        escape.remove();
    });
    let clear_filters = move || {
        text.set(String::new());
        hidden.update(BTreeSet::clear);
    };
    let shown = move || {
        results.with(|(_, users, cards)| {
            let shown = cards.len().min(limit.get());
            format!(
                "Showing {shown} of {} from {}",
                plural(cards.len(), "folder", "folders"),
                plural(*users, "user", "users")
            )
        })
    };

    view! {
        <Show when=move || done.get() || results.with(|(count, _, _)| *count != 0)
            fallback=move || view! {
                <div class="card">
                    <EmptyState icon="loader-circle" title="Waiting for responses…"
                        description="Results appear here as users answer. Searches run for about a minute."/>
                </div>
            }>
            <section class="card card--compact results-toolbar" aria-label="Result options">
                <div class="card__content stack stack--sm">
                    <div class="results-toolbar__row">
                        <div class="input-group results-filter">
                            <div class="input-icon">
                                <Icon name="list-filter"/>
                                <input id="search-result-filter" class="input" type="search" autocomplete="off"
                                    aria-label="Filter results"
                                    placeholder="Filter, e.g. flac -live minbitrate:320"
                                    prop:value=text on:input=move |event| text.set(event_target_value(&event))/>
                            </div>
                            <Button icon="x" label="Clear filter" disabled=Signal::derive(move || text.with(String::is_empty))
                                on_click=Callback::new(move |_| text.set(String::new()))/>
                        </div>
                        <div class="dropdown results-help" node_ref=help_root>
                            <Button icon="circle-help" label="Filter syntax" pressed=Signal::derive(move || help_open.get())
                                on_click=Callback::new(move |_| help_open.update(|open| *open = !*open))/>
                            <Show when=move || help_open.get()>
                                <div class="popover results-help__popover" role="dialog" aria-label="Filter syntax">
                                    <p class="font-semibold">"Filter syntax"</p>
                                    <p class="muted text-xs">"Separate terms with spaces; a file must match all of them."</p>
                                    <dl class="results-help__list">
                                        <dt><code>"word"</code></dt><dd>"File path contains word"</dd>
                                        <dt><code>"-word"</code></dt><dd>"File path does not contain word"</dd>
                                        <dt><code>"islossless"</code>" "<code>"islossy"</code></dt><dd>"Lossless (has bit depth and sample rate) or lossy files"</dd>
                                        <dt><code>"iscbr"</code>" "<code>"isvbr"</code></dt><dd>"Constant or variable bit rate"</dd>
                                        <dt><code>"minbitrate:320"</code></dt><dd>"At least this many kbps (also minbr)"</dd>
                                        <dt><code>"minbitdepth:24"</code></dt><dd>"At least this bit depth (also minbd)"</dd>
                                        <dt><code>"minfilesize:N"</code></dt><dd>"At least N bytes (also minfs)"</dd>
                                        <dt><code>"minlength:N"</code></dt><dd>"At least N seconds long (also minlen)"</dd>
                                        <dt><code>"minfilesinfolder:8"</code></dt><dd>"Users with at least this many files (also minfif)"</dd>
                                    </dl>
                                </div>
                            </Show>
                        </div>
                        <label class="sr-only" for="search-result-sort">"Sort results"</label>
                        <select id="search-result-sort" class="select select--auto results-sort"
                            on:change=move |event| sort_by_queue.set(event_target_value(&event) == "queue")>
                            <option value="speed">"Fastest upload speed"</option>
                            <option value="queue">"Shortest queue"</option>
                        </select>
                    </div>
                    <div class="results-toolbar__row results-toolbar__toggles">
                        <Switch checked=hide_locked on_change=Callback::new(move |value| hide_locked.set(value)) label="Hide locked"/>
                        <Switch checked=free_only on_change=Callback::new(move |value| free_only.set(value)) label="Only free slots"/>
                        <Button size="sm" variant="ghost"
                            icon=Signal::derive(move || if collapsed.get() { "chevron-down".to_owned() } else { "chevron-up".to_owned() })
                            text=Signal::derive(move || if collapsed.get() { "Expand all".to_owned() } else { "Collapse all".to_owned() })
                            on_click=Callback::new(move |_| collapsed.update(|value| *value = !*value))/>
                        <span class="results-toolbar__count muted text-sm" role="status">{shown}</span>
                        <Show when=move || hidden.with(|hidden| !hidden.is_empty())>
                            <Button size="sm" variant="ghost" icon="eye" text="Restore hidden users"
                                on_click=Callback::new(move |_| hidden.update(BTreeSet::clear))/>
                        </Show>
                    </div>
                    <p class="muted text-xs results-toolbar__hint hide-sm">
                        "Click a file to select it; "<kbd class="kbd">"Ctrl"</kbd>" or "<kbd class="kbd">"Shift"</kbd>
                        "+click to select more. Double-click downloads one file, "<kbd class="kbd">"Enter"</kbd>
                        " the selection. Right-click for more actions."
                    </p>
                </div>
            </section>
            <div class="results-list">
                <For each=move || visible_cards.with(|(keys, _)| keys.clone())
                    key=String::clone children=move |key: String| {
                        let row = Memo::new(move |_| visible_cards.with(|(_, cards)| cards.get(&key).cloned()));
                        view! { <FolderCard row connected collapsed hide/> }
                    }/>
            </div>
            {move || results.with(|(count, users, cards)| {
                let remaining = cards.len().saturating_sub(limit.get());
                let filtered = count - users;
                let hidden_note = (filtered != 0).then(|| format!(
                    "{filtered} {} hidden by filters",
                    if filtered == 1 { "result" } else { "results" }
                ));
                if remaining != 0 {
                    let next = remaining.min(PAGE_SIZE);
                    view! {
                        <div class="results-more">
                            <Button block=true icon="chevron-down"
                                text=format!("Show {}", plural(next, "more folder", "more folders"))
                                on_click=Callback::new(move |_| limit.update(|count| *count += PAGE_SIZE))/>
                            <p class="muted text-xs text-center" role="status">
                                {format!("{remaining} more to show")}{hidden_note.map(|note| format!(" · {note}"))}
                            </p>
                        </div>
                    }.into_any()
                } else if cards.is_empty() && filtered != 0 {
                    view! {
                        <div class="card">
                            <EmptyState icon="list-filter" title="Every result is hidden"
                                description=hidden_note.unwrap_or_default()>
                                <Button icon="x" text="Clear filters" on_click=Callback::new(move |_| clear_filters())/>
                            </EmptyState>
                        </div>
                    }.into_any()
                } else if let Some(note) = hidden_note {
                    view! { <p class="results-more muted text-sm text-center" role="status">{format!("All results shown · {note}")}</p> }.into_any()
                } else if *count == 0 && done.get() && ready.get() {
                    view! {
                        <div class="card">
                            <EmptyState icon="search" title="No results"
                                description="Nobody answered this search. Try fewer or different words."/>
                        </div>
                    }.into_any()
                } else { ().into_any() }
            })}
        </Show>
    }
}

/// The outcome of a download request, for the card's message line.
fn describe(result: Result<EnqueuedDownloads, api::ApiError>, what: &str) -> (bool, String) {
    match result {
        Ok(response) if response.failed.is_empty() && !response.enqueued.is_empty() => (
            true,
            format!(
                "Queued {} from {what}.",
                plural(response.enqueued.len(), "file", "files")
            ),
        ),
        Ok(response) if response.enqueued.is_empty() && response.failed.is_empty() => {
            (false, format!("Nothing to download in {what}."))
        }
        Ok(response) => {
            let detail = response
                .failed
                .iter()
                .filter(|failure| !failure.error.is_empty())
                .map(|failure| failure.error.as_str())
                .collect::<Vec<_>>()
                .join("; ");
            let total = response
                .files
                .max(response.enqueued.len() + response.failed.len());
            (
                false,
                format!(
                    "Queued {} of {} from {what}; {} failed. {detail}",
                    response.enqueued.len(),
                    plural(total, "file", "files"),
                    response.failed.len()
                ),
            )
        }
        Err(error) => (false, error.to_string()),
    }
}

#[component]
fn FolderCard(
    row: Memo<Option<Arc<Folder>>>,
    connected: Signal<bool>,
    collapsed: RwSignal<bool>,
    hide: Callback<String>,
) -> impl IntoView {
    let (username, directory) = row.with_untracked(|row| {
        row.as_ref()
            .map(|row| (row.username.clone(), row.directory.clone()))
            .unwrap_or_default()
    });
    let name = folder_name(&directory);
    let username = StoredValue::new(username);
    let directory = StoredValue::new(directory);
    let name = StoredValue::new(name);
    let folded = RwSignal::new(collapsed.get_untracked());
    Effect::new(move |_| folded.set(collapsed.get()));
    let has_folder = directory.with_value(|path| !path.trim_matches(['\\', '/']).is_empty());
    let selection = ListSelection::new(0);
    let pending = RwSignal::new(false);
    let message = RwSignal::new(None::<(bool, String)>);
    let extra = RwSignal::new(Vec::<File>::new());
    let looking_up = RwSignal::new(false);
    let menu = RwSignal::new(None::<ContextMenuState>);
    let navigate = StoredValue::new_local(use_navigate());

    let files = Memo::new(move |_| {
        let mut files = row
            .with(|row| row.as_ref().map(|row| row.files.clone()))
            .unwrap_or_default();
        extra.with(|extra| {
            let known = files
                .iter()
                .map(|file| file.filename.clone())
                .collect::<BTreeSet<_>>();
            files.extend(
                extra
                    .iter()
                    .filter(|file| !known.contains(&file.filename))
                    .cloned(),
            );
        });
        files.sort_unstable_by(|a, b| a.filename.cmp(&b.filename));
        files
    });
    // Indices change with the list: start over when it does.
    Effect::new(move |_| {
        let len = files.with(Vec::len);
        menu.set(None);
        selection.clear();
        selection.set_len(len);
    });
    let locked = Memo::new(move |_| {
        files.with(|files| !files.is_empty() && files.iter().all(|file| file.is_locked))
    });
    // The selected files that can be downloaded.
    let chosen = Memo::new(move |_| {
        let indices = selection.selected();
        files.with(|files| {
            indices
                .into_iter()
                .filter_map(|index| files.get(index))
                .filter(|file| !file.is_locked)
                .map(|file| DownloadFile {
                    filename: file.filename.clone(),
                    size: file.size,
                })
                .collect::<Vec<_>>()
        })
    });

    // A dropdown closes as soon as its item is clicked. Keep requests scoped to the
    // card so they can complete after the menu or file list is removed.
    let card_owner = StoredValue::new(Owner::current().expect("the folder card has an owner"));
    let enqueue = move |requests: Vec<DownloadFile>, what: String| {
        if pending.get_untracked() || !connected.get_untracked() || requests.is_empty() {
            return;
        }
        // Saved like a folder download: <downloads>/<folder name>/<file>.
        let root = directory.get_value();
        let request = EnqueueDownloadsRequest {
            downloads: EnqueueDownloads {
                username: username.get_value(),
                files: requests,
            },
            root: (!root.is_empty()).then_some(root),
        };
        pending.set(true);
        message.set(None);
        card_owner.with_value(|owner| {
            owner.with(|| {
                spawn_local_scoped_with_cancellation(async move {
                    let result =
                        api::post::<_, EnqueuedDownloads>("/transfers/downloads", &request).await;
                    let ok = matches!(&result, Ok(response) if response.failed.is_empty());
                    message.set(Some(describe(result, &what)));
                    if ok {
                        selection.clear();
                    }
                    pending.set(false);
                });
            });
        });
    };
    let download_selected = move || {
        let requests = chosen.get_untracked();
        let what = if requests.len() == 1 {
            util::get_file_name(&requests[0].filename).to_owned()
        } else {
            name.get_value()
        };
        enqueue(requests, what);
    };
    let download_folder = move |recursive: bool| {
        if !has_folder || pending.get_untracked() || !connected.get_untracked() {
            return;
        }
        pending.set(true);
        message.set(None);
        let user = username.get_value();
        let path = directory.get_value();
        let what = if recursive {
            format!("{} and its subfolders", name.get_value())
        } else {
            name.get_value()
        };
        card_owner.with_value(|owner| {
            owner.with(|| {
                spawn_local_scoped_with_cancellation(async move {
                    let result = api::download_folder(&user, &path, recursive).await;
                    message.set(Some(describe(result, &what)));
                    pending.set(false);
                });
            });
        });
    };
    let show_all = move || {
        if !has_folder || looking_up.get_untracked() || !connected.get_untracked() {
            return;
        }
        looking_up.set(true);
        message.set(None);
        let path = directory.get_value();
        let separator = separator_of(&path);
        let endpoint = format!(
            "/users/{}/directory?path={}",
            username.with_value(|name| util::encode_uri_component(name)),
            util::encode_uri_component(&path)
        );
        card_owner.with_value(|owner| {
            owner.with(|| {
                spawn_local_scoped_with_cancellation(async move {
                    match api::get::<Vec<Directory>>(&endpoint).await {
                        Ok(dirs) => {
                            if let Some(root) = dirs.into_iter().find(|directory| {
                                directory.name.trim_end_matches(['\\', '/'])
                                    == path.trim_end_matches(['\\', '/'])
                            }) {
                                extra.set(
                                    root.files
                                        .into_iter()
                                        .map(|file| {
                                            let mut file = File::new(file, false);
                                            file.filename = format!(
                                                "{path}{separator}{}",
                                                util::get_file_name(&file.filename)
                                            );
                                            file
                                        })
                                        .collect(),
                                );
                            } else {
                                message.set(Some((
                                    false,
                                    "The user sent no files for this folder.".into(),
                                )));
                            }
                        }
                        Err(error) => message.set(Some((false, error.to_string()))),
                    }
                    looking_up.set(false);
                });
            });
        });
    };
    let blocked =
        Signal::derive(move || !has_folder || !connected.get() || pending.get() || locked.get());
    let list_id = format!(
        "files-{}",
        (js_sys::Math::random() * u32::MAX as f64) as u32
    );

    let row_click = move |index: usize, ev: MouseEvent| {
        selection.click(index, ev.ctrl_key() || ev.meta_key(), ev.shift_key());
    };
    let open_menu = move |ev: MouseEvent, file_index: Option<usize>, at_pointer: bool| {
        if let Some(index) = file_index
            && !selection.is_selected_untracked(index)
        {
            selection.click(index, false, false);
        }
        let path = file_index
            .and_then(|index| {
                files.with_untracked(|files| files.get(index).map(|f| f.filename.clone()))
            })
            .unwrap_or_else(|| directory.get_value());
        let selected_count = chosen.with_untracked(Vec::len);
        let blocked = blocked.get_untracked();
        let entries = vec![
            MenuEntry::item(
                format!("Download selected ({selected_count})"),
                "download",
                download_selected,
            )
            .disabled(selected_count == 0 || !connected.get_untracked() || pending.get_untracked()),
            MenuEntry::item("Download folder", "folder", move || download_folder(false))
                .disabled(blocked),
            MenuEntry::item("Download folder + subfolders", "folder-open", move || {
                download_folder(true)
            })
            .disabled(blocked),
            MenuEntry::Separator,
            MenuEntry::item("Browse user", "user", move || {
                let user = username.with_value(|user| util::encode_uri_component(user));
                navigate.with_value(|navigate| {
                    navigate(&format!("/browse?username={user}"), Default::default())
                });
            }),
            MenuEntry::item("Open folder in Browse", "external-link", move || {
                let user = username.with_value(|user| util::encode_uri_component(user));
                let path = directory.with_value(|path| util::encode_uri_component(path));
                navigate.with_value(|navigate| {
                    navigate(
                        &format!("/browse?username={user}&path={path}"),
                        Default::default(),
                    )
                });
            }),
            MenuEntry::item(
                if file_index.is_some() {
                    "Copy file path"
                } else {
                    "Copy folder path"
                },
                "copy",
                move || {
                    let _ = window().navigator().clipboard().write_text(&path);
                },
            ),
            MenuEntry::Separator,
            MenuEntry::item("Hide results from this user", "eye-off", move || {
                hide.run(username.get_value())
            }),
        ];
        if at_pointer {
            ev.prevent_default();
            menu.set(Some(ContextMenuState::at_pointer(
                &ev,
                "Folder actions",
                entries,
            )));
        } else if let Some(element) = ev
            .current_target()
            .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
        {
            menu.set(Some(ContextMenuState::below(
                &element,
                "Folder actions",
                entries,
            )));
        }
    };

    view! {
        <article class="card folder-card" class:is-locked=locked aria-label=move || format!("{} from {}", name.get_value(), username.get_value())>
            <header class="folder-card__header" on:contextmenu=move |ev| open_menu(ev, None, true)>
                <button type="button" class="btn btn--ghost btn--icon btn--sm folder-card__fold"
                    aria-label="Fold result" aria-controls=list_id.clone()
                    title=move || if folded.get() { "Show files" } else { "Hide files" }
                    aria-expanded=move || (!folded.get()).to_string()
                    on:click=move |_| folded.update(|folded| *folded = !*folded)>
                    <Icon name=Signal::derive(move || if folded.get() { "chevron-right".to_owned() } else { "chevron-down".to_owned() })/>
                </button>
                <div class="folder-card__title">
                    <h3 class="folder-card__name">
                        <Icon name=Signal::derive(move || if locked.get() { "lock".to_owned() } else { "folder".to_owned() }) class="folder-card__icon"/>
                        <span>{name.get_value()}</span>
                    </h3>
                    <p class="folder-card__path mono" title=directory.get_value()>{directory.get_value()}</p>
                    <div class="folder-card__meta">
                        {move || files.with(|files| {
                            let size = files.iter().map(|file| file.size as f64).sum::<f64>();
                            let quality = quality_summary(files);
                            view! {
                                <Badge text=plural(files.len(), "file", "files")/>
                                <Badge text=util::format_bytes(size)/>
                                {quality.map(|quality| view! { <Badge variant="purple" text=quality/> })}
                            }
                        })}
                    </div>
                    <div class="folder-card__user">
                        {move || row.with(|row| row.as_ref().map(|row| {
                            let free = row.free_slot;
                            let queue = row.queue;
                            view! {
                                <StatusDot status=if free { "online" } else { "away" }
                                    label=if free { "Free upload slot".to_owned() } else { "No free upload slot".to_owned() }/>
                                <span class="folder-card__username">{row.username.clone()}</span>
                                <Badge variant="outline" icon="gauge" title="Upload speed" text=util::format_speed(row.speed.into())/>
                                {if free {
                                    view! { <Badge variant="success" icon="check" text="Free slot"/> }.into_any()
                                } else {
                                    view! { <Badge variant="warning" icon="clock" text="No free slot"/> }.into_any()
                                }}
                                <Badge variant="outline" icon="list" title="Queue length"
                                    text=if queue == 0 { "No queue".to_owned() } else { format!("Queue {queue}") }/>
                            }
                        }))}
                    </div>
                </div>
                <div class="folder-card__actions">
                    <div class="split-button">
                        <Button variant="primary" size="sm" icon="download" text="Download folder" loading=pending disabled=blocked
                            class="split-button__main" on_click=Callback::new(move |_| download_folder(false))/>
                        <DropdownMenu label="More download options" icon="chevron-down" variant="primary" size="sm" align="end">
                            <button type="button" class="menu__item" role="menuitem" disabled=move || blocked.get()
                                on:click=move |_| download_folder(true)>
                                <Icon name="folder-open"/>"Download folder + subfolders"
                            </button>
                            <button type="button" class="menu__item" role="menuitem" disabled=move || blocked.get()
                                on:click=move |_| download_folder(false)>
                                <Icon name="folder"/>"Download folder only"
                            </button>
                        </DropdownMenu>
                    </div>
                    <button type="button" class="btn btn--action" aria-label="More actions" title="More actions" aria-haspopup="menu"
                        aria-expanded=move || menu.with(Option::is_some).to_string()
                        on:click=move |ev| open_menu(ev, None, false)>
                        <Icon name="ellipsis"/>
                    </button>
                    <Button variant="action" class="is-danger" icon="eye-off" label="Hide results from this user"
                        on_click=Callback::new(move |_| hide.run(username.get_value()))/>
                </div>
            </header>
            <Show when=move || !folded.get()>
                <div class="file-list">
                    <div class="file-list__head" aria-hidden="true">
                        <span>"File"</span><span class="num">"Size"</span>
                        <span class="file-list__quality">"Quality"</span><span class="num">"Length"</span>
                    </div>
                    <div class="file-list__rows" role="listbox" aria-multiselectable="true" id=list_id.clone()
                        aria-label=format!("Files in {}", name.get_value())
                        on:keydown=move |ev| {
                            if selection.on_keydown(&ev, files.with_untracked(Vec::len)) == Some(ListAction::Activate) {
                                if selection.count() == 0 {
                                    selection.click(selection.focused(), false, false);
                                }
                                download_selected();
                            }
                        }>
                        {move || files.with(|files| files.iter().enumerate().map(|(index, file)| {
                            let base = util::get_file_name(&file.filename).to_owned();
                            let file_locked = file.is_locked;
                            let single = DownloadFile { filename: file.filename.clone(), size: file.size };
                            let single_name = base.clone();
                            let length = file.length.map_or_else(|| "—".to_owned(), |length| util::format_seconds(length.into()));
                            view! {
                                <div class="file-row" role="option" data-index=index.to_string()
                                    class:is-locked=file_locked
                                    aria-selected=move || selection.is_selected(index).to_string()
                                    aria-disabled=file_locked.then_some("true")
                                    tabindex=move || selection.tabindex(index)
                                    title=file.filename.clone()
                                    on:mousedown=move |ev: MouseEvent| if ev.shift_key() { ev.prevent_default(); }
                                    on:click=move |ev| row_click(index, ev)
                                    on:dblclick=move |_| if !file_locked { enqueue(vec![single.clone()], single_name.clone()); }
                                    on:contextmenu=move |ev| open_menu(ev, Some(index), true)>
                                    <span class="file-row__name">
                                        {if file_locked {
                                            view! { <Icon name="lock" size="xs" class="text-warning" label="Locked"/> }.into_any()
                                        } else {
                                            view! { <Icon name="file-audio" size="xs" class="muted"/> }.into_any()
                                        }}
                                        <span class="file-row__text">{base}</span>
                                    </span>
                                    <span class="num">{util::format_bytes(file.size as f64)}</span>
                                    <span class="file-list__quality muted">{util::format_attributes(file.bit_rate, file.is_variable_bit_rate, file.bit_depth, file.sample_rate)}</span>
                                    <span class="num muted">{length}</span>
                                </div>
                            }
                        }).collect_view())}
                    </div>
                    <Show when=move || has_folder && !locked.get()>
                        <div class="file-list__foot">
                            <Button size="sm" variant="ghost" icon="folder-open" text="Show all files in folder"
                                loading=looking_up disabled=Signal::derive(move || !connected.get())
                                on_click=Callback::new(move |_| show_all())/>
                        </div>
                    </Show>
                </div>
            </Show>
            <footer class="folder-card__bar" hidden=move || chosen.with(Vec::is_empty) && message.with(Option::is_none)>
                <Show when=move || !chosen.with(Vec::is_empty)>
                    <span class="folder-card__selection font-medium" role="status">{move || chosen.with(|chosen| {
                        let size = chosen.iter().map(|file| file.size as f64).sum();
                        format!("{} selected · {}", plural(chosen.len(), "file", "files"), util::format_bytes(size))
                    })}</span>
                    <Button variant="primary" size="sm" icon="download" loading=pending
                        disabled=Signal::derive(move || !connected.get())
                        text=Signal::derive(move || format!("Download {}", plural(chosen.with(Vec::len), "file", "files")))
                        on_click=Callback::new(move |_| download_selected())/>
                    <Button size="sm" variant="ghost" text="Clear" disabled=pending
                        on_click=Callback::new(move |_| selection.clear())/>
                </Show>
                {move || message.get().map(|(ok, text)| view! {
                    <Alert variant=if ok { "success" } else { "danger" } class="folder-card__message"
                        on_dismiss=Callback::new(move |()| message.set(None))>{text}</Alert>
                })}
            </footer>
            <ContextMenu state=menu/>
        </article>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(username: &str, files: &[&str]) -> Arc<SearchResponse> {
        Arc::new(SearchResponse {
            username: username.into(),
            files: files
                .iter()
                .map(|name| users::File {
                    filename: (*name).into(),
                    extension: "flac".into(),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        })
    }

    #[test]
    fn folder_cache_regroups_only_changed_responses() {
        let mut cache = FolderCache::default();
        let filter = Filter::default();
        let alice = response("alice", &[r"music\a\1.flac", r"music\b\2.flac"]);
        let bob = response("bob", &[r"music\c\3.flac"]);
        let pass = |cache: &mut FolderCache, rows: &[&Arc<SearchResponse>], filter: &Filter| {
            let mut previous = cache.begin(filter, true);
            rows.iter()
                .map(|row| cache.folders(&mut previous, row, filter, true))
                .collect::<Vec<_>>()
        };

        let first = pass(&mut cache, &[&alice, &bob], &filter);
        assert_eq!(first[0].len(), 2);
        // Same responses: the same cards, not regrouped.
        let second = pass(&mut cache, &[&alice, &bob], &filter);
        assert!(Arc::ptr_eq(&first[0], &second[0]) && Arc::ptr_eq(&first[1], &second[1]));

        // A replaced response is regrouped; the other is kept.
        let bob = response("bob", &[r"music\c\3.flac", r"music\d\4.flac"]);
        let third = pass(&mut cache, &[&alice, &bob], &filter);
        assert!(Arc::ptr_eq(&first[0], &third[0]));
        assert_eq!(third[1].len(), 2);

        // A different filter regroups everything, and drops users no longer listed.
        let filter = Filter::parse("\\b\\");
        let fourth = pass(&mut cache, &[&alice], &filter);
        assert!(!Arc::ptr_eq(&first[0], &fourth[0]));
        assert_eq!(cache.entries.len(), 1);
    }
}
