mod support;

use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::http::{Request, StatusCode, header};
use codex_proxy_rs::server::router;
use futures_util::StreamExt;
use http_body_util::BodyExt;
use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use support::{
    MockResponse, MockUpstream, StaticCredentials, TEST_ADMIN_KEY, codex_sse_fixture, test_state,
};
use tower::ServiceExt;

fn authed(req: Request<Body>) -> Request<Body> {
    let (mut parts, body) = req.into_parts();
    parts.headers.insert(
        header::AUTHORIZATION,
        format!("Bearer {TEST_ADMIN_KEY}").parse().unwrap(),
    );
    Request::from_parts(parts, body)
}

fn chat_request(payload: Value) -> Request<Body> {
    authed(
        Request::post("/v1/chat/completions")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(payload.to_string()))
            .unwrap(),
    )
}

async fn body_json(body: Body) -> Value {
    let bytes = body.collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

async fn body_string(body: Body) -> String {
    String::from_utf8(body.collect().await.unwrap().to_bytes().to_vec()).unwrap()
}

// ---- admin auth -------------------------------------------------------------

#[tokio::test]
async fn admin_gate_returns_500_when_key_unset() {
    let upstream = MockUpstream::start(vec![]).await;
    let mut state = test_state(&upstream.url, Arc::new(StaticCredentials::new("t", "a")));
    state.admin_api_key = None;
    let app = router(state);

    for (method, path) in [
        ("POST", "/v1/chat/completions"),
        ("POST", "/v1/responses"),
        ("POST", "/admin/credentials"),
        ("GET", "/admin/credentials/status"),
    ] {
        let req = Request::builder()
            .method(method)
            .uri(path)
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::INTERNAL_SERVER_ERROR,
            "{method} {path}"
        );
    }
}

#[tokio::test]
async fn admin_gate_accepts_bearer_and_x_api_key_rejects_bad() {
    let upstream = MockUpstream::start(vec![]).await;
    let state = test_state(&upstream.url, Arc::new(StaticCredentials::new("t", "a")));
    let app = router(state);

    // Bearer accepted.
    let resp = app
        .clone()
        .oneshot(
            Request::get("/admin/credentials/status")
                .header(header::AUTHORIZATION, format!("Bearer {TEST_ADMIN_KEY}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // X-API-Key accepted.
    let resp = app
        .clone()
        .oneshot(
            Request::get("/admin/credentials/status")
                .header("x-api-key", TEST_ADMIN_KEY)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Wrong key.
    let resp = app
        .clone()
        .oneshot(
            Request::get("/admin/credentials/status")
                .header(header::AUTHORIZATION, "Bearer wrong")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // Malformed Authorization (one part).
    let resp = app
        .clone()
        .oneshot(
            Request::get("/admin/credentials/status")
                .header(header::AUTHORIZATION, "garbage")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // No credentials at all.
    let resp = app
        .clone()
        .oneshot(
            Request::get("/admin/credentials/status")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

// ---- open endpoints -----------------------------------------------------------

#[tokio::test]
async fn health_and_not_found() {
    let upstream = MockUpstream::start(vec![]).await;
    let app = router(test_state(
        &upstream.url,
        Arc::new(StaticCredentials::new("t", "a")),
    ));

    let resp = app
        .clone()
        .oneshot(Request::get("/health").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp.into_body()).await, json!({"status": "ok"}));

    let resp = app
        .clone()
        .oneshot(Request::get("/nope").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ---- chat completions: streaming ---------------------------------------------

#[tokio::test]
async fn chat_streaming_happy_path() {
    let upstream = MockUpstream::start(vec![MockResponse::Sse(codex_sse_fixture())]).await;
    let app = router(test_state(
        &upstream.url,
        Arc::new(StaticCredentials::new("tok", "acct")),
    ));

    let resp = app
        .oneshot(chat_request(json!({
            "model": "gpt-5.1-codex",
            "stream": true,
            "messages": [{"role": "user", "content": "hi"}],
        })))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get(header::CONTENT_TYPE).unwrap(),
        "text/event-stream; charset=utf-8"
    );
    assert_eq!(
        resp.headers().get(header::CACHE_CONTROL).unwrap(),
        "no-cache"
    );

    let body = body_string(resp.into_body()).await;
    assert!(body.contains(r#""role":"assistant""#), "body: {body}");
    assert!(body.contains(r#""content":"Hello""#), "body: {body}");
    assert!(body.contains("chat.completion.chunk"), "body: {body}");
    assert!(body.ends_with("data: [DONE]\n\n"), "body: {body}");
}

#[tokio::test]
async fn chat_streaming_keepalive_on_stalled_upstream() {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(8);
    let upstream = MockUpstream::start(vec![MockResponse::Stream(rx)]).await;
    let app = router(test_state(
        &upstream.url,
        Arc::new(StaticCredentials::new("tok", "acct")),
    ));

    // One event, then stall (sender held open).
    tx.send(Ok(Bytes::from(
        "data: {\"type\":\"response.created\",\"response\":{\"id\":\"r1\"}}\n\n",
    )))
    .await
    .unwrap();

    let resp = app
        .oneshot(chat_request(json!({
            "model": "gpt-5",
            "stream": true,
            "messages": [{"role": "user", "content": "hi"}],
        })))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Keepalive interval is 50ms in test_state; a ping must arrive well
    // within 2s of stall.
    let mut stream = resp.into_body().into_data_stream();
    let mut collected = String::new();
    let deadline = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while let Some(chunk) = stream.next().await {
            collected.push_str(&String::from_utf8_lossy(&chunk.unwrap()));
            if collected.contains(": ping\n\n") {
                return true;
            }
        }
        false
    })
    .await;
    assert_eq!(deadline, Ok(true), "no keepalive seen; got: {collected}");

    // Finish the stream and confirm the sentinel.
    tx.send(Ok(Bytes::from("data: [DONE]\n\n"))).await.unwrap();
    drop(tx);
    while let Some(chunk) = stream.next().await {
        collected.push_str(&String::from_utf8_lossy(&chunk.unwrap()));
    }
    assert!(collected.ends_with("data: [DONE]\n\n"), "got: {collected}");
}

// ---- chat completions: non-streaming (buffered) --------------------------------

#[tokio::test]
async fn chat_non_streaming_buffers_to_single_completion() {
    let upstream = MockUpstream::start(vec![MockResponse::Sse(codex_sse_fixture())]).await;
    let app = router(test_state(
        &upstream.url,
        Arc::new(StaticCredentials::new("tok", "acct")),
    ));

    let resp = app
        .oneshot(chat_request(json!({
            "model": "gpt-5.1-codex",
            "messages": [{"role": "user", "content": "hi"}],
        })))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let completion = body_json(resp.into_body()).await;
    assert_eq!(completion["object"], "chat.completion");
    assert_eq!(completion["id"], "chatcmpl-resp_test1");
    assert_eq!(completion["choices"][0]["message"]["role"], "assistant");
    assert_eq!(
        completion["choices"][0]["message"]["content"],
        "Hello world"
    );
    assert_eq!(completion["choices"][0]["finish_reason"], "stop");
    assert_eq!(completion["usage"]["prompt_tokens"], 10);
    assert_eq!(completion["usage"]["completion_tokens"], 5);
}

// ---- 401 retry --------------------------------------------------------------

#[tokio::test]
async fn retries_once_after_401_with_refresh() {
    let upstream = MockUpstream::start(vec![
        MockResponse::Status(401, r#"{"error":"unauthorized"}"#.to_string()),
        MockResponse::Sse(codex_sse_fixture()),
    ])
    .await;
    let creds = Arc::new(StaticCredentials::new("tok", "acct"));
    let refreshes = creds.refreshes.clone();
    let app = router(test_state(&upstream.url, creds));

    let resp = app
        .oneshot(chat_request(json!({
            "model": "gpt-5",
            "stream": true,
            "messages": [{"role": "user", "content": "hi"}],
        })))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(upstream.hit_count(), 2);
    assert_eq!(refreshes.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[tokio::test]
async fn persistent_401_passes_through() {
    let upstream = MockUpstream::start(vec![
        MockResponse::Status(401, r#"{"error":"unauthorized"}"#.to_string()),
        MockResponse::Status(401, r#"{"error":"unauthorized"}"#.to_string()),
    ])
    .await;
    let app = router(test_state(
        &upstream.url,
        Arc::new(StaticCredentials::new("tok", "acct")),
    ));

    let resp = app
        .oneshot(chat_request(json!({
            "model": "gpt-5",
            "stream": true,
            "messages": [{"role": "user", "content": "hi"}],
        })))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(upstream.hit_count(), 2);
    let body = body_json(resp.into_body()).await;
    assert_eq!(body, json!({"error": "unauthorized"}));
}

// ---- upstream error mirroring ---------------------------------------------------

#[tokio::test]
async fn upstream_error_status_and_body_mirrored() {
    let upstream = MockUpstream::start(vec![MockResponse::Status(
        429,
        r#"{"error":{"message":"rate limited"}}"#.to_string(),
    )])
    .await;
    let app = router(test_state(
        &upstream.url,
        Arc::new(StaticCredentials::new("tok", "acct")),
    ));

    let resp = app
        .oneshot(chat_request(json!({
            "model": "gpt-5",
            "messages": [{"role": "user", "content": "hi"}],
        })))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
    let body = body_json(resp.into_body()).await;
    assert_eq!(body["error"]["message"], "rate limited");
}

// ---- /v1/responses ----------------------------------------------------------

#[tokio::test]
async fn responses_passes_through_without_done_injection() {
    let sse = concat!(
        "data: {\"type\":\"response.created\",\"response\":{\"id\":\"r1\"}}\n\n",
        "data: {\"type\":\"response.completed\",\"response\":{}}\n\n",
    );
    let upstream = MockUpstream::start(vec![MockResponse::Sse(sse.to_string())]).await;
    let app = router(test_state(
        &upstream.url,
        Arc::new(StaticCredentials::new("tok", "acct")),
    ));

    let resp = app
        .oneshot(authed(
            Request::post("/v1/responses")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({"model": "gpt-5.1-codex", "input": [], "stream": true}).to_string(),
                ))
                .unwrap(),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_string(resp.into_body()).await;
    assert!(body.contains("response.created"), "body: {body}");
    assert!(body.contains("response.completed"), "body: {body}");
    assert!(!body.contains("[DONE]"), "body: {body}");
}

#[tokio::test]
async fn responses_error_mirrored() {
    let upstream = MockUpstream::start(vec![MockResponse::Status(
        400,
        r#"{"error":"bad input"}"#.to_string(),
    )])
    .await;
    let app = router(test_state(
        &upstream.url,
        Arc::new(StaticCredentials::new("tok", "acct")),
    ));

    let resp = app
        .oneshot(authed(
            Request::post("/v1/responses")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(json!({"model": "gpt-5"}).to_string()))
                .unwrap(),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        body_json(resp.into_body()).await,
        json!({"error": "bad input"})
    );
}

#[tokio::test]
async fn set_cookie_is_not_mirrored_downstream() {
    // Backend session cookies must not cross the proxy boundary. Exercised
    // through the error-mirror path; the streaming path shares the same
    // sanitized_headers.
    let (status, body) = (429, r#"{"error":"limited"}"#.to_string());
    let upstream = MockUpstream::start(vec![MockResponse::StatusWithCookie(status, body)]).await;
    let app = router(test_state(
        &upstream.url,
        Arc::new(StaticCredentials::new("tok", "acct")),
    ));

    let resp = app
        .oneshot(chat_request(json!({
            "model": "gpt-5",
            "messages": [{"role": "user", "content": "hi"}],
        })))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(
        resp.headers().get(header::SET_COOKIE).is_none(),
        "set-cookie must not cross the proxy boundary"
    );
}

// ---- upstream header contract ---------------------------------------------------

#[tokio::test]
async fn upstream_headers_match_go_contract() {
    let upstream = MockUpstream::start(vec![MockResponse::Sse(codex_sse_fixture())]).await;
    let app = router(test_state(
        &upstream.url,
        // Pre-prefixed token must not double up.
        Arc::new(StaticCredentials::new("Bearer tok123", "acct-1")),
    ));

    app.oneshot(chat_request(json!({
        "model": "gpt-5",
        "stream": true,
        "messages": [{"role": "user", "content": "hi"}],
    })))
    .await
    .unwrap();

    let headers = upstream.headers.lock().await;
    let h = &headers[0];
    assert_eq!(h.get("authorization").unwrap(), "Bearer tok123");
    assert_eq!(h.get("version").unwrap(), "0.125.0");
    assert_eq!(h.get("openai-beta").unwrap(), "responses=experimental");
    assert_eq!(h.get("chatgpt-account-id").unwrap(), "acct-1");
    assert_eq!(h.get("originator").unwrap(), "codex_cli_rs");
    assert_eq!(
        h.get("user-agent").unwrap(),
        "codex_cli_rs/0.125.0 (Mac OS 26.3.0; arm64) Apple_Terminal/466"
    );
    assert_eq!(
        h.get("x-codex-beta-features").unwrap(),
        "multi_agent,apps,prevent_idle_sleep"
    );
    assert_eq!(h.get("accept").unwrap(), "text/event-stream");

    // session_id is a valid UUIDv4; turn metadata embeds one.
    let session = h.get("session_id").unwrap().to_str().unwrap();
    assert!(uuid::Uuid::parse_str(session).is_ok(), "session: {session}");
    let turn: Value =
        serde_json::from_str(h.get("x-codex-turn-metadata").unwrap().to_str().unwrap()).unwrap();
    assert!(
        uuid::Uuid::parse_str(turn["turn_id"].as_str().unwrap()).is_ok(),
        "turn: {turn}"
    );
    assert_eq!(turn["sandbox"], "none");
}

// ---- admin credentials on a basic store ----------------------------------------

#[tokio::test]
async fn admin_credentials_unsupported_on_env_store() {
    let upstream = MockUpstream::start(vec![]).await;
    let app = router(test_state(
        &upstream.url,
        Arc::new(StaticCredentials::new("tok", "acct-1")),
    ));

    let resp = app
        .clone()
        .oneshot(authed(
            Request::post("/admin/credentials")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({"accessToken": "a", "refreshToken": "r", "expiresAt": 123}).to_string(),
                ))
                .unwrap(),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    let resp = app
        .oneshot(authed(
            Request::get("/admin/credentials/status")
                .body(Body::empty())
                .unwrap(),
        ))
        .await
        .unwrap();
    let status = body_json(resp.into_body()).await;
    assert_eq!(status["type"], "basic");
    assert_eq!(status["hasCredentials"], true);
    assert_eq!(status["userID"], "acct-1");
}

// ---- bad request body -----------------------------------------------------------

#[tokio::test]
async fn invalid_json_body_is_400() {
    let upstream = MockUpstream::start(vec![]).await;
    let app = router(test_state(
        &upstream.url,
        Arc::new(StaticCredentials::new("tok", "acct")),
    ));

    let resp = app
        .oneshot(authed(
            Request::post("/v1/chat/completions")
                .body(Body::from("{not json"))
                .unwrap(),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(upstream.hit_count(), 0);
}

// ---- client disconnect cancels the relay ----------------------------------------

#[tokio::test]
async fn client_disconnect_cancels_upstream() {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(8);
    let upstream = MockUpstream::start(vec![MockResponse::Stream(rx)]).await;
    let app = router(test_state(
        &upstream.url,
        Arc::new(StaticCredentials::new("tok", "acct")),
    ));

    tx.send(Ok(Bytes::from(
        "data: {\"type\":\"response.created\",\"response\":{\"id\":\"r1\"}}\n\n",
    )))
    .await
    .unwrap();

    let resp = app
        .oneshot(chat_request(json!({
            "model": "gpt-5",
            "stream": true,
            "messages": [{"role": "user", "content": "hi"}],
        })))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Drop the response body — the "client disconnect".
    drop(resp);

    // The relay chain must tear down and close the upstream body channel.
    tokio::time::timeout(std::time::Duration::from_secs(5), tx.closed())
        .await
        .expect("upstream channel did not close after client disconnect");
}
