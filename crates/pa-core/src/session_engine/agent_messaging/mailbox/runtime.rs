use std::sync::{Arc, Mutex};
use std::time::Duration;

use pa_agent::abort::{aborted_error, throw_if_aborted, AbortSignal};
use serde_json::{json, Value};
use tokio::sync::oneshot;

use crate::kernel::host_channel::duplex_host_handler;
use crate::kernel::shared::{host_handler, HostRequestHandlers};
use crate::session::manager::format_iso_now;

use super::{
    find_acceptance, find_handoff, next_sequence, project, MailboxEnvelope, MailboxFilter,
    ACCEPTED_CUSTOM_TYPE, CONSUMED_CUSTOM_TYPE, HANDOFF_CUSTOM_TYPE,
};

pub type MailboxWriter = Arc<dyn Fn(Value) -> anyhow::Result<()> + Send + Sync>;
pub type MailboxProvider = Arc<dyn Fn() -> anyhow::Result<Arc<DurableMailbox>> + Send + Sync>;

pub struct DurableMailbox {
    target_session_id: String,
    state: Mutex<MailboxState>,
    writer: MailboxWriter,
}

struct MailboxState {
    entries: Vec<Value>,
    waiters: Vec<MailboxWaiter>,
    closed: Option<String>,
}

struct MailboxWaiter {
    id: uuid::Uuid,
    filter: MailboxFilter,
    sender: oneshot::Sender<anyhow::Result<MailboxEnvelope>>,
}

struct WaitRegistration<'a> {
    mailbox: &'a DurableMailbox,
    id: uuid::Uuid,
}

impl Drop for WaitRegistration<'_> {
    fn drop(&mut self) {
        let mut state = self
            .mailbox
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.waiters.retain(|waiter| waiter.id != self.id);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MailboxDelivery {
    Queued,
    Woken,
}

#[derive(Debug, Clone)]
pub struct MailboxReceipt {
    pub envelope: MailboxEnvelope,
    pub delivery_status: &'static str,
    pub handoff: &'static str,
}

impl MailboxReceipt {
    #[must_use]
    pub fn to_value(&self) -> Value {
        let envelope = &self.envelope;
        let mut receipt = json!({"id":envelope.id,"source":envelope.source,"target":envelope.target,
            "message":envelope.message,"deliveryStatus":self.delivery_status,"deliveryMode":"steer",
            "acceptedAt":envelope.accepted_at,"targetSequence":envelope.sequence,"handoff":self.handoff});
        if let Some(from) = &envelope.from {
            receipt["from"] = from.clone();
        }
        if let Some(reply_to) = &envelope.reply_to {
            receipt["replyTo"] = json!(reply_to);
        }
        receipt[if self.delivery_status == "delivered" {
            "deliveredAt"
        } else {
            "queuedAt"
        }] = json!(format_iso_now());
        receipt
    }
}

impl DurableMailbox {
    #[must_use]
    pub fn new(target_session_id: String, entries: Vec<Value>, writer: MailboxWriter) -> Self {
        Self {
            target_session_id,
            state: Mutex::new(MailboxState {
                entries,
                waiters: Vec::new(),
                closed: None,
            }),
            writer,
        }
    }

    /// # Errors
    /// Returns an error for an invalid target/source, exhausted sequence, or failed durable write.
    pub fn accept(&self, envelope: MailboxEnvelope) -> anyhow::Result<MailboxEnvelope> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let envelope = self.accept_locked(&mut state, envelope)?;
        let pending = project(&state.entries, Some(&self.target_session_id))
            .iter()
            .any(|message| message.id == envelope.id);
        if let Some(index) = pending
            .then(|| Self::waiter_index(&state, &envelope))
            .flatten()
        {
            self.consume_locked(&mut state, &envelope)?;
            let waiter = state.waiters.remove(index);
            let _ = waiter.sender.send(Ok(envelope.clone()));
        }
        Ok(envelope)
    }

    fn accept_locked(
        &self,
        state: &mut MailboxState,
        mut envelope: MailboxEnvelope,
    ) -> anyhow::Result<MailboxEnvelope> {
        anyhow::ensure!(
            state.closed.is_none(),
            "{}",
            state.closed.as_deref().unwrap_or_default()
        );
        anyhow::ensure!(
            envelope.target_session_id() == self.target_session_id,
            "agent mailbox target does not match this session"
        );
        anyhow::ensure!(
            envelope.source == super::super::AGENT_MESSAGE_SOURCE,
            "agent mailbox source is invalid"
        );
        if let Some(accepted) =
            find_acceptance(&state.entries, &envelope.id, Some(&self.target_session_id))
        {
            return Ok(accepted);
        }
        envelope.sequence = next_sequence(&state.entries, Some(&self.target_session_id))?;
        let entry = json!({"type":"custom_message", "customType":ACCEPTED_CUSTOM_TYPE,
            "content":"", "display":false, "details":{"envelope":envelope}});
        self.append_locked(state, entry)?;
        Ok(envelope)
    }

    /// # Errors
    /// Returns an error for invalid acceptance, failed delivery, or a failed durable write.
    /// A completed handoff is acknowledged on retry without repeating delivery.
    pub fn receive<F>(
        &self,
        envelope: MailboxEnvelope,
        deliver: F,
    ) -> anyhow::Result<MailboxReceipt>
    where
        F: FnOnce(&MailboxEnvelope) -> anyhow::Result<MailboxDelivery>,
    {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let envelope = self.accept_locked(&mut state, envelope)?;
        if let Some(prior) = find_handoff(&state.entries, &envelope.id, &self.target_session_id) {
            return Ok(MailboxReceipt {
                envelope,
                delivery_status: if prior.delivery_status == "delivered" {
                    "delivered"
                } else {
                    "queued"
                },
                handoff: "retry",
            });
        }
        state.waiters.retain(|waiter| !waiter.sender.is_closed());
        let (status, handoff) = if let Some(index) = Self::waiter_index(&state, &envelope) {
            self.consume_locked(&mut state, &envelope)?;
            self.handoff_locked(&mut state, &envelope, "waiter", "delivered")?;
            let waiter = state.waiters.remove(index);
            let _ = waiter.sender.send(Ok(envelope.clone()));
            ("delivered", "waiter")
        } else {
            let (status, handoff) = match deliver(&envelope)? {
                MailboxDelivery::Queued => ("queued", "queue"),
                MailboxDelivery::Woken => ("delivered", "context"),
            };
            self.handoff_locked(&mut state, &envelope, handoff, status)?;
            (status, handoff)
        };
        Ok(MailboxReceipt {
            envelope,
            delivery_status: status,
            handoff,
        })
    }

    fn waiter_index(state: &MailboxState, envelope: &MailboxEnvelope) -> Option<usize> {
        state
            .waiters
            .iter()
            .position(|waiter| !waiter.sender.is_closed() && envelope.matches(&waiter.filter))
    }

    fn append_locked(&self, state: &mut MailboxState, entry: Value) -> anyhow::Result<()> {
        (self.writer)(entry.clone())?;
        state.entries.push(entry);
        Ok(())
    }

    fn consume_locked(
        &self,
        state: &mut MailboxState,
        envelope: &MailboxEnvelope,
    ) -> anyhow::Result<()> {
        self.append_locked(state,json!({"type":"custom_message", "customType":CONSUMED_CUSTOM_TYPE,
            "content":"", "display":false, "details":{"messageId":envelope.id,
                "targetSessionId":self.target_session_id,"sequence":envelope.sequence,"consumedAt":format_iso_now()}}))
    }

    fn handoff_locked(
        &self,
        state: &mut MailboxState,
        envelope: &MailboxEnvelope,
        handoff: &str,
        status: &str,
    ) -> anyhow::Result<()> {
        self.append_locked(state,json!({"type":"custom_message", "customType":HANDOFF_CUSTOM_TYPE,
            "content":"", "display":false, "details":{"messageId":envelope.id,
                "targetSessionId":self.target_session_id,"handoff":handoff,"deliveryStatus":status,"handedOffAt":format_iso_now()}}))
    }

    /// # Errors
    /// Returns an error for an invalid limit or failed durable consumption write.
    pub fn inbox(
        &self,
        filter: &MailboxFilter,
        limit: usize,
        consume: bool,
    ) -> anyhow::Result<Vec<MailboxEnvelope>> {
        anyhow::ensure!(
            (1..=100).contains(&limit),
            "agent mailbox limit must be between 1 and 100"
        );
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let messages: Vec<_> = project(&state.entries, Some(&self.target_session_id))
            .into_iter()
            .filter(|envelope| envelope.matches(filter))
            .take(limit)
            .collect();
        if consume {
            for envelope in &messages {
                self.consume_locked(&mut state, envelope)?;
            }
        }
        Ok(messages)
    }

    /// # Errors
    /// Returns an error for invalid timeout, cancellation, or failed durable consumption.
    pub async fn wait(
        &self,
        filter: &MailboxFilter,
        timeout_ms: u64,
        signal: &AbortSignal,
    ) -> anyhow::Result<Option<MailboxEnvelope>> {
        anyhow::ensure!(
            (1..=300_000).contains(&timeout_ms),
            "agent mailbox timeout must be between 1 and 300000"
        );
        throw_if_aborted(signal)?;
        let id = uuid::Uuid::new_v4();
        let receiver = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(envelope) = project(&state.entries, Some(&self.target_session_id))
                .into_iter()
                .find(|envelope| envelope.matches(filter))
            {
                self.consume_locked(&mut state, &envelope)?;
                return Ok(Some(envelope));
            }
            anyhow::ensure!(
                state.closed.is_none(),
                "{}",
                state.closed.as_deref().unwrap_or_default()
            );
            state.waiters.retain(|waiter| !waiter.sender.is_closed());
            anyhow::ensure!(
                state.waiters.len() < 256,
                "agent mailbox waiter capacity reached"
            );
            let (sender, receiver) = oneshot::channel();
            state.waiters.push(MailboxWaiter {
                id,
                filter: filter.clone(),
                sender,
            });
            receiver
        };
        let _registration = WaitRegistration { mailbox: self, id };
        tokio::select! {
            biased;
            () = signal.aborted() => Err(aborted_error()),
            () = tokio::time::sleep(Duration::from_millis(timeout_ms)) => Ok(None),
            message = receiver => message.map_err(|_| anyhow::anyhow!("agent mailbox wait was closed"))?.map(Some),
        }
    }

    pub fn close(&self, reason: &str) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.closed.is_some() {
            return;
        }
        state.closed = Some(reason.to_string());
        for waiter in state.waiters.drain(..) {
            let _ = waiter.sender.send(Err(anyhow::anyhow!(reason.to_string())));
        }
    }
}

pub fn register_mailbox_host_handlers(
    mailbox: Arc<DurableMailbox>,
    handlers: &mut HostRequestHandlers,
) {
    register_mailbox_provider_host_handlers(Arc::new(move || Ok(mailbox.clone())), handlers);
}

pub fn register_mailbox_provider_host_handlers(
    provider: MailboxProvider,
    handlers: &mut HostRequestHandlers,
) {
    let inbox = provider.clone();
    handlers.register(
        "agent_message.inbox",
        host_handler(move |payload| {
            let provider = inbox.clone();
            async move {
                let filter = super::normalize_filter(&payload.data)?;
                let limit = super::normalize_limit(payload.data.get("limit"))?;
                let consume = payload.data["consume"] == true;
                Ok(json!({"messages":provider()?.inbox(&filter, limit, consume)?}))
            }
        }),
    );
    handlers.register_duplex(
        "agent_message.wait",
        duplex_host_handler(move |payload, channel| {
            let provider = provider.clone();
            async move {
                let filter = super::normalize_filter(&payload.data)?;
                let timeout = super::normalize_timeout(payload.data.get("timeout_ms"))?;
                let signal = AbortSignal::never();
                let mailbox = provider()?;
                tokio::select! {
                    biased;
                    () = channel.signal.cancelled() => Err(aborted_error()),
                    result = mailbox.wait(&filter, timeout, &signal) => {
                        match result? {
                            Some(message) => Ok(json!({"message":message})),
                            None => Ok(json!({})),
                        }
                    },
                }
            }
        }),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use pa_agent::abort::AbortController;

    fn envelope(id: &str) -> MailboxEnvelope {
        MailboxEnvelope {
            id: id.into(),
            source: "agent_message".into(),
            message: id.into(),
            reply_to: Some("task".into()),
            from: Some(json!({"sessionId":"sender"})),
            from_relationship: Some("child".into()),
            target: json!({"sessionId":"target"}),
            accepted_at: "now".into(),
            sequence: 0,
        }
    }

    #[tokio::test]
    async fn event_wait_consumes_once_and_retry_after_reload_preserves_identity() {
        let journal: Arc<Mutex<Vec<Value>>> = Arc::default();
        let written = journal.clone();
        let mailbox = Arc::new(DurableMailbox::new(
            "target".into(),
            vec![],
            Arc::new(move |row| {
                written.lock().unwrap().push(row);
                Ok(())
            }),
        ));
        let waiter = tokio::spawn({
            let mailbox = mailbox.clone();
            async move {
                mailbox
                    .wait(&MailboxFilter::default(), 1000, &AbortSignal::never())
                    .await
                    .unwrap()
            }
        });
        tokio::task::yield_now().await;
        let accepted = mailbox.accept(envelope("m")).unwrap();
        assert_eq!(waiter.await.unwrap().unwrap(), accepted);
        assert!(mailbox
            .inbox(&MailboxFilter::default(), 20, false)
            .unwrap()
            .is_empty());
        let recovered = DurableMailbox::new(
            "target".into(),
            journal.lock().unwrap().clone(),
            Arc::new(|_| Ok(())),
        );
        let mut retried = envelope("m");
        retried.message = "retry must not replace the original".into();
        assert_eq!(recovered.accept(retried).unwrap(), accepted);
        assert!(recovered
            .inbox(&MailboxFilter::default(), 20, false)
            .unwrap()
            .is_empty());
        assert_eq!(journal.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn concurrent_waiters_take_distinct_messages_and_timeout_or_abort_leave_inbox_intact() {
        let mailbox = Arc::new(DurableMailbox::new(
            "target".into(),
            vec![],
            Arc::new(|_| Ok(())),
        ));
        mailbox.accept(envelope("first")).unwrap();
        mailbox.accept(envelope("second")).unwrap();
        let wait = || {
            let mailbox = mailbox.clone();
            tokio::spawn(async move {
                mailbox
                    .wait(&MailboxFilter::default(), 100, &AbortSignal::never())
                    .await
                    .unwrap()
                    .unwrap()
            })
        };
        let (first, second) = tokio::join!(wait(), wait());
        assert_ne!(first.unwrap().id, second.unwrap().id);
        assert!(mailbox
            .wait(&MailboxFilter::default(), 1, &AbortSignal::never())
            .await
            .unwrap()
            .is_none());
        mailbox.accept(envelope("third")).unwrap();
        let cancelled = AbortController::new();
        cancelled.abort();
        assert!(pa_agent::abort::is_abort_error(
            &mailbox
                .wait(&MailboxFilter::default(), 100, &cancelled.signal())
                .await
                .unwrap_err()
        ));
        assert_eq!(
            mailbox.inbox(&MailboxFilter::default(), 20, false).unwrap()[0].id,
            "third"
        );
        let active_cancel = AbortController::new();
        let blocked = tokio::spawn({
            let mailbox = mailbox.clone();
            let signal = active_cancel.signal();
            async move {
                mailbox
                    .wait(
                        &MailboxFilter {
                            sender: Some("other".into()),
                            reply_to: None,
                        },
                        1000,
                        &signal,
                    )
                    .await
            }
        });
        tokio::task::yield_now().await;
        active_cancel.abort();
        let result = tokio::time::timeout(Duration::from_millis(100), blocked)
            .await
            .unwrap()
            .unwrap();
        assert!(pa_agent::abort::is_abort_error(&result.unwrap_err()));
        assert_eq!(
            mailbox.inbox(&MailboxFilter::default(), 20, false).unwrap()[0].id,
            "third"
        );
    }

    #[test]
    fn failed_acceptance_or_consumption_does_not_advance_memory() {
        let fail = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let rejected = fail.clone();
        let mailbox = DurableMailbox::new(
            "target".into(),
            vec![],
            Arc::new(move |_| {
                anyhow::ensure!(
                    !rejected.load(std::sync::atomic::Ordering::SeqCst),
                    "disk failed"
                );
                Ok(())
            }),
        );
        assert!(mailbox.accept(envelope("m")).is_err());
        assert!(mailbox
            .inbox(&MailboxFilter::default(), 20, false)
            .unwrap()
            .is_empty());
        fail.store(false, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(mailbox.accept(envelope("m")).unwrap().sequence, 1);
        fail.store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(mailbox.inbox(&MailboxFilter::default(), 20, true).is_err());
        assert_eq!(
            mailbox
                .inbox(&MailboxFilter::default(), 20, false)
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn completed_handoffs_survive_reload_without_repeating_delivery() {
        let journal: Arc<Mutex<Vec<Value>>> = Arc::default();
        let written = journal.clone();
        let mailbox = DurableMailbox::new(
            "target".into(),
            vec![],
            Arc::new(move |row| {
                written.lock().unwrap().push(row);
                Ok(())
            }),
        );
        let receipt = mailbox
            .receive(envelope("m"), |_| Ok(MailboxDelivery::Queued))
            .unwrap();
        assert_eq!(receipt.delivery_status, "queued");
        assert_eq!(receipt.handoff, "queue");
        let recovered = DurableMailbox::new(
            "target".into(),
            journal.lock().unwrap().clone(),
            Arc::new(|_| Ok(())),
        );
        let mut retry = envelope("m");
        retry.message = "changed".into();
        let receipt = recovered
            .receive(retry, |_| panic!("retry must not deliver twice"))
            .unwrap();
        assert_eq!(receipt.envelope.message, "m");
        assert_eq!(receipt.handoff, "retry");
        assert_eq!(receipt.delivery_status, "queued");
        assert_eq!(
            recovered
                .inbox(&MailboxFilter::default(), 20, true)
                .unwrap()
                .len(),
            1
        );
        assert!(recovered
            .inbox(&MailboxFilter::default(), 20, false)
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn waiter_handoff_bypasses_queue_and_consumed_retries_do_not_wake_another_waiter() {
        let mailbox = Arc::new(DurableMailbox::new(
            "target".into(),
            vec![],
            Arc::new(|_| Ok(())),
        ));
        let waiter = tokio::spawn({
            let mailbox = mailbox.clone();
            async move {
                mailbox
                    .wait(&MailboxFilter::default(), 1000, &AbortSignal::never())
                    .await
            }
        });
        tokio::task::yield_now().await;
        let receipt = mailbox
            .receive(envelope("m"), |_| panic!("waiter must receive directly"))
            .unwrap();
        assert_eq!(receipt.handoff, "waiter");
        assert_eq!(waiter.await.unwrap().unwrap().unwrap().id, "m");
        let waiter = tokio::spawn({
            let mailbox = mailbox.clone();
            async move {
                mailbox
                    .wait(&MailboxFilter::default(), 1000, &AbortSignal::never())
                    .await
            }
        });
        tokio::task::yield_now().await;
        mailbox.accept(envelope("m")).unwrap();
        assert!(!waiter.is_finished());
        mailbox.close("session disposed");
        assert_eq!(
            waiter.await.unwrap().unwrap_err().to_string(),
            "session disposed"
        );
        assert!(mailbox.accept(envelope("new")).is_err());
    }

    #[tokio::test]
    async fn dropped_waiter_leaves_delivery_in_the_durable_inbox() {
        let mailbox = Arc::new(DurableMailbox::new(
            "target".into(),
            vec![],
            Arc::new(|_| Ok(())),
        ));
        let waiter = tokio::spawn({
            let mailbox = mailbox.clone();
            async move {
                mailbox
                    .wait(&MailboxFilter::default(), 1000, &AbortSignal::never())
                    .await
            }
        });
        tokio::task::yield_now().await;
        waiter.abort();
        assert!(waiter.await.unwrap_err().is_cancelled());
        let receipt = mailbox
            .receive(envelope("m"), |_| Ok(MailboxDelivery::Woken))
            .unwrap();
        assert_eq!(receipt.handoff, "context");
        assert_eq!(
            mailbox.inbox(&MailboxFilter::default(), 20, false).unwrap()[0].id,
            "m"
        );
    }

    #[tokio::test]
    async fn kernel_handlers_validate_inbox_and_cancel_wait_without_consuming() {
        use crate::kernel::cancellation::AbortSignal as KernelAbortSignal;
        use crate::kernel::host_channel::HostRequestChannel;
        use crate::kernel::shared::HostRequestPayload;

        let mailbox = Arc::new(DurableMailbox::new(
            "target".into(),
            vec![],
            Arc::new(|_| Ok(())),
        ));
        mailbox.accept(envelope("m")).unwrap();
        let mut handlers = HostRequestHandlers::new();
        register_mailbox_host_handlers(mailbox.clone(), &mut handlers);
        let request = |data| HostRequestPayload {
            data,
            cell_source_code: None,
        };
        let inbox = handlers.get("agent_message.inbox").unwrap();
        assert_eq!(
            inbox(request(json!({}))).await.unwrap()["messages"][0]["id"],
            "m"
        );
        assert!(inbox(request(json!({"limit":101}))).await.is_err());
        let signal = KernelAbortSignal::new();
        let (channel, _sender) = HostRequestChannel::new(
            signal.clone(),
            None,
            None,
            Arc::new(|_| Box::pin(async { Ok(()) })),
            Arc::new(|_| {}),
        );
        let wait = handlers.get_duplex("agent_message.wait").unwrap().clone();
        let pending =
            tokio::spawn(async move { wait(request(json!({"sender":"missing"})), channel).await });
        tokio::task::yield_now().await;
        signal.abort();
        assert!(pa_agent::abort::is_abort_error(
            &pending.await.unwrap().unwrap_err()
        ));
        assert_eq!(
            mailbox
                .inbox(&MailboxFilter::default(), 20, false)
                .unwrap()
                .len(),
            1
        );
    }
}
