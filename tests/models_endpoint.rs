mod support;

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use codex_proxy_rs::server::misc::MODELS_JSON;
use codex_proxy_rs::server::router;
use http_body_util::BodyExt;
use pretty_assertions::assert_eq;
use serde_json::Value;
use support::{MockUpstream, StaticCredentials, test_state};
use tower::ServiceExt;

#[tokio::test]
async fn models_served_verbatim_without_spark() {
    let upstream = MockUpstream::start(vec![]).await;
    let app = router(test_state(
        &upstream.url,
        Arc::new(StaticCredentials::new("t", "a")),
    ));

    let resp = app
        .oneshot(Request::get("/v1/models").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get(header::CONTENT_TYPE).unwrap(),
        "application/json"
    );

    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    // Served byte-for-byte from the embedded dump.
    assert_eq!(&bytes[..], MODELS_JSON.as_bytes());

    let parsed: Value = serde_json::from_str(MODELS_JSON).expect("valid json");
    assert_eq!(parsed["object"], "list");
    let data = parsed["data"].as_array().expect("data array");
    assert_eq!(data.len(), 54);

    let ids: Vec<&str> = data.iter().map(|m| m["id"].as_str().unwrap()).collect();
    // Spark is not-planned (ADR 004) and filtered from the dump.
    assert!(!ids.iter().any(|id| id.contains("spark")), "ids: {ids:?}");
    // Spot-check bases and effort variants.
    for expected in [
        "gpt-5",
        "gpt-5-high",
        "gpt-5.1-codex",
        "gpt-5.1-codex-max-xhigh",
        "gpt-5.2-codex-medium",
        "gpt-5-codex-mini-high",
    ] {
        assert!(ids.contains(&expected), "missing {expected}");
    }
    // Every entry is a model object.
    for m in data {
        assert_eq!(m["object"], "model");
    }
}
