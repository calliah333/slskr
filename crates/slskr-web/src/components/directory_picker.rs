//! Choosing folders to share: the [`DirectoryPicker`] dialog (backed by `GET /api/fs/dirs`)
//! and the [`FolderList`] of `shares.directories` entries that uses it.

use leptos::{ev::KeyboardEvent, prelude::*, task::spawn_local_scoped_with_cancellation};
use slskr_types::setup::DirectoryListing;

use crate::{
    services::api,
    ui::{EmptyState, Icon, Modal, form::FieldContext, form::FormMessage},
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

/// One `shares.directories` entry: `/path`, `[Alias]/path` or `!/path` (excluded).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShareEntry {
    pub alias: Option<String>,
    pub path: String,
    pub excluded: bool,
}

impl ShareEntry {
    pub fn parse(entry: &str) -> Self {
        if let Some(path) = entry.strip_prefix('!') {
            return Self {
                alias: None,
                path: path.to_owned(),
                excluded: true,
            };
        }
        if let Some(rest) = entry.strip_prefix('[')
            && let Some((alias, path)) = rest.split_once(']')
        {
            return Self {
                alias: Some(alias.to_owned()).filter(|a| !a.is_empty()),
                path: path.to_owned(),
                excluded: false,
            };
        }
        Self {
            alias: None,
            path: entry.to_owned(),
            excluded: false,
        }
    }

    /// The name peers see: the alias, else the folder's own name.
    pub fn name(&self) -> String {
        self.alias.clone().unwrap_or_else(|| {
            self.path
                .trim_end_matches('/')
                .rsplit('/')
                .next()
                .filter(|name| !name.is_empty())
                .unwrap_or(&self.path)
                .to_owned()
        })
    }

    pub fn format(&self) -> String {
        match (&self.alias, self.excluded) {
            (_, true) => format!("!{}", self.path),
            (Some(alias), false) => format!("[{alias}]{}", self.path),
            (None, false) => self.path.clone(),
        }
    }
}

/// The shared folders of a `shares.directories` list: one row per folder (name, path,
/// rename and remove) and an "Add folder" button that opens the [`DirectoryPicker`]. Entries
/// that exclude a folder (`!/path`) are kept but not listed. Inside a `Field` the add button
/// takes the field's id.
#[component]
pub fn FolderList(
    #[prop(into)] items: Signal<Vec<String>>,
    #[prop(into)] on_change: Callback<Vec<String>>,
    #[prop(into, optional)] disabled: Signal<bool>,
) -> impl IntoView {
    let field = use_context::<FieldContext>();
    let id = field.as_ref().map(|f| f.id.clone());
    let described_by = field.as_ref().map(|f| f.described_by);
    let locked = field.as_ref().map(|f| f.locked);
    let is_disabled = Signal::derive(move || disabled.get() || locked.is_some_and(|l| l.get()));
    let picking = RwSignal::new(false);
    let renaming = RwSignal::new(None::<usize>);
    let draft = RwSignal::new(String::new());

    let shown = Signal::derive(move || {
        items.with(|all| {
            all.iter()
                .enumerate()
                .map(|(index, entry)| (index, ShareEntry::parse(entry)))
                .filter(|(_, entry)| !entry.excluded)
                .collect::<Vec<_>>()
        })
    });
    let remove = move |index: usize| {
        let mut next = items.get_untracked();
        if index < next.len() {
            next.remove(index);
            on_change.run(next);
        }
    };
    let commit_rename = move || {
        let Some(index) = renaming.get_untracked() else {
            return;
        };
        renaming.set(None);
        let mut next = items.get_untracked();
        if let Some(entry) = next.get_mut(index) {
            let mut parsed = ShareEntry::parse(entry);
            let alias = draft.get_untracked().trim().to_owned();
            let default = ShareEntry {
                alias: None,
                ..parsed.clone()
            }
            .name();
            parsed.alias = Some(alias).filter(|a| !a.is_empty() && *a != default);
            let formatted = parsed.format();
            if formatted != *entry {
                *entry = formatted;
                on_change.run(next);
            }
        }
    };
    let pick = Callback::new(move |path: String| {
        let mut next = items.get_untracked();
        if !next
            .iter()
            .any(|entry| ShareEntry::parse(entry).path == path)
        {
            next.push(path);
            on_change.run(next);
        }
    });

    view! {
        <div class="folder-list">
            <Show when=move || !shown.with(Vec::is_empty)
                fallback=|| view! {
                    <EmptyState icon="folder-open" title="No shared folders yet"
                        description="Add the folders other users may browse and download from."/>
                }>
                <ul class="folder-list__rows" aria-label="Shared folders">
                    <For each=move || shown.get() key=|(index, entry)| (*index, entry.format()) let:row>
                        {
                            let (index, entry) = row;
                            let name = entry.name();
                            let has_alias = entry.alias.is_some();
                            let editing = move || renaming.get() == Some(index);
                            let start = {
                                let name = name.clone();
                                move |_| {
                                    draft.set(name.clone());
                                    renaming.set(Some(index));
                                    request_animation_frame(move || {
                                        if let Ok(Some(input)) = document().query_selector(".folder-row__rename")
                                            && let Ok(input) = wasm_bindgen::JsCast::dyn_into::<web_sys::HtmlInputElement>(input)
                                        {
                                            let _ = input.focus();
                                            input.select();
                                        }
                                    });
                                }
                            };
                            let path = entry.path.clone();
                            view! {
                                <li class="folder-row">
                                    <span class="folder-row__icon" aria-hidden="true"><Icon name="folder"/></span>
                                    <div class="folder-row__text">
                                        <Show when=editing fallback={
                                            let name = name.clone();
                                            move || view! {
                                                <span class="folder-row__name">
                                                    {name.clone()}
                                                    {has_alias.then(|| view! { <span class="badge badge--outline">"alias"</span> })}
                                                </span>
                                            }
                                        }>
                                            <input class="input input--sm folder-row__rename" type="text" spellcheck="false"
                                                aria-label=format!("Name peers see for {}", path.clone())
                                                prop:value=move || draft.get()
                                                on:input=move |event| draft.set(event_target_value(&event))
                                                on:blur=move |_| commit_rename()
                                                on:keydown=move |event: KeyboardEvent| match event.key().as_str() {
                                                    "Enter" => { event.prevent_default(); commit_rename(); }
                                                    "Escape" => { event.stop_propagation(); renaming.set(None); }
                                                    _ => {}
                                                }/>
                                        </Show>
                                        <span class="folder-row__path mono" title=entry.path.clone()>{entry.path.clone()}</span>
                                    </div>
                                    <div class="folder-row__actions">
                                        <button type="button" class="btn btn--action folder-rename" disabled=is_disabled
                                            aria-label=format!("Rename {name}") title="Rename" on:click=start>
                                            <Icon name="pencil"/>
                                        </button>
                                        <button type="button" class="btn btn--action is-danger folder-remove" disabled=is_disabled
                                            aria-label=format!("Stop sharing {name}") title="Stop sharing"
                                            on:click=move |_| remove(index)>
                                            <Icon name="trash-2"/>
                                        </button>
                                    </div>
                                </li>
                            }
                        }
                    </For>
                </ul>
            </Show>
            <div class="folder-list__add">
                <button type="button" class="btn btn--sm directory-browse" id=id disabled=is_disabled
                    aria-describedby=move || described_by.map(|d| d.get()).filter(|d| !d.is_empty())
                    on:click=move |_| picking.set(true)>
                    <Icon name="plus"/>"Add folder"
                </button>
            </div>
            <DirectoryPicker open=picking on_pick=pick/>
        </div>
    }
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

    Effect::new(move |_| {
        if !open.get() {
            return;
        }
        let requested = path.get();
        listing.set(None);
        error.set(None);
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
        loading.set(true);
        listing.set(None);
        error.set(None);
        path.set(Some(target));
    };
    let pick = move || {
        if !loading.get_untracked()
            && let Some(current) = listing.get_untracked()
        {
            on_pick.run(current.path);
            open.set(false);
        }
    };
    let current_path = move || listing.with(|l| l.as_ref().map(|l| l.path.clone()));

    view! {
        <Modal open=open on_close=Callback::new(move |()| open.set(false))
            title="Choose a folder to share" class="directory-picker"
            description="Open a folder, then share it with everything inside."
            footer=ViewFn::from(move || view! {
                <span class="directory-picker-selected mono" title=current_path>
                    {current_path}
                </span>
                <button type="button" class="btn" on:click=move |_| open.set(false)>"Cancel"</button>
                <button type="button" class="btn btn--primary directory-pick" disabled=move || loading.get() || listing.with(Option::is_none)
                    on:click=move |_| pick()>
                    <Icon name="share-2"/>"Share this folder"
                </button>
            })>
            <div class="directory-picker-body" role="group" aria-label="Folders">
                <div class="directory-picker-bar">
                    <button type="button" class="btn btn--icon btn--sm directory-up"
                        aria-label="Parent folder" title="Parent folder"
                        disabled=move || listing.with(|l| l.as_ref().is_none_or(|l| l.parent.is_none()))
                        on:click=move |_| {
                            if let Some(parent) = listing.with_untracked(|l| l.as_ref().and_then(|l| l.parent.clone())) {
                                go(parent);
                            }
                        }>
                        <Icon name="corner-left-up"/>
                    </button>
                    <nav class="breadcrumb" aria-label="Current folder">
                        {move || listing.get().map(|listing| {
                            let all = crumbs(&listing.path);
                            let last = all.len() - 1;
                            all.into_iter().enumerate().map(|(i, (name, target))| {
                                let divider = (i > 0).then(|| view! { <Icon name="chevron-right" size="xs" class="divider"/> });
                                let label = if i == 0 {
                                    view! { <Icon name="hard-drive" size="sm"/><span class="sr-only">"/"</span> }.into_any()
                                } else {
                                    name.clone().into_any()
                                };
                                if i == last {
                                    view! { {divider}<span class="active section" aria-current="location">{label}</span> }.into_any()
                                } else {
                                    view! {
                                        {divider}
                                        <button type="button" class="section" on:click=move |_| go(target.clone())>
                                            {label}
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
                        <input class="input input--sm input--mono" type="text" spellcheck="false" aria-label="Folder path"
                            placeholder="/path/to/folder"
                            prop:value=move || typed.get()
                            on:input=move |event| typed.set(event_target_value(&event))/>
                        <button type="submit" class="btn btn--sm">"Go"</button>
                    </div>
                </form>
                {move || error.get().map(|message| view! { <FormMessage kind="danger"><p>{message}</p></FormMessage> })}
                <ul class="directory-list" aria-busy=move || loading.get().to_string() aria-label="Subfolders">
                    {move || listing.get().map(|listing| {
                        if listing.dirs.is_empty() {
                            view! { <li class="directory-empty">"No subfolders. Share this folder, or go up."</li> }.into_any()
                        } else {
                            listing.dirs.into_iter().map(|dir| view! {
                                <li>
                                    <button type="button" class="directory-entry" on:click=move |_| go(dir.path.clone())>
                                        <Icon name="folder" class="directory-entry__icon"/>
                                        {dir.name}
                                        <Icon name="chevron-right" size="sm" class="directory-entry__chevron"/>
                                    </button>
                                </li>
                            }).collect_view().into_any()
                        }
                    })}
                </ul>
                {move || listing.with(|l| l.as_ref().is_some_and(|l| l.truncated)).then(|| view! {
                    <p class="help">"Only the first 1000 folders are listed; type a path above to go further."</p>
                })}
            </div>
        </Modal>
    }
}

#[cfg(test)]
mod tests {
    use super::ShareEntry;

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

    #[test]
    fn share_entries() {
        let plain = ShareEntry::parse("/srv/music/");
        assert_eq!(plain.name(), "music");
        assert_eq!(plain.format(), "/srv/music/");
        let alias = ShareEntry::parse("[Tunes]/srv/music");
        assert_eq!(alias.alias.as_deref(), Some("Tunes"));
        assert_eq!(alias.name(), "Tunes");
        assert_eq!(alias.format(), "[Tunes]/srv/music");
        let excluded = ShareEntry::parse("!/srv/music/tmp");
        assert!(excluded.excluded);
        assert_eq!(excluded.format(), "!/srv/music/tmp");
    }
}
