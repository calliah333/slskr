use crate::services::{api, util};
use leptos::prelude::*;
use slskr_api::{
    transfers::{EnqueueDownloadsResponse, QueueDownloadRequest},
    users::File,
};

#[component]
pub fn Selection(
    username: String,
    directory: String,
    separator: String,
    files: Vec<File>,
    locked: bool,
) -> impl IntoView {
    let files = StoredValue::new(files);
    let heading = directory.clone();
    let selected = RwSignal::new(Vec::<usize>::new());
    let pending = RwSignal::new(false);
    let message = RwSignal::new(None::<String>);
    let count = move || selected.with(Vec::len);
    let size = move || {
        selected.with(|indexes| {
            files.with_value(|files| indexes.iter().map(|&index| files[index].size).sum::<u64>())
        })
    };
    let endpoint = format!(
        "/transfers/downloads/{}",
        util::encode_uri_component(&username)
    );
    let download = move |_| {
        let indexes = selected.get_untracked();
        if indexes.is_empty() || locked {
            return;
        }
        pending.set(true);
        message.set(None);
        // Selection displays only direct files; slskd batches are for grouped
        // downloads from descendant folders.
        let requests: Vec<QueueDownloadRequest> = files.with_value(|files| {
            indexes
                .into_iter()
                .map(|index| {
                    let file = &files[index];
                    QueueDownloadRequest {
                        filename: format!("{directory}{separator}{}", file.filename),
                        size: file.size,
                    }
                })
                .collect()
        });
        let endpoint = endpoint.clone();
        leptos::task::spawn_local(async move {
            match api::post::<_, EnqueueDownloadsResponse>(&endpoint, &requests).await {
                Ok(response) if !response.failed.is_empty() => {
                    let _ = message.try_set(Some(format!(
                        "Failed to enqueue {} file(s).",
                        response.failed.len()
                    )));
                }
                Ok(_) => {}
                Err(error) => {
                    let _ = message.try_set(Some(error.to_string()));
                }
            }
            let _ = pending.try_set(false);
        });
    };
    view! {
        <section class="browse-selection">
            <header><strong>{heading}</strong></header>
            <table class="ui compact table">
                <thead><tr><th></th><th>File</th><th>Size</th></tr></thead>
                <tbody>{files.with_value(|files| files.iter().enumerate().map(|(index, file)| {
                    let filename = file.filename.clone();
                    let bytes = file.size;
                    view! { <tr class:filebrowser-locked=locked>
                        <td><input type="checkbox" disabled=locked on:change=move |event| {
                            let checked = event_target_checked(&event);
                            selected.update(|items| if checked { items.push(index) } else { items.retain(|item| *item != index) });
                        }/></td>
                        <td>{filename}</td><td>{util::format_bytes(bytes as f64)}</td>
                    </tr> }
                }).collect_view())}</tbody>
            </table>
            <button type="button" class="ui button" disabled=move || count() == 0 || pending.get() || locked on:click=download>
                {move || if pending.get() { "Downloading…".to_owned() } else { format!("Download {} files ({})", count(), util::format_bytes(size() as f64)) }}
            </button>
            {move || message.get().map(|error| view! { <p role="alert">{error}</p> })}
        </section>
    }
}
