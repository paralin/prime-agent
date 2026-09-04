use super::openai_completions::{
    stream_openai_completions, stream_simple_openai_completions, OpenAICompletionsOptions,
};
use super::test_http::ScriptedServer;
use crate::env_api_keys::{get_api_key_env_vars, get_env_api_key};
use crate::fork_catalog::get_model;
use crate::models::get_supported_thinking_levels;
use crate::types::{
    Context, Model, ModelThinkingLevel, SimpleStreamOptions, StopReason, StreamOptions,
    ThinkingBudgets,
};
use serde_json::{json, Value};
use std::fmt::Write;

fn model() -> Model {
    get_model("merge-gateway", "zai/glm-5.3-flash")
        .unwrap()
        .clone()
}
fn sse(frames: Vec<Value>) -> String {
    let mut output = frames.into_iter().fold(String::new(), |mut output, frame| {
        write!(output, "data: {frame}\n\n").unwrap();
        output
    });
    output.push_str("data: [DONE]\n\n");
    output
}
#[test]
fn gateway_catalog_and_credentials_match_the_provider_contract() {
    assert_eq!(
        get_api_key_env_vars("merge-gateway"),
        Some(vec!["MERGE_GATEWAY_API_KEY"])
    );
    let original = std::env::var_os("MERGE_GATEWAY_API_KEY");
    std::env::set_var("MERGE_GATEWAY_API_KEY", "merge-test-key");
    let resolved = get_env_api_key("merge-gateway");
    match original {
        Some(value) => std::env::set_var("MERGE_GATEWAY_API_KEY", value),
        None => std::env::remove_var("MERGE_GATEWAY_API_KEY"),
    }
    assert_eq!(resolved.as_deref(), Some("merge-test-key"));
    let model = model();
    assert_eq!(model.base_url, "https://api-gateway.merge.dev/v1/ai-sdk");
    assert_eq!(model.api, "openai-completions");
    assert_eq!(
        get_supported_thinking_levels(&model),
        vec![
            ModelThinkingLevel::Low,
            ModelThinkingLevel::High,
            ModelThinkingLevel::Max
        ]
    );
    assert_eq!(model.cost.input, crate::types::JsNumber(0.015));
    assert_eq!(model.cost.output, crate::types::JsNumber(0.05));
}

#[tokio::test]
async fn selected_effort_sends_budget_and_exact_affinity_headers() {
    for (reasoning, budget, effort) in [
        (ModelThinkingLevel::Low, 1024, "low"),
        (ModelThinkingLevel::High, 4096, "high"),
        (ModelThinkingLevel::Max, 16384, "max"),
    ] {
        let server = ScriptedServer::new(vec![(
            200,
            sse(vec![
                json!({"choices":[{"delta":{"content":"ok"},"finish_reason":"stop"}]}),
            ]),
            0,
        )])
        .await;
        let mut model = model();
        model.base_url = format!("{}/v1/ai-sdk", server.url);
        let options = SimpleStreamOptions {
            base: StreamOptions {
                api_key: Some("merge-test-secret".into()),
                session_id: Some("merge-session".into()),
                ..Default::default()
            },
            reasoning: Some(reasoning),
            thinking_budgets: Some(ThinkingBudgets {
                low: Some(budget),
                high: Some(budget),
                max: Some(budget),
                ..Default::default()
            }),
            ..Default::default()
        };
        let message = stream_simple_openai_completions(
            &model,
            &Context {
                system_prompt: Some("Use tools.".into()),
                ..Default::default()
            },
            Some(&options),
        )
        .result()
        .await;
        assert_eq!(message.stop_reason, StopReason::Stop);
        let requests = server.requests.lock().unwrap();
        let (header, body) = &requests[0];
        let header = header.to_ascii_lowercase();
        assert!(header.starts_with("post /v1/ai-sdk/chat/completions "));
        assert!(header.contains("authorization: bearer merge-test-secret"));
        assert!(header.contains("x-session-affinity: merge-session"));
        assert!(header.contains("x-session-id: merge-session"));
        assert!(!header.contains("\r\nsession_id:"));
        assert!(!header.contains("x-client-request-id:"));
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["store"], false);
        assert_eq!(body["reasoning_effort"], effort);
        assert_eq!(
            body["thinking"],
            json!({"type":"enabled","budget_tokens":budget})
        );
    }
}

#[tokio::test]
async fn strict_gateway_streams_reject_missing_finish_late_output_and_incomplete_tools() {
    for frames in [
        vec![json!({"choices":[{"delta":{"content":"partial"},"finish_reason":null}]})],
        vec![
            json!({"choices":[{"delta":{"content":"Done."},"finish_reason":"stop"}]}),
            json!({"choices":[{"delta":{"content":" stale"},"finish_reason":null}]}),
        ],
        vec![
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"type":"function","function":{"arguments":"{}"}}]},"finish_reason":"tool_calls"}]}),
        ],
    ] {
        let server = ScriptedServer::new(vec![(200, sse(frames), 0)]).await;
        let mut model = model();
        model.base_url = server.url.clone();
        let message = stream_openai_completions(
            &model,
            &Context::default(),
            Some(&OpenAICompletionsOptions::from_base(StreamOptions {
                api_key: Some("merge-test-key".into()),
                ..Default::default()
            })),
        )
        .result()
        .await;
        assert_eq!(message.stop_reason, StopReason::Error);
        assert!(message.error_message.is_some());
    }
}

#[tokio::test]
async fn signed_thinking_tools_usage_and_terminal_warnings_survive_streaming() {
    let server = ScriptedServer::new(vec![(200, sse(vec![
        json!({"choices":[{"delta":{"thinking":"Plan","thinking_signature":"signed-plan"},"finish_reason":null}]}),
        json!({"choices":[{"delta":{"content":"Done.","tool_calls":[{"index":0,"id":"call_2","type":"function","function":{"name":"read","arguments":"{\"path\":\"next.txt\"}"}}]},"finish_reason":"tool_calls"}]}),
        json!({"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15,"cached_tokens":2},"warnings":[{"code":"reasoning_exhausted","message":"No answer","detail":{"model":"glm"}}]}),
    ]), 0)]).await;
    let mut model = model();
    model.base_url = server.url.clone();
    let signature = json!({"type":"openai-completions.chat_thinking_signature.v1","reasoningField":"thinking","signatureField":"thinking_signature","signature":"signed-inspection"}).to_string();
    let context: Context = serde_json::from_value(json!({
        "systemPrompt":"Use tools when available.",
        "messages":[
            {"role":"user","content":[{"type":"text","text":"Inspect this image."},{"type":"image","mimeType":"image/png","data":"iVBORw0KGgo="}],"timestamp":0},
            {"role":"assistant","content":[{"type":"thinking","thinking":"Inspecting the image.","thinkingSignature":signature},{"type":"text","text":"Let me look."},{"type":"toolCall","id":"call_1","name":"read","arguments":{"path":"image.png"}}],"api":"openai-completions","provider":"merge-gateway","model":"zai/glm-5.3-flash","usage":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"totalTokens":0,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}},"stopReason":"toolUse","timestamp":1},
            {"role":"toolResult","toolCallId":"call_1","toolName":"read","content":[{"type":"text","text":"image contents"}],"isError":false,"timestamp":2},
            {"role":"user","content":"Continue","timestamp":3}
        ],
        "tools":[{"name":"read","description":"Read a file","parameters":{"type":"object"}}]
    })).unwrap();
    let message = stream_openai_completions(
        &model,
        &context,
        Some(&OpenAICompletionsOptions::from_base(StreamOptions {
            api_key: Some("merge-test-key".into()),
            ..Default::default()
        })),
    )
    .result()
    .await;
    assert_eq!(message.stop_reason, StopReason::ToolUse);
    let value = serde_json::to_value(&message).unwrap();
    assert_eq!(value["content"][0]["thinking"], "Plan");
    assert!(value["content"][0]["thinkingSignature"]
        .as_str()
        .unwrap()
        .contains("signed-plan"));
    assert_eq!(value["content"][2]["arguments"], json!({"path":"next.txt"}));
    assert_eq!(message.usage.input, 8);
    assert_eq!(message.usage.output, 5);
    assert_eq!(message.usage.cache_read, 2);
    assert_eq!(message.usage.total_tokens, 15);
    assert_eq!(
        value["diagnostics"][0]["error"]["code"],
        "reasoning_exhausted"
    );
    assert_eq!(
        value["diagnostics"][0]["details"]["detail"],
        json!({"model":"glm"})
    );
    let requests = server.requests.lock().unwrap();
    let body = &requests[0].1;
    assert_eq!(body["messages"][0]["role"], "system");
    assert_eq!(body["messages"][2]["thinking"], "Inspecting the image.");
    assert_eq!(
        body["messages"][2]["thinking_signature"],
        "signed-inspection"
    );
    assert_eq!(body["messages"][2]["tool_calls"][0]["id"], "call_1");
    assert_eq!(body["messages"][3]["tool_call_id"], "call_1");
    assert_eq!(body["tools"][0]["function"]["strict"], false);
}
