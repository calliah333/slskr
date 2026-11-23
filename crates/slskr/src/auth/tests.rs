use super::*;
use axum::http::Request;

fn config() -> AuthConfig {
    AuthConfig {
        disabled: false,
        username: "admin".into(),
        password: "hunter2".into(),
        password_hash: String::new(),
        session_ttl: session::DEFAULT_TTL,
        api_keys: vec![
            ApiKey {
                name: "local".into(),
                key: "local-key-0123456789".into(),
                role: Role::ReadWrite,
                cidr: "127.0.0.0/8,::1/128".into(),
            },
            ApiKey {
                name: "anywhere".into(),
                key: "anywhere-key-0123456789".into(),
                role: Role::ReadOnly,
                cidr: ALLOW_ALL_CIDR.into(),
            },
        ],
    }
}

fn auth() -> Auth {
    Auth::new(config(), SessionStore::memory(session::DEFAULT_TTL))
}

fn request(method: Method, uri: &str, headers: &[(&str, &str)]) -> Parts {
    let mut builder = Request::builder().method(method).uri(uri);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    builder.body(()).unwrap().into_parts().0
}

fn parts(uri: &str, headers: &[(&str, &str)]) -> Parts {
    request(Method::GET, uri, headers)
}

fn addr(s: &str) -> SocketAddr {
    SocketAddr::new(s.parse().unwrap(), 1234)
}

async fn signed_in(auth: &Auth) -> String {
    let (token, principal) = auth.login("admin", "hunter2").await.unwrap();
    assert_eq!(principal.method, AuthMethod::Session);
    format!("{COOKIE_NAME}={token}")
}

#[test]
fn api_key_cidr() {
    let auth = auth();

    let p = auth
        .authenticate_api_key("local-key-0123456789", "127.0.0.1".parse().unwrap())
        .unwrap();
    assert_eq!(
        p,
        Principal {
            name: "local".into(),
            role: Role::ReadWrite,
            method: AuthMethod::ApiKey
        }
    );
    assert!(
        auth.authenticate_api_key("local-key-0123456789", "::ffff:127.0.0.1".parse().unwrap())
            .is_ok()
    );
    assert!(
        auth.authenticate_api_key("local-key-0123456789", "::1".parse().unwrap())
            .is_ok()
    );
    assert!(matches!(
        auth.authenticate_api_key("local-key-0123456789", "10.0.0.1".parse().unwrap()),
        Err(ApiKeyError::OutOfRange { .. })
    ));
    assert!(
        auth.authenticate_api_key("anywhere-key-0123456789", "8.8.8.8".parse().unwrap())
            .is_ok()
    );
    assert!(
        auth.authenticate_api_key("anywhere-key-0123456789", "2001:db8::1".parse().unwrap())
            .is_ok()
    );
    assert_eq!(
        auth.authenticate_api_key("nope", "127.0.0.1".parse().unwrap()),
        Err(ApiKeyError::Unknown)
    );
}

#[tokio::test]
async fn authenticate_schemes_and_roles() {
    let auth = auth();
    let cookie = signed_in(&auth).await;
    let local = addr("127.0.0.1");

    let p = parts("/api/session", &[("cookie", cookie.as_str())]);
    let principal = auth.authenticate(&p, local).unwrap();
    assert_eq!(principal.method, AuthMethod::Session);
    assert_eq!(principal.name, "admin");
    assert_eq!(principal.role, Role::Administrator);
    assert_eq!(
        auth.authenticate_with_policy(&p, local, Policy::ApiKeyOnly),
        Err(AuthError::Unauthorized)
    );
    // Other cookies around ours are fine.
    let crowded = format!("theme=dark; {cookie}; other=1");
    let p = parts("/api/session", &[("cookie", crowded.as_str())]);
    assert!(auth.authenticate(&p, local).is_ok());

    let p = parts("/api/session", &[("x-api-key", "local-key-0123456789")]);
    let principal = auth.authenticate(&p, local).unwrap();
    assert_eq!(principal.method, AuthMethod::ApiKey);
    assert_eq!(
        auth.authenticate_with_policy(&p, local, Policy::SessionOnly),
        Err(AuthError::Unauthorized)
    );
    assert!(auth.authenticate(&p, addr("192.168.1.1")).is_err());

    assert_eq!(
        auth.authorize(&p, local, Role::ReadWrite, Policy::Any)
            .unwrap()
            .role,
        Role::ReadWrite
    );
    assert_eq!(
        auth.authorize(&p, local, Role::Administrator, Policy::Any),
        Err(AuthError::Forbidden)
    );

    let none = parts("/api/session", &[]);
    assert_eq!(
        auth.authorize(&none, local, Role::ReadOnly, Policy::Any),
        Err(AuthError::Unauthorized)
    );

    // Neither bearer values nor query strings authenticate.
    let token = cookie.split_once('=').unwrap().1;
    let bearer = format!("Bearer {token}");
    for p in [
        parts("/api/session", &[("authorization", bearer.as_str())]),
        parts(
            "/api/events",
            &[("authorization", "Bearer anywhere-key-0123456789")],
        ),
        parts(&format!("/api/events?access_token={token}"), &[]),
        parts(&format!("/api/session?{COOKIE_NAME}={token}"), &[]),
    ] {
        assert_eq!(
            auth.authenticate(&p, local),
            Err(AuthError::Unauthorized),
            "{}",
            p.uri
        );
    }
}

#[test]
fn origin_matching() {
    let check = |origin: Option<&str>, host: &str| {
        let mut headers = vec![("host", host)];
        if let Some(origin) = origin {
            headers.push(("origin", origin));
        }
        let p = parts("/api/session", &headers);
        same_origin(&p.headers, &p.uri)
    };
    assert!(check(None, "localhost:5030"));
    assert!(check(Some("http://localhost:5030"), "localhost:5030"));
    assert!(check(Some("http://LOCALHOST:5030"), "localhost:5030"));
    assert!(check(Some("https://example.com"), "example.com"));
    assert!(check(Some("https://example.com"), "example.com:443"));
    assert!(check(Some("http://example.com:80"), "example.com"));
    assert!(check(Some("http://[::1]:5030"), "[::1]:5030"));
    assert!(!check(Some("http://evil.example"), "localhost:5030"));
    assert!(!check(Some("http://localhost:5031"), "localhost:5030"));
    assert!(!check(Some("https://example.com"), "example.com:80"));
    assert!(!check(Some("http://example.com:8080"), "example.com:80"));
    assert!(!check(Some("null"), "localhost:5030"));
    assert!(!check(Some("file://"), "localhost:5030"));

    assert!(!needs_origin_check(&Method::GET, &HeaderMap::new()));
    assert!(!needs_origin_check(&Method::HEAD, &HeaderMap::new()));
    assert!(!needs_origin_check(&Method::OPTIONS, &HeaderMap::new()));
    for method in [Method::POST, Method::PUT, Method::PATCH, Method::DELETE] {
        assert!(needs_origin_check(&method, &HeaderMap::new()));
    }
    let upgrade = parts("/api/events", &[("upgrade", "WebSocket")]);
    assert!(needs_origin_check(&upgrade.method, &upgrade.headers));
}

#[tokio::test]
async fn cookie_sessions_are_refused_cross_site() {
    let auth = auth();
    let cookie = signed_in(&auth).await;
    let local = addr("127.0.0.1");
    let host = ("host", "localhost:5030");
    let jar = ("cookie", cookie.as_str());
    let same = ("origin", "http://localhost:5030");
    let evil = ("origin", "https://evil.example");
    let upgrade = [("upgrade", "websocket"), ("connection", "Upgrade")];

    // Same origin and absent Origin pass, for state changes and WebSocket upgrades.
    for headers in [vec![host, jar, same], vec![host, jar]] {
        assert!(
            auth.authenticate(&request(Method::POST, "/api/x", &headers), local)
                .is_ok()
        );
        let mut ws = headers.clone();
        ws.extend(upgrade);
        assert!(auth.authenticate(&parts("/api/events", &ws), local).is_ok());
    }

    // Cross-site: reads pass, state changes and WebSocket upgrades are 403.
    assert!(
        auth.authenticate(&parts("/api/x", &[host, jar, evil]), local)
            .is_ok()
    );
    for method in [Method::POST, Method::PUT, Method::DELETE] {
        assert_eq!(
            auth.authenticate(&request(method, "/api/x", &[host, jar, evil]), local),
            Err(AuthError::Forbidden)
        );
    }
    let mut ws = vec![host, jar, evil];
    ws.extend(upgrade);
    assert_eq!(
        auth.authenticate(&parts("/api/events", &ws), local),
        Err(AuthError::Forbidden)
    );
    let mut anonymous = vec![host, same];
    anonymous.extend(upgrade);
    assert_eq!(
        auth.authenticate(&parts("/api/events", &anonymous), local),
        Err(AuthError::Unauthorized)
    );

    // API keys skip the check, even alongside a cross-site cookie.
    let key = ("x-api-key", "local-key-0123456789");
    let p = request(Method::POST, "/api/x", &[host, jar, evil, key]);
    assert_eq!(
        auth.authenticate(&p, local).unwrap().method,
        AuthMethod::ApiKey
    );
    // Without ConnectInfo the caller is unknown, so only an unrestricted key applies.
    let mut ws = vec![host, evil, ("x-api-key", "anywhere-key-0123456789")];
    ws.extend(upgrade);
    assert!(
        auth.authenticate_request(&parts("/api/events", &ws))
            .is_ok()
    );
}

#[test]
fn passthrough_when_disabled() {
    let auth = Auth::new(
        AuthConfig {
            disabled: true,
            ..config()
        },
        SessionStore::memory(session::DEFAULT_TTL),
    );
    assert!(!auth.enabled());
    let p = parts("/api/transfers", &[]);
    let principal = auth
        .authorize(
            &p,
            addr("10.0.0.1"),
            Role::Administrator,
            Policy::SessionOnly,
        )
        .unwrap();
    assert_eq!(principal, Principal::passthrough());
    assert_eq!(
        principal.session(),
        slskr_types::session::Session {
            username: "Anonymous".into(),
            role: slskr_types::session::Role::Admin
        }
    );
    // Another site still cannot drive the daemon.
    let p = request(
        Method::POST,
        "/api/transfers",
        &[
            ("host", "localhost:5030"),
            ("origin", "http://evil.example"),
        ],
    );
    assert_eq!(
        auth.authenticate(&p, addr("127.0.0.1")),
        Err(AuthError::Forbidden)
    );
}

#[tokio::test]
async fn login_semantics() {
    let auth = auth();
    assert!(auth.check_credentials("admin", "hunter2"));
    assert!(!auth.check_credentials("admin", "hunter3"));
    assert!(!auth.check_credentials("Admin", "hunter2"));

    let (token, principal) = auth.login("admin", "hunter2").await.unwrap();
    assert_eq!(principal.role, Role::Administrator);
    assert!(auth.sessions().lookup(&token).is_some());
    assert!(matches!(
        auth.login("admin", "wrong").await,
        Err(LoginError::Unauthorized)
    ));
    assert!(matches!(
        auth.login("", "x").await,
        Err(LoginError::BadRequest(_))
    ));

    let misconfigured = Auth::new(
        AuthConfig {
            password: String::new(),
            ..config()
        },
        SessionStore::memory(session::DEFAULT_TTL),
    );
    assert!(matches!(
        misconfigured.login("admin", "x").await,
        Err(LoginError::Misconfigured)
    ));

    let cookie = format!("{COOKIE_NAME}={token}");
    let headers = parts("/", &[("cookie", cookie.as_str())]).headers;
    auth.logout(&headers).await.unwrap();
    assert!(auth.session_principal(&headers).is_none());
}

#[tokio::test]
async fn credential_changes_invalidate_sessions() {
    let auth = auth();
    let cookie = signed_in(&auth).await;
    let headers = parts("/", &[("cookie", cookie.as_str())]).headers;

    // API key or TTL changes keep sessions.
    auth.update(&AuthConfig {
        api_keys: Vec::new(),
        session_ttl: Duration::from_secs(3_600),
        ..config()
    });
    assert!(auth.session_principal(&headers).is_some());
    assert_eq!(auth.sessions().ttl(), Duration::from_secs(3_600));

    auth.update(&AuthConfig {
        password: "changed".into(),
        ..config()
    });
    assert!(auth.session_principal(&headers).is_none());
    assert!(auth.login("admin", "changed").await.is_ok());

    let cookie = signed_in_as(&auth, "admin", "changed").await;
    let headers = parts("/", &[("cookie", cookie.as_str())]).headers;
    auth.update(&AuthConfig {
        username: "root".into(),
        password: "changed".into(),
        ..config()
    });
    assert!(auth.session_principal(&headers).is_none());
}

async fn signed_in_as(auth: &Auth, username: &str, password: &str) -> String {
    let (token, _) = auth.login(username, password).await.unwrap();
    format!("{COOKIE_NAME}={token}")
}

#[test]
fn cookie_attributes() {
    let auth = auth().with_url_base("/client/");
    let cookie = auth.session_cookie("abc", false);
    let cookie = cookie.to_str().unwrap();
    assert!(cookie.starts_with("slskr_session=abc; "), "{cookie}");
    let attributes: Vec<_> = cookie.split("; ").skip(1).collect();
    assert!(attributes.contains(&"HttpOnly"));
    assert!(attributes.contains(&"SameSite=Strict"));
    assert!(attributes.contains(&"Path=/client"));
    assert!(attributes.contains(&"Max-Age=2592000"));
    assert!(!attributes.contains(&"Secure"));
    let secure = auth.session_cookie("abc", true);
    assert!(secure.to_str().unwrap().ends_with("; Secure"));

    let cleared = auth.clear_cookie(false);
    let cleared = cleared.to_str().unwrap();
    assert!(cleared.starts_with("slskr_session=; "));
    assert!(cleared.contains("; Max-Age=0"));
    assert!(cleared.contains("; Path=/client"));

    let root = Auth::new(config(), SessionStore::memory(session::DEFAULT_TTL));
    assert!(
        root.session_cookie("abc", false)
            .to_str()
            .unwrap()
            .contains("; Path=/;")
    );

    let https = |value: &str| forwarded_https(&parts("/", &[("x-forwarded-proto", value)]).headers);
    assert!(https("https"));
    assert!(https("HTTPS"));
    assert!(https("https, http"));
    assert!(!https("http"));
    assert!(!https("http, https"));
    assert!(!forwarded_https(&HeaderMap::new()));
}

#[test]
fn unknown_caller_only_matches_unrestricted_keys() {
    let auth = auth();
    let unknown = IpAddr::V4(Ipv4Addr::UNSPECIFIED);
    assert!(matches!(
        auth.authenticate_api_key("local-key-0123456789", unknown),
        Err(ApiKeyError::OutOfRange { .. })
    ));
    assert!(
        auth.authenticate_api_key("anywhere-key-0123456789", unknown)
            .is_ok()
    );
    let auth = Auth::new(
        AuthConfig {
            api_keys: vec![ApiKey {
                name: "zero".into(),
                key: "zero-net-key-0123456789".into(),
                role: Role::ReadOnly,
                cidr: "0.0.0.0/8".into(),
            }],
            ..config()
        },
        SessionStore::memory(session::DEFAULT_TTL),
    );
    assert!(
        auth.authenticate_api_key("zero-net-key-0123456789", unknown)
            .is_err()
    );
}

#[test]
fn roles() {
    assert_eq!(
        "administrator".parse::<Role>().unwrap(),
        Role::Administrator
    );
    assert_eq!("READWRITE".parse::<Role>().unwrap(), Role::ReadWrite);
    assert!("root".parse::<Role>().is_err());
    assert!(Role::Administrator.satisfies(Role::ReadWrite));
    assert!(!Role::ReadOnly.satisfies(Role::ReadWrite));
    assert_eq!(
        serde_json::to_string(&Role::ReadWrite).unwrap(),
        "\"ReadWrite\""
    );
    assert_eq!(role_from_u8(Role::Administrator as u8), Role::Administrator);
}

#[test]
fn constant_time() {
    assert!(constant_time_eq(b"abc", b"abc"));
    assert!(constant_time_eq(b"", b""));
    assert!(!constant_time_eq(b"abc", b"abd"));
    assert!(!constant_time_eq(b"abc", b"abcd"));
}

#[tokio::test]
async fn used_sessions_resend_the_cookie() {
    use axum::{Router, body::Body, routing::get};
    use tower::ServiceExt as _;

    let auth = auth().with_url_base("/client");
    let (token, _) = auth.login("admin", "hunter2").await.unwrap();
    let router = Router::new().route("/x", get(|| async { "ok" })).layer(
        axum::middleware::from_fn_with_state(auth.clone(), refresh_session_cookie),
    );
    let send = |origin: Option<&'static str>| {
        let mut request = Request::get("/x")
            .header("host", "localhost:5030")
            .header("cookie", format!("{COOKIE_NAME}={token}"))
            .header("x-forwarded-proto", "https");
        if let Some(origin) = origin {
            request = request.header("origin", origin);
        }
        router.clone().oneshot(request.body(Body::empty()).unwrap())
    };
    let set_cookie = |response: &Response| {
        response
            .headers()
            .get(header::SET_COOKIE)
            .map(|value| value.to_str().unwrap().to_owned())
    };

    // Fresh sessions are not refreshed.
    assert_eq!(set_cookie(&send(None).await.unwrap()), None);

    auth.sessions()
        .backdate(&token, chrono::TimeDelta::hours(2));
    let before = auth.sessions().lookup(&token).unwrap().expires_at;
    // A cross-site request does not slide the session.
    let response = send(Some("https://evil.example")).await.unwrap();
    assert_eq!(set_cookie(&response), None);
    let response = send(Some("https://localhost:5030")).await.unwrap();
    let cookie = set_cookie(&response).expect("refreshed cookie");
    assert!(cookie.starts_with(&format!("{COOKIE_NAME}={token}; ")));
    assert!(cookie.contains("; Path=/client; "));
    assert!(cookie.ends_with("; Secure"));
    assert!(auth.sessions().lookup(&token).unwrap().expires_at > before);
    // Once per refresh interval.
    assert_eq!(set_cookie(&send(None).await.unwrap()), None);
}

#[tokio::test]
async fn hashed_password_login_and_invalidation() {
    let hash = hash_password("from-the-ui").unwrap();
    assert!(hash.starts_with("$argon2id$"), "{hash}");
    assert!(valid_password_hash(&hash));
    assert!(!valid_password_hash("plaintext"));
    assert!(verify_password("from-the-ui", &hash));
    assert!(!verify_password("wrong", &hash));

    let auth = Auth::new(
        AuthConfig {
            password: String::new(),
            password_hash: hash.clone(),
            ..config()
        },
        SessionStore::memory(session::DEFAULT_TTL),
    );
    assert!(auth.login("admin", "from-the-ui").await.is_ok());
    assert!(matches!(
        auth.login("admin", "hunter2").await,
        Err(LoginError::Unauthorized)
    ));
    assert!(matches!(
        auth.login("root", "from-the-ui").await,
        Err(LoginError::Unauthorized)
    ));

    // A new hash (a password change from the UI) signs out every session.
    let cookie = signed_in_as(&auth, "admin", "from-the-ui").await;
    let headers = parts("/", &[("cookie", cookie.as_str())]).headers;
    let changed = hash_password("next").unwrap();
    auth.update(&AuthConfig {
        password: String::new(),
        password_hash: changed,
        ..config()
    });
    assert!(auth.session_principal(&headers).is_none());
    assert!(auth.login("admin", "next").await.is_ok());

    // A plaintext password from the file takes precedence over the hash.
    let both = Auth::new(
        AuthConfig {
            password_hash: hash,
            ..config()
        },
        SessionStore::memory(session::DEFAULT_TTL),
    );
    assert!(both.login("admin", "hunter2").await.is_ok());
    assert!(both.login("admin", "from-the-ui").await.is_err());
}

#[test]
fn hashed_api_keys() {
    let key = "managed-key-0123456789abcdef";
    let digest = parse_sha256_hex(&sha256_hex(key.as_bytes())).unwrap();
    assert_eq!(digest, sha256(key.as_bytes()));
    assert!(parse_sha256_hex("zz").is_none());
    let auth = Auth::new(
        AuthConfig {
            api_keys: vec![ApiKey {
                name: "managed".into(),
                key: ApiKeySecret::Sha256(digest),
                role: Role::Administrator,
                cidr: ALLOW_ALL_CIDR.into(),
            }],
            ..config()
        },
        SessionStore::memory(session::DEFAULT_TTL),
    );
    let principal = auth
        .authenticate_api_key(key, "10.0.0.1".parse().unwrap())
        .unwrap();
    assert_eq!(principal.name, "managed");
    assert_eq!(principal.role, Role::Administrator);
    assert!(matches!(
        auth.authenticate_api_key(&sha256_hex(key.as_bytes()), "10.0.0.1".parse().unwrap()),
        Err(ApiKeyError::Unknown)
    ));
}
