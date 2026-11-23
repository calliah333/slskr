use super::ApiError;
use axum::{
    body::to_bytes,
    http::{Request, StatusCode},
    response::IntoResponse,
};
use tower::ServiceExt;

#[tokio::test]
async fn not_found_message_uses_json_string_and_empty_not_found_has_no_body() {
    let response = ApiError::NotFound("missing".into()).into_response();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        &to_bytes(response.into_body(), usize::MAX).await.unwrap()[..],
        b"\"missing\""
    );

    let response = ApiError::NotFound(String::new()).into_response();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert!(
        to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn static_spa_fallback_is_scoped_to_configured_base() {
    let dir = std::env::temp_dir().join(format!("slskr-http-{}", uuid::Uuid::new_v4()));
    tokio::fs::create_dir_all(&dir).await.unwrap();
    tokio::fs::write(
        dir.join("index.html"),
        "<head><base href=\"/\" /></head><p>spa</p>",
    )
    .await
    .unwrap();
    tokio::fs::write(dir.join("ui_bg.wasm"), b"\0asm\x01\0\0\0")
        .await
        .unwrap();
    let app = super::static_router(&dir, "/client");
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/client/route")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let html = std::str::from_utf8(&body).unwrap();
    assert!(html.contains("<base href=\"/client/\" />"));
    assert!(html.contains("window.urlBase=\"/client\""));
    for path in ["/client", "/client/"] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(path)
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{path}");
    }
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/client/ui_bg.wasm")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        response.headers()[axum::http::header::CONTENT_TYPE],
        "application/wasm"
    );
    for path in [
        "/api/missing",
        "/client/api/missing",
        "/client/hub/missing",
        "/client/missing.wasm",
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(path)
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
    }
    tokio::fs::remove_dir_all(dir).await.unwrap();
}
