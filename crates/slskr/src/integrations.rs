//! Event-driven integrations configured through `Options::integrations`.

pub mod ftp;
pub mod http;
mod vpn;

use std::{process::Stdio, sync::Arc};

use crate::{
    app::{AppState, Core},
    events::EventService,
};
use slskr_api::{
    events::{Event, EventType},
    options::Options,
};
use tokio::process::Command;

#[derive(Clone)]
pub struct IntegrationService {
    core: Core,
    events: EventService,
    http: http::HttpIntegrations,
    vpn: vpn::Gluetun,
}

impl IntegrationService {
    pub fn new(core: &Core, events: &EventService) -> anyhow::Result<Self> {
        Ok(Self {
            core: core.clone(),
            events: events.clone(),
            http: http::HttpIntegrations::new()?,
            vpn: vpn::Gluetun::new()?,
        })
    }

    pub fn start(&self, _app: AppState) {
        tokio::spawn(self.vpn.clone().observe(self.core.clone()));
        let mut events = self.events.subscribe();
        let service = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = service.core.shutdown.cancelled() => break,
                    result = events.recv() => match result {
                        Ok(event) => service.handle_event(event),
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(count)) => {
                            tracing::warn!(count, "integration event receiver lagged; events dropped");
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
            }
        });
    }

    fn handle_event(&self, event: Event) {
        let options = self.core.options.get();
        let kind = event.kind();
        let data = match serde_json::to_string(&event) {
            Ok(data) => data,
            Err(error) => {
                tracing::warn!(%error, "failed to serialize integration event");
                return;
            }
        };
        for (name, webhook) in &options.integrations.webhooks {
            if configured_for(&webhook.on, kind) {
                let client = self.http.clone();
                let event_data = data.clone();
                let webhook = webhook.clone();
                let name = name.clone();
                tokio::spawn(async move {
                    client.webhook(&name, &webhook, &event_data).await;
                });
            }
        }
        for (name, script) in &options.integrations.scripts {
            if configured_for(&script.on, kind) {
                let script = script.clone();
                let name = name.clone();
                let event_data = data.clone();
                tokio::spawn(async move {
                    run_script(&name, &script, &event_data).await;
                });
            }
        }
        match &event {
            Event::DownloadFileComplete(e) if options.integrations.ftp.enabled => {
                let options = options.clone();
                let path = e.local_filename.clone();
                tokio::spawn(async move {
                    if let Err(error) = ftp::upload(
                        &options.integrations.ftp,
                        &options.directories.downloads,
                        &path,
                    )
                    .await
                    {
                        tracing::warn!(error = %format_args!("{error:#}"), "FTP integration upload failed");
                    }
                });
            }
            Event::PrivateMessageReceived(e)
                if options.integrations.pushbullet.enabled
                    && options.integrations.pushbullet.notify_on_private_message =>
            {
                let message = &e.message;
                if message.get("wasReplayed").and_then(|v| v.as_bool()) == Some(true) {
                    return;
                }
                let title = format!(
                    "Private Message from {}",
                    message
                        .get("username")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown")
                );
                let key = message
                    .get("username")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_owned();
                let body = message
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_owned();
                self.pushbullet(&options, title, key, body);
            }
            Event::RoomMessageReceived(e)
                if options.integrations.pushbullet.enabled
                    && options.integrations.pushbullet.notify_on_room_mention =>
            {
                let message = &e.message;
                let body = message
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let username = message
                    .get("username")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                let room = message
                    .get("roomName")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if self
                    .core
                    .client
                    .username()
                    .is_some_and(|name| !name.is_empty() && body.contains(name.as_str()))
                {
                    self.pushbullet(
                        &options,
                        format!("Room Mention by {username} in {room}"),
                        room.to_owned(),
                        body.to_owned(),
                    );
                }
            }
            _ => {}
        }
    }

    fn pushbullet(&self, options: &Arc<Options>, title: String, key: String, body: String) {
        let config = options.integrations.pushbullet.clone();
        if let Some(token) = config.access_token.clone() {
            let client = self.http.clone();
            tokio::spawn(async move {
                client.pushbullet(&config, &token, title, key, body).await;
            });
        }
    }
}

fn configured_for(on: &[String], kind: EventType) -> bool {
    on.iter()
        .any(|name| name.eq_ignore_ascii_case("Any") || name.eq_ignore_ascii_case(kind.name()))
}

async fn run_script(name: &str, script: &slskr_api::options::ScriptOptions, event_data: &str) {
    let run = &script.run;
    // The configured executable is always invoked directly; event data is never parsed as a command.
    let executable = run.executable.as_deref().or(run.command.as_deref());
    let Some(executable) = executable.filter(|value| !value.trim().is_empty()) else {
        tracing::warn!(script = name, "script has no configured executable");
        return;
    };
    let mut command = Command::new(executable);
    if let Some(args) = &run.arglist {
        command.args(args);
    } else if let Some(args) = &run.args {
        command.arg(args);
    }
    command
        .env("SLSKD_SCRIPT_DATA", event_data)
        .stdin(Stdio::null());
    match command.output().await {
        Ok(output) if output.status.success() => {
            tracing::debug!(script = name, "integration script completed")
        }
        Ok(output) => {
            tracing::warn!(script = name, status = ?output.status, stderr = %String::from_utf8_lossy(&output.stderr), "integration script failed")
        }
        Err(error) => tracing::warn!(script = name, %error, "failed to launch integration script"),
    }
}
