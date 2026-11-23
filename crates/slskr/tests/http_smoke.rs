use std::{
    net::{Ipv4Addr, TcpListener},
    process::Stdio,
    time::Duration,
};

use serde_json::Value;
use tokio::{
    process::{Child, Command},
    time::{sleep, timeout},
};

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
        "[soulseek]\nconnect = false\n\n[shares]\ndirectories = [{share_path}]\n\n[web]\nlisten = \"127.0.0.1:{port}\"\nurl_base = \"/client\"\nusername = \"smoke\"\npassword = \"secret\"\n"
    );
    std::fs::write(data_dir.path().join("slskr.toml"), &config).unwrap();

    let child = Command::new(env!("CARGO_BIN_EXE_slskr"))
        .arg("--data-dir")
        .arg(data_dir.path())
        .env("SLSKR_CONTENT_PATH", &content_dir)
        .env_remove("SLSKR_DATA_DIR")
        .env_remove("SLSKR_CONFIG")
        .kill_on_drop(true)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut daemon = Daemon(child);
    let base = format!("http://127.0.0.1:{port}");
    // The session cookie's path is the url_base, so use the prefixed API.
    let api = format!("{base}/client/api/v0");
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

    let anonymous = client.get(format!("{api}/session")).send().await.unwrap();
    assert_eq!(anonymous.status(), reqwest::StatusCode::UNAUTHORIZED);

    let denied = client
        .post(format!("{api}/session"))
        .json(&serde_json::json!({"username": "smoke", "password": "wrong"}))
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), reqwest::StatusCode::UNAUTHORIZED);

    let login = client
        .post(format!("{api}/session"))
        .json(&serde_json::json!({"username": "smoke", "password": "secret"}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
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
    let login: Value = login.json().await.unwrap();
    assert_eq!(
        login,
        serde_json::json!({"username": "smoke", "role": "Administrator"})
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

    let authorized = client
        .get(format!("{api}/application"))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let state: Value = authorized.json().await.unwrap();
    assert!(state["shares"].get("scanProgress").is_some());
    assert!(state.get("distributedNetwork").is_some());

    let options: Value = client
        .get(format!("{api}/options"))
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

    let file = client
        .get(format!("{api}/options/toml"))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    assert!(
        file.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("application/toml")
    );
    assert_eq!(file.text().await.unwrap(), config);
    let invalid = client
        .put(format!("{api}/options/toml"))
        .header("content-type", "application/toml")
        .body("[web]\nlisten = \"nowhere\"\n")
        .send()
        .await
        .unwrap();
    assert_eq!(invalid.status(), reqwest::StatusCode::BAD_REQUEST);

    // Deleting a missing download is a 404; deletion is confined to the downloads root.
    let deletion = client
        .delete(format!("{api}/files/downloads/files/c2FtcGxlLnR4dA%3D%3D"))
        .send()
        .await
        .unwrap();
    assert_eq!(deletion.status(), reqwest::StatusCode::NOT_FOUND);

    let scan = client.put(format!("{api}/shares")).send().await.unwrap();
    // The startup scan may still be running.
    assert!(
        scan.status().is_success() || scan.status() == reqwest::StatusCode::CONFLICT,
        "share scan returned {}",
        scan.status()
    );
    let contents = timeout(Duration::from_secs(30), async {
        loop {
            let response = client
                .get(format!("{api}/shares/contents"))
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

    // A page on another site cannot use the cookie to change state.
    let forged = client
        .put(format!("{api}/shares"))
        .header("origin", "http://evil.example")
        .send()
        .await
        .unwrap();
    assert_eq!(forged.status(), reqwest::StatusCode::FORBIDDEN);
    let forged = client
        .post(format!("{api}/session"))
        .header("origin", "http://evil.example")
        .json(&serde_json::json!({"username": "smoke", "password": "secret"}))
        .send()
        .await
        .unwrap();
    assert_eq!(forged.status(), reqwest::StatusCode::FORBIDDEN);
    let same_origin = client
        .post(format!("{api}/application/loopback"))
        .header("origin", &base)
        .send()
        .await
        .unwrap();
    assert_eq!(same_origin.status(), reqwest::StatusCode::OK);

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
