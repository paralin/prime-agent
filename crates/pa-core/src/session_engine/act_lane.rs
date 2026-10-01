use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::session::manager::SessionManager;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, RwLock};

use pa_agent::agent::{Agent, AgentOptions, AgentPromptInput};
use pa_agent::stream::StreamFn;
use pa_agent::types::{
    AgentMessage, AgentTool, AgentToolResult, AgentToolUpdateCallback, ImageContent, Message,
    Model, StopReason, ThinkingLevel, ToolExecutionMode,
};
use serde_json::{json, Value};
use tokio::sync::Mutex as AsyncMutex;

use super::act_runtime::projection::ActProjection;
use crate::kernel::cancellation::AbortSignal;
use crate::kernel::host_channel::HostRequestChannel;

pub struct ActLaneTarget {
    pub session_key: String,
    pub model: Model,
    pub thinking_level: ThinkingLevel,
    pub stream_fn: StreamFn,
    pub depth: u32,
    pub max_depth: u32,
}

pub(crate) struct ActFallbackTarget {
    pub target: ActLaneTarget,
    pub model: pa_types::ai::Model,
}

pub(crate) type ActFallback = Arc<
    dyn Fn(
            pa_agent::types::AssistantMessage,
        ) -> pa_agent::BoxFut<'static, anyhow::Result<Option<ActFallbackTarget>>>
        + Send
        + Sync,
>;

pub type ActStartHook =
    Arc<dyn Fn(pa_types::ai::Usage) -> pa_agent::BoxFut<'static, anyhow::Result<()>> + Send + Sync>;

#[derive(Debug, PartialEq, Eq)]
pub enum ActLaneResult {
    Done,
    Text(String),
    Cancelled,
}

struct ActiveAct {
    projection: Option<Arc<ActProjection>>,
    agent: Mutex<Option<std::sync::Weak<Agent>>>,
    channel: Arc<HostRequestChannel>,
    cancelled: AbortSignal,
    finished: AbortSignal,
    completed: AtomicBool,
    cell_active: AtomicBool,
    compacting: AtomicBool,
    interrupt_scheduled: AtomicBool,
}

impl ActiveAct {
    fn cancel(&self, interrupt_cell: bool) {
        self.cancelled.abort();
        if interrupt_cell
            && self.cell_active.load(Ordering::Acquire)
            && !self.interrupt_scheduled.swap(true, Ordering::AcqRel)
        {
            self.channel.interrupt_after_grace(None);
        }
    }
}

struct RetainedActSession {
    agent: Arc<Agent>,
    stream: Arc<RwLock<StreamFn>>,
    session: Arc<AsyncMutex<SessionManager>>,
    core: Arc<super::AgentSession>,
    compact_pending: Arc<AtomicBool>,
}

#[derive(Clone)]
struct ActCompactionConfiguration {
    context: super::auxiliary_model::AuxiliaryModelContext,
    model: pa_types::ai::Model,
}

#[derive(Default)]
pub struct ActLane {
    sessions: AsyncMutex<HashMap<String, Arc<RetainedActSession>>>,
    active: Mutex<Option<Arc<ActiveAct>>>,
    disposed: AtomicBool,
    usage: Mutex<pa_types::ai::Usage>,
    persistence: Option<(PathBuf, PathBuf)>,
    compaction_context: Mutex<Option<ActCompactionConfiguration>>,
    role_fallback: Mutex<Option<ActFallback>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl ActLane {
    #[must_use]
    pub fn persisted(cwd: PathBuf, session_dir: PathBuf) -> Self {
        Self {
            persistence: Some((cwd, session_dir)),
            ..Default::default()
        }
    }

    pub(crate) fn configure_compaction(
        &self,
        cwd: PathBuf,
        agent_dir: PathBuf,
        model: pa_types::ai::Model,
    ) {
        *lock(&self.compaction_context) = Some(ActCompactionConfiguration {
            context: super::auxiliary_model::AuxiliaryModelContext { cwd, agent_dir },
            model,
        });
    }

    pub(crate) fn configure_role_fallback(&self, fallback: Option<ActFallback>) {
        *lock(&self.role_fallback) = fallback;
    }

    pub fn running(&self) -> bool {
        lock(&self.active).is_some()
    }

    pub fn cell_running(&self) -> bool {
        lock(&self.active)
            .as_ref()
            .is_some_and(|a| a.cell_active.load(Ordering::Acquire))
    }

    pub fn usage(&self) -> pa_types::ai::Usage {
        *lock(&self.usage)
    }

    #[tracing::instrument(skip_all)]
    pub(crate) async fn usage_for(&self, session_key: &str) -> pa_types::ai::Usage {
        let retained = self.sessions.lock().await.get(session_key).cloned();
        match retained {
            Some(retained) => retained_usage(&*retained.session.lock().await),
            None => pa_types::ai::Usage::default(),
        }
    }

    #[tracing::instrument(skip_all)]
    pub(crate) async fn context_snapshot(
        &self,
        session_key: &str,
    ) -> Option<(
        pa_types::session::ActModel,
        u64,
        Vec<pa_types::session::FileEntry>,
    )> {
        let retained = self.sessions.lock().await.get(session_key).cloned()?;
        let state = retained.agent.state().await;
        let entries = retained
            .session
            .lock()
            .await
            .active_branch_entries()
            .into_iter()
            .cloned()
            .collect();
        Some((
            pa_types::session::ActModel {
                provider: state.model.provider.clone(),
                id: state.model.id.clone(),
            },
            state.model.context_window,
            entries,
        ))
    }

    #[tracing::instrument(skip_all)]
    pub async fn thinking_level(&self) -> Option<ThinkingLevel> {
        let active = lock(&self.active).clone()?;
        let agent = lock(&active.agent)
            .as_ref()
            .and_then(std::sync::Weak::upgrade)?;
        Some(agent.state().await.thinking_level)
    }

    /// # Errors
    /// Returns an error when a caller message cannot be serialized.
    #[tracing::instrument(skip_all)]
    pub async fn caller_history_entries(&self) -> anyhow::Result<Vec<serde_json::Value>> {
        let active = lock(&self.active).clone();
        let agent = active.and_then(|active| {
            lock(&active.agent)
                .as_ref()
                .and_then(std::sync::Weak::upgrade)
        });
        let Some(agent) = agent else {
            return Ok(Vec::new());
        };
        agent
            .state()
            .await
            .messages
            .iter()
            .map(|message| {
                Ok(serde_json::json!({"type":"message", "message":serde_json::to_value(message)?}))
            })
            .collect()
    }

    pub fn cancel(&self) -> bool {
        let active = lock(&self.active).clone();
        if let Some(active) = active {
            active.cancel(/*interrupt_cell*/ true);
            true
        } else {
            false
        }
    }

    #[tracing::instrument(skip_all)]
    pub async fn wait_for_idle(&self) {
        let active = lock(&self.active).clone();
        if let Some(active) = active {
            active.finished.cancelled().await;
        }
    }

    #[tracing::instrument(skip_all)]
    pub async fn dispose(&self) {
        self.disposed.store(true, Ordering::Release);
        self.cancel();
        self.wait_for_idle().await;
        self.sessions.lock().await.clear();
    }

    /// # Errors
    /// Returns an error for invalid admission, provider failure, or malformed cell replies.
    #[tracing::instrument(skip_all)]
    pub async fn run(
        self: &Arc<Self>,
        prompt: String,
        channel: Arc<HostRequestChannel>,
        target: ActLaneTarget,
        history_images: Vec<ImageContent>,
    ) -> anyhow::Result<ActLaneResult> {
        self.run_with_start(prompt, channel, target, history_images, None)
            .await
    }

    /// # Errors
    /// Returns admission, persistence, provider, or malformed cell reply errors.
    #[tracing::instrument(skip_all)]
    pub async fn run_with_start(
        self: &Arc<Self>,
        prompt: String,
        channel: Arc<HostRequestChannel>,
        target: ActLaneTarget,
        history_images: Vec<ImageContent>,
        start: Option<ActStartHook>,
    ) -> anyhow::Result<ActLaneResult> {
        self.run_with_projection(prompt, channel, target, history_images, start, None)
            .await
    }

    /// # Errors
    /// Returns admission, persistence, provider, or malformed cell reply errors.
    #[tracing::instrument(skip_all)]
    pub async fn run_with_projection(
        self: &Arc<Self>,
        prompt: String,
        channel: Arc<HostRequestChannel>,
        target: ActLaneTarget,
        history_images: Vec<ImageContent>,
        start: Option<ActStartHook>,
        projection: Option<Arc<ActProjection>>,
    ) -> anyhow::Result<ActLaneResult> {
        anyhow::ensure!(
            !self.disposed.load(Ordering::Acquire),
            "Act lane has been disposed"
        );
        let active = Arc::new(ActiveAct {
            projection,
            agent: Mutex::new(None),
            channel,
            cancelled: AbortSignal::new(),
            finished: AbortSignal::new(),
            completed: AtomicBool::new(false),
            cell_active: AtomicBool::new(false),
            compacting: AtomicBool::new(false),
            interrupt_scheduled: AtomicBool::new(false),
        });
        {
            let mut slot = lock(&self.active);
            anyhow::ensure!(
                !self.disposed.load(Ordering::Acquire),
                "Act lane has been disposed"
            );
            anyhow::ensure!(
                slot.is_none(),
                "Another Act is already active in this session"
            );
            *slot = Some(active.clone());
        }
        // The cleanup task survives a dropped host-handler future: it aborts
        // provider work, restores retained context, and releases admission.
        let admission = RunAdmission(active.clone());
        let lane = self.clone();
        let task = tokio::spawn(async move {
            let result = lane
                .run_inner(prompt, active.clone(), target, history_images, start)
                .await;
            lock(&lane.active).take();
            active.finished.abort();
            result
        });
        let result = task.await.map_err(anyhow::Error::new)?;
        drop(admission);
        result
    }

    #[tracing::instrument(skip_all)]
    async fn run_inner(
        self: &Arc<Self>,
        prompt: String,
        active: Arc<ActiveAct>,
        target: ActLaneTarget,
        history_images: Vec<ImageContent>,
        start: Option<ActStartHook>,
    ) -> anyhow::Result<ActLaneResult> {
        if active.cancelled.is_aborted()
            || active.channel.signal.is_aborted()
            || active
                .channel
                .interrupt_signal
                .as_ref()
                .is_some_and(AbortSignal::is_aborted)
        {
            return Ok(ActLaneResult::Cancelled);
        }
        let retained = {
            let mut sessions = self.sessions.lock().await;
            if let Some(retained) = sessions.get(&target.session_key) {
                retained.clone()
            } else {
                let mut session = match &self.persistence {
                    Some((cwd, dir)) => {
                        open_act_session(cwd, dir, &target.session_key, &target.model)?
                    }
                    None => SessionManager::in_memory(Path::new(".")),
                };
                if session.active_context().messages.is_empty() {
                    session.append_model_change(&target.model.provider, &target.model.id)?;
                    session.append_thinking_level_change(
                        &format!("{:?}", target.thinking_level).to_lowercase(),
                    )?;
                }
                let restored = super::rebuilt_loop_messages(session.active_context().messages);
                super::rlm_usage::add_assistant_usage(
                    &mut lock(&self.usage),
                    &retained_usage(&session),
                );
                let session = Arc::new(AsyncMutex::new(session));
                let stream = Arc::new(RwLock::new(target.stream_fn.clone()));
                let live_stream = stream.clone();
                let stream_fn: StreamFn = Arc::new(move |model, context, options| {
                    let stream = live_stream
                        .read()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .clone();
                    stream(model, context, options)
                });
                let core_slot = Arc::new(Mutex::new(None::<std::sync::Weak<super::AgentSession>>));
                let compact_pending = Arc::new(AtomicBool::new(false));
                let hook_core = core_slot.clone();
                let hook_pending = compact_pending.clone();
                let hook_lane = Arc::downgrade(self);
                let agent = Arc::new(Agent::new(AgentOptions {
                    convert_to_llm: Some(super::messages::engine_convert_to_llm()),
                    initial_state: pa_agent::agent::AgentInitialState {
                        messages: Some(restored),
                        ..Default::default()
                    },
                    stream_fn: Some(stream_fn),
                    should_stop_after_turn: Some(Arc::new(move |turn| {
                        let core = lock(&hook_core).as_ref().and_then(std::sync::Weak::upgrade);
                        let pending = hook_pending.clone();
                        let lane = hook_lane.upgrade();
                        Box::pin(async move {
                            let Some(core) = core else { return Ok(false) };
                            let Some(lane) = lane else { return Ok(false) };
                            if turn.message.stop_reason != StopReason::ToolUse
                                || lock(&lane.active).as_ref().is_none_or(|active| {
                                    active.completed.load(Ordering::Acquire)
                                        || active.compacting.load(Ordering::Acquire)
                                })
                            {
                                return Ok(false);
                            }
                            let Some(context) = lock(&lane.compaction_context).clone() else {
                                return Ok(false);
                            };
                            refresh_compaction_settings(&core, &context.context);
                            let due = core.auto_compaction_due(&context.model).await;
                            pending.store(due, Ordering::Release);
                            Ok(due)
                        })
                    })),
                    ..Default::default()
                }));
                let mut core = super::AgentSession::from_session_arc(
                    agent.clone(),
                    session.clone(),
                    Vec::new(),
                    None,
                )
                .await?;
                if let Some(context) = lock(&self.compaction_context).clone() {
                    core.set_auxiliary_model_context(context.context);
                }
                let core = Arc::new(core);
                *lock(&core_slot) = Some(Arc::downgrade(&core));
                let lane = Arc::downgrade(self);
                agent
                    .subscribe(move |event, _signal| {
                        let projection = lane.upgrade().and_then(|lane| {
                            lock(&lane.active)
                                .as_ref()
                                .and_then(|active| active.projection.clone())
                        });
                        Box::pin(async move {
                            if let Some(projection) = projection {
                                projection.agent_event(&event);
                            }
                            Ok(())
                        })
                    })
                    .await;
                let retained = Arc::new(RetainedActSession {
                    agent,
                    stream,
                    session,
                    core,
                    compact_pending,
                });
                sessions.insert(target.session_key.clone(), retained.clone());
                retained
            }
        };
        *retained
            .stream
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = target.stream_fn;
        let agent = retained.agent.clone();
        *lock(&active.agent) = Some(Arc::downgrade(&agent));
        let provider_changed = agent.state().await.model.provider != target.model.provider;
        {
            let thinking = format!("{:?}", target.thinking_level).to_lowercase();
            let mut session = retained.session.lock().await;
            let context = session.active_context();
            if context.model.as_ref().is_none_or(|(provider, id)| {
                provider != &target.model.provider || id != &target.model.id
            }) {
                session.append_model_change(&target.model.provider, &target.model.id)?;
            }
            if context.thinking_level != thinking {
                session.append_thinking_level_change(&thinking)?;
            }
        }
        if let Some(projection) = &active.projection {
            projection.set_model(&target.model, target.thinking_level);
        }
        agent
            .set_model_and_thinking_level(target.model, target.thinking_level)
            .await;
        if provider_changed {
            rebuild_retained_context(&retained).await?;
        }
        agent
            .set_system_prompt(act_system_prompt(target.depth, target.max_depth))
            .await;
        let usage_before = retained_usage(&*retained.session.lock().await);
        let previous_messages = agent.state().await.messages;
        let previous_leaf = retained
            .session
            .lock()
            .await
            .get_leaf_id()
            .map(str::to_owned);
        agent.set_tools(vec![Arc::new(SharedIpythonTool {
            active: active.clone(),
            parameters: json!({"type":"object", "properties":{"code":{"type":"string"}}, "required":["code"], "additionalProperties":false}),
        })]).await;
        if let Some(start) = start {
            let baseline = retained_usage(&*retained.session.lock().await);
            if let Err(error) = start(baseline).await {
                agent.set_tools(Vec::new()).await;
                return Err(error);
            }
        }
        if let Some(projection) = &active.projection {
            projection.start();
        }
        let prompt = if history_images.is_empty() {
            prompt
        } else {
            format!("{prompt}\n\n{} attached bitmap frame(s) contain the caller's message delta since the previous Act at this depth. Use them only as context for the current assignment.", history_images.len())
        };
        let interrupted = async {
            match active.channel.interrupt_signal.as_ref() {
                Some(signal) => signal.cancelled().await,
                None => std::future::pending().await,
            }
        };
        retained.compact_pending.store(false, Ordering::Release);
        let configuration = lock(&self.compaction_context).clone();
        let preflight = if let Some(configuration) = configuration {
            refresh_compaction_settings(&retained.core, &configuration.context);
            if retained
                .core
                .auto_compaction_due(&configuration.model)
                .await
            {
                compact_retained(
                    &retained,
                    &active,
                    configuration,
                    super::scratch_handoff::ScratchBoundaryReason::Threshold,
                )
                .await
                .map(|_| ())
            } else {
                Ok(())
            }
        } else {
            Ok(())
        };
        let admission = match preflight {
            Ok(()) => {
                agent
                    .prompt_until_accepted(AgentPromptInput::Text {
                        text: prompt,
                        images: history_images,
                    })
                    .await
            }
            Err(error) => Err(error),
        };
        tokio::pin!(interrupted);
        let result = if admission.is_err() {
            Some(admission)
        } else {
            let mut overflow_retried = false;
            loop {
                let settled = tokio::select! {
                    biased;
                    () = &mut interrupted => { active.cancel(/*interrupt_cell*/ true); false },
                    () = active.channel.signal.cancelled() => { active.cancel(/*interrupt_cell*/ false); false },
                    () = active.cancelled.cancelled() => false,
                    () = agent.wait_for_idle() => true,
                };
                if !settled {
                    break None;
                }
                if active.completed.load(Ordering::Acquire) {
                    break Some(Ok(()));
                }
                let Some(configuration) = lock(&self.compaction_context).clone() else {
                    break Some(Ok(()));
                };
                if retained
                    .core
                    .recover_reasoning_exhaustion(&configuration.model, None)
                    .await?
                {
                    if let Err(error) = agent.continue_run().await {
                        break Some(Err(error));
                    }
                    continue;
                }
                let state = agent.state().await;
                let fallback = lock(&self.role_fallback).clone();
                let failed = state
                    .messages
                    .iter()
                    .rev()
                    .find_map(|message| match message {
                        AgentMessage::Standard(Message::Assistant(assistant)) => Some(assistant),
                        _ => None,
                    });
                if let (Some(fallback), Some(failed)) = (fallback, failed) {
                    if failed.provider == state.model.provider
                        && failed.model == state.model.id
                        && super::role_fallback::can_advance_role_candidate(
                            failed,
                            state.model.context_window,
                        )
                    {
                        let next = tokio::select! {
                            biased;
                            () = &mut interrupted => { active.cancel(/*interrupt_cell*/ true); break None },
                            () = active.channel.signal.cancelled() => { active.cancel(/*interrupt_cell*/ false); break None },
                            () = active.cancelled.cancelled() => break None,
                            next = fallback(failed.clone()) => next,
                        };
                        let next = match next {
                            Ok(next) => next,
                            Err(error) => break Some(Err(error)),
                        };
                        if let Some(next) = next {
                            let thinking = super::provider_adapter::model_thinking_level(
                                next.target.thinking_level,
                            );
                            let persisted = {
                                let mut session = retained.session.lock().await;
                                session
                                    .append_model_change(&next.model.provider, &next.model.id)
                                    .and_then(|_| {
                                        session.append_thinking_level_change(thinking.wire_name())
                                    })
                            };
                            if let Err(error) = persisted {
                                break Some(Err(error.into()));
                            }
                            if state.model.provider != next.model.provider {
                                if let Err(error) = rebuild_retained_context(&retained).await {
                                    break Some(Err(error));
                                }
                            }
                            retained
                                .core
                                .drop_trailing_assistant(super::TrailingAssistantFilter::ErrorOnly)
                                .await;
                            *retained
                                .stream
                                .write()
                                .unwrap_or_else(std::sync::PoisonError::into_inner) =
                                next.target.stream_fn;
                            if let Some(projection) = &active.projection {
                                projection
                                    .set_model(&next.target.model, next.target.thinking_level);
                            }
                            agent
                                .set_model_and_thinking_level(
                                    next.target.model,
                                    next.target.thinking_level,
                                )
                                .await;
                            if let Some(configuration) = lock(&self.compaction_context).as_mut() {
                                configuration.model = next.model;
                            }
                            overflow_retried = false;
                            if let Err(error) = agent.continue_run().await {
                                break Some(Err(error));
                            }
                            continue;
                        }
                    }
                }
                refresh_compaction_settings(&retained.core, &configuration.context);
                let state = agent.state().await;
                let overflow = !overflow_retried
                    && retained.core.auto_compaction_enabled()
                    && state
                        .messages
                        .iter()
                        .rev()
                        .find_map(|message| match message {
                            AgentMessage::Standard(Message::Assistant(assistant)) => {
                                Some(assistant)
                            }
                            _ => None,
                        })
                        .is_some_and(|assistant| {
                            super::provider_retry::is_context_overflow_failure(
                                assistant,
                                configuration.model.context_window,
                            )
                        });
                let threshold = retained.compact_pending.swap(false, Ordering::AcqRel);
                if !overflow && !threshold {
                    break Some(Ok(()));
                }
                let outcome = compact_retained(
                    &retained,
                    &active,
                    configuration,
                    if overflow {
                        super::scratch_handoff::ScratchBoundaryReason::Overflow
                    } else {
                        super::scratch_handoff::ScratchBoundaryReason::Threshold
                    },
                )
                .await;
                if active.cancelled.is_aborted() {
                    break None;
                }
                if active.completed.load(Ordering::Acquire) {
                    break Some(Ok(()));
                }
                let outcome = match outcome {
                    Ok(outcome) => outcome,
                    Err(error) => break Some(Err(error)),
                };
                if overflow {
                    if matches!(outcome, super::compact_session::CompactOutcome::Skipped(_)) {
                        break Some(Ok(()));
                    }
                    overflow_retried = true;
                    retained
                        .core
                        .drop_trailing_assistant(super::TrailingAssistantFilter::ErrorOnly)
                        .await;
                }
                if let Err(error) = agent.continue_run().await {
                    break Some(Err(error));
                }
            }
        };
        if result.is_none() {
            agent.abort();
            agent.wait_for_idle().await;
        }
        let messages = agent.state().await.messages;
        let usage_after = retained_usage(&*retained.session.lock().await);
        super::rlm_usage::add_assistant_usage(
            &mut lock(&self.usage),
            &super::act_runtime::usage_delta(usage_after, usage_before),
        );
        let completed = active.completed.load(Ordering::Acquire);
        if !completed {
            let rollback = {
                let mut session = retained.session.lock().await;
                if let Some(leaf) = &previous_leaf {
                    session.branch(leaf);
                } else {
                    session.reset_leaf();
                }
                session.append_custom_entry("prime-agent.act-branch-reset", None)
            };
            agent.set_messages(previous_messages).await;
            if let Err(error) = rollback {
                eprintln!("pa-core: Act branch reset not persisted: {error}");
            }
        }
        // Drop the channel reference from retained tools between assignments.
        agent.set_tools(Vec::new()).await;
        if completed {
            return Ok(ActLaneResult::Done);
        }
        if active.cancelled.is_aborted() {
            return Ok(ActLaneResult::Cancelled);
        }
        if let Some(result) = result {
            result?;
        }
        let assistant = messages.iter().rev().find_map(|message| match message {
            AgentMessage::Standard(Message::Assistant(assistant)) => Some(assistant),
            _ => None,
        });
        if let Some(assistant) = assistant {
            if assistant.stop_reason == StopReason::Error {
                anyhow::bail!(
                    "{}",
                    assistant
                        .error_message
                        .as_deref()
                        .unwrap_or("Act provider failed")
                );
            }
            let text = assistant
                .content
                .iter()
                .filter_map(|part| match part {
                    pa_agent::types::AssistantContent::Text(text) => Some(text.text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            return Ok(ActLaneResult::Text(text));
        }
        Ok(ActLaneResult::Text(String::new()))
    }
}

#[tracing::instrument(skip_all)]
async fn rebuild_retained_context(retained: &RetainedActSession) -> anyhow::Result<()> {
    let context = retained.session.lock().await.active_context();
    let messages = context
        .messages
        .into_iter()
        .map(|message| serde_json::from_value(serde_json::to_value(message)?))
        .collect::<Result<Vec<_>, serde_json::Error>>()?;
    retained.agent.set_messages(messages).await;
    Ok(())
}

#[tracing::instrument(skip_all)]
async fn compact_retained(
    retained: &RetainedActSession,
    active: &Arc<ActiveAct>,
    configuration: ActCompactionConfiguration,
    reason: super::scratch_handoff::ScratchBoundaryReason,
) -> anyhow::Result<super::compact_session::CompactOutcome> {
    if active.cancelled.is_aborted()
        || active.channel.signal.is_aborted()
        || active
            .channel
            .interrupt_signal
            .as_ref()
            .is_some_and(AbortSignal::is_aborted)
    {
        active.cancel(/*interrupt_cell*/ false);
        return Err(pa_agent::abort::aborted_error());
    }
    let context = configuration.context;
    let model = configuration.model;
    let mut registry = crate::models::registry::ModelRegistry::create(
        crate::auth::AuthStorage::create(&context.agent_dir),
        context.agent_dir.join("models.json"),
    );
    registry.load_private_authorization_from_cache();
    let auth = registry.get_api_key_and_headers(&model, model.headers.as_ref());
    anyhow::ensure!(auth.ok, "Act compaction credentials unavailable");
    let abort = pa_agent::abort::AbortController::new();
    let signal = abort.signal();
    active.compacting.store(true, Ordering::Release);
    let _compacting = CompactionAdmission(active.clone());
    let compact =
        retained
            .core
            .compact_for_reason(None, &model, auth.api_key, Some(&signal), reason);
    tokio::pin!(compact);
    let interrupted = async {
        match active.channel.interrupt_signal.as_ref() {
            Some(signal) => signal.cancelled().await,
            None => std::future::pending().await,
        }
    };
    tokio::select! {
        biased;
        () = interrupted => { active.cancel(/*interrupt_cell*/ true); abort.abort(); compact.await },
        () = active.channel.signal.cancelled() => { active.cancel(/*interrupt_cell*/ false); abort.abort(); compact.await },
        () = active.cancelled.cancelled() => { abort.abort(); compact.await },
        outcome = &mut compact => outcome,
    }
}

fn refresh_compaction_settings(
    core: &super::AgentSession,
    context: &super::auxiliary_model::AuxiliaryModelContext,
) {
    let settings = crate::settings::SettingsManager::create(&context.cwd, &context.agent_dir);
    let compaction = settings.settings().compaction.clone().unwrap_or_default();
    core.set_compaction_settings(super::compaction::CompactionSettings {
        enabled: compaction.enabled.unwrap_or(true),
        reserve_tokens: compaction
            .reserve_tokens
            .unwrap_or(super::compaction::DEFAULT_RESERVE_TOKENS),
        keep_recent_tokens: compaction
            .keep_recent_tokens
            .unwrap_or(super::compaction::DEFAULT_KEEP_RECENT_TOKENS),
    });
    core.set_native_compaction_enabled(compaction.native.unwrap_or(true));
}

struct RunAdmission(Arc<ActiveAct>);
pub(super) fn retained_usage(session: &SessionManager) -> pa_types::ai::Usage {
    let mut usage = pa_types::ai::Usage::default();
    for entry in session.retained_entries() {
        let entry_usage = match entry {
            pa_types::session::FileEntry::Message {
                message: pa_types::session::AgentMessage::Assistant(assistant),
                ..
            } => Some(&assistant.usage),
            pa_types::session::FileEntry::Compaction { payload, .. } => payload.usage.as_ref(),
            pa_types::session::FileEntry::BranchSummary { payload, .. } => payload.usage.as_ref(),
            _ => None,
        };
        if let Some(entry_usage) = entry_usage {
            super::rlm_usage::add_assistant_usage(&mut usage, entry_usage);
        }
    }
    usage
}

impl Drop for RunAdmission {
    fn drop(&mut self) {
        if !self.0.finished.is_aborted() {
            self.0.cancel(/*interrupt_cell*/ true);
        }
    }
}

struct CompactionAdmission(Arc<ActiveAct>);
impl Drop for CompactionAdmission {
    fn drop(&mut self) {
        self.0.compacting.store(false, Ordering::Release);
    }
}

struct CellAdmission(Arc<ActiveAct>);
impl Drop for CellAdmission {
    fn drop(&mut self) {
        self.0.cell_active.store(false, Ordering::Release);
    }
}

struct SharedIpythonTool {
    active: Arc<ActiveAct>,
    parameters: Value,
}
impl AgentTool for SharedIpythonTool {
    fn name(&self) -> &'static str {
        "shared_ipython"
    }
    fn description(&self) -> &'static str {
        "Run one complete cell in the directing session's live IPython namespace."
    }
    fn label(&self) -> &'static str {
        "Shared IPython"
    }
    fn parameters(&self) -> &Value {
        &self.parameters
    }
    fn execution_mode(&self) -> Option<ToolExecutionMode> {
        Some(ToolExecutionMode::Sequential)
    }
    fn execute(
        self: Arc<Self>,
        _tool_call_id: String,
        params: Value,
        signal: pa_agent::abort::AbortSignal,
        _on_update: AgentToolUpdateCallback,
    ) -> pa_agent::BoxFut<'static, anyhow::Result<AgentToolResult>> {
        Box::pin(async move {
            anyhow::ensure!(
                !self.active.completed.load(Ordering::Acquire),
                "Act has already completed"
            );
            let code = params
                .get("code")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("shared_ipython requires string code"))?;
            anyhow::ensure!(
                !self.active.cell_active.swap(true, Ordering::AcqRel),
                "An Act cell is already active"
            );
            let _cell = CellAdmission(self.active.clone());
            self.active
                .channel
                .send(json!({"type":"cell", "code":code}))
                .await?;
            let response = tokio::select! {
                biased;
                () = signal.aborted() => anyhow::bail!("Act cell cancelled"),
                response = self.active.channel.receive(Some(&self.active.cancelled)) => response?,
            };
            if response.get("type").and_then(Value::as_str) == Some("done") {
                self.active.completed.store(true, Ordering::Release);
                let mut result = AgentToolResult::text("Act completed with an in-kernel value.");
                result.details = json!({"outcome":"done"});
                result.terminate = Some(true);
                return Ok(result);
            }
            let text = format_cell_result(&response)?;
            let mut result = AgentToolResult::text(text);
            result.details = response;
            Ok(result)
        })
    }
}

fn format_cell_result(response: &Value) -> anyhow::Result<String> {
    anyhow::ensure!(
        response.get("type").and_then(Value::as_str) == Some("cell_result"),
        "Act returned an unexpected cell response"
    );
    if let Some(duration) = response.get("durationMs") {
        anyhow::ensure!(
            duration.is_number(),
            "Act cell response has invalid durationMs"
        );
    }
    let mut sections = Vec::new();
    for field in ["stdout", "stderr", "result", "error"] {
        if let Some(value) = response.get(field) {
            anyhow::ensure!(
                value.is_null() || value.is_string(),
                "Act cell response has invalid {field}"
            );
            if let Some(text) = value.as_str().filter(|text| !text.is_empty()) {
                sections.push(format!("[{field}]\n{text}"));
            }
        }
    }
    Ok(if sections.is_empty() {
        "Cell completed without output.".into()
    } else {
        sections.join("\n\n")
    })
}

#[must_use]
pub fn act_system_prompt(depth: u32, max_depth: u32) -> String {
    let base = include_str!("../../assets/act-system-prompt.txt");
    if depth < max_depth {
        format!("{base}\nOne configured Act depth remains. You may delegate one bounded next-depth action with `nested = await rlm.act(prompt, model=...)` in a shared_ipython cell. Omit `model` only when that depth has a configured default. Reuse named objects already in the namespace, identify those bindings in the action, and ask the nested Act worker to leave later-use state in named variables. After it returns, inspect the returned object and shared state before continuing or calling your own rlm.done(value).")
    } else {
        format!("{base}\nYou are at the maximum configured Act depth. Complete the action through shared_ipython and rlm.done(value). Another nested Act call is unavailable.")
    }
}

fn open_act_session(
    cwd: &Path,
    base: &Path,
    key: &str,
    model: &Model,
) -> anyhow::Result<SessionManager> {
    use sha2::{Digest, Sha256};
    use std::io::Write;

    let matches_model = |session: &SessionManager| {
        session
            .get_branch(None)
            .iter()
            .rev()
            .find_map(|entry| match entry {
                pa_types::session::FileEntry::ModelChange { payload, .. } => {
                    Some(payload.provider == model.provider && payload.model_id == model.id)
                }
                _ => None,
            })
            .unwrap_or(false)
    };
    let base_marker = base.join("model-key");
    let use_base = if base_marker.exists() {
        std::fs::read_to_string(&base_marker)?.trim() == key
    } else if base.join("session.jsonl").exists() {
        matches_model(&SessionManager::open(
            cwd,
            base,
            &base.join("session.jsonl"),
        ))
    } else {
        true
    };
    let dir = if use_base {
        base.to_path_buf()
    } else {
        let suffix = format!("{:x}", Sha256::digest(key.as_bytes()));
        let name = base
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| anyhow::anyhow!("Invalid Act session directory"))?;
        base.with_file_name(format!("{name}-model-{}", &suffix[..16]))
    };
    std::fs::create_dir_all(&dir)?;
    let marker = dir.join("model-key");
    if marker.exists() {
        anyhow::ensure!(
            std::fs::read_to_string(&marker)?.trim() == key,
            "Act model session key collision"
        );
    } else {
        if dir.join("session.jsonl").exists() {
            anyhow::ensure!(
                matches_model(&SessionManager::open(cwd, &dir, &dir.join("session.jsonl"))),
                "Act model session key collision"
            );
        }
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(marker)?;
        writeln!(file, "{key}")?;
        file.sync_all()?;
    }
    Ok(SessionManager::open(cwd, &dir, &dir.join("session.jsonl")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kernel::host_channel::ChannelSend;
    use pa_agent::scripted::{tool_call_turn_steps, ScriptStep, ScriptedProvider, ScriptedTurn};
    use pa_agent::stream::AssistantMessageEvent;
    use pa_ai::faux::{register_faux_provider, FauxModelDefinition, RegisterFauxProviderOptions};
    use std::time::Duration;
    use tokio::sync::mpsc;

    type ReceivedCells = Arc<Mutex<Vec<String>>>;

    fn channel(reply: Option<Value>) -> (Arc<HostRequestChannel>, ReceivedCells) {
        let incoming = Arc::new(Mutex::new(None::<mpsc::Sender<Value>>));
        let codes = Arc::new(Mutex::new(Vec::new()));
        let sink = codes.clone();
        let sender = incoming.clone();
        let send: ChannelSend = Arc::new(move |message| {
            let sender = lock(&sender).clone().unwrap();
            let sink = sink.clone();
            let reply = reply.clone();
            Box::pin(async move {
                lock(&sink).push(message["code"].as_str().unwrap().to_owned());
                if let Some(reply) = reply {
                    sender.send(reply).await?;
                }
                Ok(())
            })
        });
        let (channel, sender) = HostRequestChannel::new(
            AbortSignal::new(),
            None,
            Some("outer-call".into()),
            send,
            Arc::new(|_| {}),
        );
        *lock(&incoming) = Some(sender);
        (channel, codes)
    }

    fn target(provider: &Arc<ScriptedProvider>, key: &str) -> ActLaneTarget {
        ActLaneTarget {
            session_key: key.to_owned(),
            model: Model::unknown(),
            thinking_level: ThinkingLevel::High,
            stream_fn: provider.stream_fn(),
            depth: 1,
            max_depth: 1,
        }
    }

    #[tokio::test]
    async fn private_threshold_and_overflow_compaction_resume_shared_cells_and_retain_boundaries() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory.path().join("settings.json"),
            json!({"compaction":{"keepRecentTokens":1}}).to_string(),
        )
        .unwrap();
        let compact_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let captured = compact_calls.clone();
        let registration = register_faux_provider(RegisterFauxProviderOptions {
            provider: Some("act-compact-test".into()),
            models: Some(vec![FauxModelDefinition {
                id: "private".into(),
                max_tokens: Some(100),
                ..Default::default()
            }]),
            compact: Some(Arc::new(move |model, context, _options| {
                let captured = captured.clone();
                Box::pin(async move {
                    assert!(!context.messages.is_empty());
                    captured.fetch_add(1, Ordering::AcqRel);
                    let item = json!({"type":"compaction","encrypted_content":"private-only"});
                    Ok(pa_ai::types::ProviderNativeCompactionResult {
                        provider: model.provider.clone(),
                        replacement_history: vec![item.clone()],
                        compaction_item: item,
                    })
                })
            })),
            ..Default::default()
        });
        std::fs::write(
            directory.path().join("auth.json"),
            json!({"act-compact-test":{"type":"api_key","key":"test-only"}}).to_string(),
        )
        .unwrap();
        let model: Model =
            super::super::provider_adapter::json_round_trip(&registration.get_model()).unwrap();
        let provider = Arc::new(ScriptedProvider::new(model.clone()));
        provider.push_tool_call_turn(
            None,
            vec![("seed", "shared_ipython", json!({"code":"done"}))],
        );
        let mut steps = tool_call_turn_steps(
            &model,
            None,
            vec![("work", "shared_ipython", json!({"code":"work"}))],
        );
        for step in &mut steps {
            if let ScriptStep::Event(event) = step {
                if let AssistantMessageEvent::Done { message, .. } = &mut **event {
                    message.usage.total_tokens = 127_000;
                    message.usage.input = 127_000;
                    message.stop_reason = StopReason::ToolUse;
                }
            }
        }
        provider.push_turn(ScriptedTurn::Events(steps));
        provider.push_tool_call_turn(
            None,
            vec![("finish", "shared_ipython", json!({"code":"done"}))],
        );
        provider.push_stream_failure_turn("", "maximum context length exceeded");
        provider.push_tool_call_turn(
            None,
            vec![("recovered", "shared_ipython", json!({"code":"done"}))],
        );
        let lane = Arc::new(ActLane::persisted(
            directory.path().into(),
            directory.path().join("act"),
        ));
        lane.configure_compaction(
            directory.path().into(),
            directory.path().into(),
            super::super::provider_adapter::json_round_trip(&registration.get_model()).unwrap(),
        );
        for prompt in [
            "seed assignment",
            "compact assignment",
            "overflow assignment",
        ] {
            let incoming = Arc::new(Mutex::new(None::<mpsc::Sender<Value>>));
            let sender = incoming.clone();
            let send: ChannelSend = Arc::new(move |message| {
                let sender = lock(&sender).clone().unwrap();
                Box::pin(async move {
                    sender
                        .send(if message["code"] == "done" {
                            json!({"type":"done"})
                        } else {
                            json!({"type":"cell_result","stdout":"kept shared state"})
                        })
                        .await?;
                    Ok(())
                })
            });
            let (host, sender) = HostRequestChannel::new(
                AbortSignal::new(),
                None,
                Some("caller".into()),
                send,
                Arc::new(|_| {}),
            );
            *lock(&incoming) = Some(sender);
            let target = ActLaneTarget {
                session_key: "act-compact-test/private".into(),
                model: model.clone(),
                thinking_level: ThinkingLevel::Off,
                stream_fn: provider.stream_fn(),
                depth: 1,
                max_depth: 1,
            };
            assert_eq!(
                tokio::time::timeout(
                    Duration::from_secs(5),
                    lane.run(prompt.into(), host, target, Vec::new())
                )
                .await
                .unwrap()
                .unwrap(),
                ActLaneResult::Done
            );
        }
        assert_eq!(compact_calls.load(Ordering::Acquire), 2);
        assert_eq!(provider.calls().len(), 5);
        let retained = lane.sessions.lock().await["act-compact-test/private"].clone();
        assert!(retained
            .session
            .lock()
            .await
            .active_branch_entries()
            .iter()
            .any(
                |entry| matches!(entry, pa_types::session::FileEntry::Compaction { payload, .. }
                if payload.provider_native_compaction.is_some())
            ));
        assert_eq!(lane.usage().input, 127_000);
        lane.dispose().await;
        registration.unregister();
    }

    #[tokio::test]
    async fn cancelling_private_preflight_compaction_does_not_admit_the_next_assignment() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory.path().join("settings.json"),
            json!({"compaction":{"keepRecentTokens":1}}).to_string(),
        )
        .unwrap();
        std::fs::write(
            directory.path().join("auth.json"),
            json!({"act-cancel-compact":{"type":"api_key","key":"test-only"}}).to_string(),
        )
        .unwrap();
        let entered = Arc::new(tokio::sync::Notify::new());
        let captured = entered.clone();
        let registration = register_faux_provider(RegisterFauxProviderOptions {
            provider: Some("act-cancel-compact".into()),
            models: Some(vec![FauxModelDefinition {
                id: "private".into(),
                max_tokens: Some(100),
                ..Default::default()
            }]),
            compact: Some(Arc::new(move |_model, _context, options| {
                let entered = captured.clone();
                Box::pin(async move {
                    entered.notify_one();
                    options.base.signal.as_ref().unwrap().cancelled().await;
                    Err(pa_ai::ProviderError::Aborted)
                })
            })),
            ..Default::default()
        });
        let model: Model =
            super::super::provider_adapter::json_round_trip(&registration.get_model()).unwrap();
        let provider = Arc::new(ScriptedProvider::new(model.clone()));
        let mut steps = tool_call_turn_steps(
            &model,
            None,
            vec![("done", "shared_ipython", json!({"code":"done"}))],
        );
        for step in &mut steps {
            if let ScriptStep::Event(event) = step {
                if let AssistantMessageEvent::Done { message, .. } = &mut **event {
                    message.usage.input = 127_000;
                    message.usage.total_tokens = 127_000;
                    message.stop_reason = StopReason::ToolUse;
                }
            }
        }
        provider.push_turn(ScriptedTurn::Events(steps));
        let lane = Arc::new(ActLane::default());
        lane.configure_compaction(
            directory.path().into(),
            directory.path().into(),
            super::super::provider_adapter::json_round_trip(&registration.get_model()).unwrap(),
        );
        let target = ActLaneTarget {
            session_key: "act-cancel-compact/private".into(),
            model: model.clone(),
            thinking_level: ThinkingLevel::Off,
            stream_fn: provider.stream_fn(),
            depth: 1,
            max_depth: 1,
        };
        let (host, _) = channel(Some(json!({"type":"done"})));
        assert_eq!(
            lane.run("completed seed".into(), host, target, Vec::new())
                .await
                .unwrap(),
            ActLaneResult::Done
        );
        let task = {
            let lane = lane.clone();
            let target = ActLaneTarget {
                session_key: "act-cancel-compact/private".into(),
                model,
                thinking_level: ThinkingLevel::Off,
                stream_fn: provider.stream_fn(),
                depth: 1,
                max_depth: 1,
            };
            let (host, _) = channel(None);
            tokio::spawn(async move {
                lane.run("must not be admitted".into(), host, target, Vec::new())
                    .await
            })
        };
        tokio::time::timeout(Duration::from_secs(5), entered.notified())
            .await
            .unwrap();
        assert!(lane.cancel());
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), task)
                .await
                .unwrap()
                .unwrap()
                .unwrap(),
            ActLaneResult::Cancelled
        );
        assert_eq!(provider.calls().len(), 1);
        assert!(!lane.running());
        let retained = lane.sessions.lock().await["act-cancel-compact/private"].clone();
        assert!(retained.agent.state().await.tools.is_empty());
        assert!(
            !serde_json::to_string(&retained.agent.state().await.messages)
                .unwrap()
                .contains("must not be admitted")
        );
        registration.unregister();
    }

    #[tokio::test]
    async fn private_role_fallback_preserves_native_boundaries_and_resets_per_assignment() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory.path().join("settings.json"),
            json!({"compaction":{"enabled":false}}).to_string(),
        )
        .unwrap();
        let registration = register_faux_provider(RegisterFauxProviderOptions {
            provider: Some("act-role-test".into()),
            models: Some(vec![
                FauxModelDefinition {
                    id: "primary".into(),
                    ..Default::default()
                },
                FauxModelDefinition {
                    id: "backup".into(),
                    ..Default::default()
                },
            ]),
            ..Default::default()
        });
        let primary_model: Model =
            super::super::provider_adapter::json_round_trip(&registration.models[0]).unwrap();
        let mut full_backup_model = registration.models[1].clone();
        full_backup_model.provider = "act-role-backup-test".into();
        let backup_model: Model =
            super::super::provider_adapter::json_round_trip(&full_backup_model).unwrap();
        let primary = Arc::new(ScriptedProvider::new(primary_model.clone()));
        let backup = Arc::new(ScriptedProvider::new(backup_model.clone()));
        let lane = Arc::new(ActLane::default());
        for assignment in ["first", "second"] {
            primary.push_stream_failure_turn("", "provider unavailable");
            backup.push_tool_call_turn(
                None,
                vec![(assignment, "shared_ipython", json!({"code":"done"}))],
            );
            lane.configure_compaction(
                directory.path().into(),
                directory.path().into(),
                super::super::provider_adapter::json_round_trip(&registration.models[0]).unwrap(),
            );
            let candidate_model = backup_model.clone();
            let full_model = full_backup_model.clone();
            let stream = backup.stream_fn();
            let advanced = Arc::new(AtomicBool::new(false));
            lane.configure_role_fallback(Some(Arc::new(move |_| {
                let target = (!advanced.swap(true, Ordering::AcqRel)).then(|| ActFallbackTarget {
                    target: ActLaneTarget {
                        session_key: "act-role-test/backup".into(),
                        model: candidate_model.clone(),
                        thinking_level: ThinkingLevel::Off,
                        stream_fn: stream.clone(),
                        depth: 1,
                        max_depth: 1,
                    },
                    model: full_model.clone(),
                });
                Box::pin(async move { Ok(target) })
            })));
            let (host, _) = channel(Some(json!({"type":"done"})));
            let target = ActLaneTarget {
                session_key: "act-role-test/primary".into(),
                model: primary_model.clone(),
                thinking_level: ThinkingLevel::Off,
                stream_fn: primary.stream_fn(),
                depth: 1,
                max_depth: 1,
            };
            assert_eq!(
                tokio::time::timeout(
                    Duration::from_secs(5),
                    lane.run(assignment.into(), host, target, Vec::new())
                )
                .await
                .unwrap()
                .unwrap(),
                ActLaneResult::Done
            );
            assert_eq!(
                lane.context_snapshot("act-role-test/primary")
                    .await
                    .unwrap()
                    .0
                    .id,
                "backup"
            );
            if assignment == "first" {
                let retained = lane.sessions.lock().await["act-role-test/primary"].clone();
                retained.session.lock().await.append_compaction(
                    pa_types::session::CompactionEntry {
                        summary: "retained compaction boundary".into(),
                        first_kept_entry_id: "no-retained-prefix".into(),
                        provider_native_compaction: Some(json!({
                            "provider": primary_model.provider,
                            "replacementHistory": [{"type":"compaction", "encrypted_content":"opaque-primary"}]
                        })),
                        ..pa_types::session::CompactionEntry::default()
                    },
                ).unwrap();
            }
        }
        let primary_second = serde_json::to_value(&primary.calls()[1].messages).unwrap();
        let backup_second = serde_json::to_value(&backup.calls()[1].messages).unwrap();
        assert_eq!(
            primary_second[0]["providerPayload"]["items"][0]["encrypted_content"],
            "opaque-primary"
        );
        assert_eq!(backup_second[0]["content"], primary_second[0]["content"]);
        assert!(backup_second[0].get("providerPayload").is_none());
        assert_eq!(primary.calls().len(), 2);
        assert_eq!(backup.calls().len(), 2);
        let retained = lane.sessions.lock().await["act-role-test/primary"].clone();
        let session = retained.session.lock().await;
        let serving: Vec<_> = session
            .active_branch_entries()
            .into_iter()
            .filter_map(|entry| match entry {
                pa_types::session::FileEntry::ModelChange { payload, .. } => {
                    Some(payload.model_id.as_str())
                }
                _ => None,
            })
            .collect();
        assert_eq!(serving, ["primary", "backup", "primary", "backup"]);
        registration.unregister();
    }

    #[tokio::test]
    async fn completion_requires_done_and_successful_context_is_retained() {
        let provider = Arc::new(ScriptedProvider::new(Model::unknown()));
        provider.push_tool_call_turn(
            None,
            vec![(
                "cell-1",
                "shared_ipython",
                json!({"code":"rlm.done(value)"}),
            )],
        );
        provider.push_tool_call_turn(
            None,
            vec![(
                "cell-2",
                "shared_ipython",
                json!({"code":"rlm.done(value)"}),
            )],
        );
        let lane = Arc::new(ActLane::default());
        for prompt in ["first", "second"] {
            let (channel, codes) = channel(Some(json!({"type":"done"})));
            let events = Arc::new(Mutex::new(Vec::new()));
            let captured = events.clone();
            let projection = ActProjection::new(
                json!({"actId":prompt,"depth":1,"outerToolCallId":"outer-call"}),
                prompt,
                &Model::unknown(),
                ThinkingLevel::High,
                Arc::new(move |event| lock(&captured).push(event)),
            );
            assert_eq!(
                lane.run_with_projection(
                    prompt.into(),
                    channel,
                    target(&provider, "p/m"),
                    Vec::new(),
                    None,
                    Some(projection)
                )
                .await
                .unwrap(),
                ActLaneResult::Done
            );
            assert_eq!(*lock(&codes), ["rlm.done(value)"]);
            let events = lock(&events);
            assert_eq!(
                events
                    .iter()
                    .map(|event| event["event"].as_str().unwrap())
                    .collect::<Vec<_>>(),
                ["start", "cell_start", "cell_terminal"]
            );
            for (index, event) in events.iter().enumerate() {
                assert_eq!(event["actId"], prompt);
                assert_eq!(event["sequence"], index + 1);
            }
        }
        let calls = provider.calls();
        assert_eq!(calls.len(), 2);
        assert!(calls[1].messages.len() > calls[0].messages.len());
        assert!(!lane.running());
    }

    #[tokio::test]
    async fn text_response_does_not_complete_or_pollute_next_assignment() {
        let provider = Arc::new(ScriptedProvider::new(Model::unknown()));
        provider.push_text_turn("unfinished");
        provider.push_text_turn("another incomplete assignment");
        let lane = Arc::new(ActLane::default());
        for expected in ["unfinished", "another incomplete assignment"] {
            let (channel, _) = channel(None);
            assert_eq!(
                lane.run("task".into(), channel, target(&provider, "p/m"), Vec::new())
                    .await
                    .unwrap(),
                ActLaneResult::Text(expected.into())
            );
        }
        assert_eq!(
            provider.calls()[0].messages.len(),
            provider.calls()[1].messages.len()
        );
    }

    #[tokio::test]
    async fn cancellation_settles_provider_and_restores_retained_context() {
        let provider = Arc::new(ScriptedProvider::new(Model::unknown()));
        provider.push_stalled_turn("pending");
        let lane = Arc::new(ActLane::default());
        let (channel, _) = channel(None);
        let running = {
            let lane = lane.clone();
            let target = target(&provider, "p/m");
            tokio::spawn(async move { lane.run("task".into(), channel, target, Vec::new()).await })
        };
        while provider.calls().is_empty() {
            tokio::task::yield_now().await;
        }
        assert!(lane.cancel());
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), running)
                .await
                .unwrap()
                .unwrap()
                .unwrap(),
            ActLaneResult::Cancelled
        );
        assert!(!lane.running());
        assert!(lane.sessions.lock().await["p/m"]
            .agent
            .state()
            .await
            .messages
            .is_empty());
    }

    #[tokio::test]
    async fn dropped_host_future_cancels_and_releases_lane() {
        let provider = Arc::new(ScriptedProvider::new(Model::unknown()));
        provider.push_stalled_turn("pending");
        let lane = Arc::new(ActLane::default());
        let (channel, _) = channel(None);
        let running = {
            let lane = lane.clone();
            let target = target(&provider, "p/m");
            tokio::spawn(async move { lane.run("task".into(), channel, target, Vec::new()).await })
        };
        while provider.calls().is_empty() {
            tokio::task::yield_now().await;
        }
        running.abort();
        let _ = running.await;
        tokio::time::timeout(Duration::from_secs(2), lane.wait_for_idle())
            .await
            .unwrap();
        assert!(!lane.running());
    }

    #[test]
    fn malformed_cell_replies_are_rejected() {
        for response in [
            json!({"type":"unknown"}),
            json!({"type":"cell_result", "durationMs":"1"}),
            json!({"type":"cell_result", "stdout":[]}),
        ] {
            assert!(format_cell_result(&response).is_err());
        }
        assert_eq!(
            format_cell_result(&json!({"type":"cell_result", "stdout":"result", "stderr":null}))
                .unwrap(),
            "[stdout]\nresult"
        );
    }

    #[tokio::test]
    async fn durable_context_restores_completed_work_and_excludes_incomplete_work() {
        let dir = tempfile::tempdir().unwrap();
        let provider = Arc::new(ScriptedProvider::new(Model::unknown()));
        provider.push_tool_call_turn(
            None,
            vec![(
                "first-done",
                "shared_ipython",
                json!({"code":"rlm.done(value)"}),
            )],
        );
        provider.push_text_turn("unfinished");
        provider.push_tool_call_turn(
            None,
            vec![(
                "resumed-done",
                "shared_ipython",
                json!({"code":"rlm.done(value)"}),
            )],
        );
        let lane = Arc::new(ActLane::persisted(
            dir.path().to_path_buf(),
            dir.path().join("act"),
        ));
        let (first_channel, _) = channel(Some(json!({"type":"done"})));
        lane.run(
            "completed assignment".into(),
            first_channel,
            target(&provider, "p/m"),
            Vec::new(),
        )
        .await
        .unwrap();
        let (incomplete_channel, _) = channel(None);
        lane.run(
            "abandoned assignment".into(),
            incomplete_channel,
            target(&provider, "p/m"),
            Vec::new(),
        )
        .await
        .unwrap();
        lane.dispose().await;
        let restored = Arc::new(ActLane::persisted(
            dir.path().to_path_buf(),
            dir.path().join("act"),
        ));
        let (last_channel, _) = channel(Some(json!({"type":"done"})));
        restored
            .run(
                "current assignment".into(),
                last_channel,
                target(&provider, "p/m"),
                Vec::new(),
            )
            .await
            .unwrap();
        let calls = provider.calls();
        let context = serde_json::to_string(&calls[2]).unwrap();
        assert!(context.contains("completed assignment"));
        assert!(context.contains("current assignment"));
        assert!(!context.contains("abandoned assignment"));
        assert!(dir.path().join("act/session.jsonl").exists());
    }

    #[tokio::test]
    async fn retained_context_uses_fresh_provider_credentials_for_each_assignment() {
        let first = Arc::new(ScriptedProvider::new(Model::unknown()));
        let second = Arc::new(ScriptedProvider::new(Model::unknown()));
        for provider in [&first, &second] {
            provider.push_tool_call_turn(
                None,
                vec![("done", "shared_ipython", json!({"code":"rlm.done(value)"}))],
            );
        }
        let lane = Arc::new(ActLane::default());
        for provider in [&first, &second] {
            let (channel, _) = channel(Some(json!({"type":"done"})));
            lane.run(
                "assignment".into(),
                channel,
                target(provider, "p/m"),
                Vec::new(),
            )
            .await
            .unwrap();
        }
        assert_eq!(first.calls().len(), 1);
        assert_eq!(second.calls().len(), 1);
        assert!(second.calls()[0].messages.len() > first.calls()[0].messages.len());
    }

    #[tokio::test]
    async fn failed_start_persistence_prevents_provider_admission_and_releases_the_lane() {
        let provider = Arc::new(ScriptedProvider::new(Model::unknown()));
        provider.push_text_turn("must remain unconsumed");
        let lane = Arc::new(ActLane::default());
        let (host, _) = channel(None);
        let hook: ActStartHook =
            Arc::new(|_| Box::pin(async { anyhow::bail!("start persistence failed") }));
        let error = lane
            .run_with_start(
                "assignment".into(),
                host,
                target(&provider, "p/m"),
                Vec::new(),
                Some(hook),
            )
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), "start persistence failed");
        assert!(provider.calls().is_empty());
        assert!(!lane.running());
        let retained = lane.sessions.lock().await["p/m"].clone();
        assert!(retained.agent.state().await.tools.is_empty());
        assert!(retained.agent.state().await.messages.is_empty());
    }
}
