pub mod selection;
pub mod tree;

use std::{cell::Cell, rc::Rc};

use self::selection::Selection;
use self::tree::{DirectoryTree, TreeNode, build_tree, detect_separator};
use crate::services::{api, util};
use leptos::prelude::*;
use leptos_router::{
    NavigateOptions,
    hooks::{use_navigate, use_url},
};
use slskr_types::users::{BrowseResponse, BrowseStatus};

fn find_directory<'a>(nodes: &'a [TreeNode], name: &str) -> Option<&'a TreeNode> {
    for node in nodes {
        if node.directory.name == name {
            return Some(node);
        }
        if let Some(found) = find_directory(&node.children, name) {
            return Some(found);
        }
    }
    None
}

/// Summary counts and separator of the current browse result.
#[derive(Clone, Copy, Default)]
struct BrowseInfo {
    directories: u32,
    files: u32,
    locked_directories: u32,
    locked_files: u32,
    separator: char,
}

#[component]
pub fn Browse() -> impl IntoView {
    let url = use_url();
    let initial_user = url.with_untracked(|url| {
        web_sys::UrlSearchParams::new_with_str(url.search())
            .ok()
            .and_then(|params| params.get("username"))
            .unwrap_or_default()
    });
    // `username` is the browsed user; `input` is the text box, applied only on submit.
    let username = RwSignal::new(initial_user.clone());
    let input = RwSignal::new(initial_user.clone());
    let results = RwSignal::new(Vec::<TreeNode>::new());
    let info = RwSignal::new(BrowseInfo::default());
    let selected = RwSignal::new(None::<String>);
    let loading = RwSignal::new(false);
    let progress = RwSignal::new(None::<BrowseStatus>);
    let error = RwSignal::new(None::<String>);
    let generation = RwSignal::new(0u64);
    let navigate = StoredValue::new_local(use_navigate());

    let start = move |name: String| {
        let ticket = generation.get_untracked().wrapping_add(1);
        generation.set(ticket);
        username.set(name.clone());
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
                    let separator =
                        detect_separator(response.directories.iter().map(|d| d.name.as_str()));
                    let summary = BrowseInfo {
                        directories: response.directories.len() as u32,
                        files: response
                            .directories
                            .iter()
                            .map(|d| d.files.len() as u32)
                            .sum(),
                        locked_directories: response.locked_directories.len() as u32,
                        locked_files: response
                            .locked_directories
                            .iter()
                            .map(|d| d.files.len() as u32)
                            .sum(),
                        separator,
                    };
                    let all = response
                        .directories
                        .into_iter()
                        .map(|d| (d, false))
                        .chain(response.locked_directories.into_iter().map(|d| (d, true)))
                        .collect();
                    let _ = info.try_set(summary);
                    let _ = results.try_set(build_tree(all, separator));
                    let _ = selected.try_set(None);
                }
                Err(problem) => {
                    let _ = error.try_set(Some(format!("Failed to browse {name}: {problem}")));
                }
            }
            let _ = progress.try_set(None);
            let _ = loading.try_set(false);
        });
    };
    if !initial_user.is_empty() {
        start(initial_user);
    }
    let submit = move || {
        let name = input.get_untracked().trim().to_owned();
        if name.is_empty() {
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
        start(name);
    };
    let on_select = Callback::new(move |name: String| selected.set(Some(name)));
    view! {
        <div class="search-container browse-container">
            <section class="ui raised segment browse-segment">
                <span class="browse-segment-icon" aria-hidden="true">"Browse"</span>
                <input class="ui input" type="search" placeholder="Username" prop:value=move || input.get()
                    on:input=move |event| input.set(event_target_value(&event))
                    on:keydown=move |event: web_sys::KeyboardEvent| if event.key() == "Enter" { submit(); }/>
                <button type="button" class="ui button" disabled=move || loading.get() on:click=move |_| submit()>
                    {move || if loading.get() { "Browsing…" } else { "Browse" }}
                </button>
            </section>
            {move || progress.get().map(|status| view! { <div class="ui active progress"><div class="bar" style:width=format!("{}%", status.percent_complete.round())></div><div class="label">{format!("Downloaded {}% of response", status.percent_complete.round())}</div></div> })}
            {move || error.get().map(|message| view! { <p class="browse-error" role="alert">{message}</p> })}
            // The tree is rebuilt only when results change; selection only toggles row classes.
            {move || {
                let tree = results.get();
                if tree.is_empty() { return ().into_any(); }
                let user = username.get_untracked();
                let BrowseInfo { directories, files, locked_directories, locked_files, .. } = info.get_untracked();
                view! {
                    <section class="ui raised card browse-tree-card"><div class="content">
                        <div class="header">{user}</div>
                        <div class="meta browse-meta">{format!("{} files in {} directories (including {} files in {} locked directories)", files + locked_files, directories + locked_directories, locked_files, locked_directories)}</div>
                        <div class="browse-tree-wrapper"><DirectoryTree tree=tree selected=selected on_select=on_select/></div>
                    </div></section>
                }.into_any()
            }}
            {move || {
                let name = selected.get()?;
                let node = results.with(|tree| {
                    find_directory(tree, &name).map(|node| (node.directory.files.clone(), node.locked))
                });
                let (files, locked) = node?;
                let separator = info.with_untracked(|info| info.separator).to_string();
                Some(view! { <Selection username=username.get_untracked() directory=name separator=separator files=files locked=locked/> })
            }}
        </div>
    }
}
