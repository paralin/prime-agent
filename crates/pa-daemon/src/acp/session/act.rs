use std::sync::{Arc, Mutex};

use pa_core::session_engine::act_runtime::projection::ActEventSink;
use tokio::sync::{mpsc, oneshot};

use crate::acp::meta::PrimeAgentEventPhase;
use crate::acp::producer::UpdateProducer;
use crate::acp::types::AcpSessionUpdate;
use crate::acp::wire_events::{wire_updates, WireMappingState};

enum Publication {
    Update(Box<AcpSessionUpdate>),
    Barrier(oneshot::Sender<()>),
}

pub(super) struct ActRelay {
    sender: mpsc::UnboundedSender<Publication>,
    mapping: Arc<Mutex<WireMappingState>>,
    task: tokio::task::JoinHandle<()>,
}

impl ActRelay {
    pub(super) fn new(producer: Arc<UpdateProducer>) -> Self {
        let (sender, mut receiver) = mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            while let Some(publication) = receiver.recv().await {
                match publication {
                    Publication::Update(update) => {
                        let turn = producer.active_prompt_turn().await;
                        producer
                            .publish(&update, turn, PrimeAgentEventPhase::Event, None)
                            .await;
                    }
                    Publication::Barrier(done) => {
                        let _ = done.send(());
                    }
                }
            }
        });
        Self {
            sender,
            mapping: Arc::new(Mutex::new(WireMappingState::with_act_projection(true))),
            task,
        }
    }

    pub(super) fn sink(&self) -> ActEventSink {
        let sender = self.sender.clone();
        let mapping = self.mapping.clone();
        Arc::new(move |event| {
            let mut mapping = mapping
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for update in wire_updates(&event, &mut mapping) {
                let _ = sender.send(Publication::Update(Box::new(update)));
            }
        })
    }

    pub(super) async fn flush(&self) {
        let (done, wait) = oneshot::channel();
        if self.sender.send(Publication::Barrier(done)).is_ok() {
            let _ = wait.await;
        }
    }
}

impl Drop for ActRelay {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::meta::PRIME_AGENT_META_NAMESPACE;
    use serde_json::json;

    #[tokio::test]
    async fn projection_flushes_before_response_boundary_and_preserves_act_metadata() {
        let (frames, mut receiver) = mpsc::unbounded_channel();
        let producer = UpdateProducer::new("session", frames);
        producer.commit_session_new_response().await;
        let turn = producer.begin_prompt().await;
        let relay = ActRelay::new(producer.clone());
        let sink = relay.sink();
        for (kind, sequence) in [("start", 1), ("assistant_delta", 2), ("terminal", 3)] {
            sink(
                json!({"type":"act_event","actId":"act","event":kind,"sequence":sequence,
                "depth":1,"model":{"id":"model"},"prompt":"work","stream":"text","text":"answer","status":"done","usage":{"input":5}}),
            );
        }
        relay.flush().await;
        producer
            .publish(
                &AcpSessionUpdate::SessionInfoUpdate { meta: json!({}) },
                turn,
                PrimeAgentEventPhase::ResponseBoundary,
                None,
            )
            .await;
        let mut updates = vec![];
        while let Ok(frame) = receiver.try_recv() {
            updates.push(frame["params"]["update"].clone());
        }
        assert_eq!(updates.len(), 4);
        for (index, frame) in updates.iter().enumerate() {
            assert_eq!(
                frame["_meta"][PRIME_AGENT_META_NAMESPACE]["promptTurnId"],
                turn
            );
            assert_eq!(
                frame["_meta"][PRIME_AGENT_META_NAMESPACE]["eventSequence"],
                index + 1
            );
        }
        assert_eq!(
            updates[2]["_meta"][PRIME_AGENT_META_NAMESPACE]["act"]["usage"]["input"],
            5
        );
        assert_eq!(
            updates[3]["_meta"][PRIME_AGENT_META_NAMESPACE]["phase"],
            "responseBoundary"
        );
    }
}
