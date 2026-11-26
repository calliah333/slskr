//! Downloads: where files go, and how fast and how often slskr tries.

use leptos::prelude::*;

use super::fields::{Kind, Setting, use_form};
use crate::ui::form::{Advanced, Section};

#[component]
pub fn DownloadFolders() -> impl IntoView {
    let form = use_form();
    view! {
        <Section id="settings-downloads" title="Download folders">
            <Setting key="downloads.directory" label="Completed downloads" kind=Kind::Mono
                help="Relative paths are inside the data directory."/>
            <Setting key="downloads.incomplete_directory" label="Incomplete downloads" kind=Kind::Mono/>
            <Setting key="downloads.on_conflict" label="When the file exists"
                kind=Kind::Select(vec![("rename", "Rename the new file"), ("overwrite", "Overwrite it")])/>
            <Advanced open=Signal::derive(move || {
                form.error("downloads.subdirectory").is_some() || form.error("downloads.file_mode").is_some()
            })>
                <Setting key="downloads.subdirectory" label="Subfolder per download" kind=Kind::Mono
                    placeholder="${SOURCE_DIRECTORY}"
                    help="Tokens: ${SOURCE_DIRECTORY}, ${SOURCE_PATH}, ${SOURCE_USERNAME}, ${BATCH_ID}, ${SEARCH_TEXT}…; empty for none."/>
                <Setting key="downloads.file_mode" label="File mode" kind=Kind::Mono placeholder="0644"
                    help="Octal (0644) or symbolic (rw-r--r--); empty keeps the default."/>
            </Advanced>
        </Section>
    }
}

#[component]
pub fn DownloadLimits() -> impl IntoView {
    view! {
        <Section id="settings-download-limits" title="Speed and retries">
            <Setting key="downloads.slots" label="Concurrent downloads" restart=true
                kind=Kind::unlimited(Some("files"), 10)/>
            <Setting key="downloads.speed_limit" label="Speed limit" kind=Kind::unlimited(Some("KiB/s"), 1024)
                help="For all downloads together."/>
            <Setting key="downloads.resume" label="Resume partial files" kind=Kind::Toggle/>
            <Setting key="downloads.retry_attempts" label="Attempts per file" kind=Kind::count(Some("attempts"))/>
            <Setting key="downloads.retry_delay" label="First retry after" kind=Kind::Duration
                help="The wait doubles after each failure, up to the longest delay."/>
            <Setting key="downloads.retry_max_delay" label="Longest retry delay" kind=Kind::Duration/>
        </Section>
    }
}
