use std::future::Future;
use std::sync::{Arc, Mutex};

use crate::kernel::cancellation::AbortSignal;
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};
use uuid::Uuid;

tokio::task_local! {
    static CURRENT: (Uuid, Uuid);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RootForegroundActor {
    RootTurn,
    RootCell,
    Compaction,
    Refinement,
}

struct Holder {
    token: Uuid,
    act_depth: usize,
    released: AbortSignal,
    deferred_guard: Option<OwnedMutexGuard<()>>,
}

#[derive(Default)]
struct State {
    holder: Option<Holder>,
    pending: usize,
    disposed_error: Option<String>,
}

pub struct RootForegroundLease {
    id: Uuid,
    queue: Arc<AsyncMutex<()>>,
    state: Mutex<State>,
    disposed: AbortSignal,
}

impl Default for RootForegroundLease {
    fn default() -> Self {
        Self {
            id: Uuid::new_v4(),
            queue: Arc::new(AsyncMutex::new(())),
            state: Mutex::new(State::default()),
            disposed: AbortSignal::new(),
        }
    }
}

impl RootForegroundLease {
    pub fn busy(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .holder
            .is_some()
    }

    pub fn active_token(&self) -> Option<Uuid> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .holder
            .as_ref()
            .map(|holder| holder.token)
    }

    pub fn run_scope(self: &Arc<Self>) -> pa_agent::agent::AgentRunScopeFn {
        let foreground = self.clone();
        Arc::new(move |work| {
            let foreground = foreground.clone();
            Box::pin(async move {
                foreground
                    .run(RootForegroundActor::RootTurn, work, None)
                    .await
            })
        })
    }

    pub fn act_active(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .holder
            .as_ref()
            .is_some_and(|h| h.act_depth > 0)
    }

    pub fn pending_count(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pending
    }

    pub fn current_token(&self) -> Option<Uuid> {
        CURRENT
            .try_with(|(lease, token)| (*lease == self.id).then_some(*token))
            .ok()
            .flatten()
    }

    pub fn blocks_current_context(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .holder
            .as_ref()
            .is_some_and(|h| Some(h.token) != self.current_token())
    }

    /// # Errors
    /// Returns an error when admission is cancelled or the lease is disposed.
    pub async fn acquire(
        self: &Arc<Self>,
        _actor: RootForegroundActor,
        signal: Option<&AbortSignal>,
    ) -> anyhow::Result<RootForegroundHandle> {
        {
            let state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(error) = &state.disposed_error {
                anyhow::bail!("{error}");
            }
            if let Some(holder) = &state.holder {
                if self.current_token() == Some(holder.token) {
                    return Ok(RootForegroundHandle {
                        lease: self.clone(),
                        token: holder.token,
                        guard: None,
                    });
                }
            }
        }
        let pending = PendingAdmission::new(self.clone());
        let cancelled = async {
            match signal {
                Some(signal) => signal.cancelled().await,
                None => std::future::pending().await,
            }
        };
        let guard = tokio::select! {
            biased;
            () = self.disposed.cancelled() => {
                anyhow::bail!("{}", self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner).disposed_error.as_deref().unwrap_or("Root foreground lease disposed"));
            }
            () = cancelled => anyhow::bail!("Root foreground admission aborted"),
            guard = self.queue.clone().lock_owned() => guard,
        };
        drop(pending);
        let token = Uuid::new_v4();
        {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(error) = &state.disposed_error {
                anyhow::bail!("{error}");
            }
            state.holder = Some(Holder {
                token,
                act_depth: 0,
                released: AbortSignal::new(),
                deferred_guard: None,
            });
        }
        Ok(RootForegroundHandle {
            lease: self.clone(),
            token,
            guard: Some(guard),
        })
    }

    /// # Errors
    /// Returns an error on cancelled or disposed admission, or from the work.
    pub async fn run<T>(
        self: &Arc<Self>,
        actor: RootForegroundActor,
        work: impl Future<Output = anyhow::Result<T>>,
        signal: Option<&AbortSignal>,
    ) -> anyhow::Result<T> {
        let handle = self.acquire(actor, signal).await?;
        handle.run(work).await
    }

    /// # Errors
    /// Returns an error when the Act token does not identify the current holder.
    pub fn enter_act(self: &Arc<Self>, token: Uuid) -> anyhow::Result<ActForegroundGuard> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let holder = state
            .holder
            .as_mut()
            .filter(|h| h.token == token)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Act host request is not correlated to the active root foreground execution"
                )
            })?;
        holder.act_depth += 1;
        Ok(ActForegroundGuard {
            lease: self.clone(),
            token,
        })
    }

    pub async fn wait_for_current_actor_release(&self) {
        let released = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .holder
            .as_ref()
            .map(|h| h.released.clone());
        if let Some(released) = released {
            released.cancelled().await;
        }
    }

    pub fn dispose(&self, error: impl Into<String>) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.disposed_error.is_none() {
            state.disposed_error = Some(error.into());
            self.disposed.abort();
        }
    }
}

struct PendingAdmission(Arc<RootForegroundLease>);
impl PendingAdmission {
    fn new(lease: Arc<RootForegroundLease>) -> Self {
        lease
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pending += 1;
        Self(lease)
    }
}
impl Drop for PendingAdmission {
    fn drop(&mut self) {
        self.0
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pending -= 1;
    }
}

pub struct RootForegroundHandle {
    lease: Arc<RootForegroundLease>,
    pub token: Uuid,
    guard: Option<OwnedMutexGuard<()>>,
}
impl RootForegroundHandle {
    #[must_use]
    pub fn owned(&self) -> bool {
        self.guard.is_some()
    }

    pub async fn run<T>(&self, work: impl Future<Output = T>) -> T {
        CURRENT.scope((self.lease.id, self.token), work).await
    }
}
impl Drop for RootForegroundHandle {
    fn drop(&mut self) {
        if self.guard.is_some() {
            let mut state = self
                .lease
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(holder) = state
                .holder
                .as_mut()
                .filter(|holder| holder.token == self.token)
            {
                if holder.act_depth > 0 {
                    holder.deferred_guard = self.guard.take();
                } else if let Some(holder) = state.holder.take() {
                    holder.released.abort();
                }
            }
        }
    }
}

pub struct ActForegroundGuard {
    lease: Arc<RootForegroundLease>,
    token: Uuid,
}
impl Drop for ActForegroundGuard {
    fn drop(&mut self) {
        let mut state = self
            .lease
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(holder) = state.holder.as_mut().filter(|h| h.token == self.token) {
            holder.act_depth = holder.act_depth.saturating_sub(1);
            if holder.act_depth == 0 && holder.deferred_guard.is_some() {
                if let Some(holder) = state.holder.take() {
                    holder.released.abort();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn dropping_a_root_prompt_cancels_its_run_and_allows_a_later_turn() {
        use pa_agent::agent::{Agent, AgentOptions};
        use pa_agent::scripted::ScriptedProvider;
        use pa_agent::types::Model;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let provider = Arc::new(ScriptedProvider::new(Model::unknown()));
        provider.push_text_turn("resumed");
        let stream = provider.stream_fn();
        let calls = Arc::new(AtomicUsize::new(0));
        let captured = calls.clone();
        let agent = Arc::new(Agent::new(AgentOptions {
            stream_fn: Some(Arc::new(move |model, context, options| {
                let stream = stream.clone();
                let calls = captured.clone();
                Box::pin(async move {
                    if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                        std::future::pending::<()>().await;
                    }
                    stream(model, context, options).await
                })
            })),
            ..Default::default()
        }));
        let session = Arc::new(
            crate::session_engine::AgentSession::new(
                agent.clone(),
                crate::session::manager::SessionManager::in_memory(std::path::Path::new(".")),
                vec![],
            )
            .await
            .unwrap(),
        );
        let turn = {
            let session = session.clone();
            tokio::spawn(async move {
                session
                    .prompt("first", crate::session_engine::PromptOptions::default())
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
        turn.abort();
        assert!(turn.await.unwrap_err().is_cancelled());
        tokio::time::timeout(Duration::from_secs(1), agent.wait_for_idle())
            .await
            .unwrap();
        assert!(!agent.state().await.is_streaming);
        assert!(!session.foreground_lease().busy());
        tokio::time::timeout(
            Duration::from_secs(1),
            session.prompt("next", crate::session_engine::PromptOptions::default()),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn detached_accepted_turn_retains_the_lease_until_idle() {
        use pa_agent::agent::{Agent, AgentOptions};
        use pa_agent::scripted::ScriptedProvider;
        use pa_agent::types::Model;

        let provider = Arc::new(ScriptedProvider::new(Model::unknown()));
        provider.push_text_turn("done");
        let stream = provider.stream_fn();
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let stream_gate = gate.clone();
        let agent = Arc::new(Agent::new(AgentOptions {
            stream_fn: Some(Arc::new(move |model, context, options| {
                let gate = stream_gate.clone();
                let stream = stream.clone();
                Box::pin(async move {
                    let _permit = gate.acquire().await?;
                    stream(model, context, options).await
                })
            })),
            ..Default::default()
        }));
        let session = crate::session_engine::AgentSession::new(
            agent.clone(),
            crate::session::manager::SessionManager::in_memory(std::path::Path::new(".")),
            Vec::new(),
        )
        .await
        .unwrap();
        session
            .prompt(
                "go",
                crate::session_engine::PromptOptions {
                    return_after_accepted: true,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let lease = session.foreground_lease();
        assert!(lease.busy());
        assert!(agent.state().await.is_streaming);
        let waiter = {
            let lease = lease.clone();
            tokio::spawn(async move { lease.acquire(RootForegroundActor::Compaction, None).await })
        };
        tokio::time::timeout(Duration::from_secs(1), async {
            while lease.pending_count() == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(!waiter.is_finished());
        let cancelled = pa_agent::abort::AbortController::new();
        cancelled.abort();
        let registration = pa_ai::faux::register_faux_provider(
            pa_ai::faux::RegisterFauxProviderOptions::default(),
        );
        let model = registration.get_model();
        let error = session
            .compact(None, &model, None, Some(&cancelled.signal()))
            .await
            .unwrap_err();
        assert!(pa_agent::abort::is_abort_error(&error));
        registration.unregister();
        gate.add_permits(1);
        agent.wait_for_idle().await;
        let admitted = tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        drop(admitted);
        assert!(!lease.busy());
    }

    #[tokio::test]
    async fn cancelled_owner_does_not_release_a_cooperatively_cleaning_act() {
        let lease = Arc::new(RootForegroundLease::default());
        let owner = lease
            .acquire(RootForegroundActor::RootTurn, None)
            .await
            .unwrap();
        let act = lease.enter_act(owner.token).unwrap();
        drop(owner);
        assert!(lease.busy());
        assert!(lease.act_active());
        assert!(tokio::time::timeout(
            Duration::from_millis(10),
            lease.acquire(RootForegroundActor::Compaction, None)
        )
        .await
        .is_err());
        drop(act);
        assert!(!lease.busy());
        let next = lease
            .acquire(RootForegroundActor::Compaction, None)
            .await
            .unwrap();
        drop(next);
    }

    #[tokio::test]
    async fn reentrant_work_and_act_share_one_holder() {
        let lease = Arc::new(RootForegroundLease::default());
        lease
            .run(
                RootForegroundActor::RootCell,
                async {
                    let token = lease.current_token().unwrap();
                    let act = lease.enter_act(token)?;
                    assert!(lease.act_active());
                    let nested = lease.acquire(RootForegroundActor::RootTurn, None).await?;
                    assert!(!nested.owned());
                    assert_eq!(nested.token, token);
                    drop(nested);
                    assert!(lease.busy());
                    drop(act);
                    assert!(!lease.act_active());
                    Ok(())
                },
                None,
            )
            .await
            .unwrap();
        assert!(!lease.busy());
    }

    #[tokio::test]
    async fn cancellation_and_disposal_reject_waiters_without_releasing_holder() {
        let lease = Arc::new(RootForegroundLease::default());
        let holder = lease
            .acquire(RootForegroundActor::RootCell, None)
            .await
            .unwrap();
        let signal = AbortSignal::aborted();
        assert!(lease
            .acquire(RootForegroundActor::RootTurn, Some(&signal))
            .await
            .is_err());
        assert_eq!(lease.pending_count(), 0);
        lease.dispose("session gone");
        assert!(lease
            .acquire(RootForegroundActor::Compaction, None)
            .await
            .is_err());
        assert!(lease.busy());
        drop(holder);
        assert!(!lease.busy());
    }

    #[tokio::test]
    async fn dropping_work_releases_holder_and_admits_waiter() {
        let lease = Arc::new(RootForegroundLease::default());
        let holder = lease
            .acquire(RootForegroundActor::RootCell, None)
            .await
            .unwrap();
        let waiting = {
            let lease = lease.clone();
            tokio::spawn(async move { lease.acquire(RootForegroundActor::RootTurn, None).await })
        };
        tokio::task::yield_now().await;
        assert_eq!(lease.pending_count(), 1);
        assert!(!waiting.is_finished());
        drop(holder);
        let later = tokio::time::timeout(Duration::from_secs(1), waiting)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(later.owned());
        drop(later);
        assert!(!lease.busy());
    }

    #[tokio::test]
    async fn admission_is_fifo_and_release_waiters_follow_the_captured_holder() {
        let lease = Arc::new(RootForegroundLease::default());
        let holder = lease
            .acquire(RootForegroundActor::RootCell, None)
            .await
            .unwrap();
        let order = Arc::new(Mutex::new(Vec::new()));
        let mut tasks = Vec::new();
        for index in 0..3 {
            let worker = lease.clone();
            let order = order.clone();
            tasks.push(tokio::spawn(async move {
                let handle = worker
                    .acquire(RootForegroundActor::RootTurn, None)
                    .await
                    .unwrap();
                order.lock().unwrap().push(index);
                drop(handle);
            }));
            tokio::time::timeout(Duration::from_secs(1), async {
                while lease.pending_count() != index + 1 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
        }
        let released = {
            let lease = lease.clone();
            tokio::spawn(async move { lease.wait_for_current_actor_release().await })
        };
        tokio::task::yield_now().await;
        assert!(!released.is_finished());
        drop(holder);
        tokio::time::timeout(Duration::from_secs(1), released)
            .await
            .unwrap()
            .unwrap();
        for task in tasks {
            task.await.unwrap();
        }
        assert_eq!(*order.lock().unwrap(), [0, 1, 2]);
    }
}
