//! Request-transformation parity tests.
//!
//! Ports Go `internal/server/transform_responses_test.go`
//! (`TestTransformResponsesRequestBody` and
//! `TestTransformResponsesRequestBody_ModelSpecificReasoningClamp`) and adds
//! coverage for `build_codex_request_body`.

use codex_proxy_rs::prompts;
use codex_proxy_rs::request::{build_codex_request_body, transform_responses_request_body};
use pretty_assertions::assert_eq;
use serde_json::{Value, json};

// ---- transform_responses_request_body ----------------------------------

#[test]
fn responses_normalizes_model_effort_and_replaces_names() {
    let mut body = json!({
        "instructions": "Please greet Zed.",
        "input": [{
            "role": "user",
            "content": [{ "type": "input_text", "text": "Hello from Zed" }],
        }],
        "reasoning_effort": "none",
    });

    let (model, effort) =
        transform_responses_request_body(&mut body, "gpt-5-codex-preview", "none");

    assert_eq!(model, "gpt-5-codex");
    assert_eq!(effort, "low");

    // instructions is the explicit user instruction, NOT name-replaced and NOT
    // the canonical Codex-greeting form.
    let instr = body["instructions"].as_str().unwrap();
    assert!(!instr.is_empty());
    assert!(!instr.contains("Please greet Codex."));

    // user text had names replaced inside input.
    let found = body["input"].as_array().unwrap().iter().any(|msg| {
        msg["content"].as_array().is_some_and(|c| {
            c.first()
                .and_then(|item| item["text"].as_str())
                .is_some_and(|t| t == "Hello from Codex")
        })
    });
    assert!(
        found,
        "expected replaced user text in input: {:?}",
        body["input"]
    );

    assert_eq!(body["reasoning"]["effort"], json!("low"));
    assert_eq!(body["store"], json!(false));
    assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
    assert_eq!(body["tool_choice"], json!("auto"));
    assert_eq!(body["parallel_tool_calls"], json!(false));

    assert!(body.get("max_output_tokens").is_none());
    assert!(body.get("max_tokens").is_none());
    assert!(body.get("reasoning_effort").is_none());

    // every input entry is a message object, never a nested array.
    for entry in body["input"].as_array().unwrap() {
        assert!(!entry.is_array(), "input entry should be a message object");
    }
}

#[test]
fn responses_clamps_effort_per_model() {
    let base = || {
        json!({
            "instructions": "Do something.",
            "input": [{
                "role": "user",
                "content": [{ "type": "input_text", "text": "Hello" }],
            }],
        })
    };

    // explicit low clamps to medium on the mini model.
    let mut b1 = base();
    let (m1, e1) = transform_responses_request_body(&mut b1, "gpt-5.1-codex-mini", "low");
    assert_eq!(m1, "gpt-5.1-codex-mini");
    assert_eq!(e1, "medium");
    assert_eq!(b1["reasoning"]["effort"], json!("medium"));

    // no effort defaults to the model default (medium).
    let mut b2 = base();
    let (m2, e2) = transform_responses_request_body(&mut b2, "gpt-5.1-codex-mini", "");
    assert_eq!(m2, "gpt-5.1-codex-mini");
    assert_eq!(e2, "medium");
    assert_eq!(b2["reasoning"]["effort"], json!("medium"));

    // codex-max preserves xhigh.
    let mut b3 = base();
    let (m3, e3) = transform_responses_request_body(&mut b3, "gpt-5.1-codex-max", "xhigh");
    assert_eq!(m3, "gpt-5.1-codex-max");
    assert_eq!(e3, "xhigh");
    assert_eq!(b3["reasoning"]["effort"], json!("xhigh"));

    // codex-max defaults to low when unspecified.
    let mut b4 = base();
    let (m4, e4) = transform_responses_request_body(&mut b4, "gpt-5.1-codex-max", "");
    assert_eq!(m4, "gpt-5.1-codex-max");
    assert_eq!(e4, "low");
    assert_eq!(b4["reasoning"]["effort"], json!("low"));
}

// ---- build_codex_request_body ------------------------------------------

#[test]
fn build_codex_request_sets_static_fields_and_prepends_greeting() {
    let request = json!({
        "model": "gpt-5.2-high",
        "messages": [
            { "role": "system", "content": "You help with Zed." },
            { "role": "user", "content": "Fix the bug in Cursor" },
        ],
    });

    let body = build_codex_request_body(&request);

    assert_eq!(body["model"], json!("gpt-5.2"));
    assert_eq!(
        body["instructions"],
        json!(prompts::CODEX_INSTRUCTIONS_PREFIX)
    );
    assert_eq!(body["store"], json!(false));
    assert_eq!(body["stream"], json!(true));
    assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
    assert_eq!(body["tool_choice"], json!("auto"));
    assert_eq!(body["parallel_tool_calls"], json!(false));
    // gpt-5.2 with effort suffix -high resolves to high.
    assert_eq!(body["reasoning"]["effort"], json!("high"));

    let input = body["input"].as_array().unwrap();
    // [0] override greeting (developer / inverse prompt), [1] developer system,
    // [2] the user message.
    assert_eq!(input[0]["role"], json!("developer"));
    assert_eq!(
        input[0]["content"][0]["text"],
        json!(prompts::INVERSE_PROMPT)
    );
    assert_eq!(input[1]["role"], json!("developer"));
    // system text had names replaced.
    assert_eq!(
        input[1]["content"][0]["text"],
        json!("You help with Codex.")
    );
    assert_eq!(input[2]["role"], json!("user"));
    assert_eq!(
        input[2]["content"][0]["text"],
        json!("Fix the bug in Codex")
    );

    // a prompt_cache_key was derived (36-char UUID).
    let key = body["prompt_cache_key"].as_str().unwrap();
    assert_eq!(key.len(), 36);
}

#[test]
fn build_codex_request_maps_tools_and_no_tools_is_null() {
    let with_tools = json!({
        "model": "gpt-5",
        "messages": [{ "role": "user", "content": "hi" }],
        "tools": [{
            "type": "function",
            "function": {
                "name": "get_weather",
                "description": "Get weather",
                "parameters": { "type": "object" },
            },
        }, {
            "type": "other",
            "function": { "name": "ignored" },
        }],
    });
    let body = build_codex_request_body(&with_tools);
    let tools = body["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["type"], json!("function"));
    assert_eq!(tools[0]["name"], json!("get_weather"));
    assert_eq!(tools[0]["strict"], json!(false));
    assert_eq!(tools[0]["parameters"], json!({ "type": "object" }));

    let no_tools = json!({
        "model": "gpt-5",
        "messages": [{ "role": "user", "content": "hi" }],
    });
    let body = build_codex_request_body(&no_tools);
    assert_eq!(body["tools"], Value::Null);
}

#[test]
fn build_codex_request_maps_assistant_tool_calls_and_tool_output() {
    let request = json!({
        "model": "gpt-5",
        "messages": [
            { "role": "user", "content": "call the tool" },
            {
                "role": "assistant",
                "content": "",
                "tool_calls": [{
                    "id": "call_abc",
                    "function": { "name": "lookup", "arguments": "{\"q\":1}" },
                }],
            },
            { "role": "tool", "tool_call_id": "call_abc", "content": "result text" },
        ],
    });

    let body = build_codex_request_body(&request);
    let input = body["input"].as_array().unwrap();

    let fc = input
        .iter()
        .find(|m| m["type"] == json!("function_call"))
        .expect("function_call entry present");
    assert_eq!(fc["name"], json!("lookup"));
    assert_eq!(fc["call_id"], json!("call_abc"));
    assert_eq!(fc["arguments"], json!("{\"q\":1}"));

    let fco = input
        .iter()
        .find(|m| m["type"] == json!("function_call_output"))
        .expect("function_call_output entry present");
    assert_eq!(fco["call_id"], json!("call_abc"));
    assert_eq!(fco["output"], json!("result text"));
}
