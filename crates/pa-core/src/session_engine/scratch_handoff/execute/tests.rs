use std::sync::Arc;

use super::*;
use crate::session::manager::SessionManager;
use crate::session_engine::messages::engine_convert_to_llm;
use crate::session_engine::scratch_handoff::{
    ScratchBoundaryReason, SCRATCH_HANDOFF_CONTINUE_INSTRUCTION,
};
use crate::settings::types::CompactionStrategy;
use pa_agent::agent::{Agent, AgentInitialState, AgentOptions};
use pa_ai::faux::{register_faux_provider, FauxResponseStep};
use pa_types::ai::{ModelInput, UserContent, UserMessage};
use pa_types::session::AgentMessage as SessionMessage;

#[tokio::test]
async fn closeout_checkpoint_and_images_replace_context_without_a_summarizer() {
    let dir = tempfile::tempdir().unwrap();
    let registration = register_faux_provider(pa_ai::faux::RegisterFauxProviderOptions::default());
    let mut model = registration.get_model();
    model.input.push(ModelInput::Image);
    let agent = Arc::new(Agent::new(AgentOptions {
        initial_state: AgentInitialState {
            model: provider_adapter::json_round_trip(&model),
            ..Default::default()
        },
        stream_fn: Some(provider_adapter::real_stream_fn(None, model.clone())),
        convert_to_llm: Some(engine_convert_to_llm()),
        ..Default::default()
    }));
    let mut store = SessionManager::in_memory(dir.path());
    store
        .append_message(SessionMessage::User(UserMessage {
            content: UserContent::Text("* TODO important original task".into()),
            timestamp: 0,
            rest: serde_json::Map::default(),
        }))
        .unwrap();
    agent
        .set_messages(crate::session_engine::rebuilt_loop_messages(
            store.active_context().messages,
        ))
        .await;
    let session_id = store.get_session_id().to_owned();
    let session = AgentSession::new(agent.clone(), store, vec![])
        .await
        .unwrap();
    session.set_scratch_handoff_settings(ScratchHandoffRuntimeSettings {
        strategy: CompactionStrategy::ScratchHandoff,
        enabled: true,
        root_dir: "agent".into(),
        cwd: dir.path().into(),
    });
    let date = local_date().unwrap();
    let path = resolve_scratch_handoff_path(dir.path(), None, &session_id, None, None, &date);
    let checkpoint = path.absolute_path.clone();
    registration.set_responses(vec![FauxResponseStep::Factory(Arc::new(
        move |context, _, _, _| {
            let context = serde_json::to_string(context).unwrap();
            assert!(context.contains("Stop working for now"));
            assert!(context.contains("important original task"));
            std::fs::create_dir_all(checkpoint.parent().unwrap()).unwrap();
            std::fs::write(&checkpoint, "* TODO resume the important task").unwrap();
            Ok(pa_ai::faux::faux_assistant_text_message(
                "Checkpoint saved.",
                pa_ai::faux::FauxAssistantMessageOptions::default(),
            ))
        },
    ))]);
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        session.compact(None, &model, None, None),
    )
    .await
    .unwrap()
    .unwrap();
    let CompactOutcome::Ran(run) = result else {
        panic!("handoff must run")
    };
    assert!(run.result.summary.contains(&path.display_path));
    assert!(run.entry.summary.is_empty());
    assert!(run.continuation.is_some());
    let rows = session.entries().await;
    let boundary = rows.last().unwrap();
    assert_eq!(
        boundary.parent_id(),
        Some(run.entry.first_kept_entry_id.as_str())
    );
    assert_eq!(
        registration.call_count(),
        1,
        "only the closeout agent calls the provider"
    );
    assert_eq!(
        run.entry.details.as_ref().unwrap()["scratchHandoff"]["version"],
        1
    );
    let context =
        crate::session_engine::messages::loop_convert_to_llm(agent.state().await.messages);
    assert_eq!(context.len(), 1);
    let value = serde_json::to_value(&context).unwrap();
    assert_eq!(value[0]["content"][0]["type"], "image");
    let text = value[0]["content"][1]["text"].as_str().unwrap();
    assert!(text.contains("* TODO resume the important task"));
    assert!(text.contains(SCRATCH_HANDOFF_CONTINUE_INSTRUCTION));
    assert!(!text.contains("Stop working for now"));
    assert!(!session.foreground_lease().busy());
    registration.unregister();
}

#[tokio::test]
async fn missing_checkpoint_never_commits_a_scratch_boundary() {
    let dir = tempfile::tempdir().unwrap();
    let registration = register_faux_provider(pa_ai::faux::RegisterFauxProviderOptions::default());
    let mut model = registration.get_model();
    model.input.push(ModelInput::Image);
    registration.set_responses(vec![FauxResponseStep::Message(
        pa_ai::faux::faux_assistant_text_message(
            "done",
            pa_ai::faux::FauxAssistantMessageOptions::default(),
        ),
    )]);
    let agent = Arc::new(Agent::new(AgentOptions {
        stream_fn: Some(provider_adapter::real_stream_fn(None, model.clone())),
        convert_to_llm: Some(engine_convert_to_llm()),
        ..Default::default()
    }));
    let session = AgentSession::new(agent, SessionManager::in_memory(dir.path()), vec![])
        .await
        .unwrap();
    session.set_scratch_handoff_settings(ScratchHandoffRuntimeSettings {
        strategy: CompactionStrategy::ScratchHandoff,
        enabled: true,
        root_dir: "agent".into(),
        cwd: dir.path().into(),
    });
    let error = session
        .compact_for_reason(None, &model, None, None, ScratchBoundaryReason::Requested)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("non-empty Org checkpoint"));
    assert!(!session
        .entries()
        .await
        .iter()
        .any(|row| matches!(row, pa_types::session::FileEntry::Compaction { .. })));
    assert!(!session.foreground_lease().busy());
    registration.unregister();
}

#[test]
fn checkpoint_date_uses_the_local_calendar() {
    let date = local_date().unwrap();
    assert_eq!(date.len(), 8);
    assert!(date.chars().all(|ch| ch.is_ascii_digit()));
}

#[tokio::test]
async fn cancelled_closeout_settles_the_agent_and_keeps_the_old_history() {
    let dir = tempfile::tempdir().unwrap();
    let registration = Arc::new(register_faux_provider(
        pa_ai::faux::RegisterFauxProviderOptions::default(),
    ));
    let mut model = registration.get_model();
    model.input.push(ModelInput::Image);
    registration.set_responses(vec![FauxResponseStep::Delayed {
        message: pa_ai::faux::faux_assistant_text_message(
            "checkpoint",
            pa_ai::faux::FauxAssistantMessageOptions::default(),
        ),
        delay_ms: 30_000,
    }]);
    let agent = Arc::new(Agent::new(AgentOptions {
        stream_fn: Some(provider_adapter::real_stream_fn(None, model.clone())),
        convert_to_llm: Some(engine_convert_to_llm()),
        ..Default::default()
    }));
    let session = AgentSession::new(agent.clone(), SessionManager::in_memory(dir.path()), vec![])
        .await
        .unwrap();
    session.set_scratch_handoff_settings(ScratchHandoffRuntimeSettings {
        strategy: CompactionStrategy::ScratchHandoff,
        enabled: true,
        root_dir: "agent".into(),
        cwd: dir.path().into(),
    });
    let controller = Arc::new(pa_agent::abort::AbortController::new());
    let cancel = controller.clone();
    let observed = registration.clone();
    let aborter = tokio::spawn(async move {
        while observed.call_count() == 0 {
            tokio::task::yield_now().await;
        }
        cancel.abort();
    });
    let error = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        session.compact(None, &model, None, Some(&controller.signal())),
    )
    .await
    .unwrap()
    .unwrap_err();
    aborter.await.unwrap();
    assert!(pa_agent::abort::is_abort_error(&error));
    assert!(!agent.state().await.is_streaming);
    assert!(!session.foreground_lease().busy());
    assert!(!session
        .entries()
        .await
        .iter()
        .any(|row| matches!(row, pa_types::session::FileEntry::Compaction { .. })));
    registration.unregister();
}
