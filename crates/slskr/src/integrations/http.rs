//! HTTP-backed webhooks and Pushbullet notifications.

use std::time::Duration;

use reqwest::{
    Client,
    header::{CONTENT_TYPE, HeaderName, HeaderValue},
};
use slskr_api::options::{PushbulletOptions, WebhookOptions};
use tokio::time::sleep;

#[derive(Clone)]
pub struct HttpIntegrations {
    client: Client,
}

impl HttpIntegrations {
    pub fn new() -> anyhow::Result<Self> {
        Ok(Self {
            client: Client::builder().build()?,
        })
    }
    pub async fn webhook(&self, name: &str, options: &WebhookOptions, body: &str) {
        let Some(url) = options.call.url.as_deref() else {
            return;
        };
        let attempts = options.retry.attempts.max(1) as usize;
        let client = if options.call.ignore_certificate_errors {
            match Client::builder().danger_accept_invalid_certs(true).build() {
                Ok(client) => client,
                Err(error) => {
                    tracing::warn!(webhook = name, %error, "failed to configure webhook client");
                    return;
                }
            }
        } else {
            self.client.clone()
        };
        for attempt in 0..attempts {
            let mut request = client
                .post(url)
                .timeout(Duration::from_millis(options.timeout.max(1) as u64))
                .header(CONTENT_TYPE, "application/json");
            for header in &options.call.headers {
                if let (Some(name), Some(value)) = (&header.name, &header.value) {
                    if let (Ok(name), Ok(value)) =
                        (HeaderName::try_from(name), HeaderValue::try_from(value))
                    {
                        request = request.header(name, value);
                    }
                }
            }
            match request.body(body.to_owned()).send().await {
                Ok(response) if response.status().is_success() => return,
                Ok(response) => {
                    tracing::warn!(webhook = name, status = %response.status(), attempt = attempt + 1, "webhook returned failure status")
                }
                Err(error) => {
                    // The configured URL may embed credentials or tokens; never log it.
                    let error = error.without_url();
                    tracing::warn!(webhook = name, %error, attempt = attempt + 1, "webhook request failed")
                }
            }
            if attempt + 1 < attempts {
                sleep(retry_delay(attempt)).await;
            }
        }
    }

    pub async fn pushbullet(
        &self,
        options: &PushbulletOptions,
        token: &str,
        title: String,
        key: String,
        body: String,
    ) {
        if title.trim().is_empty() || key.trim().is_empty() || body.trim().is_empty() {
            return;
        }
        if !recently_sent(&key, options.cooldown_time.max(0) as u64) {
            return;
        }
        let payload = serde_json::json!({"title": format!("{} {title}", options.notification_prefix), "body": body, "type": "note"});
        for attempt in 0..options.retry_attempts.max(1) as usize {
            match self
                .client
                .post("https://api.pushbullet.com/v2/pushes")
                .header("Access-Token", token)
                .json(&payload)
                .send()
                .await
            {
                Ok(response) if response.status().is_success() => return,
                Ok(response) => {
                    tracing::warn!(status = %response.status(), attempt = attempt + 1, "Pushbullet returned failure status")
                }
                Err(error) => {
                    let error = error.without_url();
                    tracing::warn!(%error, attempt = attempt + 1, "Pushbullet request failed")
                }
            }
            if attempt + 1 < options.retry_attempts.max(1) as usize {
                sleep(retry_delay(attempt)).await;
            }
        }
    }
}

fn retry_delay(attempt: usize) -> Duration {
    Duration::from_secs(
        1u64.checked_shl(attempt.min(5) as u32)
            .unwrap_or(30)
            .min(30),
    )
}

fn recently_sent(key: &str, cooldown_ms: u64) -> bool {
    use std::{
        collections::HashMap,
        sync::{LazyLock, Mutex},
        time::Instant,
    };
    static RECENT: LazyLock<Mutex<HashMap<String, Instant>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));
    let mut recent = RECENT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let now = Instant::now();
    let cooldown = Duration::from_millis(cooldown_ms);
    if let Some(sent) = recent.get(key)
        && now.duration_since(*sent) < cooldown
    {
        return false;
    }
    // Keep entries at least as long as the cooldown so long cooldowns are honored.
    let retention = cooldown.max(Duration::from_secs(3600));
    recent.retain(|_, sent| now.duration_since(*sent) < retention);
    recent.insert(key.to_owned(), now);
    true
}
