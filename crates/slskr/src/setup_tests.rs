use super::*;
use axum::{
    Router,
    body::{Body, to_bytes},
    extract::ConnectInfo,
};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tower::ServiceExt as _;

const LOCAL: &str = "127.0.0.1:40000";
const REMOTE: &str = "192.0.2.7:40000";
const PASSWORD: &str = "correct horse";

struct Harness {
    dir: tempfile::TempDir,
    app: AppState,
    router: Router,
}

struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    body: Value,
}

impl Reply {
    fn error(&self) -> &str {
        self.body["error"].as_str().unwrap_or_default()
    }

    fn cookie(&self) -> String {
        let cookie = self.headers[header::SET_COOKIE].to_str().unwrap();
        cookie.split(';').next().unwrap().to_owned()
    }
}

async fn harness(file: &str, env: &[(&str, &str)]) -> Harness {
    harness_with_store(file, env, None).await
}

async fn harness_with_store(file: &str, env: &[(&str, &str)], store: Option<&str>) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    if let Some(store) = store {
        std::fs::write(dir.path().join(store::FILE_NAME), store).unwrap();
    }
    let file = format!("[soulseek]\nconnect = false\n{file}");
    let app = crate::bootstrap::test_app(dir.path(), &file, env)
        .await
        .unwrap();
    sync(&app);
    let router = crate::bootstrap::router(&app);
    Harness { dir, app, router }
}

impl Harness {
    async fn send(
        &self,
        method: Method,
        path: &str,
        from: &str,
        headers: &[(&str, &str)],
        body: Option<Value>,
    ) -> Reply {
        let from: SocketAddr = from.parse().unwrap();
        let mut builder = axum::http::Request::builder().method(method).uri(path);
        if !headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("host"))
        {
            let host = if from.ip().is_loopback() {
                "127.0.0.1:5030"
            } else {
                "192.0.2.1:5030"
            };
            builder = builder.header(header::HOST, host);
        }
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        let body = match body {
            Some(body) => {
                builder = builder.header(header::CONTENT_TYPE, "application/json");
                Body::from(body.to_string())
            }
            None => Body::empty(),
        };
        let mut request = builder.body(body).unwrap();
        request.extensions_mut().insert(ConnectInfo(from));
        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        Reply {
            status,
            headers,
            body,
        }
    }

    async fn get(&self, path: &str, from: &str, headers: &[(&str, &str)]) -> Reply {
        self.send(Method::GET, path, from, headers, None).await
    }

    async fn post(&self, path: &str, from: &str, headers: &[(&str, &str)], body: Value) -> Reply {
        self.send(Method::POST, path, from, headers, Some(body))
            .await
    }

    fn store(&self) -> String {
        std::fs::read_to_string(self.dir.path().join(store::FILE_NAME)).unwrap_or_default()
    }
}

fn setup_body(username: &str) -> Value {
    json!({"web": {"username": username, "password": PASSWORD}})
}

#[tokio::test]
async fn required_without_any_password() {
    let fresh = harness("", &[]).await;
    assert!(required(&fresh.app));
    assert!(fresh.app.setup.code().is_some());

    let hash = crate::auth::hash_password("from the ui").unwrap();
    let store = json!({"version": 1, "settings": {}, "web_password_hash": hash}).to_string();
    let hashed = harness_with_store("", &[], Some(&store)).await;
    assert!(!required(&hashed.app));
    assert_eq!(hashed.app.setup.code(), None);

    let file = harness("[web]\npassword = \"from the file\"\n", &[]).await;
    assert!(!required(&file.app));
    let env = harness("", &[("SLSKR_WEB_PASSWORD", "from the env")]).await;
    assert!(!required(&env.app));
    let open = harness("[web]\nauth = false\n", &[]).await;
    assert!(!required(&open.app));
    assert_eq!(open.app.setup.code(), None);
    let status = open.get("/api/setup", REMOTE, &[]).await;
    assert_eq!(status.body["required"], false);
    let refused = open
        .post("/api/setup", LOCAL, &[], setup_body("admin"))
        .await;
    assert_eq!(refused.status, StatusCode::CONFLICT);
}

#[tokio::test]
async fn other_routes_answer_503_until_setup() {
    let h = harness("[web]\nurl_base = \"/client\"\n", &[]).await;
    for path in [
        "/api/application",
        "/client/api/application",
        "/api/session",
        "/client/api/settings",
        "/api/events",
        "/api/no-such-route",
    ] {
        let reply = h.get(path, LOCAL, &[]).await;
        assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE, "{path}");
        assert_eq!(reply.body, json!({"error": "setup required"}), "{path}");
    }
    let login = h
        .post(
            "/client/api/session",
            LOCAL,
            &[],
            json!({"username": "admin", "password": "x"}),
        )
        .await;
    assert_eq!(login.status, StatusCode::SERVICE_UNAVAILABLE);

    let info = h.get("/client/api/auth", REMOTE, &[]).await;
    assert_eq!(info.status, StatusCode::OK);
    assert_eq!(info.body, json!({"enabled": true, "setup_required": true}));
    assert_eq!(
        h.get("/client/health", REMOTE, &[]).await.status,
        StatusCode::OK
    );
    let status = h.get("/client/api/setup", REMOTE, &[]).await;
    assert_eq!(
        status.body,
        json!({
            "required": true,
            "code_required": true,
            "web_username_pinned": false,
            "web_password_pinned": false,
            "soulseek_configured": false,
            "soulseek_pinned": false
        })
    );
    // The SPA still loads, so the browser can show the setup page.
    assert_eq!(
        h.get("/client/setup", REMOTE, &[]).await.status,
        StatusCode::OK
    );
}

#[tokio::test]
async fn code_is_required_unless_the_caller_is_local() {
    let h = harness("", &[]).await;
    let code_required = |reply: &Reply| reply.body["code_required"].as_bool().unwrap();
    assert!(!code_required(&h.get("/api/setup", LOCAL, &[]).await));
    assert!(!code_required(
        &h.get("/api/setup", "[::1]:40000", &[("host", "[::1]:5030")])
            .await
    ));
    assert!(!code_required(
        &h.get("/api/setup", LOCAL, &[("host", "localhost:5030")])
            .await
    ));
    assert!(code_required(&h.get("/api/setup", REMOTE, &[]).await));
    // A local reverse proxy, or a DNS name rebound to 127.0.0.1, is not a local caller.
    assert!(code_required(
        &h.get("/api/setup", LOCAL, &[("x-forwarded-for", "192.0.2.7")])
            .await
    ));
    assert!(code_required(
        &h.get("/api/setup", LOCAL, &[("host", "evil.example:5030")])
            .await
    ));
    assert!(code_required(
        &h.get("/api/setup", LOCAL, &[("host", "127.0.0.1.evil.example")])
            .await
    ));

    let missing = h.post("/api/setup", REMOTE, &[], setup_body("admin")).await;
    assert_eq!(missing.status, StatusCode::FORBIDDEN);
    assert!(
        missing.error().contains("Setup code required"),
        "{}",
        missing.error()
    );
    let wrong = h
        .post(
            "/api/setup",
            REMOTE,
            &[],
            json!({"code": "AAAA-AAAA-AAAA", "web": {"username": "admin", "password": PASSWORD}}),
        )
        .await;
    assert_eq!(wrong.status, StatusCode::FORBIDDEN);
    assert_eq!(wrong.error(), "Invalid setup code");
    // The directory picker takes the code in a header.
    let dirs = h.get("/api/fs/dirs?path=/", REMOTE, &[]).await;
    assert_eq!(dirs.status, StatusCode::FORBIDDEN);
    let code = h.app.setup.code().unwrap();
    let dirs = h
        .get("/api/fs/dirs?path=/", REMOTE, &[("x-setup-code", &code)])
        .await;
    assert_eq!(dirs.status, StatusCode::OK);

    // Another site can not drive setup from the local browser.
    let forged = h
        .post(
            "/api/setup",
            LOCAL,
            &[("origin", "http://evil.example")],
            setup_body("admin"),
        )
        .await;
    assert_eq!(forged.status, StatusCode::FORBIDDEN);

    // Lowercase and spaces are accepted.
    let typed = code.to_lowercase().replace('-', " ");
    let done = h
        .post(
            "/api/setup",
            REMOTE,
            &[],
            json!({"code": typed, "web": {"username": "admin", "password": PASSWORD}}),
        )
        .await;
    assert_eq!(done.status, StatusCode::CREATED, "{}", done.error());
    assert_eq!(h.app.setup.code(), None);
}

#[tokio::test]
async fn wrong_codes_are_limited_per_address() {
    let setup = Setup::new();
    let code = setup.refresh(true).unwrap();
    assert_eq!(setup.refresh(true), None, "the code is kept while required");
    assert_eq!(setup.code().as_deref(), Some(code.as_str()));
    assert_eq!(code.len(), 14);
    assert!(
        code.split('-')
            .all(|group| group.len() == 4 && group.bytes().all(|b| CODE_ALPHABET.contains(&b)))
    );

    let start = Instant::now();
    let attacker: IpAddr = "2001:db8::1".parse().unwrap();
    let neighbour: IpAddr = "2001:db8::2".parse().unwrap();
    let other: IpAddr = "2001:db8:0:1::1".parse().unwrap();
    assert_eq!(setup.check(attacker, None, start), Err(CodeError::Missing));
    for _ in 0..MAX_FAILURES {
        assert_eq!(
            setup.check(attacker, Some("nope"), start),
            Err(CodeError::Wrong)
        );
    }
    // Over the limit even the right code is refused, for the whole /64.
    let later = start + Duration::from_secs(20);
    assert_eq!(
        setup.check(attacker, Some(&code), later),
        Err(CodeError::Limited(Duration::from_secs(40)))
    );
    assert!(matches!(
        setup.check(neighbour, Some(&code), later),
        Err(CodeError::Limited(_))
    ));
    assert_eq!(setup.check(other, Some(&code), later), Ok(()));
    // The window passes.
    let after = start + FAILURE_WINDOW;
    assert_eq!(setup.check(attacker, Some(&code), after), Ok(()));

    // Completing setup forgets the code and the counts; a new requirement gets a new code.
    setup.refresh(false);
    assert_eq!(setup.code(), None);
    assert_eq!(
        setup.check(attacker, Some(&code), after),
        Err(CodeError::Wrong)
    );
    let next = setup.refresh(true).unwrap();
    assert_ne!(next, code);

    // Over HTTP: 429 with Retry-After once the address is over its limit.
    let h = harness("", &[]).await;
    let remote: SocketAddr = REMOTE.parse().unwrap();
    for _ in 0..MAX_FAILURES {
        let _ = h.app.setup.check(remote.ip(), Some("nope"), Instant::now());
    }
    let code = h.app.setup.code().unwrap();
    let limited = h
        .post(
            "/api/setup",
            REMOTE,
            &[],
            json!({"code": code, "web": {"username": "admin", "password": PASSWORD}}),
        )
        .await;
    assert_eq!(limited.status, StatusCode::TOO_MANY_REQUESTS);
    assert!(limited.headers.contains_key(header::RETRY_AFTER));
    // Local callers are not affected.
    let local = h.post("/api/setup", LOCAL, &[], setup_body("admin")).await;
    assert_eq!(local.status, StatusCode::CREATED);
}

#[tokio::test]
async fn setup_saves_a_hash_and_signs_in() {
    let h = harness("[web]\nurl_base = \"/client\"\n", &[]).await;
    let soulseek = json!({"username": "slskr user", "password": "soulseek pass"});
    let done = h
        .post(
            "/client/api/setup",
            LOCAL,
            &[("origin", "http://127.0.0.1:5030")],
            json!({"web": {"username": " owner ", "password": PASSWORD}, "soulseek": soulseek}),
        )
        .await;
    assert_eq!(done.status, StatusCode::CREATED, "{}", done.error());
    assert_eq!(done.body, json!({"username": "owner", "role": "admin"}));
    let set_cookie = done.headers[header::SET_COOKIE].to_str().unwrap();
    for attribute in [
        "HttpOnly",
        "SameSite=Strict",
        "Path=/client",
        "Max-Age=2592000",
    ] {
        assert!(
            set_cookie.split("; ").any(|a| a == attribute),
            "{set_cookie}"
        );
    }

    let store = h.store();
    let saved: Value = serde_json::from_str(&store).unwrap();
    assert!(
        saved["web_password_hash"]
            .as_str()
            .unwrap()
            .starts_with("$argon2id$")
    );
    assert!(!store.contains(PASSWORD), "{store}");
    assert_eq!(saved["settings"]["web"], json!({"username": "owner"}));
    assert_eq!(saved["settings"]["soulseek"]["username"], "slskr user");
    assert!(!required(&h.app));
    assert_eq!(h.app.setup.code(), None);

    let cookie = done.cookie();
    let session = h
        .get("/client/api/session", LOCAL, &[("cookie", &cookie)])
        .await;
    assert_eq!(session.status, StatusCode::OK);
    assert_eq!(session.body["username"], "owner");
    let state = h
        .get("/client/api/application", REMOTE, &[("cookie", &cookie)])
        .await;
    assert_eq!(state.status, StatusCode::OK);
    assert_eq!(
        h.get("/client/api/application", REMOTE, &[]).await.status,
        StatusCode::UNAUTHORIZED
    );
    let info = h.get("/client/api/auth", REMOTE, &[]).await;
    assert_eq!(info.body["setup_required"], false);
    let status = h.get("/client/api/setup", LOCAL, &[]).await;
    assert_eq!(status.body["required"], false);
    assert_eq!(status.body["code_required"], false);
    assert_eq!(status.body["soulseek_configured"], true);

    let again = h
        .post("/client/api/setup", LOCAL, &[], setup_body("owner"))
        .await;
    assert_eq!(again.status, StatusCode::CONFLICT);
    // The new password signs in normally.
    let login = h
        .post(
            "/client/api/session",
            REMOTE,
            &[],
            json!({"username": "owner", "password": PASSWORD}),
        )
        .await;
    assert_eq!(login.status, StatusCode::CREATED);
}

#[tokio::test]
async fn invalid_requests_list_every_problem() {
    let shared = tempfile::tempdir().unwrap();
    let file = shared.path().join("file.txt");
    std::fs::write(&file, "x").unwrap();
    let h = harness("", &[]).await;
    let missing = shared.path().join("missing");
    let bad = h
        .post(
            "/api/setup",
            LOCAL,
            &[],
            json!({
                "web": {"username": "  ", "password": "short"},
                "soulseek": {"username": " padded", "password": ""},
                "shares": [missing, file, "relative/music", shared.path()]
            }),
        )
        .await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST);
    let error = bad.error();
    for expected in [
        "web.username may not be blank".to_owned(),
        "web.password must be at least 8 characters".to_owned(),
        "soulseek.username may not start or end with a space".to_owned(),
        "soulseek.password may not be blank".to_owned(),
        format!("shares: {} does not exist", missing.display()),
        format!("shares: {} is not a directory", file.display()),
        "shares: relative/music is not an absolute path".to_owned(),
    ] {
        assert!(error.contains(&expected), "{expected} not in {error}");
    }
    assert!(
        !error.contains(&format!("{} ", shared.path().display())),
        "{error}"
    );
    assert!(required(&h.app), "nothing was saved");
    assert_eq!(h.store(), "");

    let long = "x".repeat(SOULSEEK_USERNAME_LIMIT + 1);
    assert!(soulseek_username_error(&long).is_some());
    assert!(soulseek_username_error(&"x".repeat(SOULSEEK_USERNAME_LIMIT)).is_none());
    assert!(soulseek_username_error("tab\tname").is_some());

    let unknown = h
        .post(
            "/api/setup",
            LOCAL,
            &[],
            json!({"web": {"username": "a", "password": PASSWORD}, "extra": 1}),
        )
        .await;
    assert_eq!(unknown.status, StatusCode::UNPROCESSABLE_ENTITY);

    // Shares are added and scanned.
    let done = h
        .post(
            "/api/setup",
            LOCAL,
            &[],
            json!({"web": {"username": "admin", "password": PASSWORD}, "shares": [shared.path()]}),
        )
        .await;
    assert_eq!(done.status, StatusCode::CREATED, "{}", done.error());
    let shared_path = shared.path().to_str().unwrap().to_owned();
    assert_eq!(h.app.options.get().shares.directories, [shared_path]);
    tokio::time::timeout(Duration::from_secs(10), async {
        while h.app.services.shares.shares().is_empty() || !h.app.state.get().shares.ready {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the new share was scanned");
}

#[tokio::test]
async fn pinned_keys_must_not_change() {
    let h = harness(
        "",
        &[
            ("SLSKR_WEB_USERNAME", "admin"),
            ("SLSKR_SOULSEEK_USERNAME", "pinned"),
        ],
    )
    .await;
    let status = h.get("/api/setup", LOCAL, &[]).await;
    assert_eq!(status.body["web_username_pinned"], true);
    assert_eq!(status.body["web_password_pinned"], false);
    assert_eq!(status.body["soulseek_pinned"], true);
    assert_eq!(status.body["soulseek_configured"], false);

    let renamed = h.post("/api/setup", LOCAL, &[], setup_body("other")).await;
    assert_eq!(renamed.status, StatusCode::CONFLICT);
    assert!(
        renamed.error().contains("web.username"),
        "{}",
        renamed.error()
    );
    let soulseek = h
        .post(
            "/api/setup",
            LOCAL,
            &[],
            json!({"web": {"username": "admin", "password": PASSWORD},
                   "soulseek": {"username": "someone", "password": "pw"}}),
        )
        .await;
    assert_eq!(soulseek.status, StatusCode::CONFLICT);
    assert!(
        soulseek.error().contains("soulseek.username"),
        "{}",
        soulseek.error()
    );
    assert!(required(&h.app));

    // The pinned value itself is fine.
    let done = h.post("/api/setup", LOCAL, &[], setup_body("admin")).await;
    assert_eq!(done.status, StatusCode::CREATED, "{}", done.error());
    let saved: Value = serde_json::from_str(&h.store()).unwrap();
    assert_eq!(saved["settings"], json!({}));
}

#[tokio::test]
async fn directory_listing() {
    let root = tempfile::tempdir().unwrap();
    for dir in ["beta", "Alpha", ".hidden"] {
        std::fs::create_dir(root.path().join(dir)).unwrap();
    }
    std::fs::write(root.path().join("file.flac"), "x").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(root.path().join("beta"), root.path().join("linked")).unwrap();
    let base = root.path().to_str().unwrap();

    let listing = list_directory(&root.path().join("."), 1000).unwrap();
    assert_eq!(listing.path, base);
    assert_eq!(
        listing.parent.as_deref(),
        root.path().parent().and_then(Path::to_str)
    );
    let names: Vec<_> = listing.dirs.iter().map(|d| d.name.as_str()).collect();
    #[cfg(unix)]
    assert_eq!(names, [".hidden", "Alpha", "beta", "linked"]);
    assert_eq!(listing.dirs[1].path, format!("{base}/Alpha"));
    assert!(!listing.truncated);
    let capped = list_directory(root.path(), 2).unwrap();
    assert_eq!(capped.dirs.len(), 2);
    assert!(capped.truncated);
    assert_eq!(list_directory(Path::new("/"), 1000).unwrap().parent, None);

    for (path, status) in [
        ("relative".to_owned(), StatusCode::BAD_REQUEST),
        (format!("{base}/../x"), StatusCode::BAD_REQUEST),
        (format!("{base}/missing"), StatusCode::NOT_FOUND),
        (format!("{base}/file.flac"), StatusCode::BAD_REQUEST),
    ] {
        let error = list_directory(Path::new(&path), 1000).unwrap_err();
        assert_eq!(error.status(), status, "{path}");
    }

    // Over HTTP: open to local setup callers, then administrators only.
    let h = harness("", &[]).await;
    let uri = format!("/api/fs/dirs?path={}", base.replace('/', "%2F"));
    let during = h.get(&uri, LOCAL, &[]).await;
    assert_eq!(during.status, StatusCode::OK);
    assert_eq!(during.body["dirs"][0]["name"], ".hidden");
    let home = h.get("/api/fs/dirs", LOCAL, &[]).await;
    assert_eq!(home.status, StatusCode::OK);
    assert!(home.body["path"].as_str().unwrap().starts_with('/'));

    let done = h.post("/api/setup", LOCAL, &[], setup_body("admin")).await;
    let cookie = done.cookie();
    assert_eq!(
        h.get(&uri, LOCAL, &[]).await.status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        h.get(&uri, REMOTE, &[("cookie", &cookie)]).await.status,
        StatusCode::OK
    );
    let readonly = h
        .app
        .options
        .create_api_key(slskr_types::settings::CreateApiKey {
            name: "reader".into(),
            role: slskr_types::config::ApiKeyRole::ReadOnly,
            allow: Vec::new(),
        })
        .await
        .unwrap();
    h.app
        .auth
        .update(&config::auth_config(&h.app.options.get()));
    assert_eq!(
        h.get(&uri, REMOTE, &[("x-api-key", &readonly.key)])
            .await
            .status,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn login_test_refuses_without_connect() {
    let h = harness("", &[]).await;
    let refused = h
        .post(
            "/api/soulseek/test-login",
            LOCAL,
            &[],
            json!({"username": "someone", "password": "pw"}),
        )
        .await;
    assert_eq!(refused.status, StatusCode::CONFLICT);
    assert!(refused.error().contains("soulseek.connect"));
    let remote = h
        .post(
            "/api/soulseek/test-login",
            REMOTE,
            &[],
            json!({"username": "someone", "password": "pw"}),
        )
        .await;
    assert_eq!(
        remote.status,
        StatusCode::FORBIDDEN,
        "the code is checked first"
    );
}

/// How the fake Soulseek server answers a login.
#[derive(Clone, Copy)]
enum ServerReply {
    Accept,
    Reject(&'static str),
    Silent,
}

fn wire_string(out: &mut Vec<u8>, text: &str) {
    out.extend_from_slice(&u32::try_from(text.len()).unwrap().to_le_bytes());
    out.extend_from_slice(text.as_bytes());
}

/// A one-connection server speaking just enough of the Soulseek login exchange.
async fn fake_server(reply: ServerReply) -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let length = socket.read_u32_le().await.unwrap();
        let mut login = vec![0; length as usize];
        socket.read_exact(&mut login).await.unwrap();
        assert_eq!(login[..4], 1u32.to_le_bytes(), "Login comes first");
        assert_eq!(login[8..12], *b"user");
        let mut body = 1u32.to_le_bytes().to_vec();
        match reply {
            ServerReply::Accept => {
                body.push(1);
                wire_string(&mut body, "welcome");
                body.extend_from_slice(&[1, 0, 0, 127]);
                wire_string(&mut body, "digest");
                body.push(0);
            }
            ServerReply::Reject(reason) => {
                body.push(0);
                wire_string(&mut body, reason);
            }
            ServerReply::Silent => body.clear(),
        }
        if !body.is_empty() {
            let mut frame = u32::try_from(body.len()).unwrap().to_le_bytes().to_vec();
            frame.extend_from_slice(&body);
            socket.write_all(&frame).await.unwrap();
        }
        let mut rest = Vec::new();
        let _ = socket.read_to_end(&mut rest).await;
    });
    port
}

#[tokio::test]
async fn login_test_outcomes() {
    let options = || login_test_options(ClientOptions::default());
    let run = |port: u16, limit: Duration| async move {
        login_test(options(), "127.0.0.1", port, "user", "pass", limit).await
    };
    let limit = Duration::from_secs(5);

    let ok = run(fake_server(ServerReply::Accept).await, limit).await;
    assert_eq!(
        ok,
        LoginTestResult {
            ok: true,
            reason: None,
            message: None
        }
    );
    for (wire, reason) in [
        ("INVALIDPASS", LoginTestFailure::InvalidPassword),
        ("INVALIDUSERNAME", LoginTestFailure::InvalidUsername),
        ("SVRFULL", LoginTestFailure::ServerFull),
        ("SVRPRIVATE", LoginTestFailure::ServerPrivate),
        ("INVALIDVERSION", LoginTestFailure::InvalidVersion),
        ("SOMETHING NEW", LoginTestFailure::Rejected),
    ] {
        let failed = run(fake_server(ServerReply::Reject(wire)).await, limit).await;
        assert!(!failed.ok);
        assert_eq!(failed.reason, Some(reason), "{wire}");
        assert_eq!(failed.message.as_deref(), Some(wire));
    }

    let closed = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };
    let unreachable = run(closed, limit).await;
    assert_eq!(
        unreachable.reason,
        Some(LoginTestFailure::Unreachable),
        "{unreachable:?}"
    );
    let silent = run(
        fake_server(ServerReply::Silent).await,
        Duration::from_millis(300),
    )
    .await;
    assert_eq!(silent.reason, Some(LoginTestFailure::Timeout), "{silent:?}");
}

#[test]
fn hosts_and_codes() {
    for host in [
        "localhost",
        "LOCALHOST:5030",
        "127.0.0.1",
        "127.1.2.3:80",
        "[::1]:5030",
        "[::ffff:127.0.0.1]",
    ] {
        assert!(loopback_host(host), "{host}");
    }
    for host in [
        "example.com",
        "localhost.example:5030",
        "10.0.0.1:5030",
        "[::1",
        "::1",
        "",
    ] {
        assert!(!loopback_host(host), "{host}");
    }
    assert_eq!(normalize_code(" abcd-efgh jkmn\n"), "ABCDEFGHJKMN");
    assert_eq!(format_code("ABCDEFGHJKMN"), "ABCD-EFGH-JKMN");
    assert_eq!(
        open_url(&{
            let mut config = Config::default();
            config.web.listen = "0.0.0.0:5030".parse().unwrap();
            config.web.url_base = "/client".into();
            config
        }),
        "http://localhost:5030/client/setup"
    );
    assert_eq!(open_url(&Config::default()), "http://127.0.0.1:5030/setup");
}
