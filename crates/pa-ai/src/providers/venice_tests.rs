use super::openai_completions::{stream_openai_completions, OpenAICompletionsOptions};
use super::test_http::ScriptedServer;
use crate::env_api_keys::{get_api_key_env_vars, get_env_api_key};
use crate::fork_catalog::get_models;
use crate::types::{Context, StopReason, StreamOptions};

#[test]
fn resolves_the_documented_environment_key() {
    assert_eq!(get_api_key_env_vars("venice"), Some(vec!["VENICE_API_KEY"]));
    let original = std::env::var_os("VENICE_API_KEY");
    std::env::set_var("VENICE_API_KEY", "venice-test-key");
    let resolved = get_env_api_key("venice");
    match original {
        Some(value) => std::env::set_var("VENICE_API_KEY", value),
        None => std::env::remove_var("VENICE_API_KEY"),
    }
    assert_eq!(resolved.as_deref(), Some("venice-test-key"));
    let model = get_models("venice").into_iter().next().unwrap();
    assert_eq!(model.base_url, "https://api.venice.ai/api/v1");
    assert_eq!(model.api, "openai-completions");
}

#[tokio::test]
async fn streaming_preserves_finish_reasons_and_auth_errors_hide_credentials() {
    for (status, finish, expected) in [
        (200, "stop", StopReason::Stop),
        (200, "length", StopReason::Length),
        (401, "", StopReason::Error),
    ] {
        let body = if status == 200 {
            format!("data: {{\"choices\":[{{\"delta\":{{\"content\":\"Hello\"}},\"finish_reason\":null}}]}}\n\ndata: {{\"choices\":[{{\"delta\":{{\"content\":\" from Venice\"}},\"finish_reason\":\"{finish}\"}}]}}\n\ndata: [DONE]\n\n")
        } else {
            r#"{"error":{"message":"Invalid API key","type":"authentication_error"}}"#.into()
        };
        let server = ScriptedServer::new(vec![(status, body, 0)]).await;
        let mut model = get_models("venice").into_iter().next().unwrap().clone();
        model.base_url = server.url.clone();
        model.id = "stealth-ox-alpha".into();
        let stream = stream_openai_completions(
            &model,
            &Context::default(),
            Some(&OpenAICompletionsOptions::from_base(StreamOptions {
                api_key: Some("venice-test-secret".into()),
                ..Default::default()
            })),
        );
        let message = stream.result().await;
        assert_eq!(message.stop_reason, expected);
        assert!(!serde_json::to_string(&message)
            .unwrap()
            .contains("venice-test-secret"));
        let requests = server.requests.lock().unwrap();
        assert!(requests[0].0.starts_with("POST /chat/completions "));
        assert!(requests[0]
            .0
            .to_ascii_lowercase()
            .contains("authorization: bearer venice-test-secret"));
        assert_eq!(requests[0].1["model"], "stealth-ox-alpha");
    }
}
