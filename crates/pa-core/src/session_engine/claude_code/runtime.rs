use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use pa_agent::abort::{AbortController, AbortSignal};
use serde::{Deserialize, Serialize};
use tokio::sync::{watch, Notify};

use super::input::{InputDelivery, InputMailbox};
use super::{validate_init, ClaudeCodeEvent, ClaudeCodeUsage};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RuntimeStatus {
    Queued,
    Starting,
    Running,
    Done,
    Error,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeSnapshot {
    pub status: RuntimeStatus,
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub answer_preview: Option<String>,
    pub tool_use_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub running_tool: Option<String>,
    pub usage: ClaudeCodeUsage,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub closed: bool,
    pub turn_idle: bool,
}

struct RuntimeState {
    snapshot: RuntimeSnapshot,
    tool_use_ids: HashSet<String>,
    admission: Option<RuntimeSnapshot>,
    initial_completion: Option<RuntimeSnapshot>,
}

pub struct ClaudeCodeRuntime {
    input: Arc<InputMailbox>,
    state: Mutex<RuntimeState>,
    required_tools: Vec<String>,
    abort: AbortController,
    snapshots: watch::Sender<RuntimeSnapshot>,
    settled: Notify,
}

impl ClaudeCodeRuntime {
    #[must_use]
    pub fn new(prompt: String, model: String, required_tools: Vec<String>) -> Self {
        let snapshot = RuntimeSnapshot {
            status: RuntimeStatus::Queued,
            model,
            session_id: None,
            answer_preview: None,
            tool_use_count: 0,
            running_tool: None,
            usage: ClaudeCodeUsage::default(),
            error: None,
            closed: false,
            turn_idle: false,
        };
        let (snapshots, _) = watch::channel(snapshot.clone());
        Self {
            input: Arc::new(InputMailbox::new(prompt)),
            state: Mutex::new(RuntimeState {
                snapshot,
                tool_use_ids: HashSet::new(),
                admission: None,
                initial_completion: None,
            }),
            required_tools,
            abort: AbortController::new(),
            snapshots,
            settled: Notify::new(),
        }
    }

    pub fn input(&self) -> Arc<InputMailbox> {
        self.input.clone()
    }
    pub fn signal(&self) -> AbortSignal {
        self.abort.signal()
    }
    pub fn subscribe(&self) -> watch::Receiver<RuntimeSnapshot> {
        self.snapshots.subscribe()
    }

    pub fn snapshot(&self) -> RuntimeSnapshot {
        let mut snapshot = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .snapshot
            .clone();
        snapshot.turn_idle = self.input.turn_idle();
        snapshot
    }

    /// # Errors
    /// Returns an error if this retained runtime has already started.
    pub fn begin_start(&self) -> anyhow::Result<()> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        anyhow::ensure!(
            state.snapshot.status == RuntimeStatus::Queued,
            "Claude Code runtime has already started"
        );
        state.snapshot.status = RuntimeStatus::Starting;
        self.publish(&mut state);
        Ok(())
    }

    /// # Errors
    /// Returns an error if the runtime is closed, failed, cancelled, or at input capacity.
    pub fn deliver(&self, text: String) -> anyhow::Result<InputDelivery> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        anyhow::ensure!(
            !state.snapshot.closed
                && !matches!(
                    state.snapshot.status,
                    RuntimeStatus::Error | RuntimeStatus::Cancelled
                ),
            "{}",
            state
                .snapshot
                .error
                .as_deref()
                .unwrap_or("Claude Code runtime is unavailable")
        );
        let delivery = self.input.enqueue(text)?;
        state.snapshot.status = RuntimeStatus::Running;
        state.snapshot.running_tool = None;
        self.publish(&mut state);
        Ok(delivery)
    }

    /// # Errors
    /// Rejects invalid admission, pre-init events, error results, and unexpected close.
    /// Failures close input and settle both lifecycle waits.
    pub fn handle_event(&self, event: ClaudeCodeEvent) -> anyhow::Result<()> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.snapshot.closed {
            return Ok(());
        }
        let result = self.apply_event(&mut state, event);
        if let Err(error) = &result {
            self.fail_locked(&mut state, error.to_string());
        }
        self.publish(&mut state);
        result
    }

    fn apply_event(&self, state: &mut RuntimeState, event: ClaudeCodeEvent) -> anyhow::Result<()> {
        if let ClaudeCodeEvent::Init { session_id, .. } = &event {
            validate_init(&event, &self.required_tools)?;
            anyhow::ensure!(
                state.snapshot.session_id.is_none(),
                "Claude Code emitted duplicate init"
            );
            state.snapshot.session_id = Some(session_id.clone());
            state.snapshot.status = RuntimeStatus::Running;
            state.admission = Some(state.snapshot.clone());
            return Ok(());
        }
        if let ClaudeCodeEvent::Close = event {
            anyhow::bail!("Claude Code query ended unexpectedly");
        }
        anyhow::ensure!(
            state.snapshot.session_id.is_some(),
            "Claude Code emitted an event before init"
        );
        match event {
            ClaudeCodeEvent::Assistant { text, .. } => {
                if text.is_some() {
                    state.snapshot.answer_preview = text;
                }
            }
            ClaudeCodeEvent::ToolProgress {
                tool_use_id,
                tool_name,
                ..
            } => {
                if state.tool_use_ids.insert(tool_use_id) {
                    state.snapshot.tool_use_count += 1;
                }
                state.snapshot.running_tool = Some(tool_name);
            }
            ClaudeCodeEvent::Result {
                is_error,
                text,
                usage,
            } => {
                anyhow::ensure!(
                    !is_error,
                    "{}",
                    if text.is_empty() {
                        "Claude Code query failed"
                    } else {
                        &text
                    }
                );
                if !text.is_empty() {
                    state.snapshot.answer_preview = Some(text);
                }
                add_usage(&mut state.snapshot.usage, &usage)?;
                state.snapshot.running_tool = None;
                self.input.complete_turn();
                state.snapshot.status = RuntimeStatus::Done;
                state.snapshot.turn_idle = self.input.turn_idle();
                if state.initial_completion.is_none() {
                    state.initial_completion = Some(state.snapshot.clone());
                }
            }
            ClaudeCodeEvent::Error(error) => anyhow::bail!(error),
            ClaudeCodeEvent::Aborted(reason) => {
                anyhow::bail!(reason.unwrap_or_else(|| "Claude Code query aborted".into()))
            }
            ClaudeCodeEvent::Init { .. } | ClaudeCodeEvent::Close => unreachable!(),
        }
        Ok(())
    }

    pub fn fail(&self, error: String) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.snapshot.closed {
            return;
        }
        self.fail_locked(&mut state, error);
        self.publish(&mut state);
    }

    fn fail_locked(&self, state: &mut RuntimeState, error: String) {
        state.snapshot.status = RuntimeStatus::Error;
        state.snapshot.error = Some(error);
        self.close_locked(state);
    }

    pub fn abort(&self, reason: String) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.snapshot.closed {
            return;
        }
        state.snapshot.status = RuntimeStatus::Cancelled;
        state.snapshot.error = Some(reason);
        self.close_locked(&mut state);
        self.publish(&mut state);
    }

    pub fn dispose(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.snapshot.closed {
            return;
        }
        if state.initial_completion.is_none() {
            state.snapshot.status = RuntimeStatus::Cancelled;
            state.snapshot.error = Some("Claude Code runtime disposed".into());
        }
        self.close_locked(&mut state);
        self.publish(&mut state);
    }

    fn close_locked(&self, state: &mut RuntimeState) {
        state.snapshot.closed = true;
        state.snapshot.running_tool = None;
        self.input.close();
        self.abort.abort();
        state.snapshot.turn_idle = true;
        if state.admission.is_none() {
            state.admission = Some(state.snapshot.clone());
        }
        if state.initial_completion.is_none() {
            state.initial_completion = Some(state.snapshot.clone());
        }
    }

    fn publish(&self, state: &mut RuntimeState) {
        state.snapshot.turn_idle = self.input.turn_idle();
        self.snapshots.send_replace(state.snapshot.clone());
        self.settled.notify_waiters();
    }

    pub async fn admission(&self) -> RuntimeSnapshot {
        self.wait_settle(false).await
    }
    pub async fn initial_completion(&self) -> RuntimeSnapshot {
        self.wait_settle(true).await
    }

    async fn wait_settle(&self, initial: bool) -> RuntimeSnapshot {
        loop {
            let changed = self.settled.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            {
                let state = self
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let settled = if initial {
                    &state.initial_completion
                } else {
                    &state.admission
                };
                if let Some(snapshot) = settled {
                    return snapshot.clone();
                }
            }
            changed.await;
        }
    }
}

fn add_usage(total: &mut ClaudeCodeUsage, usage: &ClaudeCodeUsage) -> anyhow::Result<()> {
    let add = |a: u64, b: u64| {
        a.checked_add(b)
            .ok_or_else(|| anyhow::anyhow!("Claude Code usage overflowed"))
    };
    let next = ClaudeCodeUsage {
        input: add(total.input, usage.input)?,
        output: add(total.output, usage.output)?,
        cache_read: add(total.cache_read, usage.cache_read)?,
        cache_write: add(total.cache_write, usage.cache_write)?,
        total_tokens: add(total.total_tokens, usage.total_tokens)?,
        cost: total.cost + usage.cost,
        requests: add(total.requests, usage.requests)?,
    };
    anyhow::ensure!(next.cost.is_finite(), "Claude Code cost overflowed");
    *total = next;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn init() -> ClaudeCodeEvent {
        ClaudeCodeEvent::Init {
            model: "sonnet".into(),
            tools: vec!["Read".into()],
            version: "v".into(),
            session_id: "id".into(),
        }
    }

    #[tokio::test]
    async fn retained_runtime_counts_results_once_and_accepts_follow_up_after_done() {
        let runtime = ClaudeCodeRuntime::new("task".into(), "sonnet".into(), vec!["Read".into()]);
        runtime.begin_start().unwrap();
        let mut input = runtime.input().consumer().unwrap();
        input.next().await.unwrap();
        runtime.handle_event(init()).unwrap();
        assert_eq!(runtime.admission().await.status, RuntimeStatus::Running);
        for _ in 0..2 {
            runtime
                .handle_event(ClaudeCodeEvent::ToolProgress {
                    tool_use_id: "call".into(),
                    tool_name: "Read".into(),
                    elapsed_seconds: 1.0,
                })
                .unwrap();
        }
        runtime
            .handle_event(ClaudeCodeEvent::Assistant {
                text: Some("preview".into()),
                usage: ClaudeCodeUsage {
                    input: 100,
                    ..Default::default()
                },
            })
            .unwrap();
        let result = || ClaudeCodeEvent::Result {
            is_error: false,
            text: "answer".into(),
            usage: ClaudeCodeUsage {
                input: 7,
                output: 3,
                total_tokens: 10,
                cost: 0.25,
                requests: 1,
                ..Default::default()
            },
        };
        runtime.handle_event(result()).unwrap();
        let first = runtime.initial_completion().await;
        assert_eq!(first.usage.input, 7);
        assert_eq!(first.tool_use_count, 1);
        assert!(first.turn_idle);
        assert!(!first.closed);
        assert_eq!(
            runtime.deliver("follow-up".into()).unwrap(),
            InputDelivery::Woken
        );
        input.next().await.unwrap();
        runtime.handle_event(result()).unwrap();
        assert_eq!(runtime.snapshot().usage.input, 14);
        assert_eq!(runtime.initial_completion().await, first);
        runtime.dispose();
        assert!(runtime.snapshot().closed);
        assert_eq!(runtime.snapshot().status, RuntimeStatus::Done);
        assert!(input.next().await.is_none());
        assert!(runtime.deliver("late".into()).is_err());
    }

    #[tokio::test]
    async fn pre_init_failure_and_cancel_settle_all_waits_without_hanging() {
        let runtime = Arc::new(ClaudeCodeRuntime::new(
            "task".into(),
            "sonnet".into(),
            vec![],
        ));
        runtime.begin_start().unwrap();
        let admission = tokio::spawn({
            let runtime = runtime.clone();
            async move { runtime.admission().await }
        });
        let initial = tokio::spawn({
            let runtime = runtime.clone();
            async move { runtime.initial_completion().await }
        });
        assert!(runtime
            .handle_event(ClaudeCodeEvent::Assistant {
                text: None,
                usage: ClaudeCodeUsage::default()
            })
            .is_err());
        assert_eq!(admission.await.unwrap().status, RuntimeStatus::Error);
        assert_eq!(initial.await.unwrap().status, RuntimeStatus::Error);
        assert!(runtime.signal().is_aborted());
        assert!(runtime.snapshot().closed);
        let cancelled = ClaudeCodeRuntime::new("task".into(), "sonnet".into(), vec![]);
        cancelled.abort("cancelled".into());
        assert_eq!(cancelled.admission().await.status, RuntimeStatus::Cancelled);
        assert_eq!(
            cancelled.initial_completion().await.status,
            RuntimeStatus::Cancelled
        );
        assert!(cancelled.begin_start().is_err());
    }

    #[tokio::test]
    async fn unexpected_close_and_denied_admission_are_failures() {
        let runtime = ClaudeCodeRuntime::new("task".into(), "sonnet".into(), vec![]);
        runtime.begin_start().unwrap();
        runtime.handle_event(init()).unwrap();
        assert!(runtime.handle_event(ClaudeCodeEvent::Close).is_err());
        assert_eq!(
            runtime.initial_completion().await.status,
            RuntimeStatus::Error
        );
        let denied = ClaudeCodeRuntime::new("task".into(), "sonnet".into(), vec!["missing".into()]);
        denied.begin_start().unwrap();
        assert!(denied.handle_event(init()).is_err());
        assert_eq!(denied.admission().await.status, RuntimeStatus::Error);
        assert!(denied.snapshot().closed);
    }
}
