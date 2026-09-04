use std::sync::Arc;

use super::*;
use pa_ai::faux::{register_faux_provider, RegisterFauxProviderOptions};
use pa_ai::types::{NativeCompactionFunction, ProviderNativeCompactionResult};

fn session() -> SessionManager {
    let mut session = SessionManager::in_memory(std::path::Path::new("."));
    for index in 0..3 {
        session
            .append_message(AgentMessage::User(UserMessage {
                content: UserContent::Text(format!("task {index} {}", "context ".repeat(20))),
                timestamp: index,
                rest: serde_json::Map::default(),
            }))
            .unwrap();
    }
    session
}

fn options<'a>(model: pa_types::ai::Model) -> CompactOptions<'a> {
    CompactOptions {
        model,
        api_key: Some("secret".into()),
        custom_instructions: None,
        settings: super::super::super::compaction::CompactionSettings {
            keep_recent_tokens: 2,
            ..Default::default()
        },
        abort: None,
        harness_digest: None,
        auxiliary: None,
        summary_delta: None,
    }
}

#[tokio::test]
async fn native_history_round_trips_in_the_durable_compaction_row() {
    let compact: NativeCompactionFunction = Arc::new(|model, context, options| {
        Box::pin(async move {
            assert_eq!(options.base.api_key.as_deref(), Some("secret"));
            assert_eq!(options.base.headers.as_ref().unwrap()["x-team"], "team");
            assert!(!context.messages.is_empty());
            assert!(options.instructions.contains("Python kernel keeps running"));
            let item = json!({"type":"compaction", "encrypted_content":"opaque"});
            Ok(ProviderNativeCompactionResult {
                provider: model.provider.clone(),
                replacement_history: vec![item.clone()],
                compaction_item: item,
            })
        })
    });
    let registration = register_faux_provider(RegisterFauxProviderOptions {
        compact: Some(compact),
        ..Default::default()
    });
    let mut session = session();
    let outcome = execute_native_compaction(
        &mut session,
        &options(registration.get_model()),
        Some([("x-team".into(), "team".into())].into_iter().collect()),
    )
    .await
    .unwrap();
    let CompactOutcome::Ran(run) = outcome else {
        panic!("native compaction must run")
    };
    assert_eq!(
        run.entry.provider_native_compaction.as_ref().unwrap()["replacementHistory"][0]
            ["encrypted_content"],
        "opaque"
    );
    let typed: pa_types::session::CompactionEntry =
        serde_json::from_value(serde_json::to_value(&run.entry).unwrap()).unwrap();
    assert_eq!(
        typed.provider_native_compaction,
        run.entry.provider_native_compaction
    );
    let row = session.retained_entries().last().unwrap();
    let json = serde_json::to_string(row).unwrap();
    let reloaded: FileEntry = serde_json::from_str(&json).unwrap();
    let history = prior_native_history(&reloaded, "faux").unwrap();
    assert_eq!(history["items"][0]["encrypted_content"], "opaque");
    assert!(prior_native_history(&reloaded, "another-provider").is_none());
    assert_eq!(
        registration.call_count(),
        0,
        "native compaction must not invoke the text summarizer"
    );
    registration.unregister();
}

#[tokio::test]
async fn failed_or_invalid_compaction_does_not_append_a_boundary() {
    for invalid in [false, true] {
        let compact: NativeCompactionFunction = Arc::new(move |model, _, _| {
            Box::pin(async move {
                if !invalid {
                    return Err(pa_ai::ProviderError::Message("native failure".into()));
                }
                Ok(ProviderNativeCompactionResult {
                    provider: model.provider.clone(),
                    replacement_history: vec![],
                    compaction_item: Value::Null,
                })
            })
        });
        let registration = register_faux_provider(RegisterFauxProviderOptions {
            compact: Some(compact),
            ..Default::default()
        });
        let mut session = session();
        let before = session.retained_entries().len();
        assert!(
            execute_native_compaction(&mut session, &options(registration.get_model()), None)
                .await
                .is_err()
        );
        assert_eq!(session.retained_entries().len(), before);
        registration.unregister();
    }
}

#[tokio::test]
async fn native_session_replays_persisted_history_on_the_next_provider_turn() {
    let compact: NativeCompactionFunction = Arc::new(|model, _, _| {
        Box::pin(async move {
            let item = json!({"type":"compaction", "encrypted_content":"opaque"});
            Ok(ProviderNativeCompactionResult {
                provider: model.provider.clone(),
                replacement_history: vec![item.clone()],
                compaction_item: item,
            })
        })
    });
    let registration = register_faux_provider(RegisterFauxProviderOptions {
        compact: Some(compact),
        ..Default::default()
    });
    let seen = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let captured = seen.clone();
    registration.set_responses(vec![pa_ai::faux::FauxResponseStep::Factory(Arc::new(
        move |context, _, _, _| {
            let Message::User(first) = &context.messages[0] else {
                panic!("summary must become a user message")
            };
            assert_eq!(
                first.rest["providerPayload"]["items"][0]["encrypted_content"],
                "opaque"
            );
            captured.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(pa_ai::faux::faux_assistant_text_message(
                "continued",
                pa_ai::faux::FauxAssistantMessageOptions::default(),
            ))
        },
    ))]);
    let model = registration.get_model();
    let agent = Arc::new(pa_agent::agent::Agent::new(pa_agent::agent::AgentOptions {
        initial_state: pa_agent::agent::AgentInitialState {
            model: super::super::super::provider_adapter::json_round_trip(&model),
            ..Default::default()
        },
        stream_fn: Some(super::super::super::provider_adapter::real_stream_fn(
            None,
            model.clone(),
        )),
        convert_to_llm: Some(super::super::super::messages::engine_convert_to_llm()),
        ..Default::default()
    }));
    let session = super::super::super::AgentSession::new(agent, session(), vec![])
        .await
        .unwrap();
    session.set_compaction_settings(options(model.clone()).settings);
    session.compact(None, &model, None, None).await.unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        session.prompt("continue", crate::session_engine::PromptOptions::default()),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(seen.load(std::sync::atomic::Ordering::SeqCst));
    registration.unregister();
}

#[tokio::test]
async fn session_uses_local_summary_when_native_fails_is_disabled_or_has_custom_instructions() {
    for mode in ["failure", "disabled", "instructions"] {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let captured = calls.clone();
        let compact: NativeCompactionFunction = Arc::new(move |_, _, _| {
            captured.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Box::pin(async { Err(pa_ai::ProviderError::Message("native failed".into())) })
        });
        let registration = register_faux_provider(RegisterFauxProviderOptions {
            compact: Some(compact),
            ..Default::default()
        });
        registration.set_responses(vec![pa_ai::faux::FauxResponseStep::Message(
            pa_ai::faux::faux_assistant_text_message(
                "local summary",
                pa_ai::faux::FauxAssistantMessageOptions::default(),
            ),
        )]);
        let model = registration.get_model();
        let session = super::super::super::AgentSession::new(
            Arc::new(pa_agent::agent::Agent::new(
                pa_agent::agent::AgentOptions::default(),
            )),
            session(),
            vec![],
        )
        .await
        .unwrap();
        session.set_compaction_settings(options(model.clone()).settings);
        session.set_native_compaction_enabled(mode != "disabled");
        let outcome = session
            .compact(
                (mode == "instructions").then_some("preserve tests"),
                &model,
                None,
                None,
            )
            .await
            .unwrap();
        let CompactOutcome::Ran(run) = outcome else {
            panic!("summary must run")
        };
        assert!(run.result.summary.contains("local summary"));
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            usize::from(mode == "failure")
        );
        assert_eq!(registration.call_count(), 1);
        registration.unregister();
    }
}

#[tokio::test]
async fn cancellation_interrupts_the_provider_without_committing() {
    let compact: NativeCompactionFunction = Arc::new(|_, _, _| Box::pin(std::future::pending()));
    let registration = register_faux_provider(RegisterFauxProviderOptions {
        compact: Some(compact),
        ..Default::default()
    });
    let controller = pa_agent::abort::AbortController::new();
    let signal = controller.signal();
    let mut options = options(registration.get_model());
    options.abort = Some(&signal);
    let mut session = session();
    let before = session.retained_entries().len();
    let aborter = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        controller.abort();
    });
    let error = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        execute_native_compaction(&mut session, &options, None),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert!(pa_agent::abort::is_abort_error(&error));
    assert_eq!(session.retained_entries().len(), before);
    aborter.await.unwrap();
    registration.unregister();
}
