mod groups;

use std::collections::{BTreeMap, BTreeSet};

use gloo_net::http::Method;
use leptos::{prelude::*, task::spawn_local_scoped_with_cancellation};
use slskr_types::transfers::{
    DownloadFile, EnqueueDownloads, EnqueuedDownloads, Transfer, TransferDirection,
    TransferState as State,
};

use crate::{
    components::app::AppContext,
    services::{api, util},
};

/// One user's transfers, grouped by remote directory.
#[derive(Clone, Debug, PartialEq)]
struct UserResponse {
    username: String,
    directories: Vec<DirectoryResponse>,
}

#[derive(Clone, Debug, PartialEq)]
struct DirectoryResponse {
    directory: String,
    files: Vec<Transfer>,
}

/// Groups transfers by user, then by remote directory, in first-seen order.
fn grouped(transfers: Vec<Transfer>) -> Vec<UserResponse> {
    let mut users = Vec::<UserResponse>::new();
    for file in transfers {
        let directory = file
            .filename
            .rsplit_once(['\\', '/'])
            .map_or("", |(directory, _)| directory)
            .to_owned();
        let user = match users.iter().position(|user| user.username == file.username) {
            Some(index) => &mut users[index],
            None => {
                users.push(UserResponse {
                    username: file.username.clone(),
                    directories: Vec::new(),
                });
                users.last_mut().expect("added user")
            }
        };
        match user
            .directories
            .iter_mut()
            .find(|group| group.directory == directory)
        {
            Some(group) => group.files.push(file),
            None => user.directories.push(DirectoryResponse {
                directory,
                files: vec![file],
            }),
        }
    }
    users
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Operation {
    Retry,
    Cancel,
    Remove,
}

impl Operation {
    fn name(self) -> &'static str {
        match self {
            Self::Retry => "Retry",
            Self::Cancel => "Cancel",
            Self::Remove => "Remove",
        }
    }

    fn allows(self, file: &Transfer) -> bool {
        if file.removed {
            return false;
        }
        let completed = file.state.is_complete();
        match self {
            Self::Retry => file.direction == TransferDirection::Download && file.state.is_failed(),
            Self::Cancel => !completed,
            Self::Remove => completed,
        }
    }
}

type Change = Callback<(Operation, Vec<Transfer>)>;

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
    let users = RwSignal::new(Vec::<UserResponse>::new());
    let loading = RwSignal::new(true);
    let load_error = RwSignal::new(None::<String>);
    let action_error = RwSignal::new(None::<String>);
    let history = RwSignal::new(false);
    let working = RwSignal::new(None::<Operation>);
    let label = direction.as_str();

    spawn_local_scoped_with_cancellation(async move {
        let active_url = collection(direction);
        let history_url = format!("{active_url}?include_removed=true");
        loop {
            let include_removed = history.get_untracked();
            let url = if include_removed {
                &history_url
            } else {
                &active_url
            };
            let snapshot = api::get::<Vec<Transfer>>(url).await.map(grouped);
            if history.get_untracked() == include_removed {
                match snapshot {
                    Ok(mut snapshot) => {
                        for user in &mut snapshot {
                            if !include_removed {
                                for directory in &mut user.directories {
                                    directory.files.retain(|file| !file.removed);
                                }
                            }
                            user.directories
                                .retain(|directory| !directory.files.is_empty());
                        }
                        snapshot.retain(|user| !user.directories.is_empty());
                        if users.with_untracked(|previous| *previous != snapshot) {
                            users.set(snapshot);
                        }
                        if load_error.with_untracked(Option::is_some) {
                            load_error.set(None);
                        }
                    }
                    Err(error) => {
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
            }
            util::sleep(1_000).await;
        }
    });

    let change = Callback::new(move |(operation, files): (Operation, Vec<Transfer>)| {
        if working.with_untracked(Option::is_some)
            || (operation == Operation::Retry && !connected.get_untracked())
        {
            return;
        }
        let files = files
            .into_iter()
            .filter(|file| file.direction == direction && operation.allows(file))
            .collect::<Vec<_>>();
        if files.is_empty() {
            return;
        }
        working.set(Some(operation));
        action_error.set(None);
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
    });

    view! {
        <div class="transfer-segment">
            <div class="ui raised segment transfers-header-segment">
                <div class="transfers-segment-icon"><i class=format!("big {label} icon") aria-hidden="true"></i></div>
                <label><input type="checkbox" prop:checked=history on:change=move |event| {
                    history.set(event_target_checked(&event));
                    loading.set(true);
                }/>" Show removed transfers"</label>
                <div class="transfers-header-buttons" hidden=move || users.with(Vec::is_empty)>
                    { (direction == TransferDirection::Download).then(|| view! {
                        <BulkAction operation=Operation::Retry choices=&["Errored", "Cancelled", "All"] users working connected change/>
                    }) }
                    <BulkAction operation=Operation::Cancel choices=&["All", "Queued", "In Progress"] users working connected change/>
                    <BulkAction operation=Operation::Remove choices=&["Succeeded", "Errored", "Cancelled", "Completed"] users working connected change/>
                </div>
            </div>
            {move || load_error.get().map(|error| view! { <div class="ui negative message" role="alert">{error}</div> })}
            {move || action_error.get().map(|error| view! { <div class="ui negative message" role="alert">{error}</div> })}
            {move || {
                if loading.get() {
                    view! { <div class="ui raised segment"><div class="ui active centered inline loader" role="status" aria-label=format!("Loading {label}s")></div></div> }.into_any()
                } else if users.with(Vec::is_empty) && load_error.with(Option::is_none) {
                    view! { <div class="ui basic placeholder segment placeholder-segment"><div class="ui icon header">
                        <i class=format!("{label} icon") aria-hidden="true"></i>{format!("No {label}s to display")}
                    </div></div> }.into_any()
                } else { ().into_any() }
            }}
            <div hidden=loading>
                <For each=move || users.with(|users| users.iter().map(|user| user.username.clone()).collect::<Vec<_>>())
                    key=String::clone children=move |username: String| view! {
                        <groups::TransferGroup username users direction connected working change/>
                    }/>
            </div>
        </div>
    }
}

fn matches_option(state: State, option: &str) -> bool {
    match option {
        "Errored" => matches!(state, State::TimedOut | State::Errored | State::Rejected),
        "Succeeded" => state == State::Succeeded,
        "Cancelled" => state == State::Cancelled,
        "Queued" => state.is_queued(),
        "In Progress" => state == State::InProgress,
        _ => true,
    }
}

#[component]
fn BulkAction(
    operation: Operation,
    choices: &'static [&'static str],
    users: RwSignal<Vec<UserResponse>>,
    working: RwSignal<Option<Operation>>,
    connected: Signal<bool>,
    change: Change,
) -> impl IntoView {
    let choice = RwSignal::new(choices[0].to_owned());
    let available = move || {
        users.with(|users| {
            choice.with(|choice| {
                all_files(users)
                    .any(|file| operation.allows(file) && matches_option(file.state, choice))
            })
        })
    };
    let run = move |_| {
        let files = users.with_untracked(|users| {
            choice.with_untracked(|choice| {
                all_files(users)
                    .filter(|file| operation.allows(file) && matches_option(file.state, choice))
                    .cloned()
                    .collect()
            })
        });
        change.run((operation, files));
    };
    let (class, icon) = match operation {
        Operation::Retry => ("ui green button", "redo icon"),
        Operation::Cancel => ("ui red button", "x icon"),
        Operation::Remove => ("ui button", "trash alternate icon"),
    };
    let label = operation.name();
    view! {
        <span class="ui buttons">
            <button type="button" class=class class:loading=move || working.get() == Some(operation)
                disabled=move || working.with(Option::is_some) || (operation == Operation::Retry && !connected.get()) || !available()
                on:click=run>
                <i class=icon aria-hidden="true"></i>
                {move || choice.with(|choice| if choice == "All" { format!("{label} All") } else { format!("{label} All {choice}") })}
            </button>
            <select class="ui compact dropdown" aria-label=format!("{label} transfers by state")
                disabled=move || working.with(Option::is_some) prop:value=choice on:change=move |event| choice.set(event_target_value(&event))>
                {choices.iter().map(|choice| view! { <option value=*choice>{*choice}</option> }).collect_view()}
            </select>
        </span>" "
    }
}

fn all_files(users: &[UserResponse]) -> impl Iterator<Item = &Transfer> {
    users
        .iter()
        .flat_map(|user| user.directories.iter())
        .flat_map(|directory| &directory.files)
}

async fn retry(files: Vec<Transfer>) -> BTreeSet<String> {
    let mut requests: BTreeMap<String, BTreeMap<String, u64>> = BTreeMap::new();
    for file in files {
        requests
            .entry(file.username)
            .or_default()
            .insert(file.filename, file.size);
    }
    let mut errors = BTreeSet::new();
    for (username, files) in requests {
        let body = EnqueueDownloads {
            username: username.clone(),
            files: files
                .into_iter()
                .map(|(filename, size)| DownloadFile { filename, size })
                .collect(),
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
    files: Vec<Transfer>,
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
