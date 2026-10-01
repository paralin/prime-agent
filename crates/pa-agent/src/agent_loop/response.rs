//! Streaming one assistant response (TS `streamAssistantResponse`): context
//! transform, LLM-bound message conversion, the model stream event loop, and
//! the aborted-message finalize path. Section of the port of
//! `packages/agent/src/agent-loop.ts`.

use std::sync::Arc;

use super::progress::TurnProgress;
use crate::abort::{is_abort_error, AbortController, AbortSignal};
use crate::repetition_detector::{RepetitionDetector, DEFAULT_REPETITION_THRESHOLD};
use crate::stream::{LlmContext, StreamFn, StreamRequestOptions, ToolDefinition};
use crate::types::StopReason;
use crate::types::{AgentContext, AgentEvent, AgentMessage, AssistantMessage};

use super::abort::{create_aborted_assistant_message, race_with_abort};
use super::{AgentEventSink, AgentLoopConfig};

// ---------------------------------------------------------------------------
// Streaming one assistant response
// ---------------------------------------------------------------------------

/// Port of `streamAssistantResponse`.
pub(crate) async fn stream_assistant_response(
    context: &mut AgentContext,
    config: &AgentLoopConfig,
    signal: Option<&AbortSignal>,
    emit: &AgentEventSink,
    stream_fn: Option<&StreamFn>,
    progress: &mut TurnProgress,
) -> anyhow::Result<(AssistantMessage, bool)> {
    for attempt in 0..3 {
        let mut partial_event = None;
        let mut added_partial = false;
        let controller = AbortController::new();
        let forwarding = signal.cloned().map(|signal| {
            let controller = controller.clone();
            tokio::spawn(async move {
                signal.aborted().await;
                controller.abort();
            })
        });
        let guard = RequestGuard {
            controller: controller.clone(),
            forwarding,
        };
        let result = stream_assistant_response_inner(
            context,
            config,
            signal,
            emit,
            stream_fn,
            &mut partial_event,
            &mut added_partial,
            &controller.signal(),
            progress,
        )
        .await;
        drop(guard);
        match result {
            Ok(message) => return Ok(message),
            Err(error) => {
                let aborted = signal.is_some_and(AbortSignal::is_aborted) && is_abort_error(&error);
                let stalled = error.downcast_ref::<StreamStallError>().is_some();
                let repetition = error.downcast_ref::<RepetitionLoopError>().is_some();
                if !aborted && !stalled && !repetition {
                    return Err(error);
                }
                if stalled && attempt < 2 && !signal.is_some_and(AbortSignal::is_aborted) {
                    if added_partial {
                        context.messages.pop();
                    }
                    continue;
                }
                let mut message = create_aborted_assistant_message(
                    config,
                    partial_event.as_deref().and_then(event_partial),
                );
                if !aborted {
                    message.stop_reason = StopReason::Error;
                    if repetition {
                        let threshold = repetition_threshold(config);
                        let error = format!("Repetition loop detected: the model repeated the same output {threshold}+ times; the provider request was cancelled");
                        message.error_message = Some(error.clone());
                        message.diagnostics =
                            Some(vec![crate::types::assistant_message_diagnostic(
                                "agent_repetition_loop",
                                &anyhow::anyhow!(error),
                                Some(serde_json::json!({"threshold":threshold})),
                            )]);
                    } else {
                        message.error_message =
                            Some(format!("{error}; gave up after {} attempts", attempt + 1));
                    }
                }
                return publish_response(context, config, emit, message, added_partial).await;
            }
        }
    }
    unreachable!("the final stream attempt always returns")
}

struct RequestGuard {
    controller: AbortController,
    forwarding: Option<tokio::task::JoinHandle<()>>,
}
impl Drop for RequestGuard {
    fn drop(&mut self) {
        self.controller.abort();
        if let Some(forwarding) = &self.forwarding {
            forwarding.abort();
        }
    }
}
#[derive(Debug, thiserror::Error)]
#[error("No model output for {0}ms")]
struct StreamStallError(u64);
#[derive(Debug, thiserror::Error)]
#[error("Repetition loop detected")]
struct RepetitionLoopError;

fn repetition_threshold(config: &AgentLoopConfig) -> usize {
    config
        .repetition_loop
        .as_ref()
        .map_or(DEFAULT_REPETITION_THRESHOLD, |options| {
            if options.enabled == Some(false) {
                0
            } else {
                options.threshold.unwrap_or(DEFAULT_REPETITION_THRESHOLD)
            }
        })
}
async fn with_timeout<T>(
    future: impl std::future::Future<Output = anyhow::Result<T>>,
    config: &AgentLoopConfig,
    signal: Option<&AbortSignal>,
) -> anyhow::Result<T> {
    let timeout = config.stream_stall_timeout_ms.unwrap_or(180_000);
    race_with_abort(
        async {
            tokio::time::timeout(std::time::Duration::from_millis(timeout), future)
                .await
                .map_err(|_| anyhow::Error::new(StreamStallError(timeout)))?
        },
        signal,
    )
    .await
}

/// Inner body of `streamAssistantResponse` (the TS `try` block).
// Direct port of the TS `try` block; refactoring is out of scope for this
// zero-behavior-change sweep.
#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
async fn stream_assistant_response_inner(
    context: &mut AgentContext,
    config: &AgentLoopConfig,
    signal: Option<&AbortSignal>,
    emit: &AgentEventSink,
    stream_fn: Option<&StreamFn>,
    partial_event: &mut Option<Arc<crate::stream::AssistantMessageEvent>>,
    added_partial: &mut bool,
    request_signal: &AbortSignal,
    progress: &mut TurnProgress,
) -> anyhow::Result<(AssistantMessage, bool)> {
    crate::abort::throw_if_aborted_signal(signal)?;

    let mut messages: Vec<AgentMessage> = context.messages.clone();
    if let Some(transform) = config.transform_context.as_ref() {
        messages = race_with_abort(
            transform(messages, signal.cloned().unwrap_or_default()),
            signal,
        )
        .await?;
    }

    let llm_messages = race_with_abort((config.convert_to_llm)(messages), signal).await?;

    let stream_fn = stream_fn.ok_or_else(|| {
        anyhow::anyhow!(
            "No stream function provided; the agent loop requires a model stream function (pa-ai integration supplies the default)"
        )
    })?;

    let resolved_api_key = match config.get_api_key.as_ref() {
        Some(get_api_key) => {
            match race_with_abort(get_api_key(config.model.provider.clone()), signal).await? {
                Some(key) => Some(key),
                None => config.api_key.clone(),
            }
        }
        None => config.api_key.clone(),
    };

    let llm_context = LlmContext {
        system_prompt: Some(
            config
                .get_system_prompt
                .as_ref()
                .map_or_else(|| context.system_prompt.clone(), |hook| hook()),
        ),
        messages: llm_messages,
        tools: context
            .tools
            .iter()
            .map(|tool| ToolDefinition {
                name: tool.name().to_string(),
                description: tool.description().to_string(),
                parameters: tool.parameters().clone(),
            })
            .collect(),
    };

    let options = StreamRequestOptions {
        temperature: config.temperature,
        max_tokens: config.max_tokens,
        reasoning: config.reasoning,
        session_id: config.session_id.clone(),
        service_tier: config.service_tier,
        api_key: resolved_api_key,
        signal: request_signal.clone(),
        // The TS loop config extends `SimpleStreamOptions`, so its own
        // `onPayload`/`onResponse` ride every stream call; the Rust loop
        // config carries no hooks yet, and the request-timing seam wrapper
        // composes them per request at the `StreamFn` boundary instead.
        on_payload: None,
        on_response: None,
    };

    let mut response = with_timeout(
        stream_fn(config.model.clone(), llm_context, options),
        config,
        signal,
    )
    .await?;

    let threshold = repetition_threshold(config);
    let mut repetition = RepetitionDetector::new(threshold);
    loop {
        let next =
            match with_timeout(async { Ok(response.next_event().await) }, config, signal).await {
                Ok(next) => next,
                Err(error) => {
                    response.close();
                    return Err(error);
                }
            };
        let Some(event) = next else {
            break;
        };

        match event {
            crate::stream::AssistantMessageEvent::Start { partial } => {
                let message = AgentMessage::from(partial.clone());
                *added_partial = true;
                context.messages.push(message.clone());
                *partial_event = Some(Arc::new(crate::stream::AssistantMessageEvent::Start {
                    partial,
                }));
                emit(AgentEvent::MessageStart { message }).await?;
            }
            event if event.is_delta() => {
                let delta = match &event {
                    crate::stream::AssistantMessageEvent::TextDelta { delta, .. }
                    | crate::stream::AssistantMessageEvent::ThinkingDelta { delta, .. } => {
                        Some(delta)
                    }
                    _ => None,
                };
                if threshold > 0 && delta.is_some_and(|delta| repetition.observe_text(delta)) {
                    response.close();
                    return Err(anyhow::Error::new(RepetitionLoopError));
                }
                let event = Arc::new(event);
                if let Some(partial) = event_partial(&event) {
                    *partial_event = Some(Arc::clone(&event));
                    emit(AgentEvent::MessageUpdate {
                        message: Arc::new(AgentMessage::from(partial.clone())),
                        assistant_message_event: event,
                    })
                    .await?;
                }
            }
            ref event if event.terminal_message().is_some() => {
                let mut final_message = event.terminal_message().unwrap().clone();
                match with_timeout(response.result(), config, signal).await {
                    Ok(result_message) => final_message = result_message,
                    Err(error) => {
                        let aborted = signal.is_some_and(AbortSignal::is_aborted);
                        if !(aborted && is_abort_error(&error)) {
                            response.close();
                            return Err(error);
                        }
                    }
                }
                final_message = progress.finalize(final_message, config.repetition_loop.as_ref());
                return publish_response(context, config, emit, final_message, *added_partial)
                    .await;
            }
            _ => {}
        }
    }

    // Stream ended without a terminal event: resolve the final message (TS
    // awaits `response.result()` here too; a stream that ends cleanly always
    // pushed done/error first).
    let final_message = progress.finalize(
        with_timeout(response.result(), config, signal).await?,
        config.repetition_loop.as_ref(),
    );
    publish_response(context, config, emit, final_message, *added_partial).await
}

async fn publish_response(
    context: &mut AgentContext,
    config: &AgentLoopConfig,
    emit: &AgentEventSink,
    mut message: AssistantMessage,
    added_partial: bool,
) -> anyhow::Result<(AssistantMessage, bool)> {
    if let Some(filter) = &config.filter_assistant_message {
        if let Some(filtered) = filter(message.clone())? {
            message = filtered;
        } else {
            if added_partial {
                context.messages.pop();
            }
            message.content.clear();
            return Ok((message, false));
        }
    }
    if added_partial {
        *context.messages.last_mut().unwrap() = message.clone().into();
    } else {
        context.messages.push(message.clone().into());
        emit(AgentEvent::MessageStart {
            message: message.clone().into(),
        })
        .await?;
    }
    emit(AgentEvent::MessageEnd {
        message: message.clone().into(),
    })
    .await?;
    Ok((message, true))
}

fn event_partial(event: &crate::stream::AssistantMessageEvent) -> Option<&AssistantMessage> {
    match event {
        crate::stream::AssistantMessageEvent::Start { partial }
        | crate::stream::AssistantMessageEvent::TextStart { partial, .. }
        | crate::stream::AssistantMessageEvent::TextDelta { partial, .. }
        | crate::stream::AssistantMessageEvent::TextEnd { partial, .. }
        | crate::stream::AssistantMessageEvent::ThinkingStart { partial, .. }
        | crate::stream::AssistantMessageEvent::ThinkingDelta { partial, .. }
        | crate::stream::AssistantMessageEvent::ThinkingEnd { partial, .. }
        | crate::stream::AssistantMessageEvent::ToolCallStart { partial, .. }
        | crate::stream::AssistantMessageEvent::ToolCallDelta { partial, .. }
        | crate::stream::AssistantMessageEvent::ToolCallEnd { partial, .. } => Some(partial),
        _ => None,
    }
}
