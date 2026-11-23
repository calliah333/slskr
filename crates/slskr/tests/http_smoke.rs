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
    let api = format!("{base}/api/v0");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
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

    let denied = client
        .post(format!("{api}/session"))
        .json(&serde_json::json!({"username": "smoke", "password": "wrong"}))
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), reqwest::StatusCode::UNAUTHORIZED);

    let login: Value = client
        .post(format!("{api}/session"))
        .json(&serde_json::json!({"username": "smoke", "password": "secret"}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(login["tokenType"], "Bearer");
    let token = login["token"].as_str().expect("login response token");
    let authorized = client
        .get(format!("{api}/application"))
        .bearer_auth(token)
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
        .bearer_auth(token)
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
        .bearer_auth(token)
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
        .bearer_auth(token)
        .header("content-type", "application/toml")
        .body("[web]\nlisten = \"nowhere\"\n")
        .send()
        .await
        .unwrap();
    assert_eq!(invalid.status(), reqwest::StatusCode::BAD_REQUEST);

    // Deleting a missing download is a 404; deletion is confined to the downloads root.
    let deletion = client
        .delete(format!("{api}/files/downloads/files/c2FtcGxlLnR4dA%3D%3D"))
        .bearer_auth(token)
        .send()
        .await
        .unwrap();
    assert_eq!(deletion.status(), reqwest::StatusCode::NOT_FOUND);

    let scan = client
        .put(format!("{api}/shares"))
        .bearer_auth(token)
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
            let response = client
                .get(format!("{api}/shares/contents"))
                .bearer_auth(token)
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
        .bearer_auth(token)
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), reqwest::StatusCode::NOT_FOUND);

    daemon.0.start_kill().unwrap();
    daemon.0.wait().await.unwrap();
}
