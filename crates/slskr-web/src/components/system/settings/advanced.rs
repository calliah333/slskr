//! Advanced: answering searches, how long records are kept, logging and metrics.

use leptos::prelude::*;

use super::fields::{Kind, Setting, use_form};
use crate::ui::form::{Advanced, RowHeading, Section};

#[component]
pub fn Search() -> impl IntoView {
    view! {
        <Section id="settings-search" title="Search throttling">
            <Setting key="search.incoming_concurrency" label="Searches answered at once" restart=true
                kind=Kind::at_least_one(Some("searches"))/>
            <Setting key="search.incoming_circuit_breaker" label="Queue limit" kind=Kind::count(Some("searches"))
                help="Searches queued beyond this are dropped."/>
            <Setting key="search.response_file_limit" label="Files per response" kind=Kind::count(Some("files"))/>
            <Setting key="search.ignore_queries" label="Ignore searches" kind=Kind::MonoList("Regular expression, then Enter")
                help="Case-insensitive regular expressions; matching searches are not answered."/>
        </Section>
    }
}

fn retention_row(key: &'static str, label: &'static str) -> impl IntoView {
    view! { <Setting key=format!("retention.{key}") label=label kind=Kind::OptionalDuration/> }
}

#[component]
pub fn Retention() -> impl IntoView {
    let form = use_form();
    let failed_open = Signal::derive(move || {
        [
            "downloads_errored",
            "downloads_cancelled",
            "downloads_failed",
            "uploads_errored",
            "uploads_cancelled",
            "uploads_failed",
        ]
        .iter()
        .any(|key| form.error(&format!("retention.{key}")).is_some())
    });
    view! {
        <Section id="settings-retention" title="Retention">
            {retention_row("searches", "Searches")}
            {retention_row("downloads_succeeded", "Completed downloads")}
            {retention_row("uploads_succeeded", "Completed uploads")}
            {retention_row("logs", "Log files")}
            <Advanced label="Unsuccessful transfers" open=failed_open>
                <RowHeading title="Downloads"/>
                {retention_row("downloads_errored", "Errored")}
                {retention_row("downloads_cancelled", "Cancelled")}
                {retention_row("downloads_failed", "Failed")}
                <RowHeading title="Uploads"/>
                {retention_row("uploads_errored", "Errored")}
                {retention_row("uploads_cancelled", "Cancelled")}
                {retention_row("uploads_failed", "Failed")}
            </Advanced>
        </Section>
    }
}

#[component]
pub fn Logging() -> impl IntoView {
    let form = use_form();
    view! {
        <Section id="settings-logging" title="Logging and metrics">
            <Setting key="logging.level" label="Log level" restart=true
                kind=Kind::Select(vec![("trace", "Trace"), ("debug", "Debug"), ("info", "Info"), ("warn", "Warn"), ("error", "Error")])
                help="RUST_LOG takes precedence."/>
            <Setting key="logging.file" label="Write log files" kind=Kind::Toggle restart=true/>
            <Setting key="metrics.enabled" label="Prometheus metrics" kind=Kind::Toggle restart=true
                help="Serve metrics for Prometheus to scrape."/>
            <Advanced label="Metrics endpoint" open=Signal::derive(move || form.has_errors_below("metrics"))>
                <Setting key="metrics.path" label="Path" kind=Kind::Mono restart=true placeholder="/metrics"/>
                <Setting key="metrics.username" label="Username" kind=Kind::Text restart=true
                    help="Basic auth; set both or neither."/>
                <Setting key="metrics.password" label="Password" kind=Kind::Secret { confirm: false } restart=true/>
            </Advanced>
        </Section>
    }
}
