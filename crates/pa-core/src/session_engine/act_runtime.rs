use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};

use serde_json::{json, Value};

use crate::kernel::host_channel::{duplex_host_handler, HostRequestChannel};
use crate::kernel::shared::{HostRequestHandlers, HostRequestPayload};
use crate::models::registry::ModelRegistry;
use crate::models::runtime_roles::{
    parse_rlm_runtime_candidate, resolve_rlm_role_candidates, RlmRuntimeKind,
};
use crate::settings::SettingsManager;

use super::act_lane::{ActFallback, ActFallbackTarget, ActLane, ActLaneResult, ActLaneTarget};
use super::provider_adapter::{
    json_round_trip, map_thinking_level, switchable_stream_fn, ProviderTarget,
};

mod context_tree;
pub mod projection;
mod records;
pub(super) use records::usage_delta;

type ParentSession = tokio::sync::Mutex<crate::session::manager::SessionManager>;

pub type ActRecordSink = Arc<dyn Fn(&str, Value) -> anyhow::Result<()> + Send + Sync>;
pub type ActModelGate = Arc<dyn Fn(&str) -> anyhow::Result<()> + Send + Sync>;

#[derive(Default)]
struct ActAuthorization {
    selected: HashMap<String, crate::auth::AuthSourceToken>,
    stale: Vec<crate::auth::AuthSourceToken>,
}

#[derive(Default)]
struct RuntimeState {
    lanes: HashMap<u32, Arc<ActLane>>,
    active: Vec<(u32, Arc<ActLane>, String)>,
    previous_caller_calls: HashMap<u32, String>,
    disposed: bool,
}

pub struct ActRuntime {
    pub(super) telemetry: Mutex<Option<Arc<super::telemetry::SessionTelemetry>>>,
    cwd: PathBuf,
    agent_dir: PathBuf,
    artifact_dir: Option<PathBuf>,
    state: Mutex<RuntimeState>,
    authorization: Mutex<ActAuthorization>,
    parent_agent: Mutex<Option<std::sync::Weak<pa_agent::agent::Agent>>>,
    parent_session: Mutex<Option<std::sync::Weak<ParentSession>>>,
    record_sink: Mutex<Option<ActRecordSink>>,
    event_sink: Mutex<Option<projection::ActEventSink>>,
    model_gate: Mutex<Option<ActModelGate>>,
    foreground: Mutex<Option<std::sync::Weak<super::root_foreground_lease::RootForegroundLease>>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl ActRuntime {
    #[must_use]
    pub fn new(cwd: PathBuf, agent_dir: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            cwd,
            agent_dir,
            artifact_dir: None,
            telemetry: Mutex::new(None),
            state: Mutex::new(RuntimeState::default()),
            authorization: Mutex::default(),
            parent_agent: Mutex::new(None),
            parent_session: Mutex::new(None),
            record_sink: Mutex::new(None),
            event_sink: Mutex::new(None),
            model_gate: Mutex::new(None),
            foreground: Mutex::new(None),
        })
    }

    #[must_use]
    pub fn with_persistence(
        cwd: PathBuf,
        agent_dir: PathBuf,
        artifact_dir: Option<PathBuf>,
    ) -> Arc<Self> {
        Arc::new(Self {
            cwd,
            agent_dir,
            artifact_dir,
            telemetry: Mutex::new(None),
            state: Mutex::new(RuntimeState::default()),
            authorization: Mutex::default(),
            parent_agent: Mutex::new(None),
            parent_session: Mutex::new(None),
            record_sink: Mutex::new(None),
            event_sink: Mutex::new(None),
            model_gate: Mutex::new(None),
            foreground: Mutex::new(None),
        })
    }

    pub fn register(self: &Arc<Self>, handlers: &mut HostRequestHandlers) {
        let runtime = self.clone();
        handlers.register_duplex(
            "rlm.act",
            duplex_host_handler(move |payload, channel| {
                let runtime = runtime.clone();
                async move { runtime.run(payload, channel).await }
            }),
        );
    }

    pub fn bind_foreground(
        &self,
        foreground: &Arc<super::root_foreground_lease::RootForegroundLease>,
    ) {
        *lock(&self.foreground) = Some(Arc::downgrade(foreground));
    }

    pub fn bind_parent(&self, agent: &Arc<pa_agent::agent::Agent>) {
        *lock(&self.parent_agent) = Some(Arc::downgrade(agent));
    }

    pub fn set_record_sink(&self, sink: ActRecordSink) {
        *lock(&self.record_sink) = Some(sink);
    }

    pub fn set_event_sink(&self, sink: projection::ActEventSink) {
        *lock(&self.event_sink) = Some(sink);
    }

    pub fn clear_event_sink(&self) {
        *lock(&self.event_sink) = None;
    }

    pub fn set_model_gate(&self, gate: ActModelGate) {
        *lock(&self.model_gate) = Some(gate);
    }

    /// # Errors
    /// Returns persistence errors while closing interrupted Act records.
    pub async fn bind_session(&self, session: &Arc<ParentSession>) -> anyhow::Result<()> {
        *lock(&self.parent_session) = Some(Arc::downgrade(session));
        self.recover_interrupted(session).await
    }

    pub fn cancel(&self) -> bool {
        let lanes: Vec<_> = lock(&self.state)
            .active
            .iter()
            .rev()
            .map(|(_, lane, _)| lane.clone())
            .collect();
        let mut cancelled = false;
        for lane in lanes {
            cancelled |= lane.cancel();
        }
        cancelled
    }

    pub fn running(&self) -> bool {
        !lock(&self.state).active.is_empty()
    }

    pub async fn dispose(&self) {
        let lanes = {
            let mut state = lock(&self.state);
            state.disposed = true;
            state.lanes.values().cloned().collect::<Vec<_>>()
        };
        for lane in lanes {
            lane.dispose().await;
        }
    }

    async fn run(
        self: &Arc<Self>,
        payload: HostRequestPayload,
        channel: Arc<HostRequestChannel>,
    ) -> anyhow::Result<Value> {
        let runtime = self.clone();
        let telemetry = lock(&self.telemetry).clone();
        if let Some(telemetry) = &telemetry {
            telemetry.note_feature_outcome("rlm.act", "initiated", None);
        }
        let result = tokio::spawn(async move { runtime.run_inner(payload, channel).await }).await?;
        if let Some(telemetry) = telemetry {
            let outcome = match &result {
                Ok(value) if value["outcome"] == "done" => "completed",
                Ok(value) if value["outcome"] == "cancelled" => "canceled",
                Ok(_) | Err(_) => "failed",
            };
            telemetry.note_feature_outcome("rlm.act", outcome, None);
        }
        result
    }

    async fn run_inner(
        self: &Arc<Self>,
        payload: HostRequestPayload,
        channel: Arc<HostRequestChannel>,
    ) -> anyhow::Result<Value> {
        let prompt = payload
            .data
            .get("prompt")
            .and_then(Value::as_str)
            .filter(|prompt| !prompt.trim().is_empty())
            .ok_or_else(|| anyhow::anyhow!("rlm.act requires a non-empty prompt"))?
            .to_owned();
        anyhow::ensure!(
            channel.outer_tool_call_id.is_some(),
            "rlm.act requires outer tool-call correlation"
        );
        let foreground = lock(&self.foreground)
            .as_ref()
            .and_then(std::sync::Weak::upgrade);
        let _foreground_act = foreground
            .as_ref()
            .map(|foreground| {
                let token = foreground.active_token().ok_or_else(|| {
                    anyhow::anyhow!("Act caller has no active root foreground execution")
                })?;
                foreground.enter_act(token)
            })
            .transpose()?;
        let requested = match payload.data.get("model") {
            None | Some(Value::Null) => None,
            Some(Value::String(model)) if !model.trim().is_empty() => Some(model.trim().to_owned()),
            Some(_) => anyhow::bail!("rlm.act model must be a non-empty string"),
        };
        let settings = SettingsManager::create(&self.cwd, &self.agent_dir);
        let max_depth = settings.get_rlm_act_max_depth()?;
        let act_id = uuid::Uuid::new_v4().to_string();
        let (depth, lane, parent_lane, parent_act_id) = {
            let mut state = lock(&self.state);
            if state.disposed || channel.signal.is_aborted() {
                return Ok(json!({"outcome":"cancelled"}));
            }
            if let Some((_, parent, _)) = state.active.last() {
                anyhow::ensure!(
                    parent.cell_running(),
                    "Nested rlm.act requires the calling Act's active shared-IPython cell"
                );
            }
            let depth = u32::try_from(state.active.len() + 1)?;
            anyhow::ensure!(
                u64::from(depth) <= max_depth,
                "rlm.act depth {depth} exceeds rlmActMaxDepth {max_depth}"
            );
            let lane = state
                .lanes
                .entry(depth)
                .or_insert_with(|| {
                    self.artifact_dir.as_ref().map_or_else(
                        || Arc::new(ActLane::default()),
                        |dir| {
                            Arc::new(ActLane::persisted(
                                self.cwd.clone(),
                                dir.join(if depth == 1 {
                                    "act".to_owned()
                                } else {
                                    format!("act-depth-{depth}")
                                }),
                            ))
                        },
                    )
                })
                .clone();
            let parent_lane = state.active.last().map(|(_, lane, _)| lane.clone());
            let parent_act_id = state.active.last().map(|(_, _, id)| id.clone());
            state.active.push((depth, lane.clone(), act_id.clone()));
            (depth, lane, parent_lane, parent_act_id)
        };
        let _frame = ActiveFrame {
            runtime: self.clone(),
            depth,
        };
        let reference = requested.or(settings.get_rlm_act_default_model(depth as usize)?).ok_or_else(|| anyhow::anyhow!("rlm.act requires an explicit model at Act depth {depth} because rlmActDefaultModel has no entry"))?;
        let parent_session = lock(&self.parent_session)
            .as_ref()
            .and_then(std::sync::Weak::upgrade);
        let root_branch = match &parent_session {
            Some(session) => session
                .lock()
                .await
                .active_branch_entries()
                .into_iter()
                .cloned()
                .collect::<Vec<_>>(),
            None => Vec::new(),
        };
        let caller_entries = if let Some(parent) = &parent_lane {
            parent.caller_history_entries().await?
        } else if parent_session.is_some() {
            root_branch
                .iter()
                .map(serde_json::to_value)
                .collect::<Result<_, _>>()?
        } else {
            let parent = lock(&self.parent_agent)
                .as_ref()
                .and_then(std::sync::Weak::upgrade);
            match parent {
                Some(parent) => parent
                    .state()
                    .await
                    .messages
                    .iter()
                    .map(|message| {
                        Ok(json!({"type":"message", "message":serde_json::to_value(message)?}))
                    })
                    .collect::<anyhow::Result<Vec<_>>>()?,
                None => Vec::new(),
            }
        };
        let root_act_records = match &parent_session {
            Some(session) => session.lock().await.act_records(),
            None => Vec::new(),
        };
        let previous_call = root_act_records
            .iter()
            .rev()
            .find_map(|entry| match entry {
                pa_types::session::FileEntry::ActStart { payload, .. }
                    if payload.depth == depth =>
                {
                    payload.outer_tool_call_id.clone()
                }
                _ => None,
            })
            .or_else(|| lock(&self.state).previous_caller_calls.get(&depth).cloned());
        let history = super::history_snapshot::build_act_caller_history(
            &caller_entries,
            channel.outer_tool_call_id.as_deref(),
            previous_call.as_deref(),
        )?;
        let inherited_thinking = if let Some(parent) = parent_lane {
            parent
                .thinking_level()
                .await
                .unwrap_or(pa_agent::types::ThinkingLevel::Off)
        } else {
            let parent = lock(&self.parent_agent)
                .as_ref()
                .and_then(std::sync::Weak::upgrade);
            match parent {
                Some(parent) => parent.state().await.thinking_level,
                None => pa_agent::types::ThinkingLevel::Off,
            }
        };
        let (target, image_capable, compaction_model) = self
            .resolve_target(&settings, &reference, depth, max_depth, inherited_thinking)
            .await?;
        anyhow::ensure!(history.images.is_empty() || image_capable,
            "rlm.act caller-history transfer requires an image-capable model after the first call at a depth");
        if let Some(call_id) = &channel.outer_tool_call_id {
            lock(&self.state)
                .previous_caller_calls
                .insert(depth, call_id.clone());
        }
        let (start, baseline) = self.start_hook(
            act_id.clone(),
            depth,
            parent_act_id.clone(),
            channel.outer_tool_call_id.clone(),
            target.session_key.clone(),
        );
        let session_key = target.session_key.clone();
        let model = pa_types::session::ActModel {
            provider: target.model.provider.clone(),
            id: target.model.id.clone(),
        };
        let cancelled = channel.signal.clone();
        let projection = lock(&self.event_sink).clone().map(|sink| projection::ActProjection::new(
            json!({"actId":act_id,"depth":depth,"parentActId":parent_act_id,"outerToolCallId":channel.outer_tool_call_id}),
            &prompt, &target.model, target.thinking_level, sink,
        ));
        let fallback: Option<ActFallback> = if let Some(role) = reference.strip_prefix('@') {
            let candidates = resolve_rlm_role_candidates(role, &settings.get_model_roles())?;
            let index = candidates
                .iter()
                .position(|candidate| candidate.selector.eq_ignore_ascii_case(&session_key))
                .unwrap_or(0);
            let cursor = Arc::new(std::sync::atomic::AtomicUsize::new(index + 1));
            let candidates = Arc::new(candidates);
            let runtime = Arc::downgrade(self);
            let selected = Arc::new(Mutex::new(session_key.clone()));
            let needs_images = !history.images.is_empty();
            Some(Arc::new(
                move |failed: pa_agent::types::AssistantMessage| {
                    let runtime = runtime.clone();
                    let candidates = candidates.clone();
                    let cursor = cursor.clone();
                    let selected = selected.clone();
                    Box::pin(async move {
                        let runtime = runtime
                            .upgrade()
                            .ok_or_else(|| anyhow::anyhow!("Act runtime was retired"))?;
                        if super::provider_retry::provider_stream_failure_kind(&failed).as_deref()
                            == Some("auth")
                            && matches!(
                                super::provider_retry::provider_stream_failure_status(&failed),
                                Some(401 | 403)
                            )
                        {
                            let mut authorization = lock(&runtime.authorization);
                            let selector = format!("{}/{}", failed.provider, failed.model);
                            if let Some(token) = authorization
                                .selected
                                .get(&selector)
                                .filter(|token| !authorization.stale.contains(token))
                                .cloned()
                            {
                                authorization.stale.push(token);
                            }
                        }
                        let settings = SettingsManager::create(&runtime.cwd, &runtime.agent_dir);
                        if !settings.get_provider_retry_policy().enabled {
                            return Ok(None);
                        }
                        loop {
                            let index = cursor.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
                            let Some(candidate) = candidates.get(index) else {
                                return Ok(None);
                            };
                            if candidate.selector.eq_ignore_ascii_case(&lock(&selected)) {
                                continue;
                            }
                            let selector = candidate.thinking_level.map_or_else(
                                || candidate.selector.clone(),
                                |level| format!("{}:{}", candidate.selector, level.wire_name()),
                            );
                            match runtime
                                .resolve_target(
                                    &settings,
                                    &selector,
                                    depth,
                                    max_depth,
                                    inherited_thinking,
                                )
                                .await
                            {
                                Ok((target, image_capable, model)) => {
                                    if needs_images && !image_capable {
                                        continue;
                                    }
                                    lock(&selected).clone_from(&target.session_key);
                                    return Ok(Some(ActFallbackTarget { target, model }));
                                }
                                Err(error)
                                    if error.downcast_ref::<ActModelUnavailable>().is_some() => {}
                                Err(error) => return Err(error),
                            }
                        }
                    })
                },
            ))
        } else {
            None
        };
        lane.configure_role_fallback(fallback);
        lane.configure_compaction(self.cwd.clone(), self.agent_dir.clone(), compaction_model);
        let result = lane
            .run_with_projection(
                prompt,
                channel,
                target,
                history.images,
                Some(start),
                projection.clone(),
            )
            .await;
        let model = lane
            .context_snapshot(&session_key)
            .await
            .map_or(model, |(model, _, _)| model);
        let usage = lane.usage_for(&session_key).await;
        let delta = lock(&baseline)
            .map(|before| records::usage_delta(usage, before))
            .unwrap_or_default();
        let started = lock(&baseline).is_some();
        let persisted = self
            .finish_record(
                records::ActRecordIdentity {
                    act_id,
                    depth,
                    parent_act_id,
                    session_key,
                    model,
                },
                &result,
                cancelled.is_aborted(),
                baseline,
                usage,
            )
            .await;
        if let Some(projection) = projection.filter(|_| started) {
            let status = match &result {
                Ok(ActLaneResult::Done) => "done",
                Ok(ActLaneResult::Cancelled) => "cancelled",
                _ if cancelled.is_aborted() => "cancelled",
                _ => "error",
            };
            let error = if let Err(error) = &persisted {
                Some(format!("Failed to persist Act completion: {error:#}"))
            } else {
                match &result {
                    Ok(ActLaneResult::Text(_)) => {
                        Some("Act ended without calling rlm.done()".into())
                    }
                    Err(error) => Some(error.to_string()),
                    _ => None,
                }
            };
            projection.terminal(
                if persisted.is_err() { "error" } else { status },
                delta,
                error.as_deref(),
            );
        }
        persisted?;
        let outcome = result?;
        Ok(match outcome {
            ActLaneResult::Done => json!({"outcome":"done"}),
            ActLaneResult::Cancelled => json!({"outcome":"cancelled"}),
            ActLaneResult::Text(text) => json!({"outcome":"text", "text":text}),
        })
    }

    async fn resolve_target(
        &self,
        settings: &SettingsManager,
        reference: &str,
        depth: u32,
        max_depth: u64,
        inherited_thinking: pa_agent::types::ThinkingLevel,
    ) -> anyhow::Result<(ActLaneTarget, bool, pa_types::ai::Model)> {
        let candidates = if let Some(role) = reference.strip_prefix('@') {
            resolve_rlm_role_candidates(role, &settings.get_model_roles())?
        } else {
            vec![parse_rlm_runtime_candidate(reference)?]
        };
        anyhow::ensure!(
            candidates
                .iter()
                .all(|c| c.runtime == RlmRuntimeKind::Native),
            "Act model selector \"{reference}\" must resolve to a native model"
        );
        let auth = crate::auth::AuthStorage::create(&self.agent_dir);
        let mut registry = ModelRegistry::create(auth, self.agent_dir.join("models.json"));
        for token in lock(&self.authorization).stale.clone() {
            registry.auth.mark_auth_source_stale(token);
        }
        registry.load_private_authorization_from_cache();
        registry
            .refresh_merge_gateway_models(crate::models::merge_gateway::CATALOG_BASE_URL)
            .await;
        let models = registry.get_executable_models().await;
        for candidate in candidates {
            let Some(model) = models.iter().find(|model| {
                format!("{}/{}", model.provider, model.id)
                    .eq_ignore_ascii_case(&candidate.model_reference)
            }) else {
                continue;
            };
            let gate = lock(&self.model_gate).clone();
            if let Some(gate) = gate {
                let selector = format!("{}/{}", model.provider, model.id);
                tokio::task::spawn_blocking(move || gate(&selector)).await??;
            }
            let mut model = model.clone();
            let level = candidate.thinking_level.unwrap_or_else(|| {
                pa_types::ai::clamp_thinking_level(
                    &model,
                    super::provider_adapter::model_thinking_level(inherited_thinking),
                )
            });
            if candidate.thinking_level.is_some() && model.reasoning {
                model
                    .thinking_level_map
                    .get_or_insert_with(std::collections::BTreeMap::new)
                    .insert(level, Some(level.wire_name().into()));
            }
            let auth = registry.get_api_key_and_headers(&model, None);
            if !auth.ok {
                continue;
            }
            let source = registry
                .auth
                .get_api_key_with_source_token(&model.provider, /*include_fallback*/ true);
            if let Some(token) = source
                .source_token
                .filter(|_| source.api_key == auth.api_key)
            {
                lock(&self.authorization)
                    .selected
                    .insert(format!("{}/{}", model.provider, model.id), token);
            }
            let target = ProviderTarget {
                api_key: auth.api_key,
                model: model.clone(),
                service_tier: Some(
                    if settings.get_default_service_tier() == pa_types::ai::ServiceTier::Priority
                        && !pa_types::ai::supports_fast_mode(&model)
                    {
                        pa_types::ai::ServiceTier::Default
                    } else {
                        settings.get_default_service_tier()
                    },
                ),
                headers: auth.headers,
            };
            let image_capable = model.input.contains(&pa_types::ai::ModelInput::Image);
            return Ok((
                ActLaneTarget {
                    session_key: format!("{}/{}", model.provider, model.id),
                    model: json_round_trip(&model)
                        .ok_or_else(|| anyhow::anyhow!("Act model wire shapes do not match"))?,
                    thinking_level: map_thinking_level(level),
                    stream_fn: switchable_stream_fn(Arc::new(std::sync::RwLock::new(Some(target)))),
                    depth,
                    max_depth: u32::try_from(max_depth).unwrap_or(u32::MAX),
                },
                image_capable,
                model.clone(),
            ));
        }
        Err(ActModelUnavailable(reference.to_owned()).into())
    }
}

#[derive(Debug, thiserror::Error)]
#[error("Act model selector \"{0}\" has no executable candidates")]
struct ActModelUnavailable(String);

struct ActiveFrame {
    runtime: Arc<ActRuntime>,
    depth: u32,
}
impl Drop for ActiveFrame {
    fn drop(&mut self) {
        lock(&self.runtime.state)
            .active
            .retain(|(depth, _, _)| *depth != self.depth);
    }
}
