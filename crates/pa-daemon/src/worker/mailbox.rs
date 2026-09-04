use std::sync::Arc;

use crate::protocol::{response_failure, response_success, DaemonResponse};
use anyhow::{Context, Result};
use pa_agent::abort::AbortSignal;
use pa_core::session_engine::agent_messaging::mailbox::runtime::DurableMailbox;
use pa_core::session_engine::agent_messaging::mailbox::{
    normalize_filter, normalize_limit, normalize_timeout,
};
use serde_json::{json, Value};

use super::Worker;

pub(super) struct WorkerMailbox {
    session_id: String,
    mailbox: Arc<DurableMailbox>,
}

impl Worker {
    pub(super) fn handle_mailbox_inbox(&self, payload: &Value) -> DaemonResponse {
        let command = "agent_message_inbox";
        if let Err(response) = self.require_created(command) {
            return response;
        }
        let result = (|| -> Result<Value> {
            let filter = normalize_filter(
                &json!({"sender":payload.get("sender"), "reply_to":payload.get("replyTo")}),
            )?;
            let limit = normalize_limit(payload.get("limit"))?;
            let consume = payload
                .get("consume")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let mailbox = self.session_mailbox()?;
            Ok(json!({"messages":mailbox.inbox(&filter, limit, consume)?}))
        })();
        match result {
            Ok(data) => response_success(None, command, Some(data)),
            Err(error) => response_failure(None, command, &error.to_string(), None),
        }
    }

    pub(super) async fn handle_mailbox_wait(&self, payload: &Value) -> DaemonResponse {
        let command = "agent_message_wait";
        if let Err(response) = self.require_created(command) {
            return response;
        }
        let result = async {
            let filter = normalize_filter(
                &json!({"sender":payload.get("sender"), "reply_to":payload.get("replyTo")}),
            )?;
            let timeout = normalize_timeout(payload.get("timeoutMs"))?;
            let mailbox = self.session_mailbox()?;
            let message = mailbox
                .wait(&filter, timeout, &AbortSignal::never())
                .await?;
            Ok::<_, anyhow::Error>(
                message.map_or_else(|| json!({}), |message| json!({"message":message})),
            )
        }
        .await;
        match result {
            Ok(data) => response_success(None, command, Some(data)),
            Err(error) => response_failure(None, command, &error.to_string(), None),
        }
    }

    pub(super) fn close_mailbox(&self, reason: &str) {
        self.mailbox_closed
            .store(true, std::sync::atomic::Ordering::Release);
        if let Some(current) = self
            .mailbox
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
        {
            current.mailbox.close(reason);
        }
    }

    pub(super) fn discard_closed_mailbox(&self) {
        let mut mailbox = self
            .mailbox
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        mailbox.take();
        self.mailbox_closed
            .store(false, std::sync::atomic::Ordering::Release);
    }

    pub(super) fn checkpoint_mailbox_delivery(
        &self,
        session_id: &str,
        operation: &str,
    ) -> Result<()> {
        let mut recovery = self
            .recovery
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(journal) = recovery.as_mut() else {
            return Ok(());
        };
        let core = self
            .core
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let store = core
            .store
            .as_ref()
            .context("Session is still initializing")?;
        anyhow::ensure!(store.session_id() == session_id, "Session was replaced");
        let active_session_id = core.active_session_id.clone();
        let session_file = store.path.to_string_lossy().to_string();
        let lanes = super::queue_lanes(&core);
        drop(core);
        journal.record_queue_checkpoint(
            &active_session_id,
            session_id,
            Some(&session_file),
            true,
            operation,
            &lanes.steering,
            &lanes.follow_up,
        )
    }

    pub(super) fn session_mailbox(&self) -> Result<Arc<DurableMailbox>> {
        restore_worker_mailbox(&self.core, &self.mailbox, &self.mailbox_closed)
    }
}

pub(super) fn restore_worker_mailbox(
    core_lock: &Arc<std::sync::Mutex<super::SessionCore>>,
    cache: &std::sync::Mutex<Option<WorkerMailbox>>,
    closed: &std::sync::atomic::AtomicBool,
) -> Result<Arc<DurableMailbox>> {
    let mut cache = cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    anyhow::ensure!(
        !closed.load(std::sync::atomic::Ordering::Acquire),
        "Session is closed"
    );
    let core = core_lock
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let store = core
        .store
        .as_ref()
        .context("Session is still initializing")?;
    let session_id = store.session_id().to_string();
    if let Some(current) = cache
        .as_ref()
        .filter(|current| current.session_id == session_id)
    {
        return Ok(current.mailbox.clone());
    }
    let path = store.path.clone();
    let mut entries = if store.window.is_some() {
        crate::session_store::parse_session_entries(&std::fs::read_to_string(&path)?)
    } else {
        Vec::new()
    };
    for entry in store.entries() {
        entries.push(serde_json::to_value(entry)?);
    }
    drop(core);
    let weak_core = Arc::downgrade(core_lock);
    let expected_session_id = session_id.clone();
    let mailbox = Arc::new(DurableMailbox::new(
        session_id.clone(),
        entries,
        Arc::new(move |mut row| {
            let core = weak_core.upgrade().context("Session was disposed")?;
            let mut core = core
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            anyhow::ensure!(
                core.created && !core.shutdown_requested,
                "Session is closed"
            );
            let store = core
                .store
                .as_mut()
                .context("Session is still initializing")?;
            anyhow::ensure!(
                store.session_id() == expected_session_id && store.path == path,
                "Session was replaced"
            );
            let fields = row
                .as_object_mut()
                .context("Mailbox entry must be an object")?;
            fields.remove("type");
            store.persist_entry("custom_message", row)?;
            Ok(())
        }),
    ));
    if let Some(previous) = cache.replace(WorkerMailbox {
        session_id,
        mailbox: mailbox.clone(),
    }) {
        previous.mailbox.close("Session was replaced");
    }
    Ok(mailbox)
}
