use std::time::Duration;

use serde_json::{json, Value};

use super::request::{
    build_sse_headers, extract_account_id, resolve_codex_url, uses_local_codex_bearer,
};
use crate::env_api_keys::get_env_api_key;
use crate::providers::openai_responses_shared::{
    convert_responses_messages, ConvertResponsesMessagesOptions, OPENAI_TOOL_CALL_PROVIDERS,
};
use crate::types::{
    Context, Model, ProviderNativeCompactionOptions, ProviderNativeCompactionResult,
};
use crate::utils_inner::http::{send, HttpResponse, RequestOptions, Transport};
use crate::utils_inner::sse::SseDecoder;
use crate::utils_inner::stream_failure::{ConnectionErrorProfile, ProviderError};

pub const OPENAI_CODEX_COMPACTION_TIMEOUT_MS: u64 = 180_000;

pub async fn compact_openai_codex_responses(
    model: &Model,
    context: &Context,
    options: &ProviderNativeCompactionOptions,
) -> Result<ProviderNativeCompactionResult, ProviderError> {
    if model.api != super::API_OPENAI_CODEX_RESPONSES {
        return Err(ProviderError::Message(format!(
            "Mismatched api: {}",
            model.api
        )));
    }
    let timeout = options
        .base
        .timeout_ms
        .filter(|timeout| *timeout > 0)
        .unwrap_or(OPENAI_CODEX_COMPACTION_TIMEOUT_MS);
    let operation = compact(model, context, options);
    let timed = tokio::time::timeout(Duration::from_millis(timeout), operation);
    let result = if let Some(signal) = &options.base.signal {
        tokio::select! {
            biased;
            () = signal.cancelled() => return Err(ProviderError::Aborted),
            result = timed => result,
        }
    } else {
        timed.await
    };
    result.map_err(|_| {
        ProviderError::Message(format!(
            "OpenAI Codex compaction request timed out after {timeout}ms"
        ))
    })?
}

async fn compact(
    model: &Model,
    context: &Context,
    options: &ProviderNativeCompactionOptions,
) -> Result<ProviderNativeCompactionResult, ProviderError> {
    let key = options
        .base
        .api_key
        .clone()
        .filter(|key| !key.is_empty())
        .or_else(|| get_env_api_key(&model.provider))
        .ok_or_else(|| {
            ProviderError::Message(format!("No API key for provider: {}", model.provider))
        })?;
    let account = if uses_local_codex_bearer(model, &options.base) {
        String::new()
    } else {
        extract_account_id(&key).map_err(ProviderError::Message)?
    };
    let input = convert_responses_messages(
        model,
        context,
        &OPENAI_TOOL_CALL_PROVIDERS,
        ConvertResponsesMessagesOptions::default(),
    );
    let headers = build_sse_headers(
        model.headers.as_ref(),
        options.base.headers.as_ref(),
        &account,
        &key,
        options.base.session_id.as_deref(),
    );
    let mut triggered = input.clone();
    triggered.push(json!({"type": "compaction_trigger"}));
    let body = json!({"model": model.id, "input": triggered, "instructions": options.instructions, "store": false, "stream": true});
    let attempt = async {
        let mut response = request(
            model,
            options,
            resolve_codex_url(&model.base_url),
            headers.clone(),
            body,
        )
        .await?;
        parse_stream(&mut response, &model.provider, &input).await
    }
    .await;
    if let Ok(result) = attempt {
        return Ok(result);
    }
    if options
        .base
        .signal
        .as_ref()
        .is_some_and(tokio_util::sync::CancellationToken::is_cancelled)
    {
        return Err(ProviderError::Aborted);
    }
    let headers = headers
        .into_iter()
        .filter(|(name, _)| !name.eq_ignore_ascii_case("accept"))
        .collect();
    let mut response = request(
        model,
        options,
        format!("{}/compact", resolve_codex_url(&model.base_url)),
        headers,
        json!({"model": model.id, "input": input, "instructions": options.instructions}),
    )
    .await?;
    let body = response.read_all_text().await?;
    let data: Value = serde_json::from_str(&body).map_err(|error| {
        ProviderError::Message(format!("Invalid Codex compaction JSON: {error}"))
    })?;
    parse_legacy(&model.provider, &data)
}

async fn request(
    model: &Model,
    options: &ProviderNativeCompactionOptions,
    url: String,
    headers: Vec<(String, String)>,
    mut body: Value,
) -> Result<HttpResponse, ProviderError> {
    if let Some(hook) = &options.base.on_payload {
        if let Some(next) = hook(body.clone(), model) {
            body = next;
        }
    }
    let mut response = send(RequestOptions {
        method: reqwest::Method::POST,
        url,
        headers,
        body: Some(body.to_string()),
        signal: options.base.signal.clone(),
        timeout_ms: options.base.timeout_ms,
        connection: ConnectionErrorProfile::RawFetch,
        transport: Transport::Http1,
    })
    .await?;
    if let Some(hook) = &options.base.on_response {
        hook(
            crate::types::ProviderResponse {
                status: response.status,
                headers: response.headers.clone().into_iter().collect(),
            },
            model,
        );
    }
    if !(200..300).contains(&response.status) {
        let body = response.read_all_text().await.unwrap_or_default();
        return Err(ProviderError::from_http_status_body(
            response.status,
            &body,
            response.headers.clone(),
        ));
    }
    Ok(response)
}

fn is_compaction(item: &Value) -> bool {
    matches!(
        item.get("type").and_then(Value::as_str),
        Some("compaction" | "compaction_summary")
    ) && item
        .get("encrypted_content")
        .and_then(Value::as_str)
        .is_some_and(|text| !text.is_empty())
}

fn parse_legacy(
    provider: &str,
    data: &Value,
) -> Result<ProviderNativeCompactionResult, ProviderError> {
    let output = data
        .get("output")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            ProviderError::Message("OpenAI Codex compaction response missing output array".into())
        })?;
    let item = output
        .last()
        .filter(|item| is_compaction(item))
        .ok_or_else(|| {
            ProviderError::Message(
                "OpenAI Codex compaction response missing final compaction item".into(),
            )
        })?;
    let history = output
        .iter()
        .filter(|item| {
            is_compaction(item)
                || (item.get("type").and_then(Value::as_str) == Some("message")
                    && matches!(
                        item.get("role").and_then(Value::as_str),
                        Some("user" | "assistant")
                    ))
        })
        .cloned()
        .collect();
    Ok(ProviderNativeCompactionResult {
        provider: provider.into(),
        replacement_history: history,
        compaction_item: item.clone(),
    })
}

async fn parse_stream(
    response: &mut HttpResponse,
    provider: &str,
    input: &[Value],
) -> Result<ProviderNativeCompactionResult, ProviderError> {
    let mut decoder = SseDecoder::new();
    let mut items = Vec::new();
    let mut completed = false;
    loop {
        let chunk = response.next_text().await?;
        let events = match &chunk {
            Some(chunk) => decoder.push_text(chunk),
            None => decoder.finish(),
        };
        for event in events {
            if event.data.trim().is_empty() || event.data.trim() == "[DONE]" {
                continue;
            }
            let parsed: Value = serde_json::from_str(&event.data).map_err(|error| {
                ProviderError::Message(format!("Invalid Codex compaction SSE: {error}"))
            })?;
            match parsed.get("type").and_then(Value::as_str) {
                Some("error" | "response.failed" | "response.incomplete") => {
                    return Err(ProviderError::Message(format!(
                        "OpenAI Codex compaction stream failed: {parsed}"
                    )))
                }
                Some("response.completed" | "response.done") => {
                    completed = true;
                    break;
                }
                Some("response.output_item.done") => {
                    if let Some(item) = parsed.get("item").filter(|item| is_compaction(item)) {
                        items.push(item.clone());
                    }
                }
                _ => {}
            }
        }
        if completed || chunk.is_none() {
            break;
        }
    }
    if !completed {
        return Err(ProviderError::Message(
            "OpenAI Codex compaction stream closed before response.completed".into(),
        ));
    }
    if items.len() != 1 {
        return Err(ProviderError::Message(format!(
            "OpenAI Codex compaction expected one output item, received {}",
            items.len()
        )));
    }
    let item = items.pop().expect("one compaction item");
    let mut history = retain_user_messages(input);
    history.push(item.clone());
    Ok(ProviderNativeCompactionResult {
        provider: provider.into(),
        replacement_history: history,
        compaction_item: item,
    })
}

fn retain_user_messages(input: &[Value]) -> Vec<Value> {
    let mut remaining = 64_000 * 4;
    let mut retained = Vec::new();
    for item in input.iter().rev().filter(|item| {
        item.get("role").and_then(Value::as_str) == Some("user")
            && matches!(
                item.get("type").and_then(Value::as_str),
                None | Some("message")
            )
    }) {
        if remaining == 0 {
            break;
        }
        let count: usize = item
            .get("content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .map(|text| text.encode_utf16().count())
            .sum();
        if count.max(4) <= remaining {
            retained.push(item.clone());
            remaining -= count.max(4);
        } else {
            let mut item = item.clone();
            if let Some(parts) = item.get_mut("content").and_then(Value::as_array_mut) {
                parts.retain_mut(|part| {
                    let Some(text) = part.get("text").and_then(Value::as_str) else {
                        return true;
                    };
                    let mut budget = remaining;
                    let text: String = text
                        .chars()
                        .take_while(|c| {
                            let size = c.len_utf16();
                            if size > budget {
                                false
                            } else {
                                budget -= size;
                                true
                            }
                        })
                        .collect();
                    remaining = budget;
                    if text.is_empty() {
                        return false;
                    }
                    part["text"] = json!(text);
                    true
                });
            }
            retained.push(item);
            remaining = 0;
        }
    }
    retained.reverse();
    retained
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::test_http::ScriptedServer;
    use crate::types::{Message, StreamOptions, UserMessage, UserMessageContent};

    fn model(url: &str) -> Model {
        serde_json::from_value(json!({"id": "m", "name": "m", "api": "openai-codex-responses", "provider": "openai-codex", "baseUrl": url, "reasoning": true, "input": ["text"], "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0}, "contextWindow": 100_000, "maxTokens": 16_384})).unwrap()
    }

    fn options() -> ProviderNativeCompactionOptions {
        ProviderNativeCompactionOptions {
            base: StreamOptions {
                api_key: Some("local-secret".into()),
                headers: Some(
                    [("x-prime-local-codex-bearer".into(), "1".into())]
                        .into_iter()
                        .collect(),
                ),
                timeout_ms: Some(5_000),
                session_id: Some("session".into()),
                ..Default::default()
            },
            instructions: "retain the task".into(),
        }
    }

    fn context() -> Context {
        Context {
            system_prompt: Some("system".into()),
            messages: vec![Message::User(UserMessage {
                content: UserMessageContent::Text("active task".into()),
                timestamp: 1,
                rest: Default::default(),
            })],
            tools: None,
        }
    }

    #[tokio::test]
    async fn streaming_compaction_uses_trigger_and_replays_opaque_history() {
        let item = json!({"type": "compaction", "encrypted_content": "opaque"});
        let body = format!(
            "data: {}\n\ndata: {}\n\n",
            json!({"type": "response.output_item.done", "item": item}),
            json!({"type": "response.completed"})
        );
        let server = ScriptedServer::new(vec![(200, body, 0)]).await;
        let model = model(&server.url);
        let result = compact_openai_codex_responses(&model, &context(), &options())
            .await
            .unwrap();
        assert_eq!(result.replacement_history.len(), 2);
        assert_eq!(result.compaction_item, item);
        let requests = server.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].0.starts_with("POST /codex/responses "));
        assert!(!requests[0]
            .0
            .to_ascii_lowercase()
            .contains("x-prime-local-codex-bearer"));
        assert!(!requests[0]
            .0
            .to_ascii_lowercase()
            .contains("chatgpt-account-id"));
        assert_eq!(
            requests[0].1["input"].as_array().unwrap().last().unwrap()["type"],
            json!("compaction_trigger")
        );
        assert_eq!(requests[0].1["instructions"], json!("retain the task"));
    }

    #[tokio::test]
    async fn fallback_requests_legacy_compaction_after_stream_endpoint_failure() {
        let item = json!({"type": "compaction_summary", "encrypted_content": "opaque"});
        let server = ScriptedServer::new(vec![
            (404, "unsupported".into(), 0),
            (200, json!({"output": [item]}).to_string(), 0),
        ])
        .await;
        let result = compact_openai_codex_responses(&model(&server.url), &context(), &options())
            .await
            .unwrap();
        assert_eq!(result.compaction_item, item);
        let requests = server.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(requests[1].0.starts_with("POST /codex/responses/compact "));
        assert!(!requests[1]
            .0
            .to_ascii_lowercase()
            .contains("accept: text/event-stream"));
        assert!(requests[1].1.get("stream").is_none());
        assert_eq!(requests[1].1["input"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn one_deadline_bounds_both_compaction_attempts() {
        let server = ScriptedServer::new(vec![
            (404, "unsupported".into(), 20),
            (200, "{}".into(), 500),
        ])
        .await;
        let mut options = options();
        options.base.timeout_ms = Some(100);
        let result = compact_openai_codex_responses(&model(&server.url), &context(), &options)
            .await
            .unwrap_err();
        assert!(result.to_string().contains("timed out after 100ms"));
        assert_eq!(server.requests.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn cancellation_does_not_attempt_fallback() {
        let server = ScriptedServer::new(vec![(200, "{}".into(), 500)]).await;
        let signal = tokio_util::sync::CancellationToken::new();
        let mut options = options();
        options.base.signal = Some(signal.clone());
        let cancel = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(40)).await;
            signal.cancel();
        });
        let result = compact_openai_codex_responses(&model(&server.url), &context(), &options)
            .await
            .unwrap_err();
        cancel.await.unwrap();
        assert_eq!(result, ProviderError::Aborted);
        assert_eq!(server.requests.lock().unwrap().len(), 1);
    }

    #[test]
    fn legacy_compaction_preserves_only_replayable_items() {
        let item = json!({"type": "compaction_summary", "encrypted_content": "opaque"});
        let result = parse_legacy("openai-codex", &json!({"output": [null, {"type": "function_call"}, {"type": "message", "role": "user", "content": "task"}, item]})).unwrap();
        assert_eq!(result.replacement_history.len(), 2);
        assert_eq!(result.compaction_item, item);
        assert!(parse_legacy("openai-codex", &json!({"output": [null]})).is_err());
    }

    #[test]
    fn retains_latest_user_messages_with_bounded_text_and_images() {
        let image = json!({"type": "input_image", "image_url": "data:image/png;base64,AA"});
        let input = vec![
            json!({"role": "assistant", "content": []}),
            json!({"role": "user", "content": [{"type": "input_text", "text": "a".repeat(300_000)}, image]}),
            json!({"role": "user", "content": [{"type": "input_text", "text": "latest"}]}),
        ];
        let retained = retain_user_messages(&input);
        assert_eq!(retained.len(), 2);
        assert_eq!(
            retained[0]["content"][0]["text"].as_str().unwrap().len(),
            256_000 - 6
        );
        assert_eq!(retained[0]["content"][1], image);
        assert_eq!(retained[1], input[2]);
    }
}
