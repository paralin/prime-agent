use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};

use pa_agent::agent::{Agent, AgentPromptInput};
use pa_agent::types::{AgentEvent, AgentMessage};
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::sync::Notify;

use crate::session::manager::SessionManager;

use super::agent_messaging::{
    create_agent_session_message_prompt, create_agent_session_message_row, AgentFamilyRelationship,
    AgentMessagePromptPayload, AgentSessionMessageRowPayload,
};
use super::external_events::{
    ExternalEventDeliveryStatus, ExternalEventEmit, ExternalEventInput, EXTERNAL_EVENT_MAX_PENDING,
};

#[derive(Default)]
pub struct LocalExternalEventAdmission {
    agent: Mutex<Weak<Agent>>,
    pending: Mutex<HashSet<String>>,
    admission: tokio::sync::Mutex<()>,
    wake: Arc<Notify>,
    changed: Notify,
    worker: Mutex<Option<tokio::task::AbortHandle>>,
    disposed: AtomicBool,
}

struct PendingReservation {
    runtime: Weak<LocalExternalEventAdmission>,
    id: String,
    accepted: bool,
}

impl Drop for PendingReservation {
    fn drop(&mut self) {
        if !self.accepted {
            if let Some(runtime) = self.runtime.upgrade() {
                lock(&runtime.pending).remove(&self.id);
                runtime.changed.notify_waiters();
            }
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn message_id(message: &AgentMessage) -> Option<&str> {
    match message {
        AgentMessage::Custom(custom) if custom.payload["customType"] == "agent_message" => {
            custom.payload["details"]["id"].as_str()
        }
        _ => None,
    }
}

fn external_message_id(input: &ExternalEventInput) -> String {
    let mut hash = Sha256::new();
    hash.update(input.name.len().to_be_bytes());
    hash.update(input.name.as_bytes());
    hash.update(input.event_id.as_bytes());
    format!("agentmsg_external_{:x}", hash.finalize())
}

impl LocalExternalEventAdmission {
    pub fn has_pending(&self) -> bool {
        !lock(&self.pending).is_empty()
    }

    pub async fn wait_for_delivery(&self) {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.disposed.load(Ordering::Acquire) || !self.has_pending() {
                return;
            }
            changed.await;
        }
    }

    pub async fn bind(self: &Arc<Self>, agent: &Arc<Agent>) {
        *lock(&self.agent) = Arc::downgrade(agent);
        let weak = Arc::downgrade(self);
        agent
            .subscribe(move |event, _| {
                if let AgentEvent::MessageEnd { message } = event {
                    if let (Some(runtime), Some(id)) = (weak.upgrade(), message_id(&message)) {
                        lock(&runtime.pending).remove(id);
                        runtime.changed.notify_waiters();
                    }
                }
                Box::pin(async { Ok(()) })
            })
            .await;
        let weak = Arc::downgrade(self);
        let agent = Arc::downgrade(agent);
        let wake = self.wake.clone();
        let task = tokio::spawn(async move {
            loop {
                wake.notified().await;
                loop {
                    let Some(agent) = agent.upgrade() else { return };
                    agent.wait_for_idle().await;
                    let Some(runtime) = weak.upgrade() else {
                        return;
                    };
                    let pending = runtime.has_pending();
                    let disposed = runtime.disposed.load(Ordering::Acquire);
                    drop(runtime);
                    if disposed || !pending || !agent.has_queued_messages() {
                        break;
                    }
                    if let Err(error) = agent.continue_run().await {
                        if agent.signal().is_some() {
                            continue;
                        }
                        tracing::warn!("external event continuation failed: {error:#}");
                        break;
                    }
                }
            }
        });
        if let Some(previous) = lock(&self.worker).replace(task.abort_handle()) {
            previous.abort();
        }
    }

    pub fn emitter(
        self: &Arc<Self>,
        session: Arc<tokio::sync::Mutex<SessionManager>>,
    ) -> ExternalEventEmit {
        let runtime = self.clone();
        Arc::new(move |input| {
            let runtime = runtime.clone();
            let session = session.clone();
            Box::pin(async move { runtime.admit(&session, input).await })
        })
    }

    async fn admit(
        self: &Arc<Self>,
        session: &tokio::sync::Mutex<SessionManager>,
        input: ExternalEventInput,
    ) -> anyhow::Result<ExternalEventDeliveryStatus> {
        let _admission = self.admission.lock().await;
        anyhow::ensure!(
            !self.disposed.load(Ordering::Acquire),
            "External event session was disposed"
        );
        let agent = lock(&self.agent)
            .upgrade()
            .ok_or_else(|| anyhow::anyhow!("External event session is unavailable"))?;
        let id = external_message_id(&input);
        {
            let mut pending = lock(&self.pending);
            anyhow::ensure!(pending.len() < EXTERNAL_EVENT_MAX_PENDING, "session.external_event.emit queue is full: maximum is {EXTERNAL_EVENT_MAX_PENDING} events");
            pending.insert(id.clone());
        }
        let mut reservation = PendingReservation {
            runtime: Arc::downgrade(self),
            id: id.clone(),
            accepted: false,
        };
        let target = {
            let session = session.lock().await;
            json!({"activeSessionId":session.get_session_id(), "sessionId":session.get_session_id(), "sessionName":session.get_session_name()})
        };
        let prompt = create_agent_session_message_prompt(&AgentMessagePromptPayload {
            message: input.text.clone(),
            sender_name: "system".into(),
            from_relationship: Some(AgentFamilyRelationship::Sibling),
        });
        let row = create_agent_session_message_row(&AgentSessionMessageRowPayload {
            id: &id,
            prompt: &prompt,
            message: &input.text,
            from: &json!({"sessionName":"system"}),
            from_relationship: Some(AgentFamilyRelationship::Sibling),
            target: &target,
            timestamp: super::now_millis(),
        });
        let message: AgentMessage = serde_json::from_value(row)?;
        let status = if agent.signal().is_some() {
            agent.steer(message);
            ExternalEventDeliveryStatus::Queued
        } else {
            match agent
                .prompt_until_accepted(AgentPromptInput::Messages(vec![message.clone()]))
                .await
            {
                Ok(()) => ExternalEventDeliveryStatus::Delivered,
                Err(_) if agent.signal().is_some() => {
                    agent.steer(message);
                    ExternalEventDeliveryStatus::Queued
                }
                Err(error) => {
                    lock(&self.pending).remove(&id);
                    return Err(error);
                }
            }
        };
        reservation.accepted = true;
        self.wake.notify_one();
        Ok(status)
    }

    pub fn dispose(&self) {
        self.disposed.store(true, Ordering::Release);
        self.changed.notify_waiters();
        if let Some(worker) = lock(&self.worker).take() {
            worker.abort();
        }
        let pending = std::mem::take(&mut *lock(&self.pending));
        if let Some(agent) = lock(&self.agent).upgrade() {
            agent.remove_queued_messages(|message| {
                message_id(message).is_some_and(|id| pending.contains(id))
            });
        }
    }
}

impl Drop for LocalExternalEventAdmission {
    fn drop(&mut self) {
        self.dispose();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_engine::{AgentSession, PromptOptions};
    use crate::tools::tool_definition::{ToolDefinition, ToolExecutionResult};
    use pa_agent::agent::{AgentInitialState, AgentOptions};
    use pa_agent::scripted::ScriptedProvider;

    #[test]
    fn event_identities_do_not_collide_at_name_boundaries() {
        let input = ExternalEventInput {
            name: "build_done".into(),
            event_id: "success".into(),
            text: "first".into(),
        };
        let other = ExternalEventInput {
            name: "build".into(),
            event_id: "done_success".into(),
            text: "second".into(),
        };
        assert_ne!(external_message_id(&input), external_message_id(&other));
        assert_eq!(
            external_message_id(&input),
            external_message_id(&ExternalEventInput {
                text: "updated".into(),
                ..input
            })
        );
    }

    #[tokio::test]
    async fn idle_event_wakes_a_local_session_and_persists_one_provenance_row() {
        let provider = Arc::new(ScriptedProvider::new(pa_agent::types::Model::unknown()));
        provider.push_text_turn("received completion");
        let agent = Arc::new(Agent::new(AgentOptions {
            stream_fn: Some(provider.stream_fn()),
            convert_to_llm: Some(super::super::messages::engine_convert_to_llm()),
            ..Default::default()
        }));
        let session = AgentSession::new(
            agent.clone(),
            SessionManager::in_memory(std::path::Path::new(".")),
            Vec::new(),
        )
        .await
        .unwrap();
        let admission = Arc::new(LocalExternalEventAdmission::default());
        admission.bind(&agent).await;
        let emit = admission.emitter(session.shared_persistence());
        let registry = Arc::new(super::super::external_events::ExternalEventRegistry::default());
        let input = ExternalEventInput {
            name: "build".into(),
            event_id: "finished".into(),
            text: "build passed".into(),
        };
        assert_eq!(
            registry
                .admit(input.name.clone(), input.event_id.clone(), || emit(
                    input.clone()
                ))
                .await
                .unwrap(),
            ExternalEventDeliveryStatus::Delivered
        );
        assert_eq!(
            registry
                .admit(input.name.clone(), input.event_id.clone(), || emit(
                    input.clone()
                ))
                .await
                .unwrap(),
            ExternalEventDeliveryStatus::Coalesced
        );
        agent.wait_for_idle().await;
        let entries = session.entries().await;
        let rows: Vec<_> = entries
            .iter()
            .filter_map(|entry| match entry {
                pa_types::session::FileEntry::CustomMessage { payload, .. }
                    if payload.custom_type == "agent_message" =>
                {
                    Some(payload)
                }
                _ => None,
            })
            .collect();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].details.as_ref().unwrap()["from"]["sessionName"],
            "system"
        );
        assert_eq!(
            rows[0].details.as_ref().unwrap()["fromRelationship"],
            "sibling"
        );
        assert_eq!(provider.calls().len(), 1);
        assert!(!admission.has_pending());
        admission.dispose();
        assert!(emit(ExternalEventInput {
            event_id: "late".into(),
            ..input
        })
        .await
        .unwrap_err()
        .to_string()
        .contains("disposed"));
    }

    #[tokio::test]
    async fn busy_event_steers_after_the_running_tool_and_does_not_duplicate_the_turn() {
        let provider = Arc::new(ScriptedProvider::new(pa_agent::types::Model::unknown()));
        provider.push_tool_call_turn(None, vec![("tool", "wait_for_test", json!({}))]);
        provider.push_text_turn("handled the completion");
        let started = Arc::new(Notify::new());
        let finish = Arc::new(Notify::new());
        let tool_started = started.clone();
        let tool_finish = finish.clone();
        let tool = super::super::tool_bridge::bridge_tool(ToolDefinition {
            name: "wait_for_test".into(),
            label: "wait".into(),
            description: "test gate".into(),
            prompt_snippet: String::new(),
            parameters: json!({"type":"object"}),
            execution_mode: None,
            prepare_arguments: None,
            execute: Arc::new(move |_, _, _, _| {
                let started = tool_started.clone();
                let finish = tool_finish.clone();
                Box::pin(async move {
                    started.notify_one();
                    finish.notified().await;
                    Ok(ToolExecutionResult::text("finished"))
                })
            }),
        });
        let agent = Arc::new(Agent::new(AgentOptions {
            initial_state: AgentInitialState {
                tools: Some(vec![tool]),
                ..Default::default()
            },
            stream_fn: Some(provider.stream_fn()),
            convert_to_llm: Some(super::super::messages::engine_convert_to_llm()),
            ..Default::default()
        }));
        let session = Arc::new(
            AgentSession::new(
                agent.clone(),
                SessionManager::in_memory(std::path::Path::new(".")),
                Vec::new(),
            )
            .await
            .unwrap(),
        );
        let admission = Arc::new(LocalExternalEventAdmission::default());
        admission.bind(&agent).await;
        let root = {
            let session = session.clone();
            tokio::spawn(async move { session.prompt("start", PromptOptions::default()).await })
        };
        started.notified().await;
        let emit = admission.emitter(session.shared_persistence());
        assert_eq!(
            emit(ExternalEventInput {
                name: "job".into(),
                event_id: "done".into(),
                text: "job completed".into()
            })
            .await
            .unwrap(),
            ExternalEventDeliveryStatus::Queued
        );
        assert!(admission.has_pending());
        finish.notify_one();
        tokio::time::timeout(std::time::Duration::from_secs(5), root)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        agent.wait_for_idle().await;
        assert_eq!(provider.calls().len(), 2);
        assert_eq!(session.entries().await.iter().filter(|entry| matches!(entry, pa_types::session::FileEntry::CustomMessage { payload, .. } if payload.custom_type == "agent_message")).count(), 1);
        assert!(!admission.has_pending());
    }
}
