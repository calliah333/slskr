use std::sync::Arc;

use super::{
    actions::{BrowseActions, MenuAnchor, copy_text, plural},
    tree::Shares,
};
use crate::{
    services::util,
    ui::{Alert, Button, Icon, ListAction, ListSelection, MenuEntry},
};
use leptos::{ev::MouseEvent, prelude::*};
use slskr_types::users::File;
use wasm_bindgen::JsCast;

const AUDIO: &[&str] = &[
    "aac", "aif", "aiff", "alac", "ape", "dsf", "flac", "m4a", "mp2", "mp3", "mpc", "ogg", "opus",
    "wav", "wma", "wv",
];
const TEXT: &[&str] = &["cue", "log", "m3u", "m3u8", "md", "nfo", "sfv", "txt"];

fn file_icon(file: &File) -> &'static str {
    let extension = if file.extension.is_empty() {
        util::get_file_name(&file.filename)
            .rsplit_once('.')
            .map(|(_, extension)| extension.to_lowercase())
            .unwrap_or_default()
    } else {
        file.extension.to_lowercase()
    };
    if AUDIO.contains(&extension.as_str()) {
        "file-audio"
    } else if TEXT.contains(&extension.as_str()) {
        "file-text"
    } else {
        "file"
    }
}

fn element(event: &MouseEvent) -> Option<web_sys::Element> {
    event
        .current_target()
        .and_then(|target| target.dyn_into::<web_sys::Element>().ok())
}

/// The selected folder: its path, folder downloads and its files, selected like in a file
/// manager (click, Ctrl/Cmd+click, Shift+click, keyboard) rather than with checkboxes.
#[component]
pub fn Selection(
    shares: Arc<Shares>,
    index: usize,
    /// Selects another folder (breadcrumb links).
    on_navigate: Callback<usize>,
) -> impl IntoView {
    let actions = expect_context::<BrowseActions>();
    let node = shares.nodes[index].clone();
    let locked = node.locked;
    let file_count = shares.files[index].len();
    let has_children = shares.has_children(index);
    let (own_count, own_size) = shares.folder_totals(index, false);
    let (all_count, all_size) = shares.folder_totals(index, true);
    let selection = ListSelection::new(file_count);
    let shares = StoredValue::new(shares);
    let size = Memo::new(move |_| {
        let selected = selection.selected();
        shares.with_value(|shares| {
            selected
                .iter()
                .map(|&file| shares.files[index][file].size)
                .sum::<u64>()
        })
    });
    let count = Memo::new(move |_| selection.count());
    let pending = actions.pending;

    let download_selection = move || {
        let mut files = selection.selected_untracked();
        if files.is_empty() && file_count > 0 {
            files.push(selection.focused());
        }
        actions.download_files(index, &files);
    };
    let file_menu = move |file: usize, anchor: MenuAnchor| {
        if !selection.is_selected_untracked(file) {
            selection.click(file, false, false);
        }
        let selected = selection.selected_untracked().len();
        let path =
            shares.with_value(|shares| shares.remote_path(index, &shares.files[index][file]));
        let mut entries = vec![
            MenuEntry::item(
                format!("Download {}", plural(selected, "file", "files")),
                "download",
                download_selection,
            )
            .disabled(locked),
            MenuEntry::Separator,
        ];
        entries.extend(actions.folder_entries(index).into_iter().filter(|entry| {
            // The folder entries end with their own "Copy path"; use the file's instead.
            !matches!(entry, MenuEntry::Item { label, .. } if label == "Copy path")
        }));
        entries.push(MenuEntry::item("Copy path", "copy", move || {
            copy_text(&path)
        }));
        let label = shares.with_value(|shares| {
            util::get_file_name(&shares.files[index][file].filename).to_owned()
        });
        actions.menu.set(Some(anchor.place(label, entries)));
    };

    let crumbs = shares.with_value(|shares| {
        let lineage = shares.lineage(index);
        let last = lineage.len() - 1;
        lineage
            .into_iter()
            .enumerate()
            .map(|(position, folder)| {
                let label = shares.nodes[folder].label.clone();
                if position == last {
                    view! { <li><span class="breadcrumb__current" aria-current="page">{label}</span></li> }
                        .into_any()
                } else {
                    view! {
                        <li>
                            <button type="button" class="link-button" on:click=move |_| on_navigate.run(folder)>{label}</button>
                            <Icon name="chevron-right" size="xs" class="breadcrumb__divider"/>
                        </li>
                    }
                    .into_any()
                }
            })
            .collect_view()
    });
    // A top-level folder's title already is its full path.
    let nested = node.parent.is_some();
    let summary = {
        let own = format!(
            "{} ({})",
            plural(file_count, "file", "files"),
            util::format_bytes(
                shares.with_value(|s| s.files[index].iter().map(|f| f.size).sum::<u64>()) as f64
            )
        );
        if has_children {
            format!(
                "{own} · {} with {} in total",
                plural(node.end - index - 1, "subfolder", "subfolders"),
                plural(all_count, "file", "files")
            )
        } else {
            own
        }
    };
    // The split button's main action: this folder, or everything below it when the folder
    // only holds subfolders (an artist folder).
    let recursive_main = own_count == 0 && has_children;
    let (main_count, main_size) = if recursive_main {
        (all_count, all_size)
    } else {
        (own_count, own_size)
    };
    let main_text = if recursive_main {
        format!("Download all ({})", util::format_bytes(main_size as f64))
    } else {
        format!("Download folder ({})", util::format_bytes(main_size as f64))
    };
    let main_label = if recursive_main {
        format!(
            "Download this folder and its subfolders: {}",
            plural(main_count, "file", "files")
        )
    } else {
        format!(
            "Download the {} in this folder",
            plural(main_count, "file", "files")
        )
    };

    let rows = shares.with_value(|shares| {
        shares.files[index]
            .iter()
            .enumerate()
            .map(|(position, file)| {
                let name = util::get_file_name(&file.filename).to_owned();
                let attributes = util::format_attributes(
                    file.bit_rate,
                    file.is_variable_bit_rate,
                    file.bit_depth,
                    file.sample_rate,
                );
                let length = file
                    .duration_ms
                    .filter(|ms| *ms > 0)
                    .map(|ms| util::format_seconds(ms as f64 / 1000.0))
                    .unwrap_or_default();
                let meta = [attributes.as_str(), length.as_str()]
                    .into_iter()
                    .filter(|part| !part.is_empty())
                    .collect::<Vec<_>>()
                    .join(" · ");
                let icon = file_icon(file);
                let size = util::format_bytes(file.size as f64);
                view! {
                    <tr class="browse-file-row" class:is-selected=move || selection.is_selected(position)
                        class:is-locked=locked
                        data-index=position
                        tabindex=move || selection.tabindex(position)
                        aria-selected=move || selection.is_selected(position).to_string()
                        on:click=move |event: MouseEvent| {
                            selection.click(position, event.ctrl_key() || event.meta_key(), event.shift_key());
                        }
                        on:dblclick=move |_| actions.download_files(index, &[position])
                        on:contextmenu=move |event: MouseEvent| {
                            event.prevent_default();
                            file_menu(position, MenuAnchor::Pointer(event));
                        }>
                        <td class="browse-file">
                            <span class="browse-file__name">
                                <Icon name=icon class="browse-file__icon"/>
                                <span class="browse-file__text">
                                    <span class="break-all">{name}</span>
                                    {(!meta.is_empty()).then(|| view! { <span class="browse-file__meta">{meta}</span> })}
                                </span>
                            </span>
                        </td>
                        <td class="num">{size}</td>
                        <td class="nowrap muted hide-sm">{attributes}</td>
                        <td class="num muted hide-sm">{length}</td>
                        <td class="actions">
                            <button type="button" class="btn btn--action btn--icon btn--sm browse-file__more" tabindex="-1"
                                aria-label="File actions" title="File actions" aria-haspopup="menu"
                                on:click=move |event: MouseEvent| {
                                    event.stop_propagation();
                                    if let Some(button) = element(&event) {
                                        file_menu(position, MenuAnchor::Below(button));
                                    }
                                }>
                                <Icon name="ellipsis"/>
                            </button>
                        </td>
                    </tr>
                }
            })
            .collect_view()
    });

    let on_keydown = move |event: web_sys::KeyboardEvent| {
        let key = event.key();
        if key == "ContextMenu" || (key == "F10" && event.shift_key()) {
            event.prevent_default();
            let focused = selection.focused();
            let row = event
                .current_target()
                .and_then(|target| target.dyn_into::<web_sys::Element>().ok())
                .and_then(|grid| {
                    grid.query_selector(&format!("[data-index=\"{focused}\"]"))
                        .ok()
                        .flatten()
                });
            if let Some(row) = row {
                file_menu(focused, MenuAnchor::Below(row));
            }
            return;
        }
        if selection.on_keydown(&event, file_count) == Some(ListAction::Activate) && !locked {
            download_selection();
        }
    };

    view! {
        <section class="card card--flush browse-folder" aria-labelledby="browse-folder-title">
            <div class="card__header">
                <div class="browse-folder__heading">
                    {nested.then(|| view! {
                        <nav aria-label="Folder path">
                            <ol class="breadcrumb browse-breadcrumb">{crumbs}</ol>
                        </nav>
                    })}
                    <h2 class="card__title browse-folder__title" id="browse-folder-title">
                        <Icon name=if locked { "lock" } else { "folder-open" } size="lg" class="muted"/>
                        <span class="break-all">{node.label.clone()}</span>
                    </h2>
                    <p class="card__description">{summary}</p>
                </div>
                <div class="card__actions">
                    <div class="btn-joined browse-folder__split">
                        <Button variant="primary" icon="download" text=main_text label=main_label
                            disabled=Signal::derive(move || locked || main_count == 0)
                            loading=pending
                            on_click=Callback::new(move |_| actions.download_folder(index, recursive_main))/>
                        <Button variant="primary" icon="chevron-down" label="More download options"
                            disabled=locked
                            on_click=Callback::new(move |event: MouseEvent| {
                                if let Some(button) = element(&event) {
                                    actions.open_folder_menu(index, MenuAnchor::Below(button));
                                }
                            })/>
                    </div>
                </div>
            </div>
            {locked.then(|| view! {
                <div class="browse-folder__notice">
                    <Alert variant="warning" icon="lock" title="Locked folder">
                        <p>"This user shares this folder only with some users. Its files can't be downloaded."</p>
                    </Alert>
                </div>
            })}
            <div class="card__content">
                {(file_count > 0).then(|| view! {
                    <p class="browse-folder__hint hide-sm">
                        "Click to select, Ctrl/Cmd+click to add, Shift+click for a range; double-click or Enter downloads."
                    </p>
                })}
                <div class="table-wrap">
                    <table class="table table--compact browse-files" role="grid" aria-multiselectable="true"
                        aria-labelledby="browse-folder-title" on:keydown=on_keydown>
                        <thead>
                            <tr>
                                <th>"Name"</th>
                                <th class="num">"Size"</th>
                                <th class="hide-sm">"Attributes"</th>
                                <th class="num hide-sm">"Length"</th>
                                <th class="actions"><span class="sr-only">"Actions"</span></th>
                            </tr>
                        </thead>
                        <tbody>
                            {rows}
                            {(file_count == 0).then(|| view! {
                                <tr class="empty-row"><td colspan="5">
                                    {if has_children { "No files directly in this folder. Open a subfolder, or download everything below it." } else { "This folder is empty." }}
                                </td></tr>
                            })}
                        </tbody>
                    </table>
                </div>
                <Show when=move || { count.get() > 0 }>
                    <div class="selection-bar" role="region" aria-label="Selected files">
                        <span class="selection-bar__text" aria-live="polite">
                            {move || format!("{} · {}", plural(count.get(), "file", "files"), util::format_bytes(size.get() as f64))}
                        </span>
                        <Button variant="primary" size="sm" icon="download" text="Download"
                            disabled=locked loading=pending
                            on_click=Callback::new(move |_| download_selection())/>
                        <Button variant="ghost" size="sm" icon="x" text="Clear"
                            on_click=Callback::new(move |_| selection.clear())/>
                    </div>
                </Show>
            </div>
        </section>
    }
}
