use std::{
    cmp::Reverse,
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use leptos::{prelude::*, task::spawn_local_scoped_with_cancellation};
use slskr_api::{
    search::{File, SearchResponse},
    transfers::{EnqueueDownloadsResponse, QueueDownloadRequest},
    users::{Directory, DirectoryContentsRequest},
};

use crate::services::{api, util};

use super::LiveResponses;

const PAGE_SIZE: usize = 5;

#[derive(Default, PartialEq, Eq)]
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

#[derive(PartialEq, Eq)]
struct Group {
    username: String,
    speed: u32,
    free_slot: bool,
    queue: u64,
    dirs: BTreeMap<String, Vec<File>>,
}

fn group(response: &SearchResponse, filter: &Filter, hide_locked: bool) -> Option<Group> {
    let count = response
        .files
        .iter()
        .filter(|file| !hide_locked || !file.is_locked)
        .count()
        + if hide_locked {
            0
        } else {
            response.locked_files.len()
        };
    if count < filter.folder_files {
        return None;
    }
    let mut dirs: BTreeMap<String, Vec<File>> = BTreeMap::new();
    for (rows, locked) in [(&response.files, false), (&response.locked_files, true)] {
        for file in rows {
            if (hide_locked && (locked || file.is_locked)) || !filter.matches(file) {
                continue;
            }
            let directory = file
                .filename
                .rsplit_once(['\\', '/'])
                .map_or("", |(path, _)| path);
            let mut file = file.clone();
            file.is_locked |= locked;
            dirs.entry(directory.to_owned()).or_default().push(file);
        }
    }
    if dirs.is_empty() {
        return None;
    }
    Some(Group {
        username: response.username.clone(),
        speed: response.upload_speed,
        free_slot: response.has_free_upload_slot,
        queue: response.queue_length,
        dirs,
    })
}

#[component]
pub(super) fn SearchResults(
    id: StoredValue<String>,
    live: RwSignal<LiveResponses>,
    saved: RwSignal<Vec<SearchResponse>>,
    connected: Signal<bool>,
    done: Memo<bool>,
    ready: Signal<bool>,
) -> impl IntoView {
    let text = RwSignal::new(String::new());
    let filter = Memo::new(move |_| text.with(|text| Filter::parse(text)));
    let hide_locked = RwSignal::new(true);
    let free_only = RwSignal::new(false);
    let fold = RwSignal::new(false);
    let sort_by_queue = RwSignal::new(false);
    let hidden = RwSignal::new(BTreeSet::<String>::new());
    let limit = RwSignal::new(PAGE_SIZE);
    let results = Memo::new(move |_| {
        let hide_locked = hide_locked.get();
        let free_only = free_only.get();
        let by_queue = sort_by_queue.get();
        let complete = done.get();
        saved.with(|saved| {
            live.with(|live| {
                hidden.with(|hidden| {
                    filter.with(|filter| {
                        let mut responses: BTreeMap<&str, &SearchResponse> = saved
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
                        let mut groups = responses
                            .into_values()
                            .filter(|row| {
                                !hidden.contains(&row.username)
                                    && (!free_only || row.has_free_upload_slot)
                            })
                            .filter_map(|row| group(row, filter, hide_locked).map(Arc::new))
                            .collect::<Vec<_>>();
                        if by_queue {
                            groups.sort_by_key(|group| group.queue);
                        } else {
                            groups.sort_by_key(|group| Reverse(group.speed));
                        }
                        (count, groups)
                    })
                })
            })
        })
    });
    let hide = Callback::new(move |username: String| {
        hidden.update(|hidden| {
            hidden.insert(username);
        });
    });

    view! {
        <Show when=move || done.get() || results.with(|(count, _)| *count != 0)>
            <div class="ui raised segment search-options">
                <select class="ui compact dropdown search-options-sort" aria-label="Sort results"
                    on:change=move |event| sort_by_queue.set(event_target_value(&event) == "queue")>
                    <option value="speed">"Upload Speed (Fastest to Slowest)"</option>
                    <option value="queue">"Queue Depth (Least to Most)"</option>
                </select>
                <div class="search-option-toggles">
                    <label class="search-options-hide-locked"><input type="checkbox" prop:checked=hide_locked
                        on:change=move |event| hide_locked.set(event_target_checked(&event))/>" Hide Locked Results"</label>
                    <label class="search-options-hide-no-slots"><input type="checkbox" prop:checked=free_only
                        on:change=move |event| free_only.set(event_target_checked(&event))/>" Hide Results with No Free Slots"</label>
                    <label class="search-options-fold-results"><input type="checkbox" prop:checked=fold
                        on:change=move |event| fold.set(event_target_checked(&event))/>" Fold Results"</label>
                </div>
                <div class="ui left labeled action input search-filter">
                    <label for="search-result-filter" class="ui label"><i class="filter icon" aria-hidden="true"></i>"Filter"</label>
                    <input id="search-result-filter" type="search" prop:value=text on:input=move |event| text.set(event_target_value(&event))
                        placeholder="-term, isvbr, iscbr, islossless, islossy, minbitrate:320, minbitdepth:24, minfilesize:10, minfilesinfolder:8, minlength:5000"/>
                    <button type="button" class="ui red icon button" aria-label="Clear filters" disabled=move || text.with(String::is_empty)
                        on:click=move |_| text.set(String::new())><i class="x icon" aria-hidden="true"></i></button>
                </div>
                <Show when=move || hidden.with(|hidden| !hidden.is_empty())>
                    <button type="button" class="ui basic mini button" on:click=move |_| hidden.update(BTreeSet::clear)>"Restore hidden results"</button>
                </Show>
            </div>
            <For each=move || results.with(|(_, rows)| rows.iter().take(limit.get()).map(|row| row.username.clone()).collect::<Vec<_>>())
                key=String::clone children=move |username: String| {
                    let key = username.clone();
                    let row = Memo::new(move |_| results.with(|(_, rows)| rows.iter().find(|row| row.username == key).cloned()));
                    view! { <ResultCard username row connected fold hide/> }
                }/>
            {move || results.with(|(count, rows)| {
                let remaining = rows.len().saturating_sub(limit.get());
                let filtered = count - rows.len();
                if remaining != 0 {
                    view! {
                        <button type="button" class="ui fluid large primary button showmore-button" on:click=move |_| limit.update(|count| *count += PAGE_SIZE)>
                            {format!("Show {} More Results ({remaining} remaining, {filtered} hidden by filter(s))", remaining.min(PAGE_SIZE))}
                        </button>
                    }.into_any()
                } else if filtered != 0 {
                    view! { <div class="ui fluid large disabled button showmore-button" role="status">
                        {format!("All results shown. {filtered} results hidden by filter(s)")}
                    </div> }.into_any()
                } else if *count == 0 && done.get() && ready.get() {
                    view! { <div class="ui basic placeholder segment placeholder-segment"><div class="ui icon header"><i class="search icon" aria-hidden="true"></i>"No results to display"</div></div> }.into_any()
                } else { ().into_any() }
            })}
        </Show>
    }
}

type Selection = BTreeMap<String, QueueDownloadRequest>;

#[component]
fn ResultCard(
    username: String,
    row: Memo<Option<Arc<Group>>>,
    connected: Signal<bool>,
    fold: RwSignal<bool>,
    hide: Callback<String>,
) -> impl IntoView {
    let username = StoredValue::new(username);
    let folded = RwSignal::new(fold.get_untracked());
    Effect::new(move |_| folded.set(fold.get()));
    let selecting = RwSignal::new(Selection::new());
    let pending = RwSignal::new(false);
    let message = RwSignal::new(None::<(bool, String)>);
    let extra = RwSignal::new(BTreeMap::<String, Vec<File>>::new());
    let looking_up = RwSignal::new(false);
    let directory_error = RwSignal::new(None::<String>);
    let name = username.get_value();
    let download = move |_| {
        if pending.get_untracked() || !connected.get_untracked() {
            return;
        }
        let requests =
            selecting.with_untracked(|selection| selection.values().cloned().collect::<Vec<_>>());
        if requests.is_empty() {
            return;
        }
        let endpoint = format!(
            "/transfers/downloads/{}",
            username.with_value(|name| util::encode_uri_component(name))
        );
        pending.set(true);
        message.set(None);
        spawn_local_scoped_with_cancellation(async move {
            match api::post::<_, EnqueueDownloadsResponse>(&endpoint, &requests).await {
                Ok(response) if response.failed.is_empty() => {
                    selecting.update(BTreeMap::clear);
                    message.set(Some((
                        true,
                        format!("Queued {} file(s)", response.enqueued.len()),
                    )));
                }
                Ok(response) => {
                    selecting.update(|selection| {
                        for queued in &response.enqueued {
                            selection.remove(&queued.filename);
                        }
                    });
                    let detail = response
                        .failed
                        .iter()
                        .filter(|failure| !failure.message.is_empty())
                        .map(|failure| failure.message.as_str())
                        .collect::<Vec<_>>()
                        .join("; ");
                    message.set(Some((
                        false,
                        format!(
                            "Queued {} file(s); {} failed. {detail}",
                            response.enqueued.len(),
                            response.failed.len()
                        ),
                    )));
                }
                Err(error) => message.set(Some((false, error.to_string()))),
            }
            pending.set(false);
        });
    };
    let load_directory = Callback::new(move |(directory, separator): (String, char)| {
        if looking_up.get_untracked() || !connected.get_untracked() {
            return;
        }
        looking_up.set(true);
        directory_error.set(None);
        let endpoint = format!(
            "/users/{}/directory",
            username.with_value(|name| util::encode_uri_component(name))
        );
        spawn_local_scoped_with_cancellation(async move {
            let request = DirectoryContentsRequest {
                directory: directory.clone(),
            };
            match api::post::<_, Vec<Directory>>(&endpoint, &request).await {
                Ok(dirs) => {
                    if let Some(root) = dirs.into_iter().next() {
                        let files = root
                            .files
                            .into_iter()
                            .map(|file| {
                                let mut file = File::from(file);
                                file.filename = format!(
                                    "{directory}{separator}{}",
                                    util::get_file_name(&file.filename)
                                );
                                file
                            })
                            .collect();
                        extra.update(|extra| {
                            extra.insert(directory, files);
                        });
                    } else {
                        directory_error
                            .set(Some("No directories were included in the response".into()));
                    }
                }
                Err(error) => directory_error.set(Some(error.to_string())),
            }
            looking_up.set(false);
        });
    });
    view! {
        <div class="ui raised card result-card">
            <div class="content">
                <div class="header">
                    <button type="button" class="ui mini basic icon button" aria-label="Fold result" aria-expanded=move || !folded.get()
                        on:click=move |_| folded.update(|folded| *folded = !*folded)>
                        <i class=move || if folded.get() { "chevron right icon" } else { "chevron down icon" } aria-hidden="true"></i>
                    </button>
                    <i class=move || if row.with(|row| row.as_ref().is_some_and(|row| row.free_slot)) { "green circle icon" } else { "yellow circle icon" } aria-hidden="true"></i>
                    {name}
                    <button type="button" class="ui mini right floated basic red icon button close-button" title="Hide result" aria-label="Hide result"
                        on:click=move |_| hide.run(username.get_value())><i class="close icon" aria-hidden="true"></i></button>
                </div>
                <div class="meta result-meta">{move || row.with(|row| row.as_ref().map(|row| {
                    format!("Upload Speed: {}/s, Free Upload Slot: {}, Queue Length: {}", util::format_bytes(row.speed.into()), if row.free_slot { "YES" } else { "NO" }, row.queue)
                }))}</div>
                <Show when=move || !folded.get()>
                    <For each=move || row.with(|row| row.as_ref().map(|row| row.dirs.keys().cloned().collect::<Vec<_>>()).unwrap_or_default())
                        key=String::clone children=move |directory: String| {
                            let key = directory.clone();
                            let files = Memo::new(move |_| {
                                let mut files = row.with(|row| row.as_ref().and_then(|row| row.dirs.get(&key)).cloned()).unwrap_or_default();
                                extra.with(|extra| {
                                    if let Some(additional) = extra.get(&key) {
                                        let known = files.iter().map(|file| file.filename.as_str()).collect::<BTreeSet<_>>();
                                        let added = additional.iter().filter(|file| !known.contains(file.filename.as_str())).cloned().collect::<Vec<_>>();
                                        files.extend(added);
                                    }
                                });
                                files.sort_unstable_by(|a, b| a.filename.cmp(&b.filename));
                                files
                            });
                            view! { <ResultDirectory directory files selecting connected pending looking_up load_directory/> }
                        }/>
                </Show>
                {move || directory_error.get().map(|error| view! { <div class="ui negative message" role="alert">{error}</div> })}
            </div>
            <div class="extra content" hidden=move || selecting.with(BTreeMap::is_empty) && message.with(Option::is_none)>
                <button type="button" class="ui green labeled icon button" class:loading=pending
                    disabled=move || !connected.get() || pending.get() || selecting.with(BTreeMap::is_empty) on:click=download>
                    <i class="download icon" aria-hidden="true"></i>"Download "
                    {move || selecting.with(|selected| {
                        let size = selected.values().map(|file| file.size as f64).sum();
                        format!("{} file(s), {}", selected.len(), util::format_bytes(size))
                    })}
                </button>
                {move || message.get().map(|(ok, message)| view! {
                    <div class=if ok { "ui positive message" } else { "ui negative message" } role=if ok { "status" } else { "alert" }>{message}</div>
                })}
            </div>
        </div>
    }
}

#[component]
fn ResultDirectory(
    directory: String,
    files: Memo<Vec<File>>,
    selecting: RwSignal<Selection>,
    connected: Signal<bool>,
    pending: RwSignal<bool>,
    looking_up: RwSignal<bool>,
    load_directory: Callback<(String, char)>,
) -> impl IntoView {
    let folded = RwSignal::new(false);
    let locked = Memo::new(move |_| {
        files.with(|files| !files.is_empty() && files.iter().all(|file| file.is_locked))
    });
    let name = if directory.is_empty() {
        "(root)".into()
    } else {
        directory.clone()
    };
    let directory = StoredValue::new(directory);
    let lookup = move |_| {
        let separator = files.with_untracked(|files| {
            files
                .first()
                .and_then(|file| {
                    file.filename
                        .chars()
                        .rev()
                        .find(|&ch| ch == '\\' || ch == '/')
                })
                .unwrap_or('\\')
        });
        load_directory.run((directory.get_value(), separator));
    };
    view! {
        <div>
            <h4 class="ui small header filelist-header">
                <button type="button" class="ui mini basic icon button" aria-label="Fold directory" aria-expanded=move || !folded.get()
                    on:click=move |_| folded.update(|folded| *folded = !*folded)>
                    <i class=move || if locked.get() { "lock icon" } else if folded.get() { "folder icon" } else { "folder open icon" } aria-hidden="true"></i>
                </button>{name}
            </h4>
            <Show when=move || !folded.get()>
                <table class="ui table">
                    <thead><tr>
                        <th class="filelist-selector"><input type="checkbox" aria-label="Select all unlocked files in directory"
                            disabled=move || pending.get() || locked.get()
                            prop:checked=move || files.with(|files| selecting.with(|selected| {
                                files.iter().any(|file| !file.is_locked)
                                    && files.iter().filter(|file| !file.is_locked).all(|file| selected.contains_key(&file.filename))
                            }))
                            on:change=move |event| {
                                let checked = event_target_checked(&event);
                                files.with_untracked(|files| selecting.update(|selected| {
                                    for file in files.iter().filter(|file| !file.is_locked) {
                                        select(selected, &file.filename, file.size, checked);
                                    }
                                }));
                            }/>
                        </th><th class="filelist-filename">"File"</th><th class="filelist-size">"Size"</th>
                        <th class="filelist-attributes">"Attributes"</th><th class="filelist-length">"Length"</th>
                    </tr></thead>
                    <tbody>{move || files.with(|files| files.iter().map(|file| {
                        let filename = StoredValue::new(file.filename.clone());
                        let locked = file.is_locked;
                        let size = file.size;
                        view! {
                            <tr class:filebrowser-locked=locked>
                                <td class="filelist-selector"><input type="checkbox" aria-label=format!("Select {}", util::get_file_name(&file.filename))
                                    disabled=move || locked || pending.get()
                                    prop:checked=move || !locked && selecting.with(|selected| filename.with_value(|name| selected.contains_key(name)))
                                    on:change=move |event| if !locked {
                                        selecting.update(|selected| filename.with_value(|name| select(selected, name, size, event_target_checked(&event))));
                                    }/></td>
                                <td class="filelist-filename">{locked.then(|| view! { <i class="yellow lock icon" title="Locked" aria-label="Locked"></i> })}{util::get_file_name(&file.filename).to_owned()}</td>
                                <td class="filelist-size">{util::format_bytes(size as f64)}</td>
                                <td class="filelist-attributes">{util::format_attributes(file.bit_rate, file.is_variable_bit_rate, file.bit_depth, file.sample_rate)}</td>
                                <td class="filelist-length">{util::format_seconds(file.length.unwrap_or_default().into())}</td>
                            </tr>
                        }
                    }).collect_view())}</tbody>
                    <Show when=move || !locked.get() && directory.with_value(|dir| !dir.is_empty())>
                        <tfoot class="full-width"><tr><th colspan="5">
                            <button type="button" class="ui fluid basic button" class:loading=looking_up
                                disabled=move || !connected.get() || looking_up.get() on:click=lookup>
                                <i class="search icon" aria-hidden="true"></i>"Search for Additional Files in This Directory"
                            </button>
                        </th></tr></tfoot>
                    </Show>
                </table>
            </Show>
        </div>
    }
}

fn select(selected: &mut Selection, filename: &str, size: u64, checked: bool) {
    if checked {
        selected
            .entry(filename.to_owned())
            .or_insert_with(|| QueueDownloadRequest {
                filename: filename.to_owned(),
                size,
            });
    } else {
        selected.remove(filename);
    }
}
