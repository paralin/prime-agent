use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};

use anyhow::{Context, Result};
use pa_agent::types::AgentMessage;
use serde_json::json;
use tokio::sync::Notify;

use super::{
    enqueue_priority, EventPump, QueuePriority, QueuedItem, SessionCore, TurnPolicy,
    WorkerRecoveryJournal,
};

pub(super) fn admit(
    core_lock: &Arc<Mutex<SessionCore>>,
    recovery: &Mutex<Option<WorkerRecoveryJournal>>,
    notify: &Notify,
    closed: &AtomicBool,
    message: AgentMessage,
    events: &Arc<EventPump>,
) -> Result<()> {
    let AgentMessage::Custom(message) = message else {
        anyhow::bail!("Runtime nudge must be a custom message")
    };
    let kind = message.payload["customType"]
        .as_str()
        .context("Runtime nudge type missing")?;
    anyhow::ensure!(
        matches!(kind, "tool_error_nudge" | "english_output_nudge"),
        "Unknown runtime nudge type"
    );
    let text = message.payload["content"]
        .as_str()
        .context("Runtime nudge text missing")?
        .to_owned();
    let key = format!("runtime_nudge:{kind}");
    let mut recovery = recovery
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut core = core_lock
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    anyhow::ensure!(
        core.created && !core.shutdown_requested && !closed.load(Ordering::Acquire),
        "Session is closed"
    );
    if core
        .steering
        .iter()
        .any(|item| item.queue_key.as_deref() == Some(&key))
    {
        return Ok(());
    }
    anyhow::ensure!(
        core.steering.len() < 20,
        "Runtime nudge steering queue is full"
    );
    let store = core
        .store
        .as_ref()
        .context("Session is still initializing")?;
    let session_id = store.session_id().to_owned();
    let session_file = store.path.to_string_lossy().to_string();
    enqueue_priority(
        &mut core.steering,
        QueuedItem {
            priority: QueuePriority::Background,
            preview: Some(text.clone()),
            message: text,
            custom_message: Some(serde_json::to_value(message)?),
            agent_message: None,
            queue_key: Some(key.clone()),
            admission_id: None,
            images: vec![],
            done: None,
            queue_visible: false,
            policy: TurnPolicy::Injected,
            forced_batch: false,
        },
    );
    if let Some(journal) = recovery.as_mut() {
        let lanes = super::queue_lanes(&core);
        if let Err(error) = journal.record_queue_checkpoint(
            &core.active_session_id,
            &session_id,
            Some(&session_file),
            true,
            "runtime_nudge_queued",
            &lanes.steering,
            &lanes.follow_up,
        ) {
            core.steering
                .retain(|item| item.queue_key.as_deref() != Some(&key));
            return Err(error);
        }
    }
    let snapshot = super::Worker::snapshot_locked(&core);
    core.last_action_snapshot = Some(snapshot.clone());
    drop(core);
    drop(recovery);
    super::emit_worker_event_with(
        core_lock,
        events,
        json!({"type":"session_action_update", "actions":snapshot}),
    );
    notify.notify_one();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn nudges_survive_worker_recovery_and_preserve_custom_message_provenance() {
        let dir = tempfile::tempdir().unwrap();
        let journal_path = dir.path().join("recovery.jsonl");
        let worker = super::super::Worker::new(
            super::super::WorkerConfig {
                socket_path: dir.path().join("worker.sock"),
                supervisor_socket_path: dir.path().join("absent.sock"),
                token: "token".into(),
                worker_instance_id: String::new(),
                active_session_id: "own".into(),
                agent_dir: dir.path().join("agent"),
                recovery_journal_path: journal_path.clone(),
                script: Some(json!({"responses":["ack"]})),
                telemetry_disabled: Some(true),
            },
            None,
        );
        *worker.recovery.lock().unwrap() =
            Some(WorkerRecoveryJournal::open(&journal_path).unwrap());
        {
            let mut core = worker.core.lock().unwrap();
            core.created = true;
            core.busy = true;
            core.store = Some(crate::session_store::SessionFile::create("/tmp", None, 0));
        }
        let notice = || {
            pa_core::session_engine::tool_error_nudge::ToolErrorNudgeKind::PythonSyntax.message(1)
        };
        let queue = || {
            admit(
                &worker.core,
                &worker.recovery,
                &worker.work_notify,
                &worker.mailbox_closed,
                notice(),
                &worker.events,
            )
        };
        queue().unwrap();
        queue().unwrap();
        let journal = WorkerRecoveryJournal::open(&journal_path).unwrap();
        let (steering, follow_up) = super::super::restore_queue_snapshot(&journal, "own");
        assert_eq!(steering.len(), 1);
        assert!(follow_up.is_empty());
        assert_eq!(
            steering[0].queue_key.as_deref(),
            Some("runtime_nudge:tool_error_nudge")
        );
        assert_eq!(
            steering[0].custom_message.as_ref().unwrap()["customType"],
            "tool_error_nudge"
        );
        assert_eq!(steering[0].policy, TurnPolicy::Injected);
        worker.mailbox_closed.store(true, Ordering::Release);
        assert!(queue().unwrap_err().to_string().contains("closed"));
    }
}
