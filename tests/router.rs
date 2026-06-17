mod support;

use std::sync::Arc;

use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode, header};
use codex_proxy_rs::affinity::{AffinityStore, InMemoryAffinityStore};
use codex_proxy_rs::config::ProxyMode;
use codex_proxy_rs::conversation::resolve_conversation_key;
use codex_proxy_rs::relay::RelayConfig;
use codex_proxy_rs::router::AccountPool;
use codex_proxy_rs::server::{AppState, router};
use http_body_util::BodyExt;
use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use support::{MockResponse, MockUpstream, StaticCredentials, TEST_ADMIN_KEY};
use tower::ServiceExt;

fn router_state(accounts: &str, affinity: Option<Arc<dyn AffinityStore>>) -> AppState {
    AppState {
        mode: ProxyMode::Router,
        // Router mode never uses credentials (the backend pods own those).
        creds: Arc::new(StaticCredentials::new("", "")),
        http: codex_proxy_rs::upstream::build_upstream_client(),
        relay: RelayConfig {
            keepalive_interval: std::time::Duration::from_millis(50),
        },
        upstream_url: "http://unused".into(),
        admin_api_key: Some(TEST_ADMIN_KEY.into()),
        accounts: Some(Arc::new(AccountPool::parse(accounts).unwrap())),
        affinity,
    }
}

fn chat_req(payload: &Value) -> Request<Body> {
    let mut req = Request::post("/v1/chat/completions")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(payload.to_string()))
        .unwrap();
    req.headers_mut().insert(
        header::AUTHORIZATION,
        format!("Bearer {TEST_ADMIN_KEY}").parse().unwrap(),
    );
    req
}

async fn body_string(body: Body) -> String {
    String::from_utf8(body.collect().await.unwrap().to_bytes().to_vec()).unwrap()
}

fn convo() -> Value {
    json!({
        "model": "gpt-5.4",
        "messages": [
            {"role": "system", "content": "sys"},
            {"role": "user", "content": "hello router"},
        ],
    })
}

#[tokio::test]
async fn proxies_to_an_account_pins_it_and_reuses_the_pin() {
    // Each pod answers up to two requests with its own body.
    let pod_a = MockUpstream::start(vec![
        MockResponse::Sse("data: from-a\n\n".into()),
        MockResponse::Sse("data: from-a\n\n".into()),
    ])
    .await;
    let pod_b = MockUpstream::start(vec![MockResponse::Sse("data: from-b\n\n".into())]).await;

    let affinity = Arc::new(InMemoryAffinityStore::default());
    let spec = format!("a={},b={}", pod_a.url, pod_b.url);
    let app = router(router_state(&spec, Some(affinity.clone())));

    // First request: round-robin picks the first account ("a") and pins it.
    let r1 = app.clone().oneshot(chat_req(&convo())).await.unwrap();
    assert_eq!(r1.status(), StatusCode::OK);
    assert_eq!(body_string(r1.into_body()).await, "data: from-a\n\n");

    // The conversation is now pinned to "a".
    let key = resolve_conversation_key(&HeaderMap::new(), &convo())
        .unwrap()
        .key;
    assert_eq!(affinity.get(&key).await.unwrap().slug, "a");

    // Second request with the same head reuses the pin → "a" again, not "b".
    let r2 = app.clone().oneshot(chat_req(&convo())).await.unwrap();
    assert_eq!(body_string(r2.into_body()).await, "data: from-a\n\n");

    assert_eq!(pod_a.hit_count(), 2, "both turns hit the pinned account");
    assert_eq!(pod_b.hit_count(), 0, "the other account is never touched");
}

#[tokio::test]
async fn repins_to_a_healthy_account_on_429() {
    // The first-picked account ("a") is rate-limited; "b" is healthy.
    let pod_a = MockUpstream::start(vec![MockResponse::Status(
        429,
        "{\"error\":\"rate\"}".into(),
    )])
    .await;
    let pod_b = MockUpstream::start(vec![MockResponse::Sse("data: from-b\n\n".into())]).await;

    let affinity = Arc::new(InMemoryAffinityStore::default());
    let spec = format!("a={},b={}", pod_a.url, pod_b.url);
    let app = router(router_state(&spec, Some(affinity.clone())));

    let resp = app.oneshot(chat_req(&convo())).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_string(resp.into_body()).await, "data: from-b\n\n");

    assert_eq!(pod_a.hit_count(), 1, "the 429'd account was tried once");
    assert_eq!(
        pod_b.hit_count(),
        1,
        "then re-pinned to the healthy account"
    );

    // The conversation is now pinned to the account that succeeded.
    let key = resolve_conversation_key(&HeaderMap::new(), &convo())
        .unwrap()
        .key;
    assert_eq!(affinity.get(&key).await.unwrap().slug, "b");
}

#[tokio::test]
async fn forwards_w3c_trace_context_to_the_pod() {
    // The pod's spans must nest in the router's trace, so traceparent is
    // forwarded (ADR 005). Query strings on the target are preserved too.
    let pod = MockUpstream::start(vec![MockResponse::Sse("data: ok\n\n".into())]).await;
    let app = router(router_state(&format!("a={}", pod.url), None));

    let mut req = chat_req(&convo());
    req.headers_mut().insert(
        "traceparent",
        "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01"
            .parse()
            .unwrap(),
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let captured = pod.headers.lock().await;
    assert_eq!(captured.len(), 1);
    assert!(
        captured[0].get("traceparent").is_some(),
        "traceparent must be forwarded so the pod span nests in the trace",
    );
}

#[tokio::test]
async fn routes_statelessly_without_an_affinity_store() {
    // No Redis configured → no pinning, but requests still proxy (ADR 006 §5c).
    let pod_a = MockUpstream::start(vec![MockResponse::Sse("data: from-a\n\n".into())]).await;
    let app = router(router_state(&format!("a={}", pod_a.url), None));

    let resp = app.oneshot(chat_req(&convo())).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_string(resp.into_body()).await, "data: from-a\n\n");
    assert_eq!(pod_a.hit_count(), 1);
}
