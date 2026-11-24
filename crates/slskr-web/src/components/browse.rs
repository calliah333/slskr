pub mod actions;
pub mod selection;
pub mod tree;

use std::{cell::Cell, rc::Rc, sync::Arc};

use self::actions::BrowseActions;
use self::selection::Selection;
use self::tree::{DirectoryTree, Shares, build_tree, detect_separator};
use crate::{
    config,
    services::{api, util},
    ui::{
        Alert, Avatar, Button, Card, ContextMenu, ContextMenuState, EmptyState, Icon, PageHeader,
        Progress, Toast,
    },
};
use leptos::prelude::*;
use leptos_router::{
    NavigateOptions,
    hooks::{use_navigate, use_url},
};
use slskr_types::users::{BrowseResponse, BrowseStatus};

/// Summary counts of the current browse result.
#[derive(Clone, Copy, Default)]
struct BrowseInfo {
    directories: usize,
    files: usize,
    size: u64,
    locked_directories: usize,
}

/// Formats a count with thousands separators: `12,345`.
pub(crate) fn group_digits(value: impl ToString) -> String {
    let digits = value.to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (position, digit) in digits.chars().enumerate() {
        if position > 0 && (digits.len() - position).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    grouped
}

fn count(value: usize, one: &str, many: &str) -> String {
    format!(
        "{} {}",
        group_digits(value),
        if value == 1 { one } else { many }
    )
}

/// Below this width the folder tree is a collapsible panel above the files.
const NARROW: &str = "(max-width: 899px)";

fn is_narrow() -> bool {
    window()
        .match_media(NARROW)
        .ok()
        .flatten()
        .is_some_and(|query| query.matches())
}

#[component]
pub fn Browse() -> impl IntoView {
    let url = use_url();
    let query = Memo::new(move |_| {
        url.with(|url| {
            let params = web_sys::UrlSearchParams::new_with_str(url.search()).ok();
            let user = params
                .as_ref()
                .and_then(|params| params.get("username"))
                .unwrap_or_default();
            let path = params.and_then(|params| params.get("path"));
            (user, path)
        })
    });
    let query_user = Memo::new(move |_| query.with(|(user, _)| user.clone()));
    // `username` is the browsed user; `input` is the text box, applied only on submit.
    let username = RwSignal::new(String::new());
    let input = RwSignal::new(query_user.get_untracked());
    let results = RwSignal::new(None::<Arc<Shares>>);
    let info = RwSignal::new(BrowseInfo::default());
    let selected = RwSignal::new(None::<usize>);
    let loading = RwSignal::new(false);
    let progress = RwSignal::new(None::<BrowseStatus>);
    let error = RwSignal::new(None::<(String, String)>);
    let generation = RwSignal::new(0u64);
    let navigate = StoredValue::new_local(use_navigate());
    // The user whose shares `results` holds (`username` changes as soon as a browse starts).
    let browsed = RwSignal::new(String::new());
    let actions = BrowseActions {
        username: browsed,
        results,
        pending: RwSignal::new(false),
        notice: RwSignal::new(None),
        menu: RwSignal::new(None::<ContextMenuState>),
    };
    provide_context(actions);

    let start = move |name: String| {
        let ticket = generation.get_untracked().wrapping_add(1);
        generation.set(ticket);
        actions.menu.set(None);
        selected.set(None);
        results.set(None);
        username.set(name.clone());
        input.set(name.clone());
        progress.set(None);
        loading.set(true);
        error.set(None);
        let current = move || generation.try_get_untracked() == Some(ticket);
        let done = Rc::new(Cell::new(false));
        let poll_done = done.clone();
        let user_for_poll = name.clone();
        leptos::task::spawn_local(async move {
            while !poll_done.get() && current() {
                if let Ok(value) = api::get::<BrowseStatus>(&format!(
                    "/users/{}/browse/status",
                    util::encode_uri_component(&user_for_poll)
                ))
                .await
                    && !poll_done.get()
                    && current()
                {
                    let _ = progress.try_set(Some(value));
                }
                util::sleep(1000).await;
            }
        });
        leptos::task::spawn_local(async move {
            let path = format!("/users/{}/browse", util::encode_uri_component(&name));
            let result = api::get::<BrowseResponse>(&path).await;
            done.set(true);
            if !current() {
                return;
            }
            match result {
                Ok(response) => {
                    let separator = detect_separator(
                        response
                            .directories
                            .iter()
                            .chain(&response.locked_directories)
                            .map(|d| d.name.as_str()),
                    );
                    let all_files = || {
                        response
                            .directories
                            .iter()
                            .chain(&response.locked_directories)
                            .flat_map(|d| &d.files)
                    };
                    let summary = BrowseInfo {
                        directories: response.directories.len() + response.locked_directories.len(),
                        files: all_files().count(),
                        size: all_files()
                            .fold(0_u64, |total, file| total.saturating_add(file.size)),
                        locked_directories: response.locked_directories.len(),
                    };
                    let all = response
                        .directories
                        .into_iter()
                        .map(|d| (d, false))
                        .chain(response.locked_directories.into_iter().map(|d| (d, true)))
                        .collect();
                    let shares = Shares::new(build_tree(all, separator), separator);
                    let _ = info.try_set(summary);
                    let _ = browsed.try_set(name.clone());
                    let _ = results.try_set(Some(Arc::new(shares)));
                }
                Err(problem) => {
                    let _ = results.try_set(None);
                    let _ = error.try_set(Some((name, problem.to_string())));
                }
            }
            let _ = progress.try_set(None);
            let _ = loading.try_set(false);
        });
    };
    // The `username` query parameter drives the page, so links to `/browse?username=…`
    // work while the page is open.
    Effect::new(move |_| {
        let name = query_user.get();
        if name == username.get_untracked() {
            return;
        }
        if name.is_empty() {
            generation.update(|ticket| *ticket = ticket.wrapping_add(1));
            actions.menu.set(None);
            actions.notice.set(None);
            username.set(String::new());
            input.set(String::new());
            browsed.set(String::new());
            selected.set(None);
            results.set(None);
            progress.set(None);
            loading.set(false);
            error.set(None);
        } else {
            start(name);
        }
    });
    Effect::new(move |_| {
        let (user, path) = query.get();
        let index = results.with(|shares| {
            let path = path.as_deref()?;
            let shares = shares.as_ref()?;
            if user != browsed.get_untracked() {
                return None;
            }
            let path = path.trim_end_matches(['\\', '/']);
            shares
                .nodes
                .iter()
                .position(|node| node.path.trim_end_matches(['\\', '/']) == path)
        });
        actions.menu.set(None);
        selected.set(index);
    });
    let submit = move || {
        let name = input.get_untracked().trim().to_owned();
        if name.is_empty() {
            return;
        }
        if name == query_user.get_untracked() {
            start(name);
            return;
        }
        // Client-side navigation keeps the SPA (and `web.url_base`) intact.
        navigate.with_value(|navigate| {
            navigate(
                &format!("/browse?username={}", util::encode_uri_component(&name)),
                NavigateOptions {
                    replace: true,
                    ..Default::default()
                },
            )
        });
    };
    let tree_open = RwSignal::new(true);
    let filter = RwSignal::new(String::new());
    let tree_scroll = NodeRef::<leptos::html::Div>::new();
    let on_select = Callback::new(move |index: usize| {
        actions.menu.set(None);
        selected.set(Some(index));
        // On phones, picking a folder without subfolders closes the folder panel; folders
        // with subfolders keep it open for drilling down.
        let leaf = results.with_untracked(|shares| {
            shares
                .as_ref()
                .is_some_and(|shares| !shares.has_children(index))
        });
        if leaf && is_narrow() {
            tree_open.set(false);
        }
    });
    let base = config::url_base().trim_end_matches('/').to_owned();

    let progress_view = move || {
        loading.get().then(|| {
            let percent = progress.with(|status| status.as_ref().map(|s| s.percent_complete));
            let user = username.get();
            let (title, detail) = match progress.get() {
                Some(status) if status.size > 0 => (
                    format!("Downloading {user}'s share list"),
                    format!(
                        "{} of {} · {}%",
                        util::format_bytes(status.bytes_transferred as f64),
                        util::format_bytes(status.size as f64),
                        status.percent_complete.round()
                    ),
                ),
                _ => (format!("Connecting to {user}…"), String::new()),
            };
            view! {
                <div class="browse-progress" role="status" aria-live="polite">
                    <div class="progress-label">
                        <span>{title}</span>
                        <span>{detail}</span>
                    </div>
                    {match percent {
                        Some(p) => view! { <Progress value=Signal::derive(move || p) label="Share list download"/> }.into_any(),
                        None => view! { <Progress label="Share list download"/> }.into_any(),
                    }}
                </div>
            }
        })
    };

    let result_view = move || {
        let shares = results.get()?;
        let user = browsed.get_untracked();
        let BrowseInfo {
            directories,
            files,
            size,
            locked_directories,
        } = info.get_untracked();
        let encoded = util::encode_uri_component(&user);
        let user_href = format!("{base}/users?username={encoded}");
        let chat_href = format!("{base}/chat?username={encoded}");
        let tree_shares = shares.clone();
        let selection_shares = shares.clone();
        let selected_label = move || {
            selected
                .get()
                .map(|index| tree_shares.nodes[index].label.clone())
        };
        let user_name = user.clone();
        Some(view! {
            <div class="browse-result stack">
                <div class="browse-summary">
                    <Avatar name=user.clone()/>
                    <div class="browse-summary__text">
                        <h2 class="browse-summary__name">{user.clone()}</h2>
                        <p class="muted text-sm">
                            {format!("{} in {} · {}", count(files, "file", "files"), count(directories, "folder", "folders"), util::format_bytes(size as f64))}
                            {(locked_directories > 0).then(|| view! {
                                " · "<span class="browse-summary__locked"><Icon name="lock" size="xs"/>{format!("{locked_directories} locked")}</span>
                            })}
                        </p>
                    </div>
                    <div class="browse-summary__actions">
                        <Button size="sm" icon="user" text="Profile" href=user_href/>
                        <Button size="sm" icon="message-square" text="Message" href=chat_href/>
                        <Button size="sm" icon="refresh-cw" label="Browse again" disabled=loading
                            on_click=Callback::new(move |_| start(user_name.clone()))/>
                    </div>
                </div>
                <div class="browse-layout">
                    <section class="card browse-tree-card" class:is-collapsed=move || !tree_open.get() aria-label="Folders">
                        <button type="button" class="browse-tree-card__toggle" aria-expanded=move || tree_open.get().to_string()
                            aria-controls="browse-tree-body" on:click=move |_| tree_open.update(|open| *open = !*open)>
                            <Icon name="folder"/>
                            <span class="grow truncate">
                                {move || match selected_label() {
                                    Some(label) => view! { "Folders" <span class="muted">{format!(" · {label}")}</span> }.into_any(),
                                    None => "Folders".into_any(),
                                }}
                            </span>
                            <Icon name="chevron-down" class="browse-tree-card__chevron"/>
                        </button>
                        <div class="browse-tree-card__body" id="browse-tree-body">
                            <div class="browse-tree-card__filter">
                                <div class="input-icon">
                                    <Icon name="filter"/>
                                    <input class="input input--sm browse-filter" type="search" placeholder="Filter folders"
                                        aria-label="Filter folders" prop:value=move || filter.get()
                                        on:input=move |event| {
                                            filter.set(event_target_value(&event));
                                            if let Some(scroll) = tree_scroll.get_untracked() { scroll.set_scroll_top(0); }
                                        }/>
                                </div>
                            </div>
                            <div class="browse-tree-card__scroll" node_ref=tree_scroll>
                                <DirectoryTree shares=shares selected=selected on_select=on_select filter=filter/>
                            </div>
                        </div>
                    </section>
                    <div class="browse-files-pane">
                        {move || match selected.get() {
                            Some(index) => view! {
                                <Selection shares=selection_shares.clone() index=index
                                    on_navigate=on_select/>
                            }.into_any(),
                            None => view! {
                                <Card>
                                    <EmptyState icon="folder-open" title="Select a folder"
                                        description="Pick a folder on the left to see its files. Use the arrow keys to move through the tree."/>
                                </Card>
                            }.into_any(),
                        }}
                    </div>
                </div>
            </div>
        })
    };

    view! {
        <div class="browse stack">
            <PageHeader title="Browse" description="Look through the folders a user shares and download what you want."/>
            <Card class="browse-form" title="Browse a user's shares" description="Downloading a large share list can take a minute.">
                <form class="input-group" on:submit=move |event| { event.prevent_default(); submit(); }>
                    <div class="input-icon grow">
                        <Icon name="user"/>
                        <input class="input browse-username" type="search" placeholder="Username" aria-label="Username"
                            autocomplete="off" spellcheck="false"
                            prop:value=move || input.get()
                            on:input=move |event| input.set(event_target_value(&event))/>
                    </div>
                    <Button variant="primary" button_type="submit" icon="folder-open" text="Browse"
                        loading=loading/>
                </form>
                {progress_view}
                {move || error.get().map(|(name, message)| view! {
                    <Alert variant="danger" title=format!("Could not browse {name}") class="browse-error">
                        <p>{message}</p>
                        <p class="muted">"The user may be offline, or their client is not accepting connections. Try again later."</p>
                    </Alert>
                })}
            </Card>
            {move || {
                let showing = results.with(Option::is_some);
                (!showing && !loading.get() && error.with(Option::is_none)).then(|| view! {
                    <Card>
                        <EmptyState icon="folder-open" title="No user selected"
                            description="Enter a username above to see the folders they share."/>
                    </Card>
                })
            }}
            {result_view}
            <ContextMenu state=actions.menu/>
            <div class="toast-region" role="status" aria-live="polite">
                {move || actions.notice.get().map(|notice| {
                    let href = format!("{}/downloads", config::url_base().trim_end_matches('/'));
                    view! {
                        <Toast variant=notice.variant on_dismiss=Callback::new(move |()| actions.notice.set(None))>
                            <div class="font-medium">{notice.title}</div>
                            {(!notice.body.is_empty()).then(|| view! { <div class="muted">{notice.body}</div> })}
                            {notice.link.then(|| view! { <a href=href>"View downloads"</a> })}
                        </Toast>
                    }
                })}
            </div>
        </div>
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn groups_digits() {
        assert_eq!(super::group_digits(0), "0");
        assert_eq!(super::group_digits(999), "999");
        assert_eq!(super::group_digits(1000), "1,000");
        assert_eq!(super::group_digits(12_345_678u64), "12,345,678");
    }
}
