use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::watch;

use crate::kernel::shared::{host_handler, HostRequestHandlers};

pub const EXTERNAL_EVENT_MAX_RETAINED_IDS: usize = 1024;
pub const EXTERNAL_EVENT_MAX_PENDING: usize = 128;
pub const EXTERNAL_EVENT_MAX_WATCHES: usize = 128;
const OPERATION: &str = "session.external_event.emit";

type EventKey = (String, String);
type AdmissionResult = Result<ExternalEventDeliveryStatus, String>;
type AdmissionSender = watch::Sender<Option<AdmissionResult>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ExternalEventDeliveryStatus {
    Delivered,
    Queued,
    Coalesced,
}

#[derive(Debug, Clone)]
pub struct ExternalEventInput {
    pub name: String,
    pub event_id: String,
    pub text: String,
}

pub type ExternalEventEmit = Arc<
    dyn Fn(
            ExternalEventInput,
        )
            -> Pin<Box<dyn Future<Output = anyhow::Result<ExternalEventDeliveryStatus>> + Send>>
        + Send
        + Sync,
>;

#[derive(Default)]
struct RegistryState {
    pending: HashMap<EventKey, AdmissionSender>,
    retained: HashSet<EventKey>,
    order: VecDeque<EventKey>,
    disposed: bool,
}

#[derive(Default)]
pub struct ExternalEventRegistry {
    state: Mutex<RegistryState>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl ExternalEventRegistry {
    pub fn retained_count(&self) -> usize {
        lock(&self.state).retained.len()
    }

    pub fn dispose(&self) {
        let mut state = lock(&self.state);
        state.disposed = true;
        for pending in state.pending.values() {
            pending.send_replace(Some(Err("External event session was disposed".into())));
        }
        state.pending.clear();
        state.retained.clear();
        state.order.clear();
    }

    /// # Errors
    /// Returns the admission failure, or an error when capacity is exhausted or disposed.
    pub async fn admit<F, Fut>(
        self: &Arc<Self>,
        name: String,
        event_id: String,
        emit: F,
    ) -> anyhow::Result<ExternalEventDeliveryStatus>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = anyhow::Result<ExternalEventDeliveryStatus>>,
    {
        let key = (name, event_id);
        let (owner, mut receiver) = {
            let mut state = lock(&self.state);
            anyhow::ensure!(
                !state.disposed,
                "Cannot admit an external event after its session was disposed."
            );
            if state.retained.contains(&key) {
                return Ok(ExternalEventDeliveryStatus::Coalesced);
            }
            if let Some(pending) = state.pending.get(&key) {
                (None, pending.subscribe())
            } else {
                anyhow::ensure!(
                    state.pending.len() < EXTERNAL_EVENT_MAX_PENDING,
                    "{OPERATION} queue is full: maximum is {EXTERNAL_EVENT_MAX_PENDING} events"
                );
                let (sender, receiver) = watch::channel(None);
                state.pending.insert(key.clone(), sender.clone());
                (
                    Some(PendingAdmission {
                        registry: self.clone(),
                        key: key.clone(),
                        sender,
                    }),
                    receiver,
                )
            }
        };
        if let Some(owner) = owner {
            let mut result = emit().await;
            {
                let mut state = lock(&self.state);
                if state.disposed {
                    result = Err(anyhow::anyhow!("External event session was disposed"));
                }
                if result.is_ok() && !state.disposed {
                    state.retained.insert(key.clone());
                    state.order.push_back(key);
                    while state.order.len() > EXTERNAL_EVENT_MAX_RETAINED_IDS {
                        if let Some(oldest) = state.order.pop_front() {
                            state.retained.remove(&oldest);
                        }
                    }
                }
                state.pending.remove(&owner.key);
                owner
                    .sender
                    .send_replace(Some(result.as_ref().copied().map_err(ToString::to_string)));
            }
            drop(owner);
            result
        } else {
            loop {
                if let Some(result) = receiver.borrow_and_update().clone() {
                    return result
                        .map(|_| ExternalEventDeliveryStatus::Coalesced)
                        .map_err(anyhow::Error::msg);
                }
                receiver
                    .changed()
                    .await
                    .map_err(|_| anyhow::anyhow!("External event admission cancelled"))?;
            }
        }
    }
}

struct PendingAdmission {
    registry: Arc<ExternalEventRegistry>,
    key: EventKey,
    sender: AdmissionSender,
}
impl Drop for PendingAdmission {
    fn drop(&mut self) {
        let mut state = lock(&self.registry.state);
        if state
            .pending
            .get(&self.key)
            .is_some_and(|sender| sender.same_channel(&self.sender))
        {
            state.pending.remove(&self.key);
            self.sender
                .send_replace(Some(Err("External event admission cancelled".into())));
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExternalEventWatchStatus {
    Running,
    Completed,
    Failed,
    TimedOut,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ExternalEventWatch {
    pub id: String,
    pub label: String,
    pub pid: Option<i64>,
    pub command: Option<String>,
    pub ssh: Option<String>,
    pub status: ExternalEventWatchStatus,
}

pub type ExternalEventWatchSink = Arc<dyn Fn(Vec<ExternalEventWatch>) + Send + Sync>;

fn bounded_field(payload: &Value, field: &str, max: usize) -> anyhow::Result<String> {
    let value = payload
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("{OPERATION} {field} must be a string"))?;
    anyhow::ensure!(
        !value.trim().is_empty(),
        "{OPERATION} {field} cannot be empty"
    );
    anyhow::ensure!(
        value.encode_utf16().count() <= max,
        "{OPERATION} {field} is too long: maximum is {max} characters"
    );
    Ok(value.to_owned())
}

/// # Errors
/// Returns an error for malformed, oversized, or duplicate watch records.
pub fn normalize_external_event_watches(
    payload: &Value,
) -> anyhow::Result<Vec<ExternalEventWatch>> {
    let jobs = payload
        .get("jobs")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("{OPERATION} watches must carry a jobs array"))?;
    anyhow::ensure!(
        jobs.len() <= EXTERNAL_EVENT_MAX_WATCHES,
        "{OPERATION} watches cannot exceed {EXTERNAL_EVENT_MAX_WATCHES} jobs"
    );
    let mut seen = HashSet::new();
    let mut watches = Vec::with_capacity(jobs.len());
    for job in jobs {
        anyhow::ensure!(job.is_object(), "{OPERATION} watch entries must be objects");
        let id = bounded_field(job, "id", 512)?;
        let label = bounded_field(job, "label", 128)?;
        anyhow::ensure!(
            seen.insert(id.clone()),
            "{OPERATION} watches must not repeat job id {id}"
        );
        if let Some(command) = job.get("command").filter(|value| !value.is_null()) {
            anyhow::ensure!(
                command
                    .as_str()
                    .is_some_and(|text| text.encode_utf16().count() <= 2000),
                "{OPERATION} watch {id} command is too long"
            );
        }
        let mut watch: ExternalEventWatch = serde_json::from_value(job.clone())
            .map_err(|error| anyhow::anyhow!("{OPERATION} watch {id} is invalid: {error}"))?;
        watch.id = id;
        watch.label = label;
        watches.push(watch);
    }
    Ok(watches)
}

#[derive(Default)]
pub struct ExternalEventRuntime {
    pub registry: Arc<ExternalEventRegistry>,
    watches: Mutex<Vec<ExternalEventWatch>>,
    watch_sink: Mutex<Option<ExternalEventWatchSink>>,
    watch_update_order: Mutex<()>,
    changed: tokio::sync::Notify,
}

impl ExternalEventRuntime {
    pub async fn wait_for_watches(&self) {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if lock(&self.registry.state).disposed || !self.has_running_watches() {
                return;
            }
            changed.await;
        }
    }

    pub fn dispose(&self) {
        self.registry.dispose();
        self.changed.notify_waiters();
    }

    pub fn watches(&self) -> Vec<ExternalEventWatch> {
        lock(&self.watches).clone()
    }

    pub fn set_watch_sink(&self, sink: ExternalEventWatchSink) {
        *lock(&self.watch_sink) = Some(sink);
    }

    pub fn has_running_watches(&self) -> bool {
        lock(&self.watches)
            .iter()
            .any(|watch| watch.status == ExternalEventWatchStatus::Running)
    }

    /// # Errors
    /// Rejects updates after the session runtime has been disposed.
    pub fn update_watches(&self, watches: Vec<ExternalEventWatch>) -> anyhow::Result<()> {
        let jobs = serde_json::to_value(watches)?;
        let watches = normalize_external_event_watches(&json!({"jobs":jobs}))?;
        let _order = lock(&self.watch_update_order);
        let state = lock(&self.registry.state);
        anyhow::ensure!(!state.disposed, "External event session was disposed");
        let mut current = lock(&self.watches);
        if *current == watches {
            return Ok(());
        }
        current.clone_from(&watches);
        self.changed.notify_waiters();
        drop(current);
        drop(state);
        let sink = lock(&self.watch_sink).clone();
        if let Some(sink) = sink {
            sink(watches);
        }
        Ok(())
    }

    pub fn register(self: &Arc<Self>, handlers: &mut HostRequestHandlers, emit: ExternalEventEmit) {
        let registry = self.registry.clone();
        handlers.register(OPERATION, host_handler(move |payload| {
            let registry = registry.clone();
            let emit = emit.clone();
            async move {
                let input = ExternalEventInput {
                    name: bounded_field(&payload.data, "name", 128)?.trim().to_owned(),
                    event_id: bounded_field(&payload.data, "event_id", 512)?.trim().to_owned(),
                    text: bounded_field(&payload.data, "text", super::agent_messaging::DEFAULT_AGENT_MESSAGE_MAX_CHARS)?,
                };
                let status = registry.admit(input.name.clone(), input.event_id.clone(), || emit(input.clone())).await?;
                Ok(json!({"accepted":true, "deliveryStatus":status, "name":input.name, "eventId":input.event_id}))
            }
        }));
        let runtime = self.clone();
        handlers.register(
            "session.external_event.watch_update",
            host_handler(move |payload| {
                let runtime = runtime.clone();
                async move {
                    let watches = normalize_external_event_watches(&payload.data)?;
                    let count = watches.len();
                    runtime.update_watches(watches)?;
                    Ok(json!({"accepted":true, "count":count}))
                }
            }),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    #[tokio::test]
    async fn coalesces_successful_concurrent_events_without_invoking_the_emitter_twice() {
        let registry = Arc::new(ExternalEventRegistry::default());
        let calls = Arc::new(AtomicUsize::new(0));
        let gate = Arc::new(tokio::sync::Notify::new());
        let first = {
            let registry = registry.clone();
            let calls = calls.clone();
            let gate = gate.clone();
            tokio::spawn(async move {
                registry
                    .admit("job".into(), "event".into(), || {
                        calls.fetch_add(1, Ordering::SeqCst);
                        async move {
                            gate.notified().await;
                            Ok(ExternalEventDeliveryStatus::Queued)
                        }
                    })
                    .await
            })
        };
        tokio::time::timeout(Duration::from_secs(1), async {
            while calls.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let duplicate = {
            let registry = registry.clone();
            let calls = calls.clone();
            tokio::spawn(async move {
                registry
                    .admit("job".into(), "event".into(), || {
                        calls.fetch_add(1, Ordering::SeqCst);
                        async { Ok(ExternalEventDeliveryStatus::Delivered) }
                    })
                    .await
            })
        };
        tokio::task::yield_now().await;
        assert!(!duplicate.is_finished());
        gate.notify_one();
        assert_eq!(
            first.await.unwrap().unwrap(),
            ExternalEventDeliveryStatus::Queued
        );
        assert_eq!(
            duplicate.await.unwrap().unwrap(),
            ExternalEventDeliveryStatus::Coalesced
        );
        assert_eq!(
            registry
                .admit("job".into(), "event".into(), || {
                    calls.fetch_add(1, Ordering::SeqCst);
                    async { Ok(ExternalEventDeliveryStatus::Delivered) }
                })
                .await
                .unwrap(),
            ExternalEventDeliveryStatus::Coalesced
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn failed_and_cancelled_admissions_release_identity_for_retry() {
        let registry = Arc::new(ExternalEventRegistry::default());
        assert!(registry
            .admit("job".into(), "event".into(), || async {
                anyhow::bail!("failed")
            })
            .await
            .is_err());
        let pending = {
            let registry = registry.clone();
            tokio::spawn(async move {
                registry
                    .admit("job".into(), "event".into(), std::future::pending)
                    .await
            })
        };
        tokio::task::yield_now().await;
        pending.abort();
        let _ = pending.await;
        assert_eq!(
            registry
                .admit("job".into(), "event".into(), || async {
                    Ok(ExternalEventDeliveryStatus::Delivered)
                })
                .await
                .unwrap(),
            ExternalEventDeliveryStatus::Delivered
        );
    }

    #[tokio::test]
    async fn disposal_rejects_inflight_admission_even_when_the_emitter_succeeds() {
        let registry = Arc::new(ExternalEventRegistry::default());
        let gate = Arc::new(tokio::sync::Notify::new());
        let started = Arc::new(tokio::sync::Notify::new());
        let owner = {
            let registry = registry.clone();
            let gate = gate.clone();
            let started = started.clone();
            tokio::spawn(async move {
                registry
                    .admit("job".into(), "event".into(), || async {
                        started.notify_one();
                        gate.notified().await;
                        Ok(ExternalEventDeliveryStatus::Delivered)
                    })
                    .await
            })
        };
        started.notified().await;
        registry.dispose();
        gate.notify_one();
        assert!(owner
            .await
            .unwrap()
            .unwrap_err()
            .to_string()
            .contains("disposed"));
        assert_eq!(registry.retained_count(), 0);
    }

    #[tokio::test]
    async fn retention_is_bounded_and_keys_do_not_collide() {
        let registry = Arc::new(ExternalEventRegistry::default());
        for index in 0..=EXTERNAL_EVENT_MAX_RETAINED_IDS {
            registry
                .admit("job".into(), index.to_string(), || async {
                    Ok(ExternalEventDeliveryStatus::Queued)
                })
                .await
                .unwrap();
        }
        assert_eq!(registry.retained_count(), EXTERNAL_EVENT_MAX_RETAINED_IDS);
        assert_eq!(
            registry
                .admit("job".into(), "0".into(), || async {
                    Ok(ExternalEventDeliveryStatus::Delivered)
                })
                .await
                .unwrap(),
            ExternalEventDeliveryStatus::Delivered
        );
        for (name, id) in [("a:b", "c"), ("a", "b:c")] {
            assert_eq!(
                registry
                    .admit(name.into(), id.into(), || async {
                        Ok(ExternalEventDeliveryStatus::Delivered)
                    })
                    .await
                    .unwrap(),
                ExternalEventDeliveryStatus::Delivered
            );
        }
        registry.dispose();
        assert_eq!(registry.retained_count(), 0);
        assert!(registry
            .admit("job".into(), "new".into(), || async {
                Ok(ExternalEventDeliveryStatus::Delivered)
            })
            .await
            .is_err());
    }

    #[test]
    fn validates_watch_lists_and_preserves_optional_metadata() {
        let job = json!({"id":"job", "label":"capture", "pid":123, "command":"sleep 1", "ssh":"remote", "status":"running"});
        let watches = normalize_external_event_watches(&json!({"jobs":[job]})).unwrap();
        assert_eq!(watches[0].pid, Some(123));
        assert_eq!(watches[0].ssh.as_deref(), Some("remote"));
        assert!(normalize_external_event_watches(&json!({"jobs":[job.clone(),job]})).is_err());
        for (field, value) in [
            ("status", json!("unknown")),
            ("pid", json!(1.5)),
            ("ssh", json!(true)),
            ("command", json!("x".repeat(2001))),
        ] {
            let mut malformed = job.clone();
            malformed[field] = value;
            assert!(normalize_external_event_watches(&json!({"jobs":[malformed]})).is_err());
        }
        assert!(bounded_field(&json!({"name":"😀".repeat(65)}), "name", 128).is_err());
    }

    #[tokio::test]
    async fn watch_wait_ends_on_completion_or_disposal_without_missing_an_update() {
        let runtime = Arc::new(ExternalEventRuntime::default());
        let mut watches = normalize_external_event_watches(&json!({"jobs":[{
            "id":"job", "label":"build", "status":"running"
        }]}))
        .unwrap();
        runtime.update_watches(watches.clone()).unwrap();
        let waiting = {
            let runtime = runtime.clone();
            tokio::spawn(async move { runtime.wait_for_watches().await })
        };
        tokio::task::yield_now().await;
        assert!(!waiting.is_finished());
        watches[0].status = ExternalEventWatchStatus::Completed;
        runtime.update_watches(watches.clone()).unwrap();
        tokio::time::timeout(Duration::from_secs(1), waiting)
            .await
            .unwrap()
            .unwrap();
        watches[0].status = ExternalEventWatchStatus::Running;
        runtime.update_watches(watches).unwrap();
        let waiting = {
            let runtime = runtime.clone();
            tokio::spawn(async move { runtime.wait_for_watches().await })
        };
        runtime.dispose();
        tokio::time::timeout(Duration::from_secs(1), waiting)
            .await
            .unwrap()
            .unwrap();
    }

    #[test]
    fn watches_notify_on_changes_and_reject_updates_after_disposal() {
        let runtime = ExternalEventRuntime::default();
        let updates = Arc::new(Mutex::new(Vec::new()));
        let recorded = updates.clone();
        runtime.set_watch_sink(Arc::new(move |watches| lock(&recorded).push(watches)));
        let running = normalize_external_event_watches(&json!({"jobs":[{
            "id":"job", "label":"capture", "status":"running"
        }]}))
        .unwrap();
        runtime.update_watches(running.clone()).unwrap();
        runtime.update_watches(running.clone()).unwrap();
        assert!(runtime.has_running_watches());
        let mut completed = running.clone();
        completed[0].status = ExternalEventWatchStatus::Completed;
        runtime.update_watches(completed.clone()).unwrap();
        assert!(!runtime.has_running_watches());
        assert_eq!(*lock(&updates), vec![running.clone(), completed.clone()]);
        runtime.registry.dispose();
        assert!(runtime.update_watches(running).is_err());
        assert_eq!(runtime.watches(), completed);
        assert_eq!(lock(&updates).len(), 2);
    }
}
