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

#[test]
fn soulseek_errors_map_to_documented_statuses() {
    use soulseek::Error as E;
    for (error, status) in [
        (
            E::InvalidState("not logged in".into()),
            StatusCode::CONFLICT,
        ),
        (E::Argument("bad".into()), StatusCode::BAD_REQUEST),
        (E::UserOffline("gone".into()), StatusCode::NOT_FOUND),
        (E::Timeout("slow".into()), StatusCode::GATEWAY_TIMEOUT),
        (E::RoomJoinForbidden("no".into()), StatusCode::FORBIDDEN),
        (
            E::Connection("reset".into()),
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
    ] {
        assert_eq!(ApiError::from(error).status(), status);
    }
}
