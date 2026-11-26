//! System → Files: browse and delete in the downloads and incomplete directories.

use std::collections::BTreeSet;

use leptos::{prelude::*, task::spawn_local_scoped_with_cancellation};
use leptos_router::{components::A, hooks::use_params_map};
use slskr_types::files::FilesystemDirectory;

use super::format;
use crate::{
    config,
    services::{
        api,
        util::{encode_uri_component, format_bytes},
    },
    ui::{Alert, Card, EmptyState, Icon, Modal},
};

const ROOTS: [(&str, &str, &str); 2] = [
    ("downloads", "Downloads", "download"),
    ("incomplete", "Incomplete", "loader-circle"),
];

fn endpoint(root: &str, path: &str) -> String {
    if path.is_empty() {
        format!("/files/{root}")
    } else {
        format!("/files/{root}?path={}", encode_uri_component(path))
    }
}

fn child_path(directory: &str, name: &str) -> String {
    if directory.is_empty() {
        name.to_owned()
    } else {
        format!("{directory}/{name}")
    }
}

#[component]
pub fn Files() -> impl IntoView {
    let params = use_params_map();
    let root = Memo::new(move |_| {
        params.with(|params| match params.get("root").as_deref() {
            None | Some("downloads") => Some("downloads"),
            Some("incomplete") => Some("incomplete"),
            _ => None,
        })
    });
    let base = config::url_base();
    let base = base.trim_end_matches('/').to_owned();
    view! {
        <div class="system-files stack">
            <nav class="tabs files-roots" aria-label="Folder">
                {ROOTS.into_iter().map(|(dir, label, icon)| {
                    let active = move || root.get() == Some(dir);
                    view! {
                        <A href=format!("{base}/system/files/{dir}")
                            attr:class=move || if active() { "tabs__tab is-active" } else { "tabs__tab" }
                            attr:aria-current=move || active().then_some("page")>
                            <Icon name=icon/>{label}
                        </A>
                    }
                }).collect_view()}
            </nav>
            {move || match root.get() {
                Some(root) => view! { <Directory root/> }.into_any(),
                None => view! { <Alert variant="danger">"Unknown folder. Choose Downloads or Incomplete."</Alert> }.into_any(),
            }}
        </div>
    }
}

/// One entry of the current listing.
#[derive(Clone, PartialEq)]
struct Entry {
    name: String,
    path: String,
    is_dir: bool,
    size: Option<u64>,
    modified: chrono::DateTime<chrono::Utc>,
}

#[component]
fn Directory(root: &'static str) -> impl IntoView {
    let label = ROOTS
        .iter()
        .find(|(dir, _, _)| *dir == root)
        .map_or(root, |(_, label, _)| *label);
    let path = RwSignal::new(String::new());
    let listing = RwSignal::new(None::<(String, Vec<Entry>)>);
    let error = RwSignal::new(None::<String>);
    let revision = RwSignal::new(0_u64);
    let loading = RwSignal::new(false);
    let deleting = RwSignal::new(false);
    let selected = RwSignal::new(BTreeSet::<String>::new());
    // Paths waiting for confirmation in the delete dialog.
    let confirm = RwSignal::new(Vec::<String>::new());

    Effect::new(move |previous: Option<String>| {
        let current = path.get();
        revision.track();
        // A new folder starts empty; a reload of the same folder keeps the rows meanwhile.
        if previous.as_deref() != Some(current.as_str()) {
            listing.set(None);
            selected.set(BTreeSet::new());
        }
        loading.set(true);
        error.set(None);
        let request_path = current.clone();
        spawn_local_scoped_with_cancellation(async move {
            match api::get::<FilesystemDirectory>(&endpoint(root, &request_path)).await {
                Ok(dir) => {
                    let mut entries: Vec<Entry> = dir
                        .directories
                        .into_iter()
                        .map(|d| Entry {
                            path: child_path(&request_path, &d.name),
                            name: d.name,
                            is_dir: true,
                            size: None,
                            modified: d.modified_at,
                        })
                        .collect();
                    entries.sort_by_cached_key(|e| e.name.to_lowercase());
                    let mut files: Vec<Entry> = dir
                        .files
                        .into_iter()
                        .map(|f| Entry {
                            path: child_path(&request_path, &f.name),
                            name: f.name,
                            is_dir: false,
                            size: Some(f.size),
                            modified: f.modified_at,
                        })
                        .collect();
                    files.sort_by_cached_key(|e| e.name.to_lowercase());
                    entries.extend(files);
                    // Drop selections that no longer exist.
                    if !selected.with_untracked(BTreeSet::is_empty) {
                        let paths: BTreeSet<_> =
                            entries.iter().map(|entry| entry.path.as_str()).collect();
                        selected.update(|s| s.retain(|path| paths.contains(path.as_str())));
                    }
                    listing.set(Some((request_path, entries)));
                }
                Err(err) => error.set(Some(err.to_string())),
            }
            loading.set(false);
        });
        current
    });

    let delete_confirmed = move || {
        let targets = confirm.get_untracked();
        confirm.set(Vec::new());
        if targets.is_empty() || deleting.get_untracked() {
            return;
        }
        deleting.set(true);
        error.set(None);
        leptos::task::spawn_local(async move {
            let mut failures = Vec::new();
            for target in &targets {
                if let Err(err) = api::delete::<()>(&endpoint(root, target)).await {
                    failures.push(format!("{target}: {err}"));
                }
            }
            if !failures.is_empty() {
                error.try_set(Some(format!("Could not delete {}", failures.join("; "))));
            }
            selected.try_update(|s| s.retain(|p| !targets.contains(p)));
            revision.try_update(|r| *r += 1);
            deleting.try_set(false);
        });
    };

    let entries = Memo::new(move |_| listing.with(|l| l.as_ref().map(|(_, e)| e.clone())));
    let all_selected = Memo::new(move |_| {
        entries.with(|e| {
            e.as_ref().is_some_and(|e| {
                !e.is_empty() && selected.with(|s| e.iter().all(|x| s.contains(&x.path)))
            })
        })
    });
    let toggle_all = move |_| {
        let paths: Vec<String> = entries.with(|e| {
            e.as_ref()
                .map(|e| e.iter().map(|x| x.path.clone()).collect())
                .unwrap_or_default()
        });
        if all_selected.get_untracked() {
            selected.set(BTreeSet::new());
        } else {
            selected.set(paths.into_iter().collect());
        }
    };

    let crumbs = move || {
        let current = path.get();
        let mut parts = vec![(label.to_owned(), String::new())];
        let mut acc = String::new();
        for part in current.split('/').filter(|p| !p.is_empty()) {
            acc = child_path(&acc, part);
            parts.push((part.to_owned(), acc.clone()));
        }
        let last = parts.len() - 1;
        parts
            .into_iter()
            .enumerate()
            .map(|(i, (name, target))| {
                let divider = (i > 0).then(|| view! { <Icon name="chevron-right" size="sm" class="divider"/> });
                let item = if i == last {
                    view! { <span class="active" aria-current="location">{name}</span> }.into_any()
                } else {
                    view! {
                        <button type="button" class="link-button" on:click=move |_| path.set(target.clone())>{name}</button>
                    }
                    .into_any()
                };
                view! { {divider}{item} }
            })
            .collect_view()
    };

    let up = move |_| {
        path.update(|p| {
            *p = p
                .rsplit_once('/')
                .map(|(parent, _)| parent.to_owned())
                .unwrap_or_default();
        });
    };

    view! {
        <Card flush=true class="files-card">
            <div class="files-toolbar">
                <button type="button" class="btn btn--sm btn--icon" aria-label="Up one folder" title="Up one folder"
                    disabled=move || path.with(String::is_empty) on:click=up>
                    <Icon name="corner-left-up"/>
                </button>
                <nav class="breadcrumb files-breadcrumb grow" aria-label="Folder path">{crumbs}</nav>
                {move || {
                    let count = selected.with(BTreeSet::len);
                    (count > 0).then(|| view! {
                        <span class="muted text-sm">{format!("{count} selected")}</span>
                        <button type="button" class="btn btn--sm btn--danger-outline files-delete-selected"
                            disabled=move || deleting.get()
                            on:click=move |_| confirm.set(selected.get_untracked().into_iter().collect())>
                            <Icon name="trash-2"/>"Delete selected"
                        </button>
                    })
                }}
                <button type="button" class="btn btn--sm btn--icon" aria-label="Refresh" title="Refresh"
                    disabled=move || loading.get() on:click=move |_| revision.update(|r| *r += 1)>
                    <Icon name="refresh-cw" spin=Signal::derive(move || loading.get())/>
                </button>
            </div>
            {move || error.get().map(|message| view! {
                <div class="files-error"><Alert variant="danger">{message}</Alert></div>
            })}
            {move || match entries.get() {
                None if error.with(Option::is_some) => ().into_any(),
                None => view! { <div class="loading" role="status"><span class="spinner"></span>"Loading…"</div> }.into_any(),
                Some(list) if list.is_empty() => view! {
                    <EmptyState icon="folder-open" title="This folder is empty"/>
                }.into_any(),
                Some(list) => view! {
                    <div class="table-wrap" aria-busy=move || loading.get().then_some("true")>
                        <table class="table files-table">
                            <thead>
                                <tr>
                                    <th class="check-col">
                                        <input type="checkbox" class="checkbox" aria-label="Select all"
                                            prop:checked=move || all_selected.get() on:change=toggle_all/>
                                    </th>
                                    <th>"Name"</th>
                                    <th class="num">"Size"</th>
                                    <th class="hide-sm">"Modified"</th>
                                    <th class="actions"><span class="sr-only">"Actions"</span></th>
                                </tr>
                            </thead>
                            <tbody>
                                {list.into_iter().map(|entry| {
                                    let Entry { name, path: entry_path, is_dir, size, modified } = entry;
                                    let is_selected = {
                                        let p = entry_path.clone();
                                        Memo::new(move |_| selected.with(|s| s.contains(&p)))
                                    };
                                    let toggle = {
                                        let p = entry_path.clone();
                                        move |_| selected.update(|s| {
                                            if !s.remove(&p) {
                                                s.insert(p.clone());
                                            }
                                        })
                                    };
                                    let delete_target = entry_path.clone();
                                    let kind = if is_dir { "folder" } else { "file" };
                                    let name_cell = if is_dir {
                                        let target = entry_path.clone();
                                        view! {
                                            <button type="button" class="link-button files-entry files-dir"
                                                on:click=move |_| path.set(target.clone())>
                                                <Icon name="folder" class="files-icon files-icon--dir"/>
                                                <span class="truncate">{name.clone()}</span>
                                            </button>
                                        }.into_any()
                                    } else {
                                        view! {
                                            <span class="files-entry">
                                                <Icon name="file" class="files-icon"/>
                                                <span class="truncate">{name.clone()}</span>
                                            </span>
                                        }.into_any()
                                    };
                                    view! {
                                        <tr class:is-selected=move || is_selected.get()>
                                            <td class="check-col">
                                                <input type="checkbox" class="checkbox" aria-label=format!("Select {name}")
                                                    prop:checked=move || is_selected.get() on:change=toggle/>
                                            </td>
                                            <td class="files-name">{name_cell}</td>
                                            <td class="num muted">{size.map_or_else(|| "—".to_owned(), |s| format_bytes(s as f64))}</td>
                                            <td class="hide-sm muted nowrap" title=format::full(modified)>{format::relative(modified)}</td>
                                            <td class="actions">
                                                <button type="button" class="btn btn--action is-danger"
                                                    aria-label=format!("Delete {kind} {name}") title=format!("Delete {kind}")
                                                    disabled=move || deleting.get()
                                                    on:click=move |_| confirm.set(vec![delete_target.clone()])>
                                                    <Icon name="trash-2"/>
                                                </button>
                                            </td>
                                        </tr>
                                    }
                                }).collect_view()}
                            </tbody>
                        </table>
                    </div>
                }.into_any(),
            }}
        </Card>
        <Modal size="sm" title="Delete permanently?"
            open=Signal::derive(move || !confirm.with(Vec::is_empty))
            on_close=Callback::new(move |()| confirm.set(Vec::new()))
            footer=ViewFn::from(move || view! {
                <button type="button" class="btn" on:click=move |_| confirm.set(Vec::new())>"Cancel"</button>
                <button type="button" class="btn btn--destructive files-confirm-delete" on:click=move |_| delete_confirmed()>
                    <Icon name="trash-2"/>"Delete"
                </button>
            })>
            {move || {
                let targets = confirm.get();
                let count = targets.len();
                view! {
                    <p>{if count == 1 {
                        "This file or folder is removed from disk, with everything inside it.".to_owned()
                    } else {
                        format!("These {count} items are removed from disk, with everything inside them.")
                    }}</p>
                    <ul class="files-confirm-list mono text-sm">
                        {targets.into_iter().take(8).map(|t| view! { <li>{format!("{label}/{t}")}</li> }).collect_view()}
                        {(count > 8).then(|| view! { <li class="muted">{format!("and {} more", count - 8)}</li> })}
                    </ul>
                }
            }}
        </Modal>
    }
}
