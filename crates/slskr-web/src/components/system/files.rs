use leptos::{prelude::*, task::spawn_local_scoped_with_cancellation};
use leptos_router::{components::A, hooks::use_params_map};
use slskr_api::files::FilesystemDirectory;

use crate::{
    components::app::AppContext,
    config,
    services::{api, util::encode_path_base64},
};

fn endpoint(root: &str, kind: &str, path: &str) -> String {
    if path.is_empty() {
        format!("/files/{root}/{kind}")
    } else {
        format!("/files/{root}/{kind}/{}", encode_path_base64(path))
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
    let options = expect_context::<AppContext>().options;
    let can_delete = Memo::new(move |_| options.with(|options| options.remote_file_management));
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
            match api::get::<FilesystemDirectory>(&endpoint(root, "directories", &path)).await {
                Ok(data) => listing.set(Some((path, data))),
                Err(err) => error.set(err.to_string()),
            }
        });
    });
    let delete = move |kind, path: &str| {
        if deleting.get_untracked() || !can_delete.get_untracked() {
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
        let url = endpoint(root, kind, path);
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
            let can_delete = can_delete.get();
            view! {
                <h3 class="ui header">{format!("/{root}/{current_path}")}</h3>
                <ul class="ui divided list">
                    {dir.directories.unwrap_or_default().into_iter().map(|child| {
                        let child_path = child_path(&current_path, &child.name);
                        let target = child_path.clone();
                        view! {
                            <li class="item">
                                <button type="button" class="ui basic button" on:click=move |_| path.set(target.clone())>{format!("📁 {}", child.name)}</button>
                                {can_delete.then(|| view! {
                                    <button type="button" class="ui negative basic button" disabled=deleting on:click=move |_| delete("directories", &child_path)>"Delete directory"</button>
                                })}
                            </li>
                        }
                    }).collect_view()}
                    {dir.files.unwrap_or_default().into_iter().map(|file| {
                        let target = child_path(&current_path, &file.name);
                        view! {
                            <li class="item">
                                {file.name}{" — "}{file.length}{" bytes"}
                                {can_delete.then(|| view! {
                                    <button type="button" class="ui negative basic button" disabled=deleting on:click=move |_| delete("files", &target)>"Delete file"</button>
                                })}
                            </li>
                        }
                    }).collect_view()}
                </ul>
            }
        })}
    }
}
