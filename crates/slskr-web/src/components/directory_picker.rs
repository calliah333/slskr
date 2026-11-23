//! A modal for choosing a local directory to share, backed by `GET /api/fs/dirs`.

use leptos::{prelude::*, task::spawn_local_scoped_with_cancellation};
use slskr_types::setup::DirectoryListing;

use crate::{
    services::api,
    ui::{Icon, Modal, form::FormMessage},
};

/// Breadcrumb segments of an absolute path: `(name, path)` from the root down.
fn crumbs(path: &str) -> Vec<(String, String)> {
    let mut out = vec![("/".to_owned(), "/".to_owned())];
    let mut current = String::new();
    for part in path.split('/').filter(|part| !part.is_empty()) {
        current.push('/');
        current.push_str(part);
        out.push((part.to_owned(), current.clone()));
    }
    out
}

/// Opens while `open` is true; "Share this folder" calls `on_pick` with the listed path and
/// closes. Starts in the daemon's home directory.
#[component]
pub fn DirectoryPicker(open: RwSignal<bool>, on_pick: Callback<String>) -> impl IntoView {
    // `None` lists the default (home) directory.
    let path = RwSignal::new(None::<String>);
    let listing = RwSignal::new(None::<DirectoryListing>);
    let error = RwSignal::new(None::<String>);
    let loading = RwSignal::new(false);
    let typed = RwSignal::new(String::new());
    let list_ref = NodeRef::<leptos::html::Ul>::new();

    Effect::new(move |_| {
        if !open.get() {
            return;
        }
        let requested = path.get();
        loading.set(true);
        spawn_local_scoped_with_cancellation(async move {
            let url = match &requested {
                Some(path) => format!(
                    "/fs/dirs?path={}",
                    crate::services::util::encode_uri_component(path)
                ),
                None => "/fs/dirs".to_owned(),
            };
            match api::get::<DirectoryListing>(&url).await {
                Ok(result) => {
                    typed.set(result.path.clone());
                    listing.set(Some(result));
                    error.set(None);
                }
                Err(failure) => error.set(Some(failure.message)),
            }
            loading.set(false);
        });
    });

    let go = move |target: String| {
        error.set(None);
        path.set(Some(target));
    };
    let pick = move || {
        if let Some(current) = listing.get_untracked() {
            on_pick.run(current.path);
            open.set(false);
        }
    };

    view! {
        <Modal open=open on_close=Callback::new(move |()| open.set(false))
            title="Choose a folder to share" class="directory-picker"
            footer=ViewFn::from(move || view! {
                <button type="button" class="btn" on:click=move |_| open.set(false)>"Cancel"</button>
                <button type="button" class="btn btn--primary directory-pick" disabled=move || listing.with(Option::is_none)
                    on:click=move |_| pick()>
                    <Icon name="check"/>"Share this folder"
                </button>
            })>
            <div role="group" aria-label="Folders">
                <div class="directory-picker-path">
                    <nav class="breadcrumb" aria-label="Current folder">
                        {move || listing.get().map(|listing| {
                            let all = crumbs(&listing.path);
                            let last = all.len() - 1;
                            all.into_iter().enumerate().map(|(i, (name, target))| {
                                let divider = (i > 1).then(|| view! { <span class="divider" aria-hidden="true">"/"</span> });
                                if i == last {
                                    view! { {divider}<span class="active section" aria-current="location">{name}</span> }.into_any()
                                } else {
                                    view! {
                                        {divider}
                                        <button type="button" class="section link-button" on:click=move |_| go(target.clone())>
                                            {name}
                                        </button>
                                    }.into_any()
                                }
                            }).collect_view()
                        })}
                    </nav>
                </div>
                <form class="directory-picker-go" on:submit=move |event| {
                    event.prevent_default();
                    let target = typed.get_untracked().trim().to_owned();
                    if !target.is_empty() {
                        go(target);
                    }
                }>
                    <div class="input-group">
                        <input class="input input--mono form-input" type="text" spellcheck="false" aria-label="Folder path"
                            prop:value=move || typed.get()
                            on:input=move |event| typed.set(event_target_value(&event))/>
                        <button type="submit" class="btn">"Go"</button>
                    </div>
                </form>
                {move || error.get().map(|message| view! { <FormMessage kind="danger">{message}</FormMessage> })}
                <div class="directory-picker-toolbar">
                    <button type="button" class="btn btn--sm directory-up"
                        disabled=move || listing.with(|l| l.as_ref().is_none_or(|l| l.parent.is_none()))
                        on:click=move |_| {
                            if let Some(parent) = listing.with_untracked(|l| l.as_ref().and_then(|l| l.parent.clone())) {
                                go(parent);
                            }
                        }>
                        <Icon name="corner-left-up"/>"Parent folder"
                    </button>
                </div>
                <ul class="list directory-list" node_ref=list_ref aria-busy=move || loading.get().to_string()>
                    {move || listing.get().map(|listing| {
                        if listing.dirs.is_empty() {
                            view! { <li class="directory-empty muted">"No subfolders"</li> }.into_any()
                        } else {
                            listing.dirs.into_iter().map(|dir| view! {
                                <li>
                                    <button type="button" class="link-button directory-entry" on:click=move |_| go(dir.path.clone())>
                                        <Icon name="folder"/>{dir.name}
                                    </button>
                                </li>
                            }).collect_view().into_any()
                        }
                    })}
                </ul>
                {move || listing.with(|l| l.as_ref().is_some_and(|l| l.truncated)).then(|| view! {
                    <p class="help form-help">"Only the first 1000 folders are listed; type a path above to go further."</p>
                })}
            </div>
        </Modal>
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn crumbs() {
        assert_eq!(
            super::crumbs("/srv/music"),
            [
                ("/".to_owned(), "/".to_owned()),
                ("srv".to_owned(), "/srv".to_owned()),
                ("music".to_owned(), "/srv/music".to_owned())
            ]
        );
        assert_eq!(super::crumbs("/").len(), 1);
    }
}
