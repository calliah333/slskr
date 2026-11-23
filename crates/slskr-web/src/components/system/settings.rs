//! System → Settings: every setting of `GET /api/settings`, edited in place and saved with
//! one sparse `PATCH`.

mod api_keys;
mod fields;
mod model;

use leptos::{prelude::*, task::spawn_local_scoped_with_cancellation};
use leptos_router::{components::A, hooks::use_query_map};
use slskr_types::{config::RESERVED_GROUPS, settings::Settings};

use self::{
    fields::{Kind, Setting},
    model::Form,
};
use crate::{
    components::app::SessionControl,
    services::{
        api,
        events::{Events, Topic},
    },
    ui::form::{Field, FieldGrid, FormMessage, SaveBar, Section, TextInput, focus_first_invalid},
};

/// Sections in display order: `(anchor, title)`. The anchor is `?section=` and the
/// element id `settings-{anchor}`.
const SECTIONS: [(&str, &str); 11] = [
    ("soulseek", "Soulseek"),
    ("web", "Web"),
    ("api-keys", "API keys"),
    ("shares", "Shares"),
    ("downloads", "Downloads"),
    ("uploads", "Uploads"),
    ("groups", "Groups"),
    ("blacklist", "Blacklist"),
    ("search", "Search"),
    ("retention", "Retention"),
    ("logging", "Logging & metrics"),
];

const SCOPE: &str = ".settings";

#[component]
pub fn SettingsPage() -> impl IntoView {
    let form = Form::new();
    provide_context(form);
    let session = use_context::<SessionControl>();
    // After a sign-in change the session is gone; stop reacting to updates.
    let signed_out = RwSignal::new(false);

    spawn_local_scoped_with_cancellation(form.reload());
    if let Some(events) = use_context::<Events>() {
        events.on(Topic::Settings, move |_| {
            if !form.saving.get_untracked() && !signed_out.get_untracked() {
                spawn_local_scoped_with_cancellation(form.reload());
            }
        });
    }

    let query = use_query_map();
    Effect::new(move |_| {
        let section = query.with(|q| q.get("section"));
        let loaded = form.settings.with(Option::is_some);
        if let (Some(section), true) = (section, loaded) {
            request_animation_frame(move || {
                if let Some(element) = document().get_element_by_id(&format!("settings-{section}"))
                {
                    element.scroll_into_view();
                }
            });
        }
    });

    let save = Callback::new(move |()| {
        if form.saving.get_untracked() {
            return;
        }
        if form
            .client_errors
            .with_untracked(|errors| !errors.is_empty())
        {
            focus_first_invalid(SCOPE);
            return;
        }
        let body = form.patch();
        let signs_out = form.changes_sign_in();
        form.saving.set(true);
        form.top_error.set(None);
        spawn_local_scoped_with_cancellation(async move {
            match api::patch::<_, Settings>("/settings", &body).await {
                Ok(settings) => {
                    form.saved(settings);
                    if signs_out {
                        signed_out.set(true);
                        form.notice.set(Some(
                            "Saved. Sign in again with the new credentials.".into(),
                        ));
                        if let Some(session) = session {
                            session.recheck.run(());
                        }
                        return;
                    }
                    form.notice.set(Some("Settings saved.".into()));
                }
                Err(error) if error.status == 400 => {
                    form.show_errors(&error.message);
                    focus_first_invalid(SCOPE);
                }
                Err(error) if error.status == 409 => {
                    form.top_error.set(Some(format!(
                        "{} The settings were reloaded.",
                        error.message
                    )));
                    form.reload().await;
                }
                Err(error) => form.top_error.set(Some(error.message)),
            }
            form.saving.set(false);
        });
    });
    let problem = Signal::derive(move || {
        let invalid = form.client_errors.with(|errors| errors.len());
        (invalid > 0).then(|| match invalid {
            1 => "Fix the highlighted field to save".to_owned(),
            n => format!("Fix the {n} highlighted fields to save"),
        })
    });

    let base = crate::config::url_base().trim_end_matches('/').to_owned();
    view! {
        <div class="settings ui form">
            <PendingRestart/>
            {move || form.top_error.get().map(|message| view! {
                <FormMessage kind="negative"><p>{message}</p></FormMessage>
            })}
            {move || form.notice.get().map(|message| view! {
                <FormMessage kind="positive"><p>{message}</p></FormMessage>
            })}
            <Show when=move || form.settings.with(Option::is_some)
                fallback=|| view! { <div class="ui active centered inline loader" role="status" aria-label="Loading settings"></div> }>
                <div class="settings-layout">
                    <nav class="ui vertical secondary pointing menu settings-nav" aria-label="Settings sections">
                        {SECTIONS.into_iter().map(|(anchor, title)| {
                            let active = move || query.with(|q| q.get("section").as_deref() == Some(anchor));
                            view! {
                                <A href=format!("{base}/system/settings?section={anchor}")
                                    attr:class=move || if active() { "active item" } else { "item" }>
                                    {title}
                                </A>
                            }
                        }).collect_view()}
                    </nav>
                    <div class="settings-sections">
                        <SoulseekSection/>
                        <WebSection/>
                        <api_keys::ApiKeys/>
                        <SharesSection/>
                        <DownloadsSection/>
                        <UploadsSection/>
                        <GroupsSection/>
                        <BlacklistSection/>
                        <SearchSection/>
                        <RetentionSection/>
                        <LoggingSection/>
                    </div>
                </div>
            </Show>
            <SaveBar dirty=Signal::derive(move || form.dirty_count()) saving=form.saving
                on_save=save on_discard=move |()| { form.discard(); form.notice.set(None); }
                problem=problem/>
        </div>
    }
}

#[component]
fn PendingRestart() -> impl IntoView {
    let form = fields::use_form();
    let restarting = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let restart = move |_| {
        restarting.set(true);
        leptos::task::spawn_local(async move {
            if let Err(failure) = api::post_empty::<()>("/application/restart").await {
                error.set(Some(failure.message));
                restarting.set(false);
            }
        });
    };
    let pending = move || {
        form.settings
            .with(|s| s.as_ref().is_some_and(|s| s.pending_restart))
    };
    view! {
        <Show when=pending>
            <FormMessage kind="warning">
                <div class="pending-restart">
                    <p>"Some saved changes take effect after slskr restarts."</p>
                    <button type="button" class="ui small primary button" class:loading=restarting
                        disabled=restarting on:click=restart>
                        "Restart now"
                    </button>
                    {move || error.get().map(|message| view! { <p role="alert">{message}</p> })}
                </div>
            </FormMessage>
        </Show>
    }
}

fn strategies() -> Kind {
    Kind::Select(vec![
        ("roundrobin", "Round robin"),
        ("firstinfirstout", "First in, first out"),
    ])
}

#[component]
fn SoulseekSection() -> impl IntoView {
    let form = fields::use_form();
    let proxy_secret = move || form.secret_set("soulseek.proxy");
    view! {
        <Section id="settings-soulseek" title="Soulseek"
            description="Your Soulseek account and how peers reach you.">
            <FieldGrid>
                <Setting key="soulseek.username" label="Username" kind=Kind::Text
                    help="If the username isn't registered yet, logging in creates it."/>
                <Setting key="soulseek.password" label="Password" kind=Kind::Secret { confirm: false }/>
            </FieldGrid>
            <FieldGrid>
                <Setting key="soulseek.server" label="Server" kind=Kind::Text help="host:port"/>
                <Setting key="soulseek.listen_address" label="Listen address" kind=Kind::Text
                    help="Address for incoming peer connections."/>
                <Setting key="soulseek.listen_port" label="Listen port"
                    kind=Kind::Number { min: 1024, max: 65535 } help="1024–65535; forward it in your router."/>
            </FieldGrid>
            <Setting key="soulseek.description" label="Description" kind=Kind::TextArea
                help="Shown to peers viewing your user info."/>
            <Setting key="soulseek.picture" label="Picture" kind=Kind::Text
                help="Path to an image file shown in your user info; empty for none."/>
            {move || if proxy_secret() {
                view! {
                    <Setting key="soulseek.proxy" label="SOCKS5 proxy" kind=Kind::Secret { confirm: false }
                        help="Contains credentials, so it is not shown. socks5://[user:pass@]host:port"/>
                }.into_any()
            } else {
                view! {
                    <Setting key="soulseek.proxy" label="SOCKS5 proxy" kind=Kind::Text
                        placeholder="socks5://[user:pass@]host:port" help="Empty connects directly."/>
                }.into_any()
            }}
            <FieldGrid>
                <Setting key="soulseek.distributed_network" label="Distributed network" kind=Kind::Toggle
                    help="Take part in the distributed search network."/>
                <Setting key="soulseek.distributed_children" label="Distributed children" kind=Kind::COUNT
                    help="0 accepts none."/>
                <Setting key="soulseek.connect" label="Connect on start" kind=Kind::Toggle restart=true/>
            </FieldGrid>
            <Setting key="soulseek.rooms" label="Rooms" kind=Kind::List("Room name")
                help="Chat rooms joined after every login."/>
        </Section>
    }
}

#[component]
fn WebSection() -> impl IntoView {
    view! {
        <Section id="settings-web" title="Web" description="Signing in to this web UI.">
            <FieldGrid>
                <Setting key="web.username" label="Username" kind=Kind::Text
                    help="Changing it signs out every browser."/>
                <Setting key="web.password" label="Password" kind=Kind::Secret { confirm: true }
                    help="At least 8 characters. Changing it signs out every browser."/>
                <Setting key="web.session_ttl" label="Stay signed in for" kind=Kind::Duration
                    help="Each use extends it; 5m to 365d."/>
            </FieldGrid>
            <FieldGrid>
                <Setting key="web.listen" label="Listen address" kind=Kind::Text restart=true
                    help="Address of the web UI and API."/>
                <Setting key="web.url_base" label="URL base" kind=Kind::Text restart=true
                    help="Path prefix behind a reverse proxy, e.g. /slskr."/>
                <Setting key="web.auth" label="Require sign-in" kind=Kind::Toggle restart=true
                    help="Off disables authentication: loopback or development only."/>
                <Setting key="web.log_requests" label="Log requests" kind=Kind::Toggle restart=true/>
            </FieldGrid>
        </Section>
    }
}

#[component]
fn SharesSection() -> impl IntoView {
    let status = RwSignal::new(None::<(bool, String)>);
    let scanning = RwSignal::new(false);
    let rescan = move |_| {
        scanning.set(true);
        leptos::task::spawn_local(async move {
            let result = api::post_empty::<()>("/shares/scan").await;
            status.set(Some(match result {
                Ok(()) => (true, "Share scan started.".to_owned()),
                Err(error) => (false, error.message),
            }));
            scanning.set(false);
        });
    };
    view! {
        <Section id="settings-shares" title="Shares" description="Folders other users can browse and download from.">
            <Setting key="shares.directories" label="Shared folders" kind=Kind::Directories
                help="Absolute paths. [Alias]/path sets the name peers see (default: the folder name); !/path excludes a folder inside another share."/>
            <Setting key="shares.exclude" label="Exclude" kind=Kind::List("e.g. \\.tmp$")
                help="Case-insensitive regular expressions on full local paths; matches are not shared."/>
            <Setting key="shares.rescan_interval" label="Rescan every" kind=Kind::OptionalDuration
                help="Empty scans only on start and on request."/>
            <div class="form-actions">
                <button type="button" class="ui button shares-rescan" class:loading=scanning disabled=scanning
                    on:click=rescan>
                    <i class="sync icon" aria-hidden="true"></i>"Rescan now"
                </button>
                {move || status.get().map(|(ok, message)| view! {
                    <span class="form-help" role=if ok { "status" } else { "alert" }>{message}</span>
                })}
            </div>
        </Section>
    }
}

#[component]
fn DownloadsSection() -> impl IntoView {
    view! {
        <Section id="settings-downloads" title="Downloads">
            <FieldGrid>
                <Setting key="downloads.directory" label="Completed downloads" kind=Kind::Text
                    help="Relative paths are inside the data directory."/>
                <Setting key="downloads.incomplete_directory" label="Incomplete downloads" kind=Kind::Text/>
                <Setting key="downloads.subdirectory" label="Subfolder per download" kind=Kind::Text
                    help="Tokens: ${SOURCE_DIRECTORY}, ${SOURCE_PATH}, ${SOURCE_USERNAME}, ${BATCH_ID}, ${SEARCH_TEXT}…; empty for none."/>
            </FieldGrid>
            <FieldGrid>
                <Setting key="downloads.slots" label="Concurrent downloads" kind=Kind::COUNT restart=true help="0 is unlimited."/>
                <Setting key="downloads.speed_limit" label="Speed limit (KiB/s)" kind=Kind::COUNT help="0 is unlimited."/>
                <Setting key="downloads.retry_attempts" label="Attempts per file" kind=Kind::COUNT/>
                <Setting key="downloads.retry_delay" label="First retry after" kind=Kind::Duration
                    help="Doubles up to the longest retry delay."/>
                <Setting key="downloads.retry_max_delay" label="Longest retry delay" kind=Kind::Duration/>
                <Setting key="downloads.resume" label="Resume partial files" kind=Kind::Toggle/>
                <Setting key="downloads.on_conflict" label="When the file exists"
                    kind=Kind::Select(vec![("rename", "Rename the new file"), ("overwrite", "Overwrite")])/>
                <Setting key="downloads.file_mode" label="File mode" kind=Kind::Text
                    help="Octal (0644) or symbolic (rw-r--r--); empty keeps the default."/>
            </FieldGrid>
        </Section>
    }
}

/// The eight per-user limits below `prefix` (`uploads.limits`, `groups.<name>.limits`).
#[component]
fn Limits(prefix: String) -> impl IntoView {
    const LIMITS: [(&str, &str); 8] = [
        ("queued_files", "Queued files"),
        ("queued_megabytes", "Queued MB"),
        ("daily_files", "Files per day"),
        ("daily_megabytes", "MB per day"),
        ("weekly_files", "Files per week"),
        ("weekly_megabytes", "MB per week"),
        ("daily_failures", "Failures per day"),
        ("weekly_failures", "Failures per week"),
    ];
    view! {
        <FieldGrid>
            {LIMITS.into_iter().map(|(key, label)| view! {
                <Setting key=format!("{prefix}.{key}") label=label kind=Kind::COUNT/>
            }).collect_view()}
        </FieldGrid>
    }
}

#[component]
fn UploadsSection() -> impl IntoView {
    view! {
        <Section id="settings-uploads" title="Uploads">
            <FieldGrid>
                <Setting key="uploads.slots" label="Concurrent uploads" restart=true
                    kind=Kind::Number { min: 1, max: u64::from(u32::MAX) } help="At least 1."/>
                <Setting key="uploads.speed_limit" label="Speed limit (KiB/s)" kind=Kind::COUNT help="0 is unlimited."/>
            </FieldGrid>
            <h4 class="ui header">"Limits per user"</h4>
            <p class="form-help">"0 is unlimited. A group's own non-zero limits take precedence."</p>
            <Limits prefix="uploads.limits".to_owned()/>
        </Section>
    }
}

#[component]
fn GroupFields(name: String) -> impl IntoView {
    let prefix = StoredValue::new(format!("groups.{name}"));
    let leechers = name == "leechers";
    let custom = !model::BUILT_IN_GROUPS.contains(&name.as_str());
    let form = fields::use_form();
    let key = move |suffix: &str| format!("{}.{suffix}", prefix.get_value());
    let members = key("members");
    let limits = key("limits");
    let open_limits = limits.clone();
    view! {
        <FieldGrid>
            <Setting key=key("priority") label="Priority" kind=Kind::COUNT help="Lower is served first."/>
            <Setting key=key("strategy") label="Queue strategy" kind=strategies()/>
            <Setting key=key("slots") label="Upload slots" kind=Kind::COUNT
                help="0 leaves only the global limit."/>
            <Setting key=key("speed_limit") label="Speed limit (KiB/s)" kind=Kind::COUNT help="0 is unlimited."/>
            {leechers.then(|| view! {
                <Setting key="groups.leechers.min_shared_files" label="Minimum shared files" kind=Kind::COUNT
                    help="Users sharing fewer files are leechers."/>
                <Setting key="groups.leechers.min_shared_directories" label="Minimum shared folders" kind=Kind::COUNT
                    help="Users sharing fewer folders are leechers."/>
            })}
        </FieldGrid>
        {custom.then(|| view! {
            <Setting key=members label="Members" kind=Kind::List("Username")
                help="A user can be in one group."/>
        })}
        <details class="form-details" open=move || custom || form.has_errors_below(&open_limits)>
            <summary>"Limits per user (non-zero values override the upload limits)"</summary>
            <Limits prefix=limits/>
        </details>
    }
}

#[component]
fn GroupsSection() -> impl IntoView {
    let form = fields::use_form();
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
        }
    };
    view! {
        <Section id="settings-groups" title="Groups"
            description="Upload queue groups. Privileged users always go first, then lower priority numbers. Users in no group are in Default; users sharing too little are Leechers.">
            <h4 class="ui header">"Default"</h4>
            <GroupFields name="default".to_owned()/>
            <h4 class="ui header">"Leechers"</h4>
            <GroupFields name="leechers".to_owned()/>
            <For each=move || form.custom_groups() key=|name| name.clone() let:group>
                {
                    let removing = group.clone();
                    let locked = {
                        let group = group.clone();
                        move || form.pinned_by(&format!("groups.{group}")).is_some()
                    };
                    view! {
                        <div class="form-subsection custom-group">
                            <h4 class="ui header">
                                {group.clone()}
                                <button type="button" class="ui mini basic negative button group-remove" disabled=locked
                                    aria-label=format!("Remove group {group}")
                                    on:click=move |_| form.remove_group(&removing)>
                                    "Remove group"
                                </button>
                            </h4>
                            <GroupFields name=group.clone()/>
                        </div>
                    }
                }
            </For>
            <Field id="settings-new-group" label="New group" error=error
                help="Saved with the other changes.">
                <div class="ui action input">
                    <TextInput value=name on_input=move |t: String| name.set(t)
                        placeholder="e.g. friends"/>
                    <button type="button" class="ui button group-add" on:click=move |_| add()>"Add group"</button>
                </div>
            </Field>
        </Section>
    }
}

#[component]
fn BlacklistSection() -> impl IntoView {
    view! {
        <Section id="settings-blacklist" title="Blacklist"
            description="Users refused searches, browsing and transfers.">
            <Setting key="blacklist.users" label="Users" kind=Kind::List("Username")
                help="Case-insensitive."/>
            <Setting key="blacklist.patterns" label="Username patterns" kind=Kind::List("e.g. spam*")
                help="Wildcards * and ?, case-insensitive."/>
            <Setting key="blacklist.cidrs" label="IP ranges" kind=Kind::List("e.g. 203.0.113.0/24")
                help="When set, search responses and downloads also check the peer's address."/>
        </Section>
    }
}

#[component]
fn SearchSection() -> impl IntoView {
    view! {
        <Section id="settings-search" title="Search" description="Answering other users' searches.">
            <FieldGrid>
                <Setting key="search.incoming_concurrency" label="Searches answered at once" restart=true
                    kind=Kind::Number { min: 1, max: u64::from(u32::MAX) }/>
                <Setting key="search.incoming_circuit_breaker" label="Queue limit" kind=Kind::COUNT
                    help="Queued searches beyond this are dropped."/>
                <Setting key="search.response_file_limit" label="Files per response" kind=Kind::COUNT/>
            </FieldGrid>
            <Setting key="search.ignore_queries" label="Ignore searches" kind=Kind::List("Regular expression")
                help="Case-insensitive regular expressions; matching searches are not answered."/>
        </Section>
    }
}

#[component]
fn RetentionSection() -> impl IntoView {
    const KEYS: [(&str, &str); 10] = [
        ("searches", "Searches"),
        ("downloads_succeeded", "Succeeded downloads"),
        ("downloads_errored", "Errored downloads"),
        ("downloads_cancelled", "Cancelled downloads"),
        ("downloads_failed", "Failed downloads"),
        ("uploads_succeeded", "Succeeded uploads"),
        ("uploads_errored", "Errored uploads"),
        ("uploads_cancelled", "Cancelled uploads"),
        ("uploads_failed", "Failed uploads"),
        ("logs", "Log files"),
    ];
    view! {
        <Section id="settings-retention" title="Retention"
            description="How long finished records are kept; empty keeps them forever.">
            <FieldGrid>
                {KEYS.into_iter().map(|(key, label)| view! {
                    <Setting key=format!("retention.{key}") label=label kind=Kind::OptionalDuration/>
                }).collect_view()}
            </FieldGrid>
        </Section>
    }
}

#[component]
fn LoggingSection() -> impl IntoView {
    view! {
        <Section id="settings-logging" title="Logging & metrics"
            description="Usually set by the deployment; locked when set in slskr.toml or the environment.">
            <FieldGrid>
                <Setting key="logging.level" label="Log level" restart=true
                    kind=Kind::Select(vec![("trace", "Trace"), ("debug", "Debug"), ("info", "Info"), ("warn", "Warn"), ("error", "Error")])
                    help="RUST_LOG takes precedence."/>
                <Setting key="logging.file" label="Write log files" kind=Kind::Toggle restart=true/>
            </FieldGrid>
            <FieldGrid>
                <Setting key="metrics.enabled" label="Prometheus metrics" kind=Kind::Toggle restart=true/>
                <Setting key="metrics.path" label="Metrics path" kind=Kind::Text restart=true/>
                <Setting key="metrics.username" label="Metrics username" kind=Kind::Text restart=true
                    help="Basic auth; set both or neither."/>
                <Setting key="metrics.password" label="Metrics password" kind=Kind::Secret { confirm: false } restart=true/>
            </FieldGrid>
        </Section>
    }
}
