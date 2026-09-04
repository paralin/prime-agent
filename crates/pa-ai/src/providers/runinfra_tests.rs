use crate::env_api_keys::{get_api_key_env_vars, get_env_api_key};
use crate::fork_catalog::{get_model, get_models};
use crate::types::{Context, ModelInput, StopReason, StreamOptions};

use super::openai_completions::{stream_openai_completions, OpenAICompletionsOptions};
use super::test_http::ScriptedServer;

#[test]
fn documented_models_and_environment_key_are_registered() {
    assert_eq!(
        get_api_key_env_vars("runinfra"),
        Some(vec!["RUNINFRA_GATEWAY_KEY"])
    );
    assert_eq!(
        get_models("runinfra")
            .iter()
            .map(|model| model.id.as_str())
            .collect::<Vec<_>>(),
        vec![
            "deepseek-v4-flash",
            "deepseek-v4-pro",
            "glm-5-3-flash",
            "nemotron-3-5-lightning-30b",
            "ornith-1-5-35b",
            "qwen3-8-2-4t-a95b",
            "qwen3-8-27b"
        ]
    );
    let model = get_model("runinfra", "deepseek-v4-flash").unwrap();
    assert_eq!(model.api, "openai-completions");
    assert_eq!(model.base_url, "https://api.runinfra.ai/v1");
    assert_eq!(model.context_window, 1_048_576);
    assert_eq!(model.max_tokens, 1_048_576);
    assert!(get_model("runinfra", "qwen3-8-27b")
        .unwrap()
        .input
        .contains(&ModelInput::Image));
    let original = std::env::var_os("RUNINFRA_GATEWAY_KEY");
    std::env::set_var("RUNINFRA_GATEWAY_KEY", "test-runinfra-key");
    let resolved = get_env_api_key("runinfra");
    match original {
        Some(value) => std::env::set_var("RUNINFRA_GATEWAY_KEY", value),
        None => std::env::remove_var("RUNINFRA_GATEWAY_KEY"),
    }
    assert_eq!(resolved.as_deref(), Some("test-runinfra-key"));
}

#[tokio::test]
async fn streaming_requests_use_bearer_auth_and_the_system_role() {
    let body = "data: {\"choices\":[{\"delta\":{\"content\":\"ready\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
    let server = ScriptedServer::new(vec![(200, body.into(), 0)]).await;
    let mut model = get_model("runinfra", "glm-5-3-flash").unwrap().clone();
    model.base_url = format!("{}/v1", server.url);
    let message = stream_openai_completions(
        &model,
        &Context {
            system_prompt: Some("be terse".into()),
            ..Default::default()
        },
        Some(&OpenAICompletionsOptions::from_base(StreamOptions {
            api_key: Some("test-runinfra-key".into()),
            max_tokens: Some(128),
            ..Default::default()
        })),
    )
    .result()
    .await;
    assert_eq!(message.stop_reason, StopReason::Stop);
    let requests = server.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].0.starts_with("POST /v1/chat/completions "));
    assert!(requests[0]
        .0
        .to_ascii_lowercase()
        .contains("authorization: bearer test-runinfra-key"));
    assert_eq!(requests[0].1["messages"][0]["role"], "system");
    assert_eq!(requests[0].1["max_tokens"], 128);
    assert!(requests[0].1.get("store").is_none());
}
