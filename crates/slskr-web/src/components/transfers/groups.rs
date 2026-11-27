//! A user's card (collapsible header, directory sections, file rows) and the details
//! dialog. Every row reads only its own transfer signal (see the module docs of
//! `transfers`).

use std::{
    collections::{HashMap, HashSet},
    sync::atomic::{AtomicUsize, Ordering},
};

use chrono::{DateTime, Utc};
use leptos::{prelude::*, task::spawn_local_scoped_with_cancellation};
use slskr_types::transfers::{QueuePosition, Transfer, TransferDirection, TransferState as State};

use crate::{
    services::{api, util},
    ui::{Avatar, Badge, Button, DropdownMenu, Icon, Modal},
};

use super::{
    Category, Counts, Entry, FileRef, Operation, Page, UserLayout, directory_of, transfer_url,
};

static NEXT_GROUP: AtomicUsize = AtomicUsize::new(0);

/// The folders that get a row: each one with files, and each one where paths branch,
/// except `root` (the one shared folder every upload is in), which is implied.
fn touched_folders<'a>(
    directories: impl IntoIterator<Item = &'a str>,
    root: Option<&str>,
) -> HashSet<&'a str> {
    let mut touched = HashSet::new();
    let mut first_child = HashMap::new();
    for directory in directories {
        let mut child = directory;
        touched.insert(child);
        let mut parent = directory_of(child);
        while !parent.is_empty() {
            if let Some(previous) = first_child.insert(parent, child) {
                if previous != child && Some(parent) != root {
                    touched.insert(parent);
                }
                // Ancestors of this parent were already visited.
                break;
            }
            child = parent;
            parent = directory_of(child);
        }
    }
    touched
}

/// The nearest ancestor of `path` that has a row.
fn touched_parent<'a>(path: &'a str, touched: &HashSet<&str>) -> Option<&'a str> {
    std::iter::successors(Some(directory_of(path)), |path| Some(directory_of(path)))
        .take_while(|path| !path.is_empty())
        .find(|path| touched.contains(path))
}

/// Each row's child rows (`""` holds the top level), alphabetically. Folders
/// without files that lead to only one branch fold into their child's row.
fn folder_tree<'a>(
    directories: impl IntoIterator<Item = &'a str> + Clone,
    root: Option<&str>,
) -> HashMap<String, Vec<String>> {
    let touched = touched_folders(directories.clone(), root);
    let mut children = HashMap::<String, Vec<String>>::new();
    let mut seen = HashSet::new();
    for mut path in directories {
        while seen.insert(path) {
            let parent = touched_parent(path, &touched).unwrap_or_default();
            children
                .entry(parent.to_owned())
                .or_default()
                .push(path.to_owned());
            if parent.is_empty() {
                break;
            }
            path = parent;
        }
    }
    for rows in children.values_mut() {
        rows.sort_by_cached_key(|path| (path.to_lowercase(), path.clone()));
    }
    children
}

#[derive(Default, PartialEq)]
struct FolderFiles {
    /// Files directly in the folder.
    direct: Vec<FileRef>,
    /// Selectable files in the folder and the rows below it.
    selectable: Vec<String>,
    /// All files in the folder and the rows below it.
    count: usize,
    /// States of the listed files in the folder and the rows below it.
    counts: Counts,
}

fn folder_files(user: &UserLayout, root: Option<&str>) -> HashMap<String, FolderFiles> {
    let paths = user.directories.iter().map(|dir| dir.directory.as_str());
    let touched = touched_folders(paths, root);
    let mut folders = HashMap::<String, FolderFiles>::new();
    for directory in &user.directories {
        let path = directory.directory.as_str();
        folders
            .entry(path.to_owned())
            .or_default()
            .direct
            .clone_from(&directory.files);
        let selectable = selectable(&directory.files);
        for path in std::iter::successors(Some(path), |path| touched_parent(path, &touched)) {
            let folder = folders.entry(path.to_owned()).or_default();
            folder.count += directory.files.len();
            folder.counts.merge(&directory.counts);
            folder.selectable.extend(selectable.iter().cloned());
        }
    }
    folders
}

/// A folder row's label: its path below the row it is nested under, or at the top
/// level its whole path without `root` (unless that is all there is).
fn folder_label(parent: &str, path: &str, root: Option<&str>) -> String {
    let base = if parent.is_empty() {
        root
    } else {
        Some(parent)
    };
    let relative = base
        .and_then(|base| path.strip_prefix(base))
        .and_then(|rest| rest.strip_prefix(['\\', '/']))
        .filter(|rest| !rest.is_empty())
        .unwrap_or(path);
    relative.split(['\\', '/']).collect::<Vec<_>>().join(" / ")
}

fn indent(depth: usize) -> String {
    format!("--transfer-indent: {}ch", depth.min(8) * 2)
}

/// Selectable (not removed) ids of a set of files.
fn selectable<'a>(files: impl IntoIterator<Item = &'a FileRef>) -> Vec<String> {
    files
        .into_iter()
        .filter(|file| !file.removed)
        .map(|file| file.id.clone())
        .collect()
}

/// A header checkbox that selects or clears `ids`: checked when all are selected,
/// indeterminate when some are.
#[component]
fn SelectAll(
    ids: Memo<Vec<String>>,
    selected: RwSignal<HashSet<String>>,
    label: String,
) -> impl IntoView {
    let state = Memo::new(move |_| {
        ids.with(|ids| {
            selected.with(|selected| {
                let chosen = ids.iter().filter(|id| selected.contains(*id)).count();
                (chosen, ids.len())
            })
        })
    });
    view! {
        <input type="checkbox" class="checkbox" aria-label=label
            disabled=move || state.with(|(_, total)| *total == 0)
            prop:checked=move || state.with(|(chosen, total)| *total > 0 && chosen == total)
            prop:indeterminate=move || state.with(|(chosen, total)| *chosen > 0 && chosen < total)
            on:change=move |event| {
                let checked = event_target_checked(&event);
                ids.with_untracked(|ids| selected.update(|selected| {
                    for id in ids {
                        if checked { selected.insert(id.clone()); } else { selected.remove(id); }
                    }
                }));
            }/>
    }
}

#[component]
pub(super) fn TransferGroup(username: String, page: Page) -> impl IntoView {
    let Page {
        direction,
        layout,
        selected,
        working,
        connected,
        change,
        ..
    } = page;
    let name = StoredValue::new(username.clone());
    let user = Memo::new(move |_| {
        name.with_value(|name| {
            layout.with(|layout| layout.iter().find(|user| &user.username == name).cloned())
        })
    });
    let ids = Memo::new(move |_| {
        user.with(|user| {
            selectable(
                user.iter()
                    .flat_map(|user| &user.directories)
                    .flat_map(|directory| &directory.files),
            )
        })
    });
    let folded = RwSignal::new(false);
    // Folders the user toggled away from the page default (`page.collapse`); choosing a
    // new default resets them.
    let toggled_folders = RwSignal::new(HashSet::<String>::new());
    Effect::new(move |previous: Option<bool>| {
        let collapse = page.collapse.get();
        if previous.is_some_and(|previous| previous != collapse) {
            toggled_folders.update(HashSet::clear);
        }
        collapse
    });
    let tree = Memo::new(move |_| {
        let root = page.share_root.get();
        user.with(|user| {
            user.as_ref()
                .map(|user| {
                    let paths = user.directories.iter().map(|dir| dir.directory.as_str());
                    folder_tree(paths, root.as_deref())
                })
                .unwrap_or_default()
        })
    });
    let folders = Memo::new(move |_| {
        let root = page.share_root.get();
        user.with(|user| {
            user.as_ref()
                .map(|user| folder_files(user, root.as_deref()))
                .unwrap_or_default()
        })
    });
    // Rows of open folders, depth first.
    let visible = Memo::new(move |_| {
        let root = page.share_root.get();
        let collapse = page.collapse.get();
        tree.with(|tree| {
            toggled_folders.with(|toggled| {
                let rows = |parent: &str, depth: usize| {
                    tree.get(parent)
                        .into_iter()
                        .flatten()
                        .rev()
                        .map(|path| {
                            let label = folder_label(parent, path, root.as_deref());
                            (path.clone(), depth, label)
                        })
                        .collect::<Vec<_>>()
                };
                let mut pending = rows("", 0);
                let mut visible = Vec::new();
                while let Some((path, depth, label)) = pending.pop() {
                    // Open unless folded: by default, or by the user's own toggle.
                    if !path.is_empty() && collapse == toggled.contains(&path) {
                        pending.extend(rows(&path, depth + 1));
                    }
                    visible.push((path, depth, label));
                }
                visible
            })
        })
    });
    let content_id = format!(
        "transfer-group-{}",
        NEXT_GROUP.fetch_add(1, Ordering::Relaxed)
    );
    let base = page.base();
    let encoded = util::encode_uri_component(&username);
    let user_href = format!("{base}/users?username={encoded}");
    let browse_href = format!("{base}/browse?username={encoded}");
    let busy = move || working.with(Option::is_some);
    let has = move |operation: Operation, keep: fn(&Entry) -> bool| {
        name.with_value(|name| {
            page.index.with(|index| {
                index.iter().any(|entry| {
                    &entry.username == name && operation.allows(direction, entry) && keep(entry)
                })
            })
        })
    };
    let run = move |operation: Operation, keep: fn(&Entry) -> bool| {
        let files = name.with_value(|name| page.targets(operation, Some(name), keep));
        change.run((operation, files));
    };
    let is_download = direction == TransferDirection::Download;

    view! {
        <section class="card transfer-group" class:is-folded=folded>
            <div class="transfer-group__header">
                <button type="button" class="btn btn--ghost btn--icon btn--sm transfer-group__toggle"
                    aria-expanded=move || (!folded.get()).to_string() aria-controls=content_id.clone()
                    aria-label=format!("Show {username}'s transfers") title="Collapse or expand"
                    on:click=move |_| folded.update(|folded| *folded = !*folded)>
                    <Icon name=Signal::derive(move || if folded.get() { "chevron-right".to_owned() } else { "chevron-down".to_owned() })/>
                </button>
                <Avatar name=username.clone() size="sm"/>
                <div class="transfer-group__title">
                    <a class="transfer-group__user" href=user_href.clone()>{username.clone()}</a>
                    <span class="transfer-group__totals muted text-xs tabular">
                        {move || user.with(|user| user.as_ref().map(totals))}
                    </span>
                </div>
                <div class="cluster transfer-group__badges">
                    {move || user.with(|user| user.as_ref().map(group_badges))}
                </div>
                <div class="transfer-group__menu">
                    <DropdownMenu label=format!("Actions for {username}") icon="ellipsis" variant="ghost" size="sm" align="end">
                        {is_download.then(|| view! {
                            <button type="button" class="menu__item" role="menuitem"
                                disabled=move || busy() || !connected.get() || !has(Operation::Retry, |entry| entry.category() == Category::Failed)
                                on:click=move |_| run(Operation::Retry, |entry| entry.category() == Category::Failed)>
                                <Icon name="rotate-cw"/>"Retry failed"
                            </button>
                        })}
                        <button type="button" class="menu__item menu__item--danger" role="menuitem"
                            disabled=move || busy() || !has(Operation::Cancel, |_| true)
                            on:click=move |_| run(Operation::Cancel, |_| true)>
                            <Icon name="circle-x"/>"Cancel all"
                        </button>
                        <button type="button" class="menu__item" role="menuitem"
                            disabled=move || busy() || !has(Operation::Remove, |_| true)
                            on:click=move |_| run(Operation::Remove, |_| true)>
                            <Icon name="trash-2"/>"Clear completed"
                        </button>
                        <div class="menu__separator"></div>
                        <a class="menu__item" role="menuitem" href=browse_href.clone()>
                            <Icon name="folder-open"/>"Browse files"
                        </a>
                        <a class="menu__item" role="menuitem" href=user_href.clone()>
                            <Icon name="user"/>"User info"
                        </a>
                    </DropdownMenu>
                </div>
            </div>
            <div class="table-wrap transfer-group__content" id=content_id hidden=folded>
                <table class="table table--compact transfer-table">
                    <thead><tr>
                        <th class="check-col">
                            <SelectAll ids selected label=format!("Select all transfers from {username}")/>
                        </th>
                        <th class="transfer-table__name">"File"</th>
                        <th class="num transfer-table__size">"Size"</th>
                        <th class="transfer-table__progress">"Progress"</th>
                        <th class="transfer-table__status">"Status"</th>
                        <th class="actions"><span class="sr-only">"Actions"</span></th>
                    </tr></thead>
                    <For each=move || visible.get()
                        key=Clone::clone children=move |(directory, depth, label)| view! {
                            <TransferDirectory directory depth label toggled_folders folders page/>
                        }/>
                </table>
            </div>
        </section>
    }
}

fn totals(user: &UserLayout) -> String {
    let counts = user.counts;
    let folders = user.directories.len();
    format!(
        "{} {} in {} {} · {}",
        counts.files,
        if counts.files == 1 { "file" } else { "files" },
        folders,
        if folders == 1 { "folder" } else { "folders" },
        util::format_bytes(counts.bytes as f64)
    )
}

/// A folder row's status: one badge when every file is in the same state, otherwise
/// a count per unfinished, failed or cancelled state (succeeded files are then left out).
fn folder_status(counts: Counts) -> impl IntoView + use<> {
    let states = [
        (counts.active, "primary", "activity", "Active", "active"),
        (counts.queued, "warning", "clock", "Queued", "queued"),
        (counts.failed, "danger", "circle-alert", "Failed", "failed"),
        (counts.cancelled, "outline", "ban", "Cancelled", "cancelled"),
        (
            counts.succeeded,
            "success",
            "circle-check",
            "Succeeded",
            "succeeded",
        ),
    ];
    let present = states.iter().filter(|(count, ..)| *count > 0).count();
    let badges = states
        .into_iter()
        .filter(|&(count, .., label)| count > 0 && (present == 1 || label != "succeeded"))
        .map(|(count, variant, icon, title, label)| {
            let text = if present == 1 {
                title.to_owned()
            } else {
                format!("{} {label}", util::group_digits(count))
            };
            view! { <Badge variant=variant icon=icon text=text/> }
        })
        .collect_view();
    view! { <div class="cluster transfer-dir__status">{badges}</div> }
}

fn group_badges(user: &UserLayout) -> impl IntoView + use<> {
    let counts = user.counts;
    let mut badges = Vec::new();
    for (count, variant, label) in [
        (counts.active, "primary", "active"),
        (counts.queued, "warning", "queued"),
        (counts.failed, "danger", "failed"),
    ] {
        if count > 0 {
            badges.push(view! { <Badge variant=variant text=format!("{count} {label}")/> });
        }
    }
    badges.collect_view()
}

#[component]
fn TransferDirectory(
    directory: String,
    depth: usize,
    label: String,
    toggled_folders: RwSignal<HashSet<String>>,
    folders: Memo<HashMap<String, FolderFiles>>,
    page: Page,
) -> impl IntoView {
    let title = if directory.is_empty() {
        "(root)".to_owned()
    } else {
        directory.clone()
    };
    let short = if directory.is_empty() {
        title.clone()
    } else {
        label
    };
    // The parent folders give way first when the row is too narrow; the name stays.
    let (lead, name) = match short.rsplit_once(" / ") {
        Some((lead, name)) => (format!("{lead} / "), name.to_owned()),
        None => (String::new(), short.clone()),
    };
    let dir = StoredValue::new(directory);
    let ids = Memo::new(move |_| {
        dir.with_value(|dir| {
            folders.with(|folders| {
                folders
                    .get(dir)
                    .map(|folder| folder.selectable.clone())
                    .unwrap_or_default()
            })
        })
    });
    let folded = Signal::derive(move || {
        page.collapse.get()
            != dir.with_value(|dir| toggled_folders.with(|toggled| toggled.contains(dir)))
    });
    view! {
        <tbody class="transfer-dir" class:is-folded=folded style=indent(depth)>
            <tr class="transfer-dir__row">
                <td class="check-col">
                    <SelectAll ids selected=page.selected label=format!("Select all transfers in {short}")/>
                </td>
                <td class="transfer-dir__cell">
                    <button type="button" class="transfer-dir__toggle"
                        aria-expanded=move || (!folded.get()).to_string()
                        on:click=move |_| dir.with_value(|dir| toggled_folders.update(|toggled| {
                            if !toggled.remove(dir) { toggled.insert(dir.clone()); }
                        }))>
                        <Icon name=Signal::derive(move || if folded.get() { "chevron-right".to_owned() } else { "chevron-down".to_owned() })/>
                        <Icon name=Signal::derive(move || if folded.get() { "folder".to_owned() } else { "folder-open".to_owned() }) class="transfer-dir__icon"/>
                        <span class="transfer-dir__path" title=title.clone()>
                            <span class="transfer-dir__parent">{lead}</span>
                            <span class="transfer-dir__name">{name}</span>
                        </span>
                    </button>
                </td>
                // Same columns as the file rows: the folder's total under Size, its file
                // count under Progress.
                <td class="num transfer-table__size transfer-dir__count muted tabular">
                    {move || dir.with_value(|dir| folders.with(|folders| {
                        util::format_bytes(folders.get(dir).map_or(0, |folder| folder.counts.bytes) as f64)
                    }))}
                </td>
                <td class="transfer-table__progress transfer-dir__count muted">
                    {move || dir.with_value(|dir| folders.with(|folders| {
                        let n = folders.get(dir).map_or(0, |folder| folder.count);
                        format!("{n} {}", if n == 1 { "file" } else { "files" })
                    }))}
                </td>
                <td class="transfer-table__status">
                    {move || dir.with_value(|dir| folders.with(|folders| {
                        folders.get(dir).map(|folder| folder_status(folder.counts))
                    }))}
                </td>
                <td class="actions"></td>
            </tr>
            <Show when=move || !folded.get()>
                <For each=move || dir.with_value(|dir| folders.with(|folders| folders.get(dir).into_iter().flat_map(|folder| &folder.direct).map(|file| file.id.clone()).collect::<Vec<_>>()))
                    key=String::clone children=move |id: String| view! { <TransferRow id page/> }/>
            </Show>
        </tbody>
    }
}

/// What the progress cell shows; the cell is rebuilt only when this changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Connecting,
    Transferring,
    Waiting,
    Finished,
}

fn phase(state: State) -> Phase {
    match state {
        State::Initializing => Phase::Connecting,
        State::InProgress => Phase::Transferring,
        State::Requested | State::QueuedLocally | State::QueuedRemotely => Phase::Waiting,
        _ => Phase::Finished,
    }
}

/// `(variant, icon)` of a state badge.
pub(super) fn state_badge(state: State) -> (&'static str, &'static str) {
    match Category::of(state) {
        Category::Active if state == State::Requested => ("neutral", "clock"),
        Category::Active => ("primary", "activity"),
        Category::Queued => ("warning", "clock"),
        Category::Succeeded => ("success", "circle-check"),
        Category::Cancelled => ("outline", "ban"),
        Category::Failed => ("danger", "circle-alert"),
    }
}

fn file_icon(filename: &str) -> &'static str {
    let extension = filename
        .rsplit_once('.')
        .map(|(_, extension)| extension.to_ascii_lowercase())
        .unwrap_or_default();
    match extension.as_str() {
        "mp3" | "flac" | "ogg" | "opus" | "m4a" | "aac" | "wav" | "aiff" | "aif" | "alac"
        | "wma" | "ape" | "wv" => "file-audio",
        "txt" | "nfo" | "log" | "cue" | "md" | "pdf" => "file-text",
        _ => "file",
    }
}

#[component]
fn TransferRow(id: String, page: Page) -> impl IntoView {
    let Some(signal) = page.rows.with_value(|rows| rows.get(&id).cloned()) else {
        return ().into_any();
    };
    let Page {
        direction,
        selected,
        working,
        connected,
        change,
        details,
        now,
        ..
    } = page;
    // An arena handle owned by this row: the row keeps its signal while it is mounted.
    let file = RwSignal::from(signal);
    let id = StoredValue::new(id);
    let (path, name) = file.with_untracked(|file| {
        (
            file.filename.clone(),
            util::get_file_name(&file.filename).to_owned(),
        )
    });
    let icon = file_icon(&name);
    let state = Memo::new(move |_| file.with(|file| file.state));
    let removed = Memo::new(move |_| file.with(|file| file.removed));
    let stage = Memo::new(move |_| phase(state.get()));
    let is_selected =
        Memo::new(move |_| id.with_value(|id| selected.with(|selected| selected.contains(id))));
    let position = RwSignal::new(None::<u32>);
    let refreshing = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let is_download = direction == TransferDirection::Download;
    let actions = Memo::new(move |_| {
        let (state, removed) = (state.get(), removed.get());
        (
            Operation::Retry.permits(direction, state, removed),
            Operation::Cancel.permits(direction, state, removed),
            Operation::Remove.permits(direction, state, removed),
            is_download && !removed && state.is_queued(),
            is_download && !removed && state == State::Succeeded,
        )
    });
    let busy = Signal::derive(move || working.with(Option::is_some));
    let act = move |operation: Operation| {
        let entry = file.with_untracked(Entry::new);
        change.run((operation, vec![entry]));
    };
    // The request belongs to the row, not to the button (rebuilt when the state changes).
    let owner = StoredValue::new(Owner::current().expect("the row has an owner"));
    let refresh_position = move || {
        if refreshing.get_untracked() || !connected.get_untracked() {
            return;
        }
        refreshing.set(true);
        error.set(None);
        let url = format!(
            "{}/position",
            id.with_value(|id| transfer_url(direction, id))
        );
        owner.with_value(|owner| {
            owner.with(|| {
                spawn_local_scoped_with_cancellation(async move {
                    match api::get::<QueuePosition>(&url).await {
                        Ok(place) => position.set(Some(place.place_in_queue)),
                        Err(err) => error.set(Some(err.to_string())),
                    }
                    refreshing.set(false);
                });
            });
        });
    };
    let base = page.base();
    let reveal_href = format!("{base}/system/files/downloads");

    let size = move || {
        file.with(|file| {
            if matches!(stage.get(), Phase::Transferring) {
                bytes_transferred(file)
            } else {
                util::format_bytes(file.size as f64)
            }
        })
    };
    let progress = move || {
        match stage.get() {
        Phase::Transferring => {
            let percent = Memo::new(move |_| {
                file.with(|file| file.percent_complete.round().clamp(0.0, 100.0) as u32)
            });
            view! {
                <div class="transfer-progress">
                    <div class="progress" role="progressbar" aria-label="Transfer progress"
                        aria-valuemin="0" aria-valuemax="100" aria-valuenow=move || percent.get()>
                        <div class="progress__bar" style:width=move || format!("{}%", percent.get())></div>
                    </div>
                    <div class="transfer-progress__meta tabular">
                        <span class="font-medium">{move || format!("{}%", percent.get())}</span>
                        <span>{move || file.with(|file| util::format_speed(file.average_speed))}</span>
                        <span>{move || file.with(|file| file.remaining_ms.map(|ms| format!("{} left", short_duration(ms))))}</span>
                    </div>
                </div>
            }
            .into_any()
        }
        Phase::Connecting => view! {
            <div class="transfer-progress">
                <div class="progress progress--indeterminate" role="progressbar" aria-label="Connecting">
                    <div class="progress__bar"></div>
                </div>
                <div class="transfer-progress__meta"><span>"Connecting to the peer…"</span></div>
            </div>
        }
        .into_any(),
        Phase::Waiting => view! {
            <span class="transfer-note muted text-xs">
                {move || file.with(|file| waiting_note(file, position.get(), now.get()))}
            </span>
        }
        .into_any(),
        Phase::Finished => view! {
            <span class="transfer-note text-xs" class:text-danger=move || state.get().is_failed() && state.get() != State::Cancelled
                class:muted=move || !state.get().is_failed() || state.get() == State::Cancelled
                title=move || file.with(finished_title)>
                {move || file.with(|file| finished_note(file, now.get()))}
            </span>
        }
        .into_any(),
    }
    };

    view! {
        <tr class="transfer-row" class:is-selected=is_selected class:is-removed=removed
            data-state=move || state.get().as_str()>
            <td class="check-col">
                <input type="checkbox" class="checkbox" aria-label=format!("Select {name}")
                    disabled=removed prop:checked=move || is_selected.get() && !removed.get()
                    on:change=move |event| {
                        let checked = event_target_checked(&event);
                        selected.update(|selected| {
                            if checked { selected.insert(id.get_value()); }
                            else { id.with_value(|id| selected.remove(id)); }
                        });
                    }/>
            </td>
            <td class="transfer-table__name">
                <div class="transfer-name">
                    <Icon name=icon class="transfer-name__icon"/>
                    <span class="truncate" title=path>{name.clone()}</span>
                    {move || removed.get().then(|| view! { <Badge variant="outline" text="Removed"/> })}
                </div>
                {move || error.get().map(|error| view! { <div class="text-xs text-danger" role="alert">{error}</div> })}
            </td>
            <td class="num transfer-table__size">{size}</td>
            <td class="transfer-table__progress">{progress}</td>
            <td class="transfer-table__status">
                {move || {
                    let state = state.get();
                    let (variant, icon) = state_badge(state);
                    view! { <Badge variant=variant icon=icon text=state.label() class="transfer-state"/> }
                }}
            </td>
            <td class="actions">
                <div class="transfer-actions">
                    {move || {
                        let (retry, cancel, remove, queue, reveal) = actions.get();
                        view! {
                            {queue.then(|| view! {
                                <Button variant="action" icon="refresh-cw" label="Refresh queue position"
                                    loading=refreshing disabled=Signal::derive(move || !connected.get())
                                    on_click=Callback::new(move |_| refresh_position())/>
                            })}
                            {retry.then(|| view! {
                                <Button variant="action" icon="rotate-cw" label="Retry download"
                                    disabled=Signal::derive(move || busy.get() || !connected.get())
                                    on_click=Callback::new(move |_| act(Operation::Retry))/>
                            })}
                            {cancel.then(|| view! {
                                <Button variant="action" icon="circle-x" label="Cancel transfer" class="is-danger"
                                    disabled=busy on_click=Callback::new(move |_| act(Operation::Cancel))/>
                            })}
                            {reveal.then(|| view! {
                                <Button variant="action" icon="folder-open" label="Show in the downloads folder"
                                    href=reveal_href.clone()/>
                            })}
                            {remove.then(|| view! {
                                <Button variant="action" icon="trash-2" label="Remove from list" class="is-danger"
                                    disabled=busy on_click=Callback::new(move |_| act(Operation::Remove))/>
                            })}
                        }
                    }}
                    <Button variant="action" icon="info" label="Transfer details"
                        on_click=Callback::new(move |_| details.set(Some(id.get_value())))/>
                </div>
            </td>
        </tr>
    }
    .into_any()
}

fn waiting_note(file: &Transfer, refreshed: Option<u32>, now: DateTime<Utc>) -> String {
    let place = file.place_in_queue.or(refreshed).filter(|place| *place > 0);
    let mut note = match (file.state, place) {
        (_, Some(place)) => format!("#{place} in queue"),
        (State::QueuedRemotely, None) => "Waiting in the peer's queue".to_owned(),
        (State::QueuedLocally, None) => "Waiting for a free slot".to_owned(),
        _ => format!("Requested {}", relative(file.requested_at, now)),
    };
    if file.attempts > 1 {
        note.push_str(&format!(" · attempt {}", file.attempts));
    }
    note
}

fn finished_note(file: &Transfer, now: DateTime<Utc>) -> String {
    let when = file
        .ended_at
        .map(|ended| relative(ended, now))
        .unwrap_or_default();
    let mut note = match file.state {
        State::Succeeded => format!("Finished {when}"),
        State::Cancelled => format!("Cancelled {when}"),
        _ => match &file.error {
            Some(error) if !error.is_empty() => format!("{error} · {when}"),
            _ => format!("{} {when}", file.state.label()),
        },
    };
    if file.attempts > 1 {
        note.push_str(&format!(" · attempt {}", file.attempts));
    }
    note.trim_end_matches([' ', '·']).to_owned()
}

fn finished_title(file: &Transfer) -> String {
    let when = file
        .ended_at
        .map(|ended| date(Some(ended)))
        .unwrap_or_default();
    match &file.error {
        Some(error) if !error.is_empty() => format!("{error}\n{when}"),
        _ => when,
    }
}

/// "just now", "5m ago", "2h ago", "3d ago" (or "in 5m" for future times).
fn relative(then: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let seconds = (now - then).num_seconds();
    let (amount, future) = (seconds.unsigned_abs(), seconds < 0);
    if amount < 45 {
        return "just now".to_owned();
    }
    let text = if amount < 3_600 {
        format!("{}m", (amount / 60).max(1))
    } else if amount < 86_400 {
        format!("{}h", amount / 3_600)
    } else {
        format!("{}d", amount / 86_400)
    };
    if future {
        format!("in {text}")
    } else {
        format!("{text} ago")
    }
}

/// "45s", "3m 10s", "1h 5m".
fn short_duration(ms: u64) -> String {
    let seconds = ms / 1_000;
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3_600 {
        format!("{}m {}s", seconds / 60, seconds % 60)
    } else {
        format!("{}h {}m", seconds / 3_600, (seconds % 3_600) / 60)
    }
}

fn date(value: Option<DateTime<Utc>>) -> String {
    value.map_or_else(|| "—".into(), |date| util::format_date(&date.to_rfc3339()))
}

fn bytes_transferred(file: &Transfer) -> String {
    let total = util::format_bytes_with(file.size as f64, 1, 0, ' ');
    let (size, unit) = total.split_once(' ').unwrap_or((&total, "B"));
    let transferred = util::format_bytes_as_unit(file.bytes_transferred as f64, unit, 1);
    let transferred = if unit == "B" {
        transferred.strip_suffix(" B").unwrap_or(&transferred)
    } else {
        &transferred
    };
    format!("{transferred} / {size} {unit}")
}

/// The details dialog for `Page::details`; it follows the live transfer.
#[component]
pub(super) fn DetailsDialog(page: Page) -> impl IntoView {
    let details = page.details;
    let file = Signal::derive(move || {
        details
            .get()
            .and_then(|id| page.rows.with_value(|rows| rows.get(&id).cloned()))
            .map(|signal| signal.get())
    });
    view! {
        <Modal open=Signal::derive(move || details.with(Option::is_some)) size="lg"
            on_close=Callback::new(move |()| details.set(None))
            title="Transfer details">
            {move || match file.get() {
                Some(file) => transfer_details(&file).into_any(),
                None => view! { <p class="muted">"This transfer is no longer listed."</p> }.into_any(),
            }}
        </Modal>
    }
}

fn transfer_details(file: &Transfer) -> impl IntoView + use<> {
    let time = |value: Option<u64>| value.map_or_else(|| "—".into(), short_duration);
    let (variant, icon) = state_badge(file.state);
    let fields = [
        ("Username", file.username.clone()),
        ("Filename", file.filename.clone()),
        ("Size", util::format_bytes(file.size as f64)),
        ("Requested", date(Some(file.requested_at))),
        ("Enqueued", date(file.enqueued_at)),
        ("Started", date(file.started_at)),
        ("Ended", date(file.ended_at)),
        (
            "Transferred",
            format!(
                "{} ({:.1}%)",
                util::format_bytes(file.bytes_transferred as f64),
                file.percent_complete
            ),
        ),
        ("Remaining", util::format_bytes(file.bytes_remaining as f64)),
        ("Average speed", util::format_speed(file.average_speed)),
        ("Elapsed", time(file.elapsed_ms)),
        ("Time left", time(file.remaining_ms)),
        (
            "Place in queue",
            file.place_in_queue
                .map_or_else(|| "—".into(), |place| place.to_string()),
        ),
        ("Attempts", file.attempts.to_string()),
        ("Next attempt", date(file.next_attempt_at)),
        ("Error", file.error.clone().unwrap_or_else(|| "—".into())),
        ("ID", file.id.clone()),
    ];
    view! {
        <div class="stack stack--sm transfer-details">
            <div class="cluster">
                <Badge variant=variant icon=icon text=file.state.label()/>
                {file.removed.then(|| view! { <Badge variant="outline" text="Removed"/> })}
            </div>
            <dl class="dl">
                {fields.into_iter().map(|(field, value)| view! {
                    <dt>{field}</dt><dd class="break-all">{value}</dd>
                }).collect_view()}
            </dl>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folder_rows_show_the_whole_path_without_the_one_shared_folder() {
        for separator in ["/", "\\"] {
            let album = ["music", "artist", "album1"].join(separator);
            assert_eq!(folder_label("", &album, None), "music / artist / album1");
            assert_eq!(folder_label("", &album, Some("music")), "artist / album1");
            assert_eq!(folder_label("", "music", Some("music")), "music");
            assert_eq!(
                folder_label("", &album, Some("mus")),
                "music / artist / album1"
            );
            let artist = ["music", "artist"].join(separator);
            assert_eq!(folder_label(&artist, &album, Some("music")), "album1");
        }
    }

    #[test]
    fn folders_nest_where_paths_branch_but_not_under_the_shared_folder() {
        let dirs = [
            r"music\Beatles\White Album\CD02",
            r"music\Beatles\White Album\CD01",
            r"music\Beatles\Magical Mystery Tour",
            r"music\Beatles\Revolver",
            r"music\Other\Album",
        ];
        let tree = folder_tree(dirs, Some("music"));
        let children = |path: &str| tree.get(path).cloned().unwrap_or_default();
        assert_eq!(
            children(""),
            [r"music\Beatles", r"music\Other\Album"],
            "the shared folder has no row, and a one-branch chain shares its child's row"
        );
        assert_eq!(
            children(r"music\Beatles"),
            [
                r"music\Beatles\Magical Mystery Tour",
                r"music\Beatles\Revolver",
                r"music\Beatles\White Album"
            ]
        );
        assert_eq!(
            children(r"music\Beatles\White Album"),
            [
                r"music\Beatles\White Album\CD01",
                r"music\Beatles\White Album\CD02"
            ]
        );
        // Without a shared folder to leave out, `music` is where everything branches.
        assert_eq!(folder_tree(dirs, None)[""], ["music"]);
    }
}
