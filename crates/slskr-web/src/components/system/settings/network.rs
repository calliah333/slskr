//! Network: how peers reach slskr, the proxy, the distributed network and the web server.

use leptos::prelude::*;

use super::fields::{Kind, Setting, use_form};
use crate::ui::form::Section;

#[component]
pub fn Connection() -> impl IntoView {
    let form = use_form();
    let proxy_secret = move || form.secret_set("soulseek.proxy");
    view! {
        <Section id="settings-connection" title="Incoming connections">
            <Setting key="soulseek.listen_address" label="Listen address" kind=Kind::Mono
                help="0.0.0.0 listens on every network interface."/>
            <Setting key="soulseek.listen_port" label="Listen port"
                kind=Kind::Number { min: 1024, max: 65535, unit: None, zero: None, fallback: 2234 }
                help="1024–65535. Forward it in your router so peers can reach you."/>
            {move || if proxy_secret() {
                view! {
                    <Setting key="soulseek.proxy" label="SOCKS5 proxy" kind=Kind::Secret { confirm: false }
                        help="Contains credentials, so it is not shown. socks5://[user:pass@]host:port"/>
                }.into_any()
            } else {
                view! {
                    <Setting key="soulseek.proxy" label="SOCKS5 proxy" kind=Kind::Mono
                        placeholder="socks5://host:port" help="Empty connects directly."/>
                }.into_any()
            }}
        </Section>
    }
}

#[component]
pub fn Distributed() -> impl IntoView {
    view! {
        <Section id="settings-distributed" title="Distributed network">
            <Setting key="soulseek.distributed_network" label="Take part" kind=Kind::Toggle
                help="Receive searches from a parent and answer them."/>
            <Setting key="soulseek.distributed_children" label="Children" kind=Kind::count(Some("users"))
                help="Users slskr passes searches on to; 0 accepts none."/>
        </Section>
    }
}

#[component]
pub fn WebServer() -> impl IntoView {
    view! {
        <Section id="settings-web-server" title="Web server">
            <Setting key="web.listen" label="Listen address" kind=Kind::Mono restart=true
                help="address:port of the web UI and API."/>
            <Setting key="web.url_base" label="URL base" kind=Kind::Mono restart=true placeholder="/"
                help="Path prefix behind a reverse proxy, e.g. /slskr."/>
            <Setting key="web.log_requests" label="Log requests" kind=Kind::Toggle restart=true
                help="Write every HTTP request to the log."/>
        </Section>
    }
}
