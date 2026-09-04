use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};

use anyhow::{Context, Result};
use pa_core::session_engine::agent_messaging::{
    create_agent_session_message_prompt, create_agent_session_message_row, AgentFamilyRelationship,
    AgentMessagePromptPayload, AgentSessionMessageRowPayload,
};
use pa_core::session_engine::external_events::{
    ExternalEventDeliveryStatus, ExternalEventInput, EXTERNAL_EVENT_MAX_PENDING,
};
use serde_json::json;
use tokio::sync::Notify;

use super::{
    enqueue_priority, EventPump, QueuePriority, QueuedItem, SessionCore, TurnPolicy,
    WorkerRecoveryJournal, AUTONOMOUS_QUEUE_KEY,
};

pub(super) fn admit(
    core_lock: &Arc<Mutex<SessionCore>>,
    recovery: &Mutex<Option<WorkerRecoveryJournal>>,
    notify: &Notify,
    closed: &AtomicBool,
    input: &ExternalEventInput,
    events: &Arc<EventPump>,
) -> Result<ExternalEventDeliveryStatus> {
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
    let store = core
        .store
        .as_ref()
        .context("Session is still initializing")?;
    let session_id = store.session_id().to_string();
    let session_file = store.path.to_string_lossy().to_string();
    let session_name = store.session_name().map(str::to_owned);
    let key = format!("external_event:{}", json!([input.name, input.event_id]));
    if core
        .steering
        .iter()
        .chain(&core.follow_up)
        .any(|item| item.queue_key.as_deref() == Some(&key))
    {
        return Ok(ExternalEventDeliveryStatus::Coalesced);
    }
    anyhow::ensure!(
        core.steering.len() + core.follow_up.len() < EXTERNAL_EVENT_MAX_PENDING,
        "session.external_event.emit queue is full: maximum is {EXTERNAL_EVENT_MAX_PENDING} events"
    );
    let prompt = create_agent_session_message_prompt(&AgentMessagePromptPayload {
        message: input.text.clone(),
        sender_name: "system".into(),
        from_relationship: Some(AgentFamilyRelationship::Sibling),
    });
    let message_id = format!("agentmsg_external_{}_{}", input.name, input.event_id);
    let row = create_agent_session_message_row(&AgentSessionMessageRowPayload {
        id: &message_id,
        prompt: &prompt,
        message: &input.text,
        from: &json!({"sessionName":"system"}),
        from_relationship: Some(AgentFamilyRelationship::Sibling),
        target: &json!({"activeSessionId":core.active_session_id, "sessionId":session_id, "sessionName":session_name}),
        timestamp: crate::util::now_ms(),
    });
    let queued = core.busy;
    let suspended = core.queued_input_suspended;
    core.queued_input_suspended = false;
    let mut withdrawn = Vec::new();
    for (index, item) in std::mem::take(&mut core.follow_up).into_iter().enumerate() {
        if item.queue_key.as_deref() == Some(AUTONOMOUS_QUEUE_KEY) {
            withdrawn.push((index, item));
        } else {
            core.follow_up.push_back(item);
        }
    }
    enqueue_priority(
        &mut core.steering,
        QueuedItem {
            priority: QueuePriority::Background,
            preview: Some(format!("Agent message received: {}", input.text)),
            message: prompt,
            custom_message: Some(row),
            agent_message: Some(input.text.clone()),
            queue_key: Some(key.clone()),
            admission_id: None,
            images: vec![],
            done: None,
            queue_visible: queued,
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
            "steer_queued",
            &lanes.steering,
            &lanes.follow_up,
        ) {
            core.steering
                .retain(|item| item.queue_key.as_deref() != Some(&key));
            core.queued_input_suspended = suspended;
            for (index, item) in withdrawn {
                core.follow_up.insert(index, item);
            }
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
    Ok(if queued {
        ExternalEventDeliveryStatus::Queued
    } else {
        ExternalEventDeliveryStatus::Delivered
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn external_events_resume_coalesce_and_reject_capacity_or_closed_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let worker = super::super::Worker::new(
            super::super::WorkerConfig {
                socket_path: dir.path().join("worker.sock"),
                supervisor_socket_path: dir.path().join("absent.sock"),
                token: "token".into(),
                worker_instance_id: String::new(),
                active_session_id: "own".into(),
                agent_dir: dir.path().join("agent"),
                recovery_journal_path: dir.path().join("recovery.jsonl"),
                script: Some(json!({"responses":["ack"]})),
                telemetry_disabled: Some(true),
            },
            None,
        );
        {
            let mut core = worker.core.lock().unwrap();
            core.created = true;
            core.store = Some(crate::session_store::SessionFile::create("/tmp", None, 0));
            core.queued_input_suspended = true;
        }
        let input = ExternalEventInput {
            name: "job".into(),
            event_id: "first".into(),
            text: "finished".into(),
        };
        let emit = |input: &ExternalEventInput| {
            admit(
                &worker.core,
                &worker.recovery,
                &worker.work_notify,
                &worker.mailbox_closed,
                input,
                &worker.events,
            )
        };
        assert_eq!(
            emit(&input).unwrap(),
            ExternalEventDeliveryStatus::Delivered
        );
        assert_eq!(
            emit(&input).unwrap(),
            ExternalEventDeliveryStatus::Coalesced
        );
        {
            let core = worker.core.lock().unwrap();
            assert!(!core.queued_input_suspended);
            assert_eq!(core.steering.len(), 1);
            let row = core.steering[0].custom_message.as_ref().unwrap();
            assert_eq!(row["details"]["from"]["sessionName"], "system");
            assert_eq!(row["details"]["fromRelationship"], "sibling");
        }
        worker.core.lock().unwrap().busy = true;
        for index in 1..EXTERNAL_EVENT_MAX_PENDING {
            assert_eq!(
                emit(&ExternalEventInput {
                    event_id: index.to_string(),
                    ..input.clone()
                })
                .unwrap(),
                ExternalEventDeliveryStatus::Queued
            );
        }
        assert!(emit(&ExternalEventInput {
            event_id: "overflow".into(),
            ..input.clone()
        })
        .unwrap_err()
        .to_string()
        .contains("queue is full"));
        worker.mailbox_closed.store(true, Ordering::Release);
        assert!(emit(&input).unwrap_err().to_string().contains("closed"));
    }
}
