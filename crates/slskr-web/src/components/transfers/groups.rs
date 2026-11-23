use std::{collections::BTreeSet, time::Duration};

use leptos::{prelude::*, task::spawn_local_scoped_with_cancellation};
use slskr_types::transfers::{QueuePosition, Transfer, TransferDirection, TransferState as State};

use crate::services::{api, util};

use super::{Change, Operation, UserResponse, transfer_url};

type Groups = RwSignal<Vec<UserResponse>>;
type Selection = RwSignal<BTreeSet<String>>;

fn in_user<T>(
    users: Groups,
    user: StoredValue<String>,
    f: impl FnOnce(Option<&UserResponse>) -> T,
) -> T {
    user.with_value(|name| users.with(|users| f(users.iter().find(|user| &user.username == name))))
}

fn in_directory<T>(
    users: Groups,
    user: StoredValue<String>,
    dir: StoredValue<String>,
    f: impl FnOnce(&[Transfer]) -> T,
) -> T {
    in_user(users, user, |user| {
        dir.with_value(|name| {
            f(user
                .and_then(|user| user.directories.iter().find(|dir| &dir.directory == name))
                .map_or(&[], |dir| dir.files.as_slice()))
        })
    })
}

#[component]
pub(super) fn TransferGroup(
    username: String,
    users: Groups,
    direction: TransferDirection,
    connected: Signal<bool>,
    working: RwSignal<Option<Operation>>,
    change: Change,
) -> impl IntoView {
    let user = StoredValue::new(username.clone());
    let selected = RwSignal::new(BTreeSet::new());
    let folded = RwSignal::new(false);
    let info = Memo::new(move |_| {
        in_user(users, user, |user| {
            selected.with(|selected| {
                let mut count = 0;
                let mut any_cancel = false;
                let mut all_retry = direction == TransferDirection::Download;
                let mut all_remove = true;
                for file in user
                    .into_iter()
                    .flat_map(|user| &user.directories)
                    .flat_map(|dir| &dir.files)
                    .filter(|file| !file.removed && selected.contains(&file.id))
                {
                    count += 1;
                    any_cancel |= Operation::Cancel.allows(file);
                    all_retry &= Operation::Retry.allows(file);
                    all_remove &= Operation::Remove.allows(file);
                }
                (count, all_retry, any_cancel, all_remove)
            })
        })
    });
    let run = move |operation| {
        let files = untrack(|| {
            in_user(users, user, |user| {
                selected.with(|selected| {
                    user.into_iter()
                        .flat_map(|user| &user.directories)
                        .flat_map(|dir| &dir.files)
                        .filter(|file| selected.contains(&file.id))
                        .cloned()
                        .collect()
                })
            })
        });
        change.run((operation, files));
    };
    view! {
        <div class="ui raised card transfer-card">
            <div class="content">
                <div class="header">
                    <button type="button" class="ui mini basic icon button" aria-label="Fold transfer group" aria-expanded=move || !folded.get()
                        on:click=move |_| folded.update(|folded| *folded = !*folded)>
                        <i class=move || if folded.get() { "chevron right icon" } else { "chevron down icon" } aria-hidden="true"></i>
                    </button>{username}
                </div>
                <Show when=move || !folded.get()>
                    <For each=move || in_user(users, user, |user| user.map(|user| user.directories.iter().map(|dir| dir.directory.clone()).collect::<Vec<_>>()).unwrap_or_default())
                        key=String::clone children=move |directory: String| view! {
                            <TransferDirectory directory user users direction connected selected working change/>
                        }/>
                </Show>
            </div>
            <Show when=move || info.with(|(count, ..)| *count != 0)>
                <div class="extra content"><div class="ui buttons">
                    <Show when=move || info.with(|(_, retry, _, _)| *retry)>
                        <button type="button" class="ui green button" disabled=move || !connected.get() || working.with(Option::is_some)
                            on:click=move |_| run(Operation::Retry)><i class="redo icon" aria-hidden="true"></i>"Retry Selected"</button>
                    </Show>
                    <Show when=move || info.with(|(_, _, cancel, _)| *cancel)>
                        <button type="button" class="ui red button" disabled=move || working.with(Option::is_some)
                            on:click=move |_| run(Operation::Cancel)><i class="x icon" aria-hidden="true"></i>"Cancel Selected"</button>
                    </Show>
                    <Show when=move || info.with(|(_, _, _, remove)| *remove)>
                        <button type="button" class="ui button" disabled=move || working.with(Option::is_some)
                            on:click=move |_| run(Operation::Remove)><i class="trash alternate icon" aria-hidden="true"></i>"Remove Selected"</button>
                    </Show>
                </div></div>
            </Show>
        </div>
    }
}

#[component]
fn TransferDirectory(
    directory: String,
    user: StoredValue<String>,
    users: Groups,
    direction: TransferDirection,
    connected: Signal<bool>,
    selected: Selection,
    working: RwSignal<Option<Operation>>,
    change: Change,
) -> impl IntoView {
    let title = if directory.is_empty() {
        "(root)".into()
    } else {
        directory.clone()
    };
    let dir = StoredValue::new(directory);
    let folded = RwSignal::new(false);
    let ids = Memo::new(move |_| {
        in_directory(users, user, dir, |files| {
            let mut ordered = files.iter().collect::<Vec<_>>();
            ordered.sort_unstable_by(|a, b| {
                util::get_file_name(&a.filename)
                    .cmp(util::get_file_name(&b.filename))
                    .then_with(|| a.id.cmp(&b.id))
            });
            ordered
                .into_iter()
                .map(|file| file.id.clone())
                .collect::<Vec<_>>()
        })
    });
    view! {
        <div>
            <h4 class="ui small header filelist-header">
                <button type="button" class="ui mini basic icon button" aria-label="Fold directory" aria-expanded=move || !folded.get()
                    on:click=move |_| folded.update(|folded| *folded = !*folded)>
                    <i class=move || if folded.get() { "folder icon" } else { "folder open icon" } aria-hidden="true"></i>
                </button>{title}
            </h4>
            <Show when=move || !folded.get()>
                <table class="ui table">
                    <thead><tr>
                        <th class="transferlist-selector"><input type="checkbox" aria-label="Select all transfers in directory"
                            disabled=move || !in_directory(users, user, dir, |files| files.iter().any(|file| !file.removed))
                            prop:checked=move || in_directory(users, user, dir, |files| selected.with(|selected| {
                                files.iter().any(|file| !file.removed) && files.iter().filter(|file| !file.removed).all(|file| selected.contains(&file.id))
                            }))
                            on:change=move |event| {
                                let checked = event_target_checked(&event);
                                untrack(|| in_directory(users, user, dir, |files| selected.update(|selected| {
                                    for file in files.iter().filter(|file| !file.removed) {
                                        if checked { selected.insert(file.id.clone()); } else { selected.remove(&file.id); }
                                    }
                                })));
                            }/>
                        </th>
                        <th class="transferlist-filename">"File"</th>
                        <th class="transferlist-progress">"Progress"</th>
                        <th class="transferlist-size">"Size"</th>
                        <th class="transferlist-detail"><i class="small info circle icon" title="Details"></i></th>
                    </tr></thead>
                    <tbody>
                        <For each=move || ids.get() key=String::clone children=move |id: String| {
                            let id = StoredValue::new(id);
                            let file = Memo::new(move |_| in_directory(users, user, dir, |files| {
                                id.with_value(|id| files.iter().find(|file| &file.id == id).cloned())
                            }));
                            view! { <TransferRow id file direction connected selected working change/> }
                        }/>
                    </tbody>
                </table>
            </Show>
        </div>
    }
}

#[component]
fn TransferRow(
    id: StoredValue<String>,
    file: Memo<Option<Transfer>>,
    direction: TransferDirection,
    connected: Signal<bool>,
    selected: Selection,
    working: RwSignal<Option<Operation>>,
    change: Change,
) -> impl IntoView {
    let open = RwSignal::new(false);
    let position = RwSignal::new(None::<i64>);
    let refreshing = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let title = Memo::new(move |_| {
        file.with(|file| {
            file.as_ref()
                .map(|file| util::get_file_name(&file.filename).to_owned())
                .unwrap_or_default()
        })
    });
    let state = Memo::new(move |_| file.with(|file| file.as_ref().map(|file| file.state)));
    let active = Memo::new(move |_| state.get() == Some(State::InProgress));
    let queued = Memo::new(move |_| state.get().is_some_and(State::is_queued));
    let can_retry = move || {
        file.with(|file| {
            file.as_ref()
                .is_some_and(|file| Operation::Retry.allows(file))
        })
    };
    let is_download = direction == TransferDirection::Download;
    let removed = move || file.with(|file| file.as_ref().is_none_or(|file| file.removed));
    let interactive = move || is_download && !removed() && (queued.get() || can_retry());
    let click = move |_| {
        if refreshing.get_untracked()
            || working.with_untracked(Option::is_some)
            || !connected.get_untracked()
        {
            return;
        }
        let Some(file) = file.get_untracked() else {
            return;
        };
        if is_download && !file.removed && file.state.is_queued() {
            refreshing.set(true);
            error.set(None);
            spawn_local_scoped_with_cancellation(async move {
                let url = format!("{}/position", transfer_url(direction, &file.id));
                match api::get::<QueuePosition>(&url).await {
                    Ok(place) => position.set(Some(i64::from(place.place_in_queue))),
                    Err(err) => error.set(Some(err.to_string())),
                }
                refreshing.set(false);
            });
        } else if Operation::Retry.allows(&file) {
            change.run((Operation::Retry, vec![file]));
        }
    };
    view! {
        <tr>
            <td class="transferlist-selector"><input type="checkbox" aria-label=move || format!("Select {}", title.get())
                disabled=removed prop:checked=move || !removed() && selected.with(|selected| id.with_value(|id| selected.contains(id)))
                on:change=move |event| selected.update(|selected| {
                    if event_target_checked(&event) { selected.insert(id.get_value()); }
                    else { id.with_value(|id| selected.remove(id)); }
                })/>
            </td>
            <td class="transferlist-filename">{move || title.get()} {move || removed().then_some(" (removed)")}</td>
            <td class="transferlist-progress">
                <Show when=move || active.get() fallback=move || view! {
                    <button type="button" class=move || file.with(|file| state_button(file.as_ref()))
                        class:loading=refreshing disabled=move || !interactive() || !connected.get() || refreshing.get() || working.with(Option::is_some)
                        title=move || if interactive() && queued.get() { "Refresh queue position" } else if interactive() { "Retry download" } else { "Transfer state" }
                        on:click=click>
                        {move || interactive().then(|| view! { <i class=if queued.get() { "refresh icon" } else { "redo icon" } aria-hidden="true"></i> })}
                        {move || file.with(|file| file.as_ref().map(|file| {
                            let mut label = file.state.label().to_owned();
                            let position = if file.state.is_queued() { file.place_in_queue.map(i64::from).or_else(|| position.get()) } else { None };
                            if let Some(position) = position.filter(|&position| position > 0) { label.push_str(&format!(" (#{position})")); }
                            if file.attempts > 1 { label.push_str(&format!(" (Retry #{})", file.attempts)); }
                            label
                        }))}
                    </button>
                }>
                    <div class="ui active blue progress" style="margin:0" role="progressbar" aria-label="Transfer progress"
                        aria-valuemin="0" aria-valuemax="100" aria-valuenow=move || percent(file)>
                        <div class="bar" style:width=move || format!("{}%", percent(file))><div class="progress">{move || format!("{}%", percent(file))}</div></div>
                    </div>
                </Show>
                {move || error.get().map(|error| view! { <small role="alert">{error}</small> })}
            </td>
            <td class="transferlist-size">{move || file.with(|file| file.as_ref().map(bytes_transferred))}</td>
            <td class="transferlist-detail" style="position:relative">
                <button type="button" class="ui mini basic icon button" aria-label="Transfer details" aria-expanded=open
                    on:click=move |_| open.update(|open| *open = !*open)><i class="small grey info circle icon" aria-hidden="true"></i></button>
                <Show when=move || open.get()>
                    <div class="ui flowing left center popup visible" style="right:100%;left:auto;top:0;max-width:600px;z-index:3">
                        {move || file.with(|file| file.as_ref().map(transfer_details))}
                    </div>
                </Show>
            </td>
        </tr>
    }
}

fn percent(file: Memo<Option<Transfer>>) -> u32 {
    file.with(|file| {
        file.as_ref().map_or(0, |file| {
            file.percent_complete.round().clamp(0.0, 100.0) as u32
        })
    })
}

fn state_button(file: Option<&Transfer>) -> &'static str {
    let Some(file) = file else {
        return "ui fluid mini button";
    };
    let state = file.state;
    if state.is_complete() {
        if state == State::Succeeded {
            "ui fluid mini green button"
        } else {
            "ui fluid mini red button"
        }
    } else if state == State::Initializing {
        "ui fluid mini teal button"
    } else if file.attempts > 1 {
        "ui fluid mini yellow button"
    } else {
        "ui fluid mini button"
    }
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
    format!("{transferred}/{size} {unit}")
}

fn transfer_details(file: &Transfer) -> impl IntoView + use<> {
    let date = |value: Option<chrono::DateTime<chrono::Utc>>| {
        value.map_or_else(|| "—".into(), |date| util::format_date(&date.to_rfc3339()))
    };
    let time = |value: Option<u64>| {
        value.map_or_else(
            || "—".into(),
            |ms| util::format_seconds(Duration::from_millis(ms).as_secs_f64()),
        )
    };
    let fields = [
        ("ID", file.id.clone()),
        ("Username", file.username.clone()),
        ("Direction", format!("{:?}", file.direction)),
        ("Filename", file.filename.clone()),
        ("Size", util::format_bytes(file.size as f64)),
        ("State", file.state.label().to_owned()),
        ("Requested At", date(Some(file.requested_at))),
        ("Enqueued At", date(file.enqueued_at)),
        ("Started At", date(file.started_at)),
        ("Ended At", date(file.ended_at)),
        (
            "Bytes Transferred",
            util::format_bytes(file.bytes_transferred as f64),
        ),
        (
            "Average Speed",
            format!("{}/s", util::format_bytes(file.average_speed)),
        ),
        (
            "Bytes Remaining",
            util::format_bytes(file.bytes_remaining as f64),
        ),
        ("Elapsed Time", time(file.elapsed_ms)),
        ("Percent Complete", format!("{:.2}%", file.percent_complete)),
        ("Remaining Time", time(file.remaining_ms)),
        ("Attempts", file.attempts.to_string()),
        ("Next Attempt At", date(file.next_attempt_at)),
        (
            "Exception",
            file.error.clone().unwrap_or_else(|| "—".into()),
        ),
    ];
    view! {
        <table class="ui very basic compact small table">
            <tbody>{fields.into_iter().map(|(field, value)| view! { <tr><th scope="row">{field}</th><td>{value}</td></tr> }).collect_view()}</tbody>
        </table>
    }
}
