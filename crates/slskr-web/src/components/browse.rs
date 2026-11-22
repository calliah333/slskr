pub mod selection;
pub mod tree;

use self::selection::Selection;
use self::tree::{DirectoryTree, TreeNode, build_tree};
use crate::services::{api, util};
use leptos::prelude::*;
use slskr_api::users::{BrowseResponse, BrowseStatus, Directory};
use web_sys::window;

fn query_value(key: &str) -> Option<String> {
    let query = window()?.location().search().ok()?;
    web_sys::UrlSearchParams::new_with_str(&query)
        .ok()?
        .get(key)
}

fn set_query(username: &str) {
    if let Some(window) = window() {
        let _ = window.location().set_search(&format!(
            "?username={}",
            util::encode_uri_component(username)
        ));
    }
}

fn find_directory(nodes: &[TreeNode], name: &str) -> Option<TreeNode> {
    for node in nodes {
        if node.directory.name == name {
            return Some(node.clone());
        }
        if let Some(found) = find_directory(&node.children, name) {
            return Some(found);
        }
    }
    None
}

#[component]
pub fn Browse() -> impl IntoView {
    let username = RwSignal::new(query_value("username").unwrap_or_default());
    let input = username.clone();
    let results = RwSignal::new(Vec::<TreeNode>::new());
    let dirs = RwSignal::new(Vec::<Directory>::new());
    let selected = RwSignal::new(None::<String>);
    let loading = RwSignal::new(false);
    let progress = RwSignal::new(None::<BrowseStatus>);
    let error = RwSignal::new(None::<String>);
    let locked_counts = RwSignal::new((0u32, 0u32));
    let cancelled = RwSignal::new(false);
    on_cleanup(move || cancelled.set(true));
    let initial_user = username.get_untracked();
    if !initial_user.is_empty() {
        let run = initial_user;
        leptos::task::spawn_local(async move {
            load_browse(run, dirs, results, locked_counts, loading, error, selected).await;
        });
    }
    let submit = move || {
        let name = input.get_untracked().trim().to_owned();
        if name.is_empty() {
            return;
        }
        set_query(&name);
        username.set(name.clone());
        progress.set(None);
        let dirs = dirs;
        let results = results;
        let counts = locked_counts;
        let loading = loading;
        let error = error;
        let selected = selected;
        let progress = progress;
        loading.set(true);
        error.set(None);
        let stop = cancelled;
        leptos::task::spawn_local(async move {
            let user_for_poll = name.clone();
            let status = progress;
            let done = RwSignal::new(false);
            leptos::task::spawn_local(async move {
                while !done.get_untracked() && !stop.get_untracked() {
                    if let Ok(value) = api::get::<BrowseStatus>(&format!(
                        "/users/{}/browse/status",
                        util::encode_uri_component(&user_for_poll)
                    ))
                    .await
                    {
                        status.set(Some(value));
                    }
                    util::sleep(1000).await;
                }
            });
            load_browse(name, dirs, results, counts, loading, error, selected).await;
            done.set(true);
        });
    };
    view! {
        <div class="search-container browse-container">
            <section class="ui raised segment browse-segment">
                <span class="browse-segment-icon" aria-hidden="true">"Browse"</span>
                <input class="ui input" type="search" placeholder="Username" prop:value=move || username.get()
                    on:input=move |event| input.set(event_target_value(&event))
                    on:keydown=move |event: web_sys::KeyboardEvent| if event.key() == "Enter" { submit(); }/>
                <button type="button" class="ui button" disabled=move || loading.get() on:click=move |_| submit()>
                    {move || if loading.get() { "Browsing…" } else { "Browse" }}
                </button>
            </section>
            {move || progress.get().map(|status| view! { <div class="ui active progress"><div class="bar" style:width=format!("{}%", status.percent_complete.round())></div><div class="label">{format!("Downloaded {}% of response", status.percent_complete.round())}</div></div> })}
            {move || error.get().map(|message| view! { <p class="browse-error" role="alert">{message}</p> })}
            {move || {
                let tree = results.get();
                if tree.is_empty() { return ().into_any(); }
                let username = username.get();
                let (locked_dirs, locked_files) = locked_counts.get();
                let (total_dirs, total_files, separator) = dirs.with(|dirs| {
                    let separator = dirs.iter().find_map(|dir| {
                        if dir.name.contains('\\') { Some("\\") }
                        else if dir.name.contains('/') { Some("/") }
                        else { None }
                    }).unwrap_or("\\");
                    let total_dirs = (dirs.len() as u32).saturating_sub(locked_dirs);
                    let total_files = dirs.iter().map(|d| d.file_count).sum::<u32>().saturating_sub(locked_files);
                    (total_dirs, total_files, separator)
                });
                let chosen = selected.get();
                let directory = chosen.as_deref().and_then(|name| find_directory(&tree, name));
                view! {
                    <section class="ui raised card browse-tree-card"><div class="content">
                        <div class="header">{username.clone()}</div>
                        <div class="meta browse-meta">{format!("{} files in {} directories (including {} files in {} locked directories)", total_files + locked_files, total_dirs + locked_dirs, locked_files, locked_dirs)}</div>
                        <div class="browse-tree-wrapper"><DirectoryTree tree=tree selected=chosen.clone() on_select=Callback::new(move |node: TreeNode| selected.set(Some(node.directory.name.clone())))/></div>
                    </div></section>
                    {directory.map(|node| view! { <Selection username=username directory=node.directory.name separator=separator.to_owned() files=node.directory.files locked=node.locked/> })}
                }.into_any()
            }}
        </div>
    }
}

async fn load_browse(
    username: String,
    dirs: RwSignal<Vec<Directory>>,
    tree: RwSignal<Vec<TreeNode>>,
    locked_counts: RwSignal<(u32, u32)>,
    loading: RwSignal<bool>,
    error: RwSignal<Option<String>>,
    selected: RwSignal<Option<String>>,
) {
    let path = format!("/users/{}/browse", util::encode_uri_component(&username));
    match api::get::<BrowseResponse>(&path).await {
        Ok(response) => {
            let mut all = response.directories.clone();
            all.extend(response.locked_directories.clone());
            let built = build_tree(&response.directories, false)
                .into_iter()
                .chain(build_tree(&response.locked_directories, true))
                .collect();
            locked_counts.set((
                response.locked_directory_count,
                response
                    .locked_directories
                    .iter()
                    .map(|d| d.file_count)
                    .sum(),
            ));
            dirs.set(all);
            tree.set(built);
            selected.set(None);
        }
        Err(problem) => error.set(Some(format!("Failed to browse {username}: {problem}"))),
    }
    loading.set(false);
}
