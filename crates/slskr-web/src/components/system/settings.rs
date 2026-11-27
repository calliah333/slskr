//! `/settings/<category>`: every setting of `GET /api/settings`, edited in place and saved
//! with one sparse `PATCH`.
//!
//! A sticky sidebar lists five categories; each shows a stack of cards of setting rows.
//! All categories stay rendered (inactive ones `hidden`) so changes, errors and the save
//! bar span the whole page. `?section=<card>` scrolls to a card (`settings-<card>`) and
//! also accepts the anchors of the old one-page layout (`?section=soulseek`).

mod account;
mod advanced;
mod api_keys;
mod downloads;
mod fields;
mod model;
mod network;
mod sharing;

use leptos::{prelude::*, task::spawn_local_scoped_with_cancellation};
use leptos_router::{
    NavigateOptions,
    components::A,
    hooks::{use_navigate, use_params_map, use_query_map},
};
use slskr_types::settings::Settings;
use wasm_bindgen::JsCast;

use self::model::Form;
use crate::{
    components::app::SessionControl,
    services::{
        api,
        events::{Events, Topic},
    },
    ui::{
        Icon, Loading, PageHeader, Toast,
        form::{FormMessage, SaveBar},
    },
};

/// A group of settings cards with its own URL (`/settings/<slug>`).
struct Category {
    slug: &'static str,
    title: &'static str,
    icon: &'static str,
    /// `(anchor, title)` of its cards; the card's element id is `settings-<anchor>`.
    cards: &'static [(&'static str, &'static str)],
}

const CATEGORIES: [Category; 5] = [
    Category {
        slug: "account",
        title: "Account",
        icon: "user",
        cards: &[
            ("soulseek", "Soulseek account"),
            ("profile", "Profile"),
            ("web", "Web sign-in"),
            ("api-keys", "API keys"),
        ],
    },
    Category {
        slug: "sharing",
        title: "Sharing",
        icon: "share-2",
        cards: &[
            ("shares", "Shared folders"),
            ("uploads", "Upload limits"),
            ("groups", "Groups"),
            ("blacklist", "Blacklist"),
        ],
    },
    Category {
        slug: "downloads",
        title: "Downloads",
        icon: "download",
        cards: &[
            ("downloads", "Download folders"),
            ("download-limits", "Speed and retries"),
        ],
    },
    Category {
        slug: "network",
        title: "Network",
        icon: "globe",
        cards: &[
            ("connection", "Incoming connections"),
            ("distributed", "Distributed network"),
            ("web-server", "Web server"),
        ],
    },
    Category {
        slug: "advanced",
        title: "Advanced",
        icon: "settings",
        cards: &[
            ("search", "Search throttling"),
            ("retention", "Retention"),
            ("logging", "Logging and metrics"),
        ],
    },
];

const DEFAULT_CATEGORY: &str = "account";
const SCOPE: &str = ".settings";

fn is_category(slug: &str) -> bool {
    CATEGORIES.iter().any(|category| category.slug == slug)
}

/// The category holding the card `anchor`.
fn category_of_card(anchor: &str) -> Option<&'static str> {
    CATEGORIES
        .iter()
        .find(|category| category.cards.iter().any(|(card, _)| *card == anchor))
        .map(|category| category.slug)
}

fn replace() -> NavigateOptions {
    NavigateOptions {
        replace: true,
        scroll: false,
        ..Default::default()
    }
}

fn scroll_to_card(anchor: &str) {
    let id = format!("settings-{anchor}");
    request_animation_frame(move || {
        if let Some(element) = document().get_element_by_id(&id) {
            element.scroll_into_view();
        }
    });
}

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
                leptos::task::spawn_local(form.reload());
            }
        });
    }

    let params = use_params_map();
    let query = use_query_map();
    let navigate = use_navigate();
    let current = Memo::new(move |_| {
        params
            .with(|p| p.get("category"))
            .filter(|slug| is_category(slug))
            .unwrap_or_else(|| DEFAULT_CATEGORY.to_owned())
    });

    // Deep links: `/settings/<card>`, `?section=<category>` and `?section=<card>`.
    {
        let navigate = navigate.clone();
        Effect::new(move |_| {
            let param = params.with(|p| p.get("category"));
            let section = query.with(|q| q.get("section"));
            let loaded = form.settings.with(Option::is_some);
            if let Some(param) = param.clone().filter(|p| !is_category(p)) {
                let target = match category_of_card(&param) {
                    Some(category) => format!("/settings/{category}?section={param}"),
                    None => "/settings".to_owned(),
                };
                navigate(&target, replace());
                return;
            }
            let Some(section) = section else {
                return;
            };
            if is_category(&section) {
                navigate(&format!("/settings/{section}"), replace());
            } else if let Some(category) = category_of_card(&section) {
                if param.is_none() || category != current.get_untracked() {
                    navigate(
                        &format!("/settings/{category}?section={section}"),
                        replace(),
                    );
                } else if loaded {
                    scroll_to_card(&section);
                }
            }
        });
    }

    // Shows the first invalid control: switches to its category, opens its disclosure
    // and focuses it.
    let reveal_invalid = {
        let navigate = navigate.clone();
        move || {
            let navigate = navigate.clone();
            request_animation_frame(move || {
                let selector = format!("{SCOPE} [aria-invalid=true]");
                let Ok(Some(element)) = document().query_selector(&selector) else {
                    return;
                };
                if let Ok(Some(details)) = element.closest("details") {
                    let _ = details.set_attribute("open", "");
                }
                let category = element
                    .closest("[data-category]")
                    .ok()
                    .flatten()
                    .and_then(|panel| panel.get_attribute("data-category"));
                let focus = move || {
                    if let Ok(element) = element.clone().dyn_into::<web_sys::HtmlElement>() {
                        let _ = element.focus();
                    }
                };
                match category {
                    Some(category) if category != current.get_untracked() => {
                        navigate(&format!("/settings/{category}"), replace());
                        gloo_timers::callback::Timeout::new(50, focus).forget();
                    }
                    _ => focus(),
                }
            });
        }
    };

    let save = {
        let reveal_invalid = reveal_invalid.clone();
        Callback::new(move |()| {
            if form.saving.get_untracked() {
                return;
            }
            if form
                .client_errors
                .with_untracked(|errors| !errors.is_empty())
            {
                reveal_invalid();
                return;
            }
            let body = form.patch();
            let signs_out = form.changes_sign_in();
            form.saving.set(true);
            form.invalidate_reload();
            form.top_error.set(None);
            let reveal_invalid = reveal_invalid.clone();
            spawn_local_scoped_with_cancellation(async move {
                match api::patch::<_, Settings>("/settings", &body).await {
                    Ok(settings) => {
                        form.saved(settings);
                        if signs_out {
                            signed_out.set(true);
                            if let Some(session) = session {
                                session.recheck.run(());
                            }
                            return;
                        }
                        form.notice.set(Some("Settings saved.".into()));
                    }
                    Err(error) if error.status == 400 => {
                        form.show_errors(&error.message);
                        reveal_invalid();
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
        })
    };
    let problem = Signal::derive(move || {
        let invalid = form.client_errors.with(|errors| errors.len());
        (invalid > 0).then(|| match invalid {
            1 => "Fix the highlighted field to save".to_owned(),
            n => format!("Fix the {n} highlighted fields to save"),
        })
    });

    // The success toast goes away by itself.
    Effect::new(move |_| {
        if form.notice.with(Option::is_some) {
            let shown = form.notice.get_untracked();
            gloo_timers::callback::Timeout::new(4000, move || {
                if form.notice.try_get_untracked().flatten() == shown {
                    form.notice.try_set(None);
                }
            })
            .forget();
        }
    });

    let base = crate::config::url_base().trim_end_matches('/').to_owned();
    view! {
        <div class="settings">
            <PageHeader title="Settings" show_title=true/>
            <PendingRestart/>
            {move || form.top_error.get().map(|message| view! {
                <FormMessage kind="danger" role="alert" title="Could not save the settings"><p>{message}</p></FormMessage>
            })}
            <Show when=move || form.settings.with(Option::is_some)
                fallback=|| view! { <Loading label="Loading settings"/> }>
                <div class="settings-layout">
                    <nav class="settings-nav" aria-label="Settings">
                        <ul class="settings-nav__list">
                            {CATEGORIES.iter().map(|category| {
                                let slug = category.slug;
                                let active = move || current.get() == slug;
                                let cards = category.cards.iter().map(|(anchor, title)| {
                                    let anchor = *anchor;
                                    view! {
                                        <li>
                                            <a class="settings-nav__card" href=format!("{base}/settings/{slug}?section={anchor}")
                                                on:click=move |event| { event.prevent_default(); scroll_to_card(anchor); }>
                                                {*title}
                                            </a>
                                        </li>
                                    }
                                }).collect_view();
                                view! {
                                    <li class="settings-nav__group">
                                        <A href=format!("{base}/settings/{slug}")
                                            attr:class=move || if active() { "settings-nav__item is-active" } else { "settings-nav__item" }>
                                            <Icon name=category.icon/>
                                            <span>{category.title}</span>
                                        </A>
                                        <ul class="settings-nav__cards" hidden=move || !active()>{cards}</ul>
                                    </li>
                                }
                            }).collect_view()}
                        </ul>
                    </nav>
                    <div class="settings-panels" inert=move || form.saving.get()>
                        {CATEGORIES.iter().map(|category| {
                            let slug = category.slug;
                            view! {
                                <div class="settings-panel" data-category=slug hidden=move || current.get() != slug>
                                    <h2 class="sr-only">{category.title}</h2>
                                    {panel(slug)}
                                </div>
                            }
                        }).collect_view()}
                    </div>
                </div>
            </Show>
            <div class="toast-region" aria-live="polite">
                {move || form.notice.get().map(|message| view! {
                    <Toast variant="success" on_dismiss=Callback::new(move |()| form.notice.set(None))>
                        {message}
                    </Toast>
                })}
            </div>
            <SaveBar dirty=Signal::derive(move || form.dirty_count()) saving=form.saving
                on_save=save on_discard=move |()| { form.discard(); form.notice.set(None); }
                problem=problem/>
        </div>
    }
}

/// The cards of a category.
fn panel(slug: &str) -> AnyView {
    match slug {
        "account" => view! {
            <account::SoulseekAccount/>
            <account::Profile/>
            <account::WebLogin/>
            <api_keys::ApiKeys/>
        }
        .into_any(),
        "sharing" => view! {
            <sharing::SharedFolders/>
            <sharing::UploadLimits/>
            <sharing::Groups/>
            <sharing::Blacklist/>
        }
        .into_any(),
        "downloads" => view! {
            <downloads::DownloadFolders/>
            <downloads::DownloadLimits/>
        }
        .into_any(),
        "network" => view! {
            <network::Connection/>
            <network::Distributed/>
            <network::WebServer/>
        }
        .into_any(),
        _ => view! {
            <advanced::Search/>
            <advanced::Retention/>
            <advanced::Logging/>
        }
        .into_any(),
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
            <div class="alert alert--warning pending-restart" role="status">
                <Icon name="rotate-cw"/>
                <div class="alert__body">
                    <div class="alert__title">"Restart to apply changes"</div>
                    {move || error.get().map(|message| view! { <p class="text-danger" role="alert">{message}</p> })}
                </div>
                <button type="button" class="btn btn--sm pending-restart-button" disabled=restarting
                    aria-busy=move || restarting.get().then_some("true") on:click=restart>
                    {move || if restarting.get() {
                        view! { <span class="spinner" aria-hidden="true"></span> }.into_any()
                    } else {
                        view! { <Icon name="power"/> }.into_any()
                    }}
                    "Restart now"
                </button>
            </div>
        </Show>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cards_belong_to_one_category() {
        assert_eq!(category_of_card("soulseek"), Some("account"));
        assert_eq!(category_of_card("groups"), Some("sharing"));
        assert_eq!(category_of_card("logging"), Some("advanced"));
        assert_eq!(category_of_card("nothing"), None);
        let mut anchors: Vec<&str> = CATEGORIES
            .iter()
            .flat_map(|c| c.cards.iter().map(|(a, _)| *a))
            .collect();
        let count = anchors.len();
        anchors.sort_unstable();
        anchors.dedup();
        assert_eq!(anchors.len(), count);
        // Every anchor of the old one-page layout still resolves.
        for old in [
            "soulseek",
            "web",
            "api-keys",
            "shares",
            "downloads",
            "uploads",
            "groups",
            "blacklist",
            "search",
            "retention",
            "logging",
        ] {
            assert!(category_of_card(old).is_some(), "{old}");
        }
    }
}
