use pa_agent::abort::AbortController;
use pa_agent::agent_loop::{run_agent_loop, AgentEventSink, AgentLoopConfig, RepetitionLoopConfig};
use pa_agent::scripted::{text_turn_steps, ScriptStep, ScriptedProvider, ScriptedTurn};
use pa_agent::stream::{AssistantMessageEvent, StreamFn};
use pa_agent::types::{
    AgentContext, AgentEvent, AgentMessage, AssistantMessage, Message, Model, StopReason,
};
use serde_json::json;
use std::sync::{Arc, Mutex};

fn model() -> Model {
    Model::unknown()
}
fn config() -> AgentLoopConfig {
    AgentLoopConfig::new(model(), AgentLoopConfig::default_convert_to_llm())
}
fn sink() -> (AgentEventSink, Arc<Mutex<Vec<AgentEvent>>>) {
    let events = Arc::new(Mutex::new(Vec::new()));
    let captured = events.clone();
    (
        Arc::new(move |event| {
            captured.lock().unwrap().push(event);
            Box::pin(async { Ok(()) })
        }),
        events,
    )
}
fn final_message(messages: &[AgentMessage]) -> &AssistantMessage {
    let AgentMessage::Standard(Message::Assistant(message)) = messages.last().unwrap() else {
        panic!("assistant expected");
    };
    message
}
fn response(reason: StopReason, text: &str) -> ScriptedTurn {
    let mut steps = text_turn_steps(&model(), text);
    let ScriptStep::Event(event) = steps.last_mut().unwrap() else {
        panic!("terminal event expected");
    };
    let AssistantMessageEvent::Done {
        reason: event_reason,
        message,
    } = &mut **event
    else {
        panic!("done expected");
    };
    *event_reason = reason;
    message.stop_reason = reason;
    ScriptedTurn::Events(steps)
}

#[tokio::test]
async fn incomplete_responses_continue_with_context_and_stop_after_three() {
    for reason in [StopReason::Unknown, StopReason::Stop, StopReason::Length] {
        let provider = Arc::new(ScriptedProvider::new(model()));
        provider.push_turn(response(reason, ""));
        provider.push_text_turn("complete");
        let (emit, _) = sink();
        let messages = run_agent_loop(
            vec![AgentMessage::user("finish")],
            AgentContext::default(),
            &config(),
            emit,
            None,
            Some(&provider.stream_fn()),
        )
        .await
        .unwrap();
        assert_eq!(provider.calls().len(), 2);
        assert_eq!(provider.calls()[1].messages.len(), 2);
        assert_eq!(final_message(&messages).stop_reason, StopReason::Stop);
        let provider = Arc::new(ScriptedProvider::new(model()));
        for _ in 0..3 {
            provider.push_turn(response(reason, ""));
        }
        let (emit, events) = sink();
        let messages = run_agent_loop(
            vec![AgentMessage::user("finish")],
            AgentContext::default(),
            &config(),
            emit,
            None,
            Some(&provider.stream_fn()),
        )
        .await
        .unwrap();
        assert_eq!(provider.calls().len(), 3);
        assert_eq!(final_message(&messages).stop_reason, StopReason::Error);
        assert!(final_message(&messages)
            .error_message
            .as_deref()
            .unwrap()
            .contains("3 consecutive"));
        let ended = events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|event| match event {
                AgentEvent::MessageEnd {
                    message: AgentMessage::Standard(Message::Assistant(message)),
                } => Some(message.stop_reason),
                _ => None,
            })
            .next_back();
        assert_eq!(ended, Some(StopReason::Error));
    }
}

#[tokio::test]
async fn stalled_attempts_cancel_and_rollback_partial_context() {
    for stalls in [1, 3] {
        let provider = Arc::new(ScriptedProvider::new(model()));
        for _ in 0..stalls {
            provider.push_stalled_turn("abandoned partial");
        }
        if stalls == 1 {
            provider.push_text_turn("complete");
        }
        let mut config = config();
        config.stream_stall_timeout_ms = Some(10);
        let (emit, events) = sink();
        let messages = run_agent_loop(
            vec![AgentMessage::user("finish")],
            AgentContext::default(),
            &config,
            emit,
            None,
            Some(&provider.stream_fn()),
        )
        .await
        .unwrap();
        assert_eq!(provider.calls().len(), if stalls == 1 { 2 } else { 3 });
        assert_eq!(provider.calls()[1].messages.len(), 1);
        assert_eq!(messages.len(), 2);
        assert_eq!(
            final_message(&messages).stop_reason,
            if stalls == 1 {
                StopReason::Stop
            } else {
                StopReason::Error
            }
        );
        assert_eq!(
            events
                .lock()
                .unwrap()
                .iter()
                .filter(|event| matches!(
                    event,
                    AgentEvent::MessageEnd {
                        message: AgentMessage::Standard(Message::Assistant(_))
                    }
                ))
                .count(),
            1
        );
    }
}

#[tokio::test]
async fn establishment_stalls_cancel_the_request_signal_before_retry() {
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let signals = Arc::new(Mutex::new(Vec::new()));
    let captured_calls = calls.clone();
    let captured_signals = signals.clone();
    let stream: StreamFn = Arc::new(move |_, _, options| {
        captured_calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        captured_signals.lock().unwrap().push(options.signal);
        Box::pin(std::future::pending())
    });
    let mut config = config();
    config.stream_stall_timeout_ms = Some(5);
    let (emit, _) = sink();
    let messages = run_agent_loop(
        vec![AgentMessage::user("finish")],
        AgentContext::default(),
        &config,
        emit,
        None,
        Some(&stream),
    )
    .await
    .unwrap();
    assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 3);
    assert!(signals
        .lock()
        .unwrap()
        .iter()
        .all(pa_agent::abort::AbortSignal::is_aborted));
    assert_eq!(final_message(&messages).stop_reason, StopReason::Error);
}

#[tokio::test]
async fn repetition_cancels_instead_of_retrying_and_can_be_disabled() {
    for enabled in [true, false] {
        let provider = Arc::new(ScriptedProvider::new(model()));
        provider.push_text_turn(&"The same repeating output segment. ".repeat(10));
        let mut config = config();
        config.repetition_loop = Some(RepetitionLoopConfig {
            enabled: Some(enabled),
            threshold: Some(5),
        });
        let (emit, _) = sink();
        let messages = run_agent_loop(
            vec![AgentMessage::user("finish")],
            AgentContext::default(),
            &config,
            emit,
            None,
            Some(&provider.stream_fn()),
        )
        .await
        .unwrap();
        assert_eq!(provider.calls().len(), 1);
        assert_eq!(
            final_message(&messages).stop_reason,
            if enabled {
                StopReason::Error
            } else {
                StopReason::Stop
            }
        );
        if enabled {
            assert_eq!(
                final_message(&messages).diagnostics.as_ref().unwrap()[0].kind,
                "agent_repetition_loop"
            );
        }
    }
}

#[tokio::test]
async fn explicit_user_abort_does_not_retry_as_a_stall() {
    let provider = Arc::new(ScriptedProvider::new(model()));
    provider.push_stalled_turn("partial");
    let controller = AbortController::new();
    let aborter = controller.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        aborter.abort();
    });
    let (emit, _) = sink();
    let messages = run_agent_loop(
        vec![AgentMessage::user("finish")],
        AgentContext::default(),
        &config(),
        emit,
        Some(&controller.signal()),
        Some(&provider.stream_fn()),
    )
    .await
    .unwrap();
    assert_eq!(provider.calls().len(), 1);
    assert_eq!(final_message(&messages).stop_reason, StopReason::Aborted);
}

#[tokio::test]
async fn reasoning_exhausted_warning_ends_before_another_provider_call() {
    let provider = Arc::new(ScriptedProvider::new(model()));
    let mut turn = response(StopReason::Length, "");
    let ScriptedTurn::Events(steps) = &mut turn else {
        unreachable!()
    };
    let ScriptStep::Event(event) = steps.last_mut().unwrap() else {
        unreachable!()
    };
    let AssistantMessageEvent::Done { message, .. } = &mut **event else {
        unreachable!()
    };
    message.diagnostics = Some(vec![serde_json::from_value(
        json!({"type":"provider_warning","timestamp":0,"error":{"code":"reasoning_exhausted"}}),
    )
    .unwrap()]);
    provider.push_turn(turn);
    let (emit, _) = sink();
    let messages = run_agent_loop(
        vec![AgentMessage::user("finish")],
        AgentContext::default(),
        &config(),
        emit,
        None,
        Some(&provider.stream_fn()),
    )
    .await
    .unwrap();
    assert_eq!(provider.calls().len(), 1);
    assert_eq!(final_message(&messages).stop_reason, StopReason::Error);
}

struct PollTool {
    executions: Arc<std::sync::atomic::AtomicUsize>,
    advancing: bool,
    schema: serde_json::Value,
}
impl pa_agent::types::AgentTool for PollTool {
    fn name(&self) -> &'static str {
        "poll"
    }
    fn description(&self) -> &'static str {
        "Poll a task"
    }
    fn parameters(&self) -> &serde_json::Value {
        &self.schema
    }
    fn execute(
        self: Arc<Self>,
        _: String,
        _: serde_json::Value,
        _: pa_agent::abort::AbortSignal,
        _: pa_agent::types::AgentToolUpdateCallback,
    ) -> pa_agent::BoxFut<'static, anyhow::Result<pa_agent::types::AgentToolResult>> {
        let count = self
            .executions
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let result = if self.advancing {
            format!("progress {count}")
        } else {
            "unchanged".into()
        };
        Box::pin(async move { Ok(pa_agent::types::AgentToolResult::text(result)) })
    }
}

#[tokio::test]
async fn identical_tools_stop_before_execution_but_changing_results_keep_polling() {
    for advancing in [false, true] {
        let provider = Arc::new(ScriptedProvider::new(model()));
        let executions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let tools = vec![Arc::new(PollTool {
            executions: executions.clone(),
            advancing,
            schema: json!({"type":"object"}),
        }) as Arc<dyn pa_agent::types::AgentTool>];
        for id in ["first", "second", "third", "fourth", "fifth"] {
            provider.push_tool_call_turn(None, vec![(id, "poll", json!({"task":"same"}))]);
        }
        provider.push_text_turn("completed");
        let (emit, _) = sink();
        let messages = run_agent_loop(
            vec![AgentMessage::user("poll")],
            AgentContext {
                tools,
                ..Default::default()
            },
            &config(),
            emit,
            None,
            Some(&provider.stream_fn()),
        )
        .await
        .unwrap();
        assert_eq!(
            executions.load(std::sync::atomic::Ordering::Relaxed),
            if advancing { 5 } else { 2 }
        );
        assert_eq!(provider.calls().len(), if advancing { 6 } else { 3 });
        assert_eq!(
            final_message(&messages).stop_reason,
            if advancing {
                StopReason::Stop
            } else {
                StopReason::Error
            }
        );
        if !advancing {
            assert_eq!(
                final_message(&messages).diagnostics.as_ref().unwrap()[0]
                    .details
                    .as_ref()
                    .unwrap()["kind"],
                "tool_call_batch"
            );
        }
    }
}

struct UnsettledResult;
impl pa_agent::stream::ModelStream for UnsettledResult {
    fn next_event(&mut self) -> pa_agent::BoxFut<'_, Option<AssistantMessageEvent>> {
        Box::pin(async { None })
    }
    fn result(&mut self) -> pa_agent::BoxFut<'_, anyhow::Result<AssistantMessage>> {
        Box::pin(std::future::pending())
    }
}

#[tokio::test]
async fn closed_iteration_with_unsettled_result_is_bounded() {
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let captured = calls.clone();
    let stream: StreamFn = Arc::new(move |_, _, _| {
        captured.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Box::pin(async { Ok(Box::new(UnsettledResult) as Box<dyn pa_agent::stream::ModelStream>) })
    });
    let mut config = config();
    config.stream_stall_timeout_ms = Some(5);
    let (emit, _) = sink();
    let messages = run_agent_loop(
        vec![AgentMessage::user("finish")],
        AgentContext::default(),
        &config,
        emit,
        None,
        Some(&stream),
    )
    .await
    .unwrap();
    assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 3);
    assert_eq!(final_message(&messages).stop_reason, StopReason::Error);
}

#[tokio::test]
async fn thinking_deltas_are_checked_for_repetition() {
    let provider = Arc::new(ScriptedProvider::new(model()));
    let steps = text_turn_steps(&model(), &"Repeated reasoning block. ".repeat(10));
    let steps = steps
        .into_iter()
        .map(|step| {
            let ScriptStep::Event(event) = step else {
                return step;
            };
            let convert = |mut partial: AssistantMessage| {
                partial.content = partial
                    .content
                    .into_iter()
                    .map(|part| match part {
                        pa_agent::types::AssistantContent::Text(text) => {
                            pa_agent::types::AssistantContent::Thinking(
                                pa_agent::types::ThinkingContent {
                                    thinking: text.text,
                                    thinking_signature: None,
                                    redacted: None,
                                },
                            )
                        }
                        part => part,
                    })
                    .collect();
                partial
            };
            let event = match *event {
                AssistantMessageEvent::TextStart {
                    content_index,
                    partial,
                } => AssistantMessageEvent::ThinkingStart {
                    content_index,
                    partial: convert(partial),
                },
                AssistantMessageEvent::TextDelta {
                    content_index,
                    delta,
                    partial,
                } => AssistantMessageEvent::ThinkingDelta {
                    content_index,
                    delta,
                    partial: convert(partial),
                },
                AssistantMessageEvent::TextEnd {
                    content_index,
                    partial,
                    ..
                } => AssistantMessageEvent::ThinkingEnd {
                    content_index,
                    partial: convert(partial),
                },
                AssistantMessageEvent::Done { reason, message } => AssistantMessageEvent::Done {
                    reason,
                    message: convert(message),
                },
                event => event,
            };
            ScriptStep::Event(Box::new(event))
        })
        .collect();
    provider.push_turn(ScriptedTurn::Events(steps));
    let (emit, _) = sink();
    let messages = run_agent_loop(
        vec![AgentMessage::user("finish")],
        AgentContext::default(),
        &config(),
        emit,
        None,
        Some(&provider.stream_fn()),
    )
    .await
    .unwrap();
    assert_eq!(final_message(&messages).stop_reason, StopReason::Error);
    assert!(matches!(
        final_message(&messages).content.first(),
        Some(pa_agent::types::AssistantContent::Thinking(_))
    ));
}
