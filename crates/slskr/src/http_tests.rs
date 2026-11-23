use super::{ApiError, Json, Query, json_errors};
use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Deserialize;
use tower::ServiceExt;

async fn body_json(response: Response) -> serde_json::Value {
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn errors_are_json_objects() {
    let response = ApiError::NotFound("missing".into()).into_response();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(response.headers()[header::CONTENT_TYPE], "application/json");
    assert_eq!(
        body_json(response).await,
        serde_json::json!({"error": "missing"})
    );
    for (error, status, message) in [
        (ApiError::not_found(), StatusCode::NOT_FOUND, "Not Found"),
        (
            ApiError::Unauthorized,
            StatusCode::UNAUTHORIZED,
            "Unauthorized",
        ),
        (
            ApiError::Internal(anyhow::anyhow!("secret detail")),
            StatusCode::INTERNAL_SERVER_ERROR,
            "Internal Server Error",
        ),
    ] {
        let response = error.into_response();
        assert_eq!(response.status(), status);
        assert_eq!(body_json(response).await["error"], message);
    }
}

#[derive(Deserialize)]
struct Payload {
    #[allow(dead_code)]
    count: u32,
}

#[tokio::test]
async fn rejections_and_unrouted_requests_answer_json_errors() {
    let app = Router::new()
        .route("/json", post(|Json(_): Json<Payload>| async { "ok" }))
        .route("/query", get(|Query(_): Query<Payload>| async { "ok" }))
        .route(
            "/text",
            get(|| async { (StatusCode::BAD_GATEWAY, "upstream failed") }),
        )
        .layer(axum::middleware::from_fn(json_errors));
    let send = |method: &str, uri: &str, content_type: Option<&str>, body: &'static str| {
        let mut request = Request::builder().method(method).uri(uri);
        if let Some(content_type) = content_type {
            request = request.header(header::CONTENT_TYPE, content_type);
        }
        app.clone().oneshot(request.body(Body::from(body)).unwrap())
    };
    for (method, uri, content_type, body, status) in [
        (
            "POST",
            "/json",
            Some("application/json"),
            "{",
            StatusCode::BAD_REQUEST,
        ),
        (
            "POST",
            "/json",
            Some("application/json"),
            r#"{"count":"x"}"#,
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            "POST",
            "/json",
            None,
            r#"{"count":1}"#,
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
        ),
        ("GET", "/query?count=x", None, "", StatusCode::BAD_REQUEST),
        ("GET", "/json", None, "", StatusCode::METHOD_NOT_ALLOWED),
        ("GET", "/text", None, "", StatusCode::BAD_GATEWAY),
    ] {
        let response = send(method, uri, content_type, body).await.unwrap();
        assert_eq!(response.status(), status, "{method} {uri} {body}");
        let error = body_json(response).await;
        let message = error["error"].as_str().unwrap();
        assert!(!message.is_empty(), "{method} {uri}");
        if uri == "/text" {
            assert_eq!(message, "upstream failed");
        }
    }
    let response = send("POST", "/json", Some("application/json"), r#"{"count":1}"#)
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
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
        "/client/api/events/missing",
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
