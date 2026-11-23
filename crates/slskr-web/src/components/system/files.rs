use leptos::{prelude::*, task::spawn_local_scoped_with_cancellation};
use leptos_router::{components::A, hooks::use_params_map};
use slskr_types::files::FilesystemDirectory;

use crate::{
    config,
    services::{api, util::encode_uri_component},
};

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
    let base = base.trim_end_matches('/');
    view! {
        <nav class="ui secondary menu" aria-label="File roots">
            { [("downloads", "Downloads"), ("incomplete", "Incomplete")].into_iter().map(|(dir, label)| view! {
                <A href=format!("{base}/system/files/{dir}") attr:class=move || if root.get() == Some(dir) { "active item" } else { "item" }>{label}</A>
            }).collect_view() }
        </nav>
        {move || match root.get() {
            Some(root) => view! { <Directory root/> }.into_any(),
            None => view! { <p role="alert">"Unknown file root."</p> }.into_any(),
        }}
    }
}

#[component]
fn Directory(root: &'static str) -> impl IntoView {
    let path = RwSignal::new(String::new());
    let listing = RwSignal::new(None::<(String, FilesystemDirectory)>);
    let error = RwSignal::new(String::new());
    let revision = RwSignal::new(0_u64);
    let deleting = RwSignal::new(false);
    Effect::new(move |_| {
        let path = path.get();
        revision.track();
        listing.set(None);
        error.set(String::new());
        spawn_local_scoped_with_cancellation(async move {
            match api::get::<FilesystemDirectory>(&endpoint(root, &path)).await {
                Ok(data) => listing.set(Some((path, data))),
                Err(err) => error.set(err.to_string()),
            }
        });
    });
    let delete = move |path: &str| {
        if deleting.get_untracked() {
            return;
        }
        let confirmed = web_sys::window().is_some_and(|window| {
            window
                .confirm_with_message(&format!("Delete '{path}'?"))
                .unwrap_or(false)
        });
        if !confirmed {
            return;
        }
        let url = endpoint(root, path);
        deleting.set(true);
        error.set(String::new());
        leptos::task::spawn_local(async move {
            match api::delete::<()>(&url).await {
                Ok(()) => {
                    revision.try_update(|revision| *revision += 1);
                }
                Err(err) => {
                    error.try_set(err.to_string());
                }
            }
            deleting.try_set(false);
        });
    };
    view! {
        <button type="button" class="ui button" on:click=move |_| path.set(String::new())>"Root"</button>
        <p role="alert">{move || error.get()}</p>
        {move || listing.get().map(|(current_path, dir)| {
            view! {
                <h3 class="ui header">{format!("/{root}/{current_path}")}</h3>
                <ul class="ui divided list">
                    {dir.directories.into_iter().map(|child| {
                        let child_path = child_path(&current_path, &child.name);
                        let target = child_path.clone();
                        view! {
                            <li class="item">
                                <button type="button" class="ui basic button" on:click=move |_| path.set(target.clone())>{format!("📁 {}", child.name)}</button>
                                <button type="button" class="ui negative basic button" disabled=deleting on:click=move |_| delete(&child_path)>"Delete directory"</button>
                            </li>
                        }
                    }).collect_view()}
                    {dir.files.into_iter().map(|file| {
                        let target = child_path(&current_path, &file.name);
                        view! {
                            <li class="item">
                                {file.name}{" — "}{file.size}{" bytes"}
                                <button type="button" class="ui negative basic button" disabled=deleting on:click=move |_| delete(&target)>"Delete file"</button>
                            </li>
                        }
                    }).collect_view()}
                </ul>
            }
        })}
    }
}
