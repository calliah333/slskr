use std::{
    net::{Ipv4Addr, TcpListener},
    process::Stdio,
    time::Duration,
};

use futures::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::{Child, Command},
    time::{sleep, timeout},
};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{Error as WsError, Message as WsMessage, client::IntoClientRequest},
};

/// The web password chosen during first-run setup.
const PASSWORD: &str = "smoke-secret";

struct Daemon(Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.start_kill();
    }
}

#[tokio::test]
async fn daemon_http_smoke() {
    let data_dir = tempfile::tempdir().unwrap();
    let shares_dir = data_dir.path().join("shared");
    let content_dir = data_dir.path().join("www");
    std::fs::create_dir_all(&shares_dir).unwrap();
    std::fs::create_dir_all(content_dir.join("assets")).unwrap();
    std::fs::write(shares_dir.join("sample.txt"), "shared sample").unwrap();
    std::fs::write(content_dir.join("index.html"), "smoke index").unwrap();
    std::fs::write(content_dir.join("assets/smoke.txt"), "static asset").unwrap();

    // Reserve an ephemeral loopback port, then let the daemon own it.
    let port = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let share_path = serde_json::to_string(&shares_dir.to_string_lossy()).unwrap();
    // JSON string escaping is valid TOML basic-string escaping.
    let config = format!(
        "[soulseek]\nconnect = false\n\n[shares]\ndirectories = [{share_path}]\n\n[web]\nlisten = \"127.0.0.1:{port}\"\nurl_base = \"/client\"\nusername = \"smoke\"\n"
    );
    std::fs::write(data_dir.path().join("slskr.toml"), &config).unwrap();

    let child = Command::new(env!("CARGO_BIN_EXE_slskr"))
        .arg("--data-dir")
        .arg(data_dir.path())
        .arg("--web-dir")
        .arg(&content_dir)
        .env_remove("SLSKR_DATA_DIR")
        .env_remove("SLSKR_CONFIG")
        .kill_on_drop(true)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut daemon = Daemon(child);
    let mut stderr = BufReader::new(daemon.0.stderr.take().unwrap()).lines();
    let base = format!("http://127.0.0.1:{port}");
    // The session cookie's path is the url_base, so use the prefixed API.
    let api = format!("{base}/client/api");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .cookie_store(true)
        .build()
        .unwrap();

    timeout(Duration::from_secs(30), async {
        loop {
            if daemon.0.try_wait().unwrap().is_some() {
                panic!("slskr exited before the HTTP listener became ready");
            }
            if client
                .get(format!("{api}/application"))
                .send()
                .await
                .is_ok()
            {
                break;
            }
            sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("slskr HTTP listener did not become ready");

    // First run: no web password anywhere, so the API waits for setup.
    let gated = client
        .get(format!("{api}/application"))
        .send()
        .await
        .unwrap();
    assert_eq!(gated.status(), reqwest::StatusCode::SERVICE_UNAVAILABLE);
    let gated: Value = gated.json().await.unwrap();
    assert_eq!(gated, serde_json::json!({"error": "setup required"}));
    let auth: Value = client
        .get(format!("{api}/auth"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        auth,
        serde_json::json!({"enabled": true, "setup_required": true})
    );
    // The setup code and URL are printed on stderr.
    let (code, url) = timeout(Duration::from_secs(10), async {
        let (mut code, mut url) = (None, None);
        while code.is_none() || url.is_none() {
            let line = stderr.next_line().await.unwrap().expect("setup banner");
            if let Some(rest) = line.split("Setup code").nth(1) {
                code = rest.rsplit(' ').next().map(str::to_owned);
            }
            if let Some(rest) = line.split("Open ").nth(1) {
                url = rest.split(' ').next().map(str::to_owned);
            }
        }
        (code.unwrap(), url.unwrap())
    })
    .await
    .expect("the setup banner was printed");
    assert_eq!(url, format!("{base}/client/setup"));
    // Keep draining stderr so the daemon never blocks on it.
    tokio::spawn(async move { while let Ok(Some(_)) = stderr.next_line().await {} });

    // Through a (pretend) reverse proxy the caller is not local and needs the code.
    let setup_status = |proxied: bool| {
        let mut request = client.get(format!("{api}/setup"));
        if proxied {
            request = request.header("x-forwarded-for", "192.0.2.7");
        }
        request.send()
    };
    let status: Value = setup_status(true).await.unwrap().json().await.unwrap();
    assert_eq!(status["required"], true);
    assert_eq!(status["code_required"], true);
    assert_eq!(status["web_username_pinned"], true);
    let status: Value = setup_status(false).await.unwrap().json().await.unwrap();
    assert_eq!(status["code_required"], false);
    let setup = |code: &str| {
        client
            .post(format!("{api}/setup"))
            .header("x-forwarded-for", "192.0.2.7")
            .json(&serde_json::json!({
                "code": code,
                "web": {"username": "smoke", "password": PASSWORD},
            }))
            .send()
    };
    let wrong = setup("AAAA-AAAA-AAAA").await.unwrap();
    assert_eq!(wrong.status(), reqwest::StatusCode::FORBIDDEN);
    let done = setup(&code).await.unwrap();
    assert_eq!(done.status(), reqwest::StatusCode::CREATED);
    assert!(
        done.headers()["set-cookie"]
            .to_str()
            .unwrap()
            .starts_with("slskr_session=")
    );
    // The setup cookie signs in; setup is over.
    let session = client.get(format!("{api}/session")).send().await.unwrap();
    assert_eq!(session.status(), reqwest::StatusCode::OK);
    assert_eq!(
        setup(&code).await.unwrap().status(),
        reqwest::StatusCode::CONFLICT
    );
    let store = std::fs::read_to_string(data_dir.path().join("settings.json")).unwrap();
    assert!(store.contains("$argon2id$") && !store.contains(PASSWORD));
    client
        .delete(format!("{api}/session"))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();

    let anonymous = client.get(format!("{api}/session")).send().await.unwrap();
    assert_eq!(anonymous.status(), reqwest::StatusCode::UNAUTHORIZED);

    let denied = client
        .post(format!("{api}/session"))
        .json(&serde_json::json!({"username": "smoke", "password": "wrong"}))
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), reqwest::StatusCode::UNAUTHORIZED);
    let denied: Value = denied.json().await.unwrap();
    assert_eq!(denied, serde_json::json!({"error": "Unauthorized"}));

    let login = client
        .post(format!("{api}/session"))
        .json(&serde_json::json!({"username": "smoke", "password": PASSWORD}))
        .send()
        .await
        .unwrap();
    assert_eq!(login.status(), reqwest::StatusCode::CREATED);
    let cookie = login.headers()["set-cookie"].to_str().unwrap().to_owned();
    assert!(cookie.starts_with("slskr_session="), "{cookie}");
    for attribute in [
        "HttpOnly",
        "SameSite=Strict",
        "Path=/client",
        "Max-Age=2592000",
    ] {
        assert!(cookie.split("; ").any(|a| a == attribute), "{cookie}");
    }
    assert!(!cookie.contains("Secure"), "{cookie}");

    // JSON responses are compressed when the client asks for it.
    for (accept, expected) in [
        ("br", Some("br")),
        ("gzip", Some("gzip")),
        ("identity", None),
    ] {
        let response = client
            .get(format!("{api}/application"))
            .header("accept-encoding", accept)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        let encoding = response.headers().get("content-encoding");
        assert_eq!(encoding.map(|v| v.to_str().unwrap()), expected, "{accept}");
    }
    let login: Value = login.json().await.unwrap();
    assert_eq!(
        login,
        serde_json::json!({"username": "smoke", "role": "admin"})
    );
    let session: Value = client
        .get(format!("{api}/session"))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(session, login);

    // Live events: the socket under url_base takes the session cookie and sends the
    // state snapshot on subscribe; a foreign Origin and a missing cookie are refused.
    let session_cookie = cookie.split(';').next().unwrap();
    let events_url = format!("ws://127.0.0.1:{port}/client/api/events");
    let events_request = |headers: &[(&str, &str)]| {
        let mut request = events_url.as_str().into_client_request().unwrap();
        for (name, value) in headers {
            request.headers_mut().insert(
                reqwest::header::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                value.parse().unwrap(),
            );
        }
        request
    };
    for (headers, status) in [
        (vec![], 401),
        (
            vec![
                ("cookie", session_cookie),
                ("origin", "http://evil.example"),
            ],
            403,
        ),
    ] {
        match connect_async(events_request(&headers)).await {
            Err(WsError::Http(response)) => assert_eq!(response.status().as_u16(), status),
            other => panic!("expected HTTP {status}, got {:?}", other.map(|_| ())),
        }
    }
    let (mut events, _) = timeout(
        Duration::from_secs(5),
        connect_async(events_request(&[
            ("cookie", session_cookie),
            ("origin", base.as_str()),
        ])),
    )
    .await
    .unwrap()
    .unwrap();
    events
        .send(WsMessage::Text(
            r#"{"type":"subscribe","topics":["state"]}"#.into(),
        ))
        .await
        .unwrap();
    let snapshot = loop {
        match timeout(Duration::from_secs(5), events.next())
            .await
            .unwrap()
        {
            Some(Ok(WsMessage::Text(text))) => break serde_json::from_str::<Value>(&text).unwrap(),
            Some(Ok(WsMessage::Ping(_))) => continue,
            other => panic!("expected the state snapshot, got {other:?}"),
        }
    };
    assert_eq!(snapshot["type"], "state");
    assert!(snapshot["data"]["shares"].get("scan_progress").is_some());
    events.close(None).await.unwrap();

    let authorized = client
        .get(format!("{api}/application"))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let state: Value = authorized.json().await.unwrap();
    assert!(state["shares"].get("scan_progress").is_some());
    assert!(state.get("distributed_network").is_some());
    assert_eq!(state["server"]["state"], "disconnected");

    let options: Value = client
        .get(format!("{api}/config"))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(options["web"]["url_base"], "/client");
    assert_eq!(options["web"]["username"], "smoke");
    assert_eq!(options["web"]["password"], "*****");
    assert!(options["shares"].get("directories").is_some());

    // Settings: keys from slskr.toml are pinned, secrets are never returned.
    let settings: Value = client
        .get(format!("{api}/settings"))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let pinned: Vec<&str> = settings["pinned"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    for key in [
        "soulseek.connect",
        "shares.directories",
        "web.listen",
        "web.url_base",
        "web.username",
    ] {
        assert!(pinned.contains(&key), "{key} not in {pinned:?}");
    }
    assert!(!pinned.contains(&"uploads.slots"));
    // Set during first-run setup: stored as a hash, not pinned.
    assert!(!pinned.contains(&"web.password"));
    assert_eq!(settings["secrets_set"], serde_json::json!(["web.password"]));
    assert!(settings["values"]["web"].get("password").is_none());
    assert_eq!(settings["values"]["web"]["username"], "smoke");
    assert!(!settings.to_string().contains(PASSWORD), "{settings}");
    assert_eq!(settings["pending_restart"], false);

    let patch = |body: Value| client.patch(format!("{api}/settings")).json(&body).send();
    let changed: Value = patch(serde_json::json!({"uploads": {"slots": 3}}))
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(changed["values"]["uploads"]["slots"], 3);
    assert_eq!(changed["pending_restart"], true);
    let store: Value = serde_json::from_str(
        &std::fs::read_to_string(data_dir.path().join("settings.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        store["settings"],
        serde_json::json!({"uploads": {"slots": 3}})
    );
    let pinned = patch(serde_json::json!({"web": {"listen": "127.0.0.1:1"}}))
        .await
        .unwrap();
    assert_eq!(pinned.status(), reqwest::StatusCode::CONFLICT);
    let pinned: Value = pinned.json().await.unwrap();
    assert!(pinned["error"].as_str().unwrap().contains("web.listen"));
    let invalid = patch(serde_json::json!({"uploads": {"slots": 0}}))
        .await
        .unwrap();
    assert_eq!(invalid.status(), reqwest::StatusCode::BAD_REQUEST);
    let invalid: Value = invalid.json().await.unwrap();
    assert!(invalid["error"].as_str().unwrap().contains("uploads.slots"));
    let reset = client
        .delete(format!("{api}/settings/uploads.slots"))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let reset: Value = reset.json().await.unwrap();
    assert_eq!(reset["values"]["uploads"]["slots"], 20);
    assert_eq!(reset["pending_restart"], false);
    // Removed endpoints and the old versioned prefix answer JSON 404s.
    for path in ["/options", "/v0/application", "/application/loopback"] {
        let gone = client.get(format!("{api}{path}")).send().await.unwrap();
        assert_eq!(gone.status(), reqwest::StatusCode::NOT_FOUND, "{path}");
        let gone: Value = gone.json().await.unwrap();
        assert!(gone["error"].is_string(), "{path}");
    }

    // API keys: created once, used with X-API-Key, refused after deletion.
    let created = client
        .post(format!("{api}/api-keys"))
        .json(&serde_json::json!({"name": "smoke-key", "role": "readwrite"}))
        .send()
        .await
        .unwrap();
    assert_eq!(created.status(), reqwest::StatusCode::CREATED);
    let created: Value = created.json().await.unwrap();
    let key = created["key"].as_str().unwrap().to_owned();
    let keyed = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
        .unwrap();
    let with_key = |path: &str| {
        keyed
            .get(format!("{api}{path}"))
            .header("X-API-Key", &key)
            .send()
    };
    assert_eq!(
        with_key("/application").await.unwrap().status(),
        reqwest::StatusCode::OK
    );
    // Settings need an interactive sign-in.
    assert_eq!(
        with_key("/settings").await.unwrap().status(),
        reqwest::StatusCode::UNAUTHORIZED
    );
    let listed: Value = client
        .get(format!("{api}/settings"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        listed["api_keys"],
        serde_json::json!([{"name": "smoke-key", "role": "readwrite", "allow": ["0.0.0.0/0", "::/0"], "pinned": false}])
    );
    assert!(!listed.to_string().contains(&key));
    let deleted = client
        .delete(format!("{api}/api-keys/smoke-key"))
        .send()
        .await
        .unwrap();
    assert_eq!(deleted.status(), reqwest::StatusCode::NO_CONTENT);
    assert_eq!(
        with_key("/application").await.unwrap().status(),
        reqwest::StatusCode::UNAUTHORIZED
    );

    // Deleting a missing download is a 404; deletion is confined to the downloads root.
    let deletion = client
        .delete(format!("{api}/files/downloads?path=sample.txt"))
        .send()
        .await
        .unwrap();
    assert_eq!(deletion.status(), reqwest::StatusCode::NOT_FOUND);
    for path in ["..%2Fslskr.toml", "", "%2E%2E"] {
        let refused = client
            .delete(format!("{api}/files/downloads?path={path}"))
            .send()
            .await
            .unwrap();
        assert!(
            matches!(refused.status().as_u16(), 400 | 403),
            "{path}: {}",
            refused.status()
        );
    }
    let listing: Value = client
        .get(format!("{api}/files/downloads"))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(listing["path"], "");
    assert!(listing["files"].is_array());

    // Requests the daemon cannot serve while disconnected, and malformed bodies.
    let search = client
        .post(format!("{api}/searches"))
        .json(&serde_json::json!({"query": "smoke test"}))
        .send()
        .await
        .unwrap();
    assert_eq!(search.status(), reqwest::StatusCode::CONFLICT);
    let search: Value = search.json().await.unwrap();
    assert!(search["error"].as_str().unwrap().contains("logged in"));
    // Server-dependent reads and sends answer 409 too, not 500.
    for (method, path) in [
        (reqwest::Method::GET, "users/someone/info"),
        (reqwest::Method::GET, "users/someone/browse"),
        (reqwest::Method::GET, "rooms"),
    ] {
        let response = client
            .request(method, format!("{api}/{path}"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::CONFLICT, "{path}");
    }
    let message = client
        .post(format!("{api}/conversations/someone/messages"))
        .json(&serde_json::json!({"message": "hi"}))
        .send()
        .await
        .unwrap();
    assert_eq!(message.status(), reqwest::StatusCode::CONFLICT);
    let connect = client
        .post(format!("{api}/server/connect"))
        .send()
        .await
        .unwrap();
    assert_eq!(connect.status(), reqwest::StatusCode::CONFLICT);
    let connect: Value = connect.json().await.unwrap();
    assert!(connect["error"].as_str().unwrap().contains("credentials"));
    let malformed = client
        .post(format!("{api}/searches"))
        .header("content-type", "application/json")
        .body("{not json")
        .send()
        .await
        .unwrap();
    assert_eq!(malformed.status(), reqwest::StatusCode::BAD_REQUEST);
    let malformed: Value = malformed.json().await.unwrap();
    assert!(malformed["error"].is_string());
    let wrong_shape = client
        .post(format!("{api}/searches"))
        .json(&serde_json::json!({"query": 5}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        wrong_shape.status(),
        reqwest::StatusCode::UNPROCESSABLE_ENTITY
    );
    let not_allowed = client.put(format!("{api}/searches")).send().await.unwrap();
    assert_eq!(
        not_allowed.status(),
        reqwest::StatusCode::METHOD_NOT_ALLOWED
    );
    let not_allowed: Value = not_allowed.json().await.unwrap();
    assert!(not_allowed["error"].is_string());
    let unknown_transfer = client
        .get(format!(
            "{api}/transfers/downloads/00000000-0000-0000-0000-000000000000"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(unknown_transfer.status(), reqwest::StatusCode::NOT_FOUND);
    let downloads: Value = client
        .get(format!("{api}/transfers/downloads"))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(downloads, serde_json::json!([]));

    let scan = client
        .post(format!("{api}/shares/scan"))
        .send()
        .await
        .unwrap();
    // The startup scan may still be running.
    assert!(
        scan.status().is_success() || scan.status() == reqwest::StatusCode::CONFLICT,
        "share scan returned {}",
        scan.status()
    );
    let contents = timeout(Duration::from_secs(30), async {
        loop {
            let shares: Value = client
                .get(format!("{api}/shares"))
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            let id = shares[0]["id"].as_str().unwrap();
            let response = client
                .get(format!("{api}/shares/{id}/contents"))
                .send()
                .await
                .unwrap();
            if response.status().is_success() {
                let body: Value = response.json().await.unwrap();
                if body.to_string().contains("sample.txt") {
                    break body;
                }
            }
            sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("share scan did not expose the local sample through browse");
    assert!(contents.to_string().contains("sample.txt"));

    let asset = client
        .get(format!("{base}/client/assets/smoke.txt"))
        .send()
        .await
        .unwrap();
    assert_eq!(asset.status(), reqwest::StatusCode::OK);
    assert_eq!(asset.text().await.unwrap(), "static asset");
    for root in ["/client", "/client/"] {
        let index = client.get(format!("{base}{root}")).send().await.unwrap();
        assert_eq!(index.status(), reqwest::StatusCode::OK, "{root}");
        assert_eq!(index.text().await.unwrap(), "smoke index");
    }
    let deep_link = client
        .get(format!("{base}/client/library/local"))
        .send()
        .await
        .unwrap();
    assert_eq!(deep_link.status(), reqwest::StatusCode::OK);
    assert_eq!(deep_link.text().await.unwrap(), "smoke index");

    let missing = client
        .get(format!("{api}/not-a-route"))
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), reqwest::StatusCode::NOT_FOUND);
    let missing: Value = missing.json().await.unwrap();
    assert_eq!(missing, serde_json::json!({"error": "Not Found"}));

    // A page on another site cannot use the cookie to change state.
    let forged = client
        .post(format!("{api}/shares/scan"))
        .header("origin", "http://evil.example")
        .send()
        .await
        .unwrap();
    assert_eq!(forged.status(), reqwest::StatusCode::FORBIDDEN);
    let forged = client
        .post(format!("{api}/session"))
        .header("origin", "http://evil.example")
        .json(&serde_json::json!({"username": "smoke", "password": PASSWORD}))
        .send()
        .await
        .unwrap();
    assert_eq!(forged.status(), reqwest::StatusCode::FORBIDDEN);
    let same_origin = client
        .post(format!("{api}/server/disconnect"))
        .header("origin", &base)
        .send()
        .await
        .unwrap();
    assert_eq!(same_origin.status(), reqwest::StatusCode::NO_CONTENT);

    let logout = client
        .delete(format!("{api}/session"))
        .send()
        .await
        .unwrap();
    assert_eq!(logout.status(), reqwest::StatusCode::NO_CONTENT);
    let cleared = logout.headers()["set-cookie"].to_str().unwrap();
    assert!(cleared.starts_with("slskr_session=;"), "{cleared}");
    assert!(cleared.contains("Max-Age=0"), "{cleared}");
    for path in ["/session", "/application"] {
        let response = client.get(format!("{api}{path}")).send().await.unwrap();
        assert_eq!(
            response.status(),
            reqwest::StatusCode::UNAUTHORIZED,
            "{path}"
        );
    }

    daemon.0.start_kill().unwrap();
    daemon.0.wait().await.unwrap();
}
