use std::sync::{
    atomic::{AtomicU16, Ordering},
    Arc,
};

use serde_json::json;

use crate::event_stream::{
    create_assistant_message_event_stream, AssistantMessageEvent, AssistantMessageEventStream,
};
use crate::types::{Context, Model, ModelCompat, SimpleStreamOptions, StopReason};

pub(crate) fn stream(
    model: &Model,
    context: &Context,
    options: &SimpleStreamOptions,
) -> AssistantMessageEventStream {
    let model = model.clone();
    let context = context.clone();
    let mut options = options.clone();
    options.open_router_responses = Some(false);
    let (writer, reader) = create_assistant_message_event_stream();
    tokio::spawn(async move {
        let mut responses_model = model.clone();
        responses_model.api = super::openai_responses::API_OPENAI_RESPONSES.into();
        let mut raw = json!({"sendSessionIdHeader": false, "supportsLongCacheRetention": false})
            .as_object()
            .unwrap()
            .clone();
        if let Some(routing) = model
            .compat
            .as_ref()
            .and_then(|compat| compat.raw.get("openRouterRouting"))
        {
            raw.insert("openRouterRouting".into(), routing.clone());
        }
        responses_model.compat = Some(ModelCompat { raw });
        let status = Arc::new(AtomicU16::new(0));
        let captured = status.clone();
        let mut responses_options = options.clone();
        let previous = responses_options.base.on_response.clone();
        responses_options.base.on_response = Some(Arc::new(move |response, model| {
            captured.store(response.status, Ordering::Relaxed);
            if let Some(hook) = &previous {
                hook(response, model);
            }
        }));
        let mut attempt = super::openai_responses::stream_simple_openai_responses(
            &responses_model,
            &context,
            Some(&responses_options),
        );
        let mut started = false;
        let mut fallback = false;
        while let Some(event) = attempt.next_event().await {
            if matches!(event, AssistantMessageEvent::Start { .. }) {
                started = true;
            }
            if let AssistantMessageEvent::Error { error, .. } = &event {
                if !started
                    && error.stop_reason != StopReason::Aborted
                    && !options
                        .base
                        .signal
                        .as_ref()
                        .is_some_and(tokio_util::sync::CancellationToken::is_cancelled)
                {
                    fallback = should_fallback(
                        status.load(Ordering::Relaxed),
                        error.error_message.as_deref().unwrap_or(""),
                        error
                            .diagnostics
                            .as_ref()
                            .and_then(|diagnostics| {
                                diagnostics
                                    .iter()
                                    .rev()
                                    .find(|entry| entry.type_ == "provider_stream_failure")
                            })
                            .and_then(|entry| entry.details.as_ref())
                            .and_then(|details| details.get("kind"))
                            .and_then(serde_json::Value::as_str),
                    );
                    if fallback {
                        break;
                    }
                }
            }
            writer.push(event);
        }
        if fallback {
            let mut attempt = super::openai_completions::stream_simple_openai_completions(
                &model,
                &context,
                Some(&options),
            );
            while let Some(event) = attempt.next_event().await {
                writer.push(event);
            }
        }
        writer.end(None);
    });
    reader
}

fn should_fallback(status: u16, message: &str, kind: Option<&str>) -> bool {
    if matches!(status, 404 | 405 | 501)
        || matches!(kind, Some("server_error" | "malformed_response"))
    {
        return true;
    }
    let lower = message.to_ascii_lowercase();
    let invalid_request = kind == Some("invalid_request")
        || lower.contains("invalid_request")
        || lower.contains("invalid request");
    invalid_request
        && (lower.contains("response") || lower.contains("endpoint"))
        && ["unsupported", "not supported", "unavailable", "not found"]
            .iter()
            .any(|text| lower.contains(text))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::test_http::ScriptedServer;
    use crate::types::StreamOptions;

    async fn run_attempts(
        script: Vec<(u16, String, u64)>,
    ) -> (Vec<AssistantMessageEvent>, ScriptedServer) {
        let server = ScriptedServer::new(script).await;
        let model: Model = serde_json::from_value(json!({
            "id":"test", "name":"test", "api":"openai-completions", "provider":"openrouter",
            "baseUrl":server.url, "reasoning":false, "input":["text"],
            "cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0}, "contextWindow":10000,"maxTokens":1000
        })).unwrap();
        let context = Context {
            system_prompt: None,
            messages: vec![],
            tools: None,
        };
        let mut options = SimpleStreamOptions::from_base(StreamOptions {
            api_key: Some("test".into()),
            session_id: Some("conversation".into()),
            ..Default::default()
        });
        options.open_router_responses = Some(true);
        let events = stream(&model, &context, &options).collect().await;
        (events, server)
    }

    #[tokio::test]
    async fn unsupported_responses_falls_back_before_start() {
        let chat = "data: {\"choices\":[{\"delta\":{\"content\":\"answer\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
        let (events, server) = run_attempts(vec![
            (404, "missing route".into(), 0),
            (200, chat.into(), 0),
        ])
        .await;
        assert!(matches!(
            events.last(),
            Some(AssistantMessageEvent::Done { .. })
        ));
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, AssistantMessageEvent::Start { .. }))
                .count(),
            1
        );
        let requests = server.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(requests[0].0.starts_with("POST /responses "));
        assert!(requests[1].0.starts_with("POST /chat/completions "));
        assert_eq!(requests[0].1["session_id"], "conversation");
        assert_eq!(requests[1].1["session_id"], "conversation");
    }

    #[tokio::test]
    async fn auth_errors_and_errors_after_start_do_not_fallback() {
        for (status, body) in [(401, "unauthorized"), (200, "data: {\"type\":\"response.created\",\"response\":{\"id\":\"r\"}}\n\ndata: {\"type\":\"error\",\"code\":\"server_error\",\"message\":\"server_error\"}\n\n")] {
            let (events, server) = run_attempts(vec![(status, body.into(), 0)]).await;
            assert!(matches!(events.last(), Some(AssistantMessageEvent::Error { .. })));
            assert_eq!(server.requests.lock().unwrap().len(), 1);
        }
    }

    #[test]
    fn fallback_is_limited_to_transport_support_failures() {
        assert!(should_fallback(404, "missing route", None));
        assert!(should_fallback(
            400,
            "invalid_request_error: Responses endpoint is not supported",
            None
        ));
        assert!(should_fallback(500, "failure", Some("server_error")));
        assert!(!should_fallback(429, "rate limited", None));
        assert!(!should_fallback(401, "unauthorized", None));
        assert!(!should_fallback(
            400,
            "invalid_request_error: bad tool schema",
            None
        ));
    }
    #[tokio::test]
    async fn chat_is_default_and_responses_preserves_stateless_request_metadata() {
        for enabled in [false, true] {
            let body = if enabled {
                "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r\",\"status\":\"completed\",\"usage\":{\"input_tokens\":4,\"output_tokens\":1,\"total_tokens\":5}}}\n\n"
            } else {
                "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n"
            };
            let server = ScriptedServer::new(vec![(200, body.into(), 0)]).await;
            let model: Model = serde_json::from_value(json!({
                "id":"openai/gpt-5.6-luna", "name":"Luna", "api":"openai-completions", "provider":"openrouter",
                "baseUrl":server.url, "reasoning":true,"thinkingLevelMap":{"high":"high"}, "input":["text"],
                "cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0}, "contextWindow":1_050_000,"maxTokens":128_000,
                "compat":{"thinkingFormat":"openrouter","openRouterRouting":{"order":["openai","azure"]}}
            })).unwrap();
            let context: Context = serde_json::from_value(json!({"systemPrompt":"Use tools.","messages":[{"role":"user","content":"Reply briefly.","timestamp":1}],"tools":[{"name":"read","description":"Read one file","parameters":{"type":"object","properties":{"path":{"type":"string"}}}}]})).unwrap();
            let mut options = SimpleStreamOptions::from_base(StreamOptions {
                api_key: Some("test".into()),
                session_id: Some("conversation".into()),
                headers: Some(std::collections::HashMap::from([(
                    "X-OpenRouter-Title".into(),
                    "Local override".into(),
                )])),
                ..Default::default()
            });
            options.open_router_responses = Some(enabled);
            options.reasoning = Some(crate::types::ModelThinkingLevel::High);
            let result = super::super::openai_completions::stream_simple_openai_completions(
                &model,
                &context,
                Some(&options),
            )
            .result()
            .await;
            assert_eq!(result.stop_reason, StopReason::Stop);
            let requests = server.requests.lock().unwrap();
            assert_eq!(requests.len(), 1);
            let (header, body) = &requests[0];
            let header = header.to_ascii_lowercase();
            assert!(
                header.contains("http-referer: https://github.com/primeintellect-ai/prime-agent")
            );
            assert!(header.contains("x-openrouter-title: local override"));
            assert!(header.contains("x-openrouter-categories: cli-agent"));
            assert_eq!(body["session_id"], "conversation");
            if enabled {
                assert!(header.starts_with("post /responses "));
                assert_eq!(body["prompt_cache_key"], "conversation");
                assert_eq!(body["provider"], json!({"order":["openai","azure"]}));
                assert_eq!(body["store"], false);
                assert_eq!(body["reasoning"], json!({"effort":"high","summary":"auto"}));
                assert_eq!(body["tools"][0]["name"], "read");
                assert_eq!(body["input"].as_array().unwrap().len(), 2);
            } else {
                assert!(header.starts_with("post /chat/completions "));
            }
        }
    }

    #[tokio::test]
    async fn context_overflow_and_rate_limits_do_not_fallback() {
        for status in [400, 429] {
            let (events, server) = run_attempts(vec![(
                status,
                "context_length_exceeded or rate limit".into(),
                0,
            )])
            .await;
            assert!(matches!(
                events.last(),
                Some(AssistantMessageEvent::Error { .. })
            ));
            assert_eq!(server.requests.lock().unwrap().len(), 1);
        }
    }
}
