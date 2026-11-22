//! Gluetun status polling. The connection watchdog consumes readiness from application state.

use std::{net::IpAddr, time::Duration};

use reqwest::{Client, header::HeaderValue, redirect::Policy};
use serde::{Deserialize, de::DeserializeOwned};
use slskr_api::{
    application::{State, VpnState},
    options::{GluetunVpnOptions, VpnOptions},
};
use soulseek::ClientOptionsPatch;
use tokio::sync::watch;

use crate::app::Core;

#[derive(Clone)]
pub(super) struct Gluetun(Client);

impl Gluetun {
    pub(super) fn new() -> reqwest::Result<Self> {
        // Never forward control-server credentials to a redirect destination.
        Ok(Self(Client::builder().redirect(Policy::none()).build()?))
    }

    pub(super) async fn observe(self, core: Core) {
        let mut options = core.options.subscribe();
        let states = core.state.subscribe();
        while !core.shutdown.is_cancelled() {
            let snapshot = options.borrow_and_update().clone();
            let vpn = &snapshot.integrations.vpn;
            let state = if vpn.enabled {
                tokio::select! {
                    biased;
                    _ = core.shutdown.cancelled() => break,
                    change = options.wait_for(|current| current.integrations.vpn != *vpn) => {
                        if change.is_err() { break; }
                        continue;
                    }
                    state = self.poll(&core, vpn) => state,
                }
            } else {
                // Disabling the integration takes effect for polling immediately. The
                // watchdog's startup VPN requirement still applies until restart.
                VpnState::default()
            };
            if options.borrow().integrations.vpn != *vpn {
                continue;
            }
            let required = vpn.enabled || core.options.startup().integrations.vpn.enabled;
            publish(&core, &states, state, required).await;

            tokio::select! {
                biased;
                _ = core.shutdown.cancelled() => break,
                change = options.wait_for(|current| current.integrations.vpn != *vpn) => {
                    if change.is_err() { break; }
                }
                _ = tokio::time::sleep(milliseconds(vpn.polling_interval)), if vpn.enabled => {}
            }
        }
    }

    async fn poll(&self, core: &Core, options: &VpnOptions) -> VpnState {
        let mut state = match self.status(options).await {
            Ok(state) => state,
            Err(reason) => {
                // URLs, auth headers, bodies and reqwest errors may contain secrets.
                tracing::warn!(reason, "Could not fetch Gluetun VPN status");
                return VpnState::default();
            }
        };
        if let Some(port) = state.forwarded_port {
            let port = port as u16; // status only accepts nonzero u16 ports.
            if core.client.options().listen_port != Some(port)
                && core
                    .client
                    .reconfigure(ClientOptionsPatch {
                        listen_port: Some(port),
                        ..Default::default()
                    })
                    .await
                    .is_err()
            {
                tracing::warn!(port, "Could not configure the VPN forwarded listen port");
                state.is_ready = false;
            } else if core.options.get().soulseek.listen_port != i32::from(port) {
                // Keep the effective configuration and the generic reload watcher in sync.
                core.options.patch(|current| {
                    if current.integrations.vpn == *options {
                        current.soulseek.listen_port = i32::from(port);
                    }
                });
            }
        }
        state
    }

    async fn status(&self, options: &VpnOptions) -> Result<VpnState, &'static str> {
        let public: PublicIp = self.get(&options.gluetun, "/v1/publicip/ip").await?;
        let Some(ip) = public.public_ip.as_deref().filter(|ip| !ip.is_empty()) else {
            return Ok(VpnState::default());
        };
        let ip: IpAddr = ip.parse().map_err(|_| "invalid public IP")?;
        let port = if options.port_forwarding {
            let response: ForwardedPort = self.get(&options.gluetun, "/v1/portforward").await?;
            response.port.filter(|port| *port != 0)
        } else {
            None
        };
        Ok(VpnState {
            is_ready: !options.port_forwarding || port.is_some(),
            is_connected: true,
            public_ip_address: Some(ip.to_string()),
            location: Some(format!(
                "{}, {}",
                public.city.as_deref().unwrap_or(""),
                public.country.as_deref().unwrap_or("")
            )),
            forwarded_port: port.map(i32::from),
        })
    }

    async fn get<T: DeserializeOwned>(
        &self,
        options: &GluetunVpnOptions,
        path: &str,
    ) -> Result<T, &'static str> {
        let url = options
            .url
            .as_deref()
            .filter(|url| !url.trim().is_empty())
            .ok_or("missing control server URL")?;
        let mut request = self
            .0
            .get(format!("{}{path}", url.trim_end_matches('/')))
            .timeout(milliseconds(options.timeout));
        if let Some(key) = nonblank(options.api_key.as_deref()) {
            let mut key = HeaderValue::from_str(key).map_err(|_| "invalid API key header")?;
            key.set_sensitive(true);
            request = request.header("X-API-Key", key);
        } else if let Some(username) = nonblank(options.username.as_deref()) {
            request = request.basic_auth(username, options.password.as_deref());
        }
        let response = request
            .send()
            .await
            .map_err(|_| "control server request failed")?;
        if !response.status().is_success() {
            return Err("control server returned an unsuccessful status");
        }
        response
            .json()
            .await
            .map_err(|_| "invalid control server response")
    }
}

async fn publish(core: &Core, states: &watch::Receiver<State>, state: VpnState, required: bool) {
    let ready = state.is_ready;
    let (changed, was_ready) = {
        let current = states.borrow();
        (current.vpn != state, current.vpn.is_ready)
    };
    if changed {
        if was_ready != ready {
            if ready {
                tracing::info!("VPN is connected and ready");
            } else {
                tracing::warn!("VPN is no longer ready");
            }
        }
        core.state.update(|current| current.vpn = state);
    }
    // Keep the watchdog enabled: it handles reconnection when VPN readiness returns.
    if !ready && required {
        core.client
            .disconnect("VPN client is not ready", None)
            .await;
    }
}

fn milliseconds(value: i32) -> Duration {
    Duration::from_millis(value.max(1) as u64)
}

fn nonblank(value: Option<&str>) -> Option<&str> {
    value.filter(|value| !value.trim().is_empty())
}

#[derive(Deserialize)]
struct PublicIp {
    public_ip: Option<String>,
    city: Option<String>,
    country: Option<String>,
}

#[derive(Deserialize)]
struct ForwardedPort {
    port: Option<u16>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Json, Router, http::HeaderMap, routing::get};
    use serde_json::json;
    use std::sync::{
        Arc,
        atomic::{AtomicU16, AtomicUsize, Ordering},
    };

    #[tokio::test]
    async fn gluetun_status_gates_forwarding_and_supports_both_auth_methods() {
        let port = Arc::new(AtomicU16::new(0));
        let requests = Arc::new(AtomicUsize::new(0));
        let router = Router::new()
            .route(
                "/v1/publicip/ip",
                get(|headers: HeaderMap| async move {
                    if let Some(key) = headers.get("x-api-key") {
                        assert_eq!(key, "local-key");
                        assert!(!headers.contains_key("authorization"));
                        Json(
                            json!({"public_ip": "192.0.2.1", "city": "City", "country": "Country"}),
                        )
                    } else {
                        assert_eq!(headers["authorization"], "Basic dXNlcjpwYXNz");
                        Json(json!({"public_ip": ""}))
                    }
                }),
            )
            .route(
                "/v1/portforward",
                get({
                    let (port, requests) = (port.clone(), requests.clone());
                    move |headers: HeaderMap| async move {
                        assert_eq!(headers["x-api-key"], "local-key");
                        requests.fetch_add(1, Ordering::Relaxed);
                        Json(json!({"port": port.load(Ordering::Relaxed)}))
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let client = Gluetun::new().unwrap();
        let mut options = VpnOptions {
            enabled: true,
            gluetun: GluetunVpnOptions {
                url: Some(url),
                username: Some("user".into()),
                password: Some("pass".into()),
                api_key: Some("local-key".into()),
                ..Default::default()
            },
            ..Default::default()
        };
        let state = client.status(&options).await.unwrap();
        assert!(state.is_connected && state.is_ready);
        assert_eq!(state.public_ip_address.as_deref(), Some("192.0.2.1"));
        assert_eq!(state.location.as_deref(), Some("City, Country"));
        assert_eq!(requests.load(Ordering::Relaxed), 0);

        options.port_forwarding = true;
        let state = client.status(&options).await.unwrap();
        assert!(state.is_connected && !state.is_ready);
        assert_eq!(state.forwarded_port, None);
        port.store(12345, Ordering::Relaxed);
        let state = client.status(&options).await.unwrap();
        assert!(state.is_ready);
        assert_eq!(state.forwarded_port, Some(12345));

        options.gluetun.api_key = None;
        assert_eq!(client.status(&options).await.unwrap(), VpnState::default());
        assert_eq!(requests.load(Ordering::Relaxed), 2);
        server.abort();
    }
}
