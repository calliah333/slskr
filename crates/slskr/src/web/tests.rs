use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
};
use tower::ServiceExt;

use super::*;

async fn get(app: &Router, uri: &str, headers: &[(header::HeaderName, &str)]) -> Response {
    let mut request = Request::builder().uri(uri);
    for (name, value) in headers {
        request = request.header(name, *value);
    }
    app.clone()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap()
}

async fn text(response: Response) -> String {
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    String::from_utf8(body.to_vec()).unwrap()
}

#[tokio::test]
async fn directory_spa_fallback_is_scoped_to_configured_base() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("index.html"),
        "<head><base href=\"/\" /></head><p>spa</p>",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("ui-0123456789abcdef_bg.wasm"),
        b"\0asm\x01\0\0\0",
    )
    .unwrap();
    std::fs::write(dir.path().join("LICENSE"), "license text").unwrap();
    let app = router(Some(dir.path().to_owned()), "/client/");

    let response = get(&app, "/client/route", &[]).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-cache");
    let html = text(response).await;
    assert!(html.contains("<base href=\"/client/\" />"));
    assert!(html.contains("window.urlBase=\"/client\""));
    for path in ["/client", "/client/", "/client/index.html"] {
        let response = get(&app, path, &[]).await;
        assert_eq!(response.status(), StatusCode::OK, "{path}");
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "text/html; charset=utf-8"
        );
    }

    let response = get(&app, "/client/ui-0123456789abcdef_bg.wasm", &[]).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CONTENT_TYPE], "application/wasm");
    assert_eq!(
        response.headers()[header::CACHE_CONTROL],
        "public, max-age=31536000, immutable"
    );
    let etag = response.headers()[header::ETAG]
        .to_str()
        .unwrap()
        .to_owned();
    let response = get(
        &app,
        "/client/ui-0123456789abcdef_bg.wasm",
        &[(header::IF_NONE_MATCH, &format!("\"other\", W/{etag}"))],
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
    assert_eq!(response.headers()[header::ETAG], etag.as_str());
    assert!(text(response).await.is_empty());

    let response = get(&app, "/client/LICENSE", &[]).await;
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "text/plain; charset=utf-8"
    );
    assert_eq!(text(response).await, "license text");

    for path in [
        "/api/missing",
        "/client/api/missing",
        "/client/api/events/missing",
        "/client/missing.wasm",
        "/client/../index.html",
        "/client/%2e%2e/secret",
        "/route",
    ] {
        let response = get(&app, path, &[]).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
    }
}

#[tokio::test]
async fn directory_mode_does_not_follow_links_out_of_the_root() {
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret.txt"), "secret").unwrap();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("index.html"), "<head></head>").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(outside.path(), dir.path().join("linked")).unwrap();
    let app = router(Some(dir.path().to_owned()), "/");
    let response = get(&app, "/linked/secret.txt", &[]).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn embedded_index_is_rewritten_for_the_url_base() {
    let app = router(None, "/client");
    let response = get(&app, "/client/searches", &[]).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-cache");
    let etag = response.headers()[header::ETAG]
        .to_str()
        .unwrap()
        .to_owned();
    assert!(text(response).await.contains("<base href=\"/client/\" />"));
    let response = get(&app, "/client/", &[(header::IF_NONE_MATCH, &etag)]).await;
    assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
    let response = get(&app, "/client/api/session", &[]).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[test]
fn negotiates_precompressed_variants() {
    let asset = Asset {
        content_type: "application/wasm",
        etag: "\"abc\"".into(),
        immutable: true,
        body: Bytes::from_static(b"raw"),
        br: Some(Bytes::from_static(b"br")),
        gzip: Some(Bytes::from_static(b"gz")),
    };
    let headers = |value: &str| {
        let mut headers = HeaderMap::new();
        if !value.is_empty() {
            headers.insert(header::ACCEPT_ENCODING, value.parse().unwrap());
        }
        headers
    };
    for (accept, encoding, etag) in [
        ("gzip, deflate, br, zstd", Some("br"), "\"abc-br\""),
        ("gzip;q=0.5, br;q=0", Some("gzip"), "\"abc-gzip\""),
        ("identity", None, "\"abc\""),
        ("", None, "\"abc\""),
    ] {
        let response = respond(&asset, &headers(accept));
        let headers = response.headers();
        assert_eq!(
            headers
                .get(header::CONTENT_ENCODING)
                .map(|v| v.to_str().unwrap()),
            encoding,
            "{accept}"
        );
        assert_eq!(headers[header::ETAG], etag, "{accept}");
        assert_eq!(headers[header::VARY], "accept-encoding");
    }
}

#[test]
fn hashed_names_are_detected() {
    for hashed in [
        "slskr-web-d044874f3f4faf1_bg.wasm",
        "slskr-web-d044874f3f4faf1.js",
        "static/media/icons.38c6d8bab26db77d8c80.woff2",
        "static/css/slskd-0f1e2d3c4b5a6978.css",
        "snippets/leptos-0123abcd4567ef89/inline0.js",
    ] {
        assert!(meta::is_hashed(hashed), "{hashed}");
    }
    for plain in [
        "index.html",
        "LICENSE",
        "licenses/slskd-NOTICE",
        "a-cafe.css",
    ] {
        assert!(!meta::is_hashed(plain), "{plain}");
    }
}
