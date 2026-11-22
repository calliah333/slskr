use leptos::prelude::*;
use slskr_api::{
    application::State,
    telemetry::{CurrentMetrics, DirectionMetrics},
};

use crate::services::util::format_bytes_with;

fn format_speed(speed: f64) -> String {
    if speed < 1.0 || !speed.is_finite() {
        "   0 B/s".to_string()
    } else {
        format!("{}/s", format_bytes_with(speed, 1, 4, ' '))
    }
}

#[component]
pub(super) fn AppFooter(
    state: RwSignal<State>,
    metrics: RwSignal<CurrentMetrics>,
) -> impl IntoView {
    let connected = Memo::new(move |_| state.with(|s| s.server.is_connected));
    let username = Memo::new(move |_| state.with(|s| s.user.username.clone()));
    let version = Memo::new(move |_| state.with(|s| s.version.current.clone()));

    view! {
        <footer class="ui inverted footer menu">
            <div class="item">
                <i class="circle icon" class:green=move || connected.get() class:red=move || !connected.get() aria-hidden="true"></i>
                {move || if connected.get() {
                    username.get().unwrap_or_else(|| "Connected".into())
                } else {
                    "Disconnected".into()
                }}
            </div>
            <TransferStats metrics=metrics direction=|metrics| &metrics.downloads
                label="Downloads" icon="blue arrow down icon" speed_class="footer-download" />
            <TransferStats metrics=metrics direction=|metrics| &metrics.uploads
                label="Uploads" icon="green arrow up icon" speed_class="footer-upload" />
            <div class="right menu">
                <div class="item">
                    <strong>"slskr"</strong>
                    <span class="footer-version">{move || version.get()}</span>
                    <span class="footer-license">"AGPLv3"</span>
                </div>
                <a class="item" href="https://github.com/slskd/slskd" rel="noreferrer" target="_blank">
                    "UI adapted from slskd (AGPLv3)"
                </a>
            </div>
        </footer>
    }
}

#[component]
fn TransferStats(
    metrics: RwSignal<CurrentMetrics>,
    direction: fn(&CurrentMetrics) -> &DirectionMetrics,
    label: &'static str,
    icon: &'static str,
    speed_class: &'static str,
) -> impl IntoView {
    let counts = Memo::new(move |_| {
        metrics.with(|metrics| {
            let metrics = direction(metrics);
            (metrics.in_progress.files, metrics.queued.files)
        })
    });
    let speed =
        Memo::new(move |_| metrics.with(|metrics| direction(metrics).in_progress.average_speed));

    view! {
        <div class="item" aria-label=label>
            <i class=icon aria-hidden="true"></i>
            <span class=speed_class>{move || format_speed(speed.get())}</span>
            <span class="footer-stat-detail">{move || {
                let (active, queued) = counts.get();
                format!("{active} active · {queued} queued")
            }}</span>
        </div>
    }
}
