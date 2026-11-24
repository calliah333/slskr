//! Sharing: shared folders, upload limits, upload groups and the blacklist.

use leptos::prelude::*;
use serde_json::Value;
use slskr_types::config::RESERVED_GROUPS;

use super::{
    fields::{Kind, Setting, lock_reason, row_actions, use_form},
    model::{self, field_id, text_of},
};
use crate::{
    components::{app::AppContext, directory_picker::FolderList},
    services::api,
    ui::{
        Badge, Icon,
        form::{Advanced, ChipInput, Field, Section, TextInput},
    },
};

const DIRECTORIES: &str = "shares.directories";

/// The scan state of the shares, as a badge.
#[component]
fn ScanStatus() -> impl IntoView {
    let Some(AppContext { state, .. }) = use_context::<AppContext>() else {
        return ().into_any();
    };
    let status = Memo::new(move |_| {
        state.with(|state| {
            let shares = &state.shares;
            if shares.scanning {
                (
                    "primary",
                    format!("Scanning {:.0}%", shares.scan_progress * 100.0),
                )
            } else if shares.faulted {
                ("danger", "Scan failed".to_owned())
            } else if shares.scan_pending {
                ("warning", "Rescan needed".to_owned())
            } else {
                (
                    "neutral",
                    format!("{} folders, {} files", shares.directories, shares.files),
                )
            }
        })
    });
    view! {
        <span class=move || format!("badge badge--{} shares-scan-status", status.get().0) role="status">
            {move || status.get().1}
        </span>
    }
    .into_any()
}

#[component]
pub fn SharedFolders() -> impl IntoView {
    let form = use_form();
    form.register(DIRECTORIES);
    let key = StoredValue::new(DIRECTORIES.to_owned());
    let message = RwSignal::new(None::<(bool, String)>);
    let scanning = RwSignal::new(false);
    let rescan = move |_| {
        scanning.set(true);
        leptos::task::spawn_local(async move {
            let result = api::post_empty::<()>("/shares/scan").await;
            message.set(Some(match result {
                Ok(()) => (true, "Share scan started.".to_owned()),
                Err(error) => (false, error.message),
            }));
            scanning.set(false);
        });
    };
    let entries = Signal::derive(move || {
        form.value(DIRECTORIES)
            .as_array()
            .map(|items| items.iter().map(text_of).collect::<Vec<_>>())
            .unwrap_or_default()
    });
    let set = Callback::new(move |items: Vec<String>| {
        form.set(
            DIRECTORIES,
            Value::Array(items.into_iter().map(Value::String).collect()),
        );
    });
    let locked = Signal::derive(move || form.pinned_by(DIRECTORIES).map(|p| lock_reason(&p)));
    let is_locked = Signal::derive(move || locked.with(Option::is_some));
    // `!/path` entries: folders left out of another share.
    let excluded = Signal::derive(move || {
        entries.with(|all| {
            all.iter()
                .filter_map(|entry| entry.strip_prefix('!').map(str::to_owned))
                .collect::<Vec<_>>()
        })
    });
    let set_excluded = Callback::new(move |paths: Vec<String>| {
        let mut next: Vec<String> = entries
            .get_untracked()
            .into_iter()
            .filter(|entry| !entry.starts_with('!'))
            .collect();
        next.extend(
            paths
                .into_iter()
                .map(|path| format!("!{}", path.trim_start_matches('!'))),
        );
        set.run(next);
    });
    view! {
        <Section id="settings-shares" title="Shared folders"
            description="Folders other users can browse and download from."
            actions=ViewFn::from(move || view! {
                <ScanStatus/>
                <button type="button" class="btn btn--sm shares-rescan" disabled=scanning
                    aria-busy=move || scanning.get().then_some("true") on:click=rescan>
                    <Icon name="refresh-cw" spin=scanning/>"Rescan now"
                </button>
            })>
            {move || message.get().map(|(ok, text)| view! {
                <p class=if ok { "help shares-scan-message" } else { "error shares-scan-message" }
                    role=if ok { "status" } else { "alert" }>{text}</p>
            })}
            <Field id=field_id(DIRECTORIES) label="Folders" row=true wide=true
                help="Peers see each folder under its name, or an alias you give it."
                error=Signal::derive(move || form.error(DIRECTORIES))
                locked_reason=locked
                actions=row_actions(form, key, Callback::new(|()| {}))>
                <FolderList items=entries on_change=set disabled=is_locked/>
            </Field>
            <Setting key="shares.rescan_interval" label="Rescan every" kind=Kind::OptionalDuration
                help="Never scans only on start and when you ask."/>
            <Advanced>
                <Field id="setting-shares-excluded" label="Excluded folders" row=true wide=true
                    locked_reason=locked
                    help="Folders inside a shared folder that are left out.">
                    <ChipInput items=excluded on_change=set_excluded mono=true
                        placeholder="/path/inside/a/share, then Enter"/>
                </Field>
                <Setting key="shares.exclude" label="Exclusion patterns" kind=Kind::MonoList("Regular expression, then Enter")
                    help="Case-insensitive regular expressions on full local paths; matching files are not shared."/>
            </Advanced>
        </Section>
    }
}

/// The eight per-user limits below `prefix` (`uploads.limits`, `groups.<name>.limits`):
/// queued files and size first, the rest in a disclosure when `split`.
#[component]
fn Limits(prefix: String, zero: &'static str) -> impl IntoView {
    let row = |prefix: &str, key: &str, label: &'static str, unit: &'static str, fallback: u64| {
        view! {
            <Setting key=format!("{prefix}.{key}") label=label
                kind=Kind::zero_means(Some(unit), zero, fallback)/>
        }
    };
    view! {
        {row(&prefix, "queued_files", "Queued files", "files", 500)}
        {row(&prefix, "queued_megabytes", "Queued size", "MB", 5000)}
        {row(&prefix, "daily_files", "Files per day", "files", 1000)}
        {row(&prefix, "daily_megabytes", "Size per day", "MB", 10000)}
        {row(&prefix, "weekly_files", "Files per week", "files", 5000)}
        {row(&prefix, "weekly_megabytes", "Size per week", "MB", 50000)}
        {row(&prefix, "daily_failures", "Failures per day", "failures", 100)}
        {row(&prefix, "weekly_failures", "Failures per week", "failures", 500)}
    }
}

#[component]
pub fn UploadLimits() -> impl IntoView {
    let form = use_form();
    view! {
        <Section id="settings-uploads" title="Upload limits"
            description="How much slskr uploads at once, and how much each user may take.">
            <Setting key="uploads.slots" label="Upload slots" restart=true
                kind=Kind::at_least_one(Some("uploads")) help="Uploads that run at the same time."/>
            <Setting key="uploads.speed_limit" label="Speed limit" kind=Kind::unlimited(Some("KiB/s"), 1024)
                help="For all uploads together."/>
            <Advanced label="Per-user limits" open=Signal::derive(move || form.has_errors_below("uploads.limits"))>
                <p class="help">"How much each user may queue and take. A group's own limits take precedence."</p>
                <Limits prefix="uploads.limits".to_owned() zero="Unlimited"/>
            </Advanced>
        </Section>
    }
}

/// One upload group: its rows inside a bordered box.
#[component]
fn Group(name: String) -> impl IntoView {
    let form = use_form();
    let prefix = format!("groups.{name}");
    let leechers = name == "leechers";
    let custom = !model::BUILT_IN_GROUPS.contains(&name.as_str());
    let key = |suffix: &str| format!("{prefix}.{suffix}");
    let limits = key("limits");
    let open_limits = limits.clone();
    let (title, description) = match name.as_str() {
        "default" => ("Default".to_owned(), "Everyone who is in no other group."),
        "leechers" => (
            "Leechers".to_owned(),
            "Users who share less than the minimums below.",
        ),
        _ => (name.clone(), "Members are listed below."),
    };
    let removing = name.clone();
    let group_key = prefix.clone();
    let locked = move || form.pinned_by(&group_key).is_some();
    view! {
        <div class="settings-group" id=format!("settings-group-{name}")>
            <div class="settings-group__head">
                <span class="settings-group__icon" aria-hidden="true"><Icon name="users"/></span>
                <div class="grow">
                    <h3 class="settings-group__title">
                        {title}
                        {(!custom).then(|| view! { <Badge text="Built-in"/> })}
                    </h3>
                    <p class="help">{description}</p>
                </div>
                {custom.then(|| view! {
                    <button type="button" class="btn btn--action is-danger group-remove" disabled=locked
                        aria-label=format!("Remove group {name}") title="Remove group"
                        on:click=move |_| form.remove_group(&removing)>
                        <Icon name="trash-2"/>
                    </button>
                })}
            </div>
            <div class="form-rows">
                {custom.then(|| view! {
                    <Setting key=key("members") label="Members" kind=Kind::List("Username, then Enter")
                        help="A user can be in one group."/>
                })}
                <Setting key=key("priority") label="Priority" kind=Kind::count(None)
                    help="Lower numbers are served first."/>
                <Setting key=key("strategy") label="Queue order"
                    kind=Kind::Select(vec![("roundrobin", "Round robin"), ("firstinfirstout", "First in, first out")])/>
                <Setting key=key("slots") label="Upload slots" kind=Kind::zero_means(Some("slots"), "No limit", 5)
                    help="No limit leaves only the global slots."/>
                <Setting key=key("speed_limit") label="Speed limit" kind=Kind::unlimited(Some("KiB/s"), 1024)/>
                {leechers.then(|| view! {
                    <Setting key="groups.leechers.min_shared_files" label="Minimum shared files" kind=Kind::count(Some("files"))
                        help="Users sharing fewer files are leechers."/>
                    <Setting key="groups.leechers.min_shared_directories" label="Minimum shared folders" kind=Kind::count(Some("folders"))
                        help="Users sharing fewer folders are leechers."/>
                })}
                <Advanced label="Per-user limits" open=Signal::derive(move || form.has_errors_below(&open_limits))>
                    <p class="help">"Inherit uses the upload limits."</p>
                    <Limits prefix=limits zero="Inherit"/>
                </Advanced>
            </div>
        </div>
    }
}

#[component]
pub fn Groups() -> impl IntoView {
    let form = use_form();
    let adding = RwSignal::new(false);
    let name = RwSignal::new(String::new());
    let error = RwSignal::new(None::<String>);
    let add = move || {
        let candidate = name.get_untracked().trim().to_owned();
        let problem = if candidate.is_empty() {
            Some("Enter a group name".to_owned())
        } else if RESERVED_GROUPS.contains(&candidate.as_str()) {
            Some(format!("{candidate} is a built-in group"))
        } else if candidate.contains(['.', '/', '~']) {
            Some("Group names can not contain . / or ~".to_owned())
        } else if form.custom_groups().contains(&candidate) {
            Some(format!("There is already a group named {candidate}"))
        } else {
            None
        };
        error.set(problem.clone());
        if problem.is_none() {
            form.add_group(&candidate);
            name.set(String::new());
            adding.set(false);
        }
    };
    let open_add = move |_| {
        adding.set(true);
        request_animation_frame(|| {
            if let Some(input) = document().get_element_by_id("settings-new-group")
                && let Ok(input) = wasm_bindgen::JsCast::dyn_into::<web_sys::HtmlElement>(input)
            {
                let _ = input.focus();
            }
        });
    };
    view! {
        <Section id="settings-groups" title="Groups"
            description="Upload queues for sets of users. Privileged users always go first, then lower priority numbers."
            actions=ViewFn::from(move || view! {
                <button type="button" class="btn btn--sm group-add-open" on:click=open_add>
                    <Icon name="user-plus"/>"Add group"
                </button>
            })>
            <div class="settings-groups">
                <Group name="default".to_owned()/>
                <Group name="leechers".to_owned()/>
                <For each=move || form.custom_groups() key=|name| name.clone() let:group>
                    <Group name=group/>
                </For>
                <Show when=move || adding.get()>
                    <form class="settings-group settings-group--new" novalidate
                        on:submit=move |event| { event.prevent_default(); add(); }>
                        <Field id="settings-new-group" label="New group" error=error
                            help="Saved with the other changes.">
                            <div class="input-group">
                                <TextInput value=name on_input=move |t: String| name.set(t)
                                    placeholder="e.g. friends"/>
                                <button type="submit" class="btn group-add"><Icon name="plus"/>"Add"</button>
                            </div>
                        </Field>
                        <div>
                            <button type="button" class="btn btn--ghost btn--sm"
                                on:click=move |_| { adding.set(false); error.set(None); name.set(String::new()); }>
                                "Cancel"
                            </button>
                        </div>
                    </form>
                </Show>
            </div>
        </Section>
    }
}

#[component]
pub fn Blacklist() -> impl IntoView {
    view! {
        <Section id="settings-blacklist" title="Blacklist"
            description="Users who can not search, browse or download from you.">
            <Setting key="blacklist.users" label="Users" kind=Kind::List("Username, then Enter")
                help="Exact usernames, case-insensitive."/>
            <Setting key="blacklist.patterns" label="Username patterns" kind=Kind::MonoList("e.g. spam*, then Enter")
                help="Wildcards * and ?, case-insensitive."/>
            <Setting key="blacklist.cidrs" label="IP ranges" kind=Kind::MonoList("e.g. 203.0.113.0/24, then Enter")
                help="Search responses and downloads also check the peer's address."/>
        </Section>
    }
}
