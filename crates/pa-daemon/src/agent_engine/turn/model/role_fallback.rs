use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use pa_agent::agent::Agent;
use pa_agent::types::StopReason;
use pa_core::models::{parse_rlm_runtime_candidate, ModelRegistry, RlmRuntimeKind};
use pa_core::session_engine::auto_retry::{AutoRetryEvent, RetryStartReason};
use pa_core::session_engine::can_advance_role_candidate as can_advance;
use pa_core::session_engine::provider_retry::{
    provider_stream_failure_kind, provider_stream_failure_status,
};

use super::{
    drop_trailing_assistant, json_round_trip, map_thinking_level, retry_event_to_engine_event,
    AgentSessionEngine, EngineEvent, ProviderTarget, TurnOnce, TurnPrompt,
};

impl AgentSessionEngine {
    pub(super) async fn run_turn_with_role_fallback(
        &self,
        agent: &Arc<Agent>,
        prompt: &TurnPrompt,
        first_attempt: bool,
        boundary_passed: &Arc<AtomicBool>,
        aborted: &dyn Fn() -> bool,
        emit: &mut dyn FnMut(EngineEvent) -> bool,
    ) -> anyhow::Result<TurnOnce> {
        let selectors = self
            .create_resources
            .read()
            .expect("create resources lock")
            .rlm_model_candidates
            .clone();
        if selectors.len() < 2 || !self.retry_policy().enabled {
            return self
                .run_turn_once(agent, prompt, first_attempt, boundary_passed, aborted, emit)
                .await;
        }
        let candidates = selectors
            .iter()
            .map(|selector| parse_rlm_runtime_candidate(selector))
            .collect::<anyhow::Result<Vec<_>>>()?;
        let mut first = first_attempt;
        let mut advances = 0;
        let (mut current_index, stale_sources) = {
            let mut retained = self
                .role_candidate_state
                .lock()
                .expect("role candidate lock");
            if retained.selectors != selectors {
                retained.selectors = selectors;
                retained.current_index = None;
            }
            (retained.current_index, retained.stale_sources.clone())
        };
        let mut registry = ModelRegistry::create(
            pa_core::auth::AuthStorage::create(&self.config.agent_dir),
            self.config.agent_dir.join("models.json"),
        );
        registry.load_private_authorization_from_cache();
        for token in stale_sources {
            registry.auth.mark_auth_source_stale(token);
        }
        loop {
            let outcome = self
                .run_turn_once(agent, prompt, first, boundary_passed, aborted, emit)
                .await?;
            let TurnOnce::Message { assistant } = &outcome else {
                return Ok(outcome);
            };
            let state = agent.state().await;
            if advances > 0
                && assistant.stop_reason == StopReason::Stop
                && !emit(retry_event_to_engine_event(AutoRetryEvent::End {
                    success: true,
                    attempt: advances,
                    final_error: None,
                    restored_model: None,
                }))
            {
                return Ok(TurnOnce::Aborted);
            }
            if candidates.len() < 2
                || self.armed_image_route().is_some()
                || aborted()
                || !can_advance(assistant, state.model.context_window)
                || assistant.provider != state.model.provider
                || assistant.model != state.model.id
            {
                return Ok(outcome);
            }
            let current = format!("{}/{}", state.model.provider, state.model.id);
            let Some(index) = current_index.or_else(|| {
                candidates
                    .iter()
                    .position(|candidate| candidate.selector.eq_ignore_ascii_case(&current))
            }) else {
                return Ok(outcome);
            };
            if provider_stream_failure_kind(assistant).as_deref() == Some("auth")
                && matches!(provider_stream_failure_status(assistant), Some(401 | 403))
            {
                let source = registry
                    .auth
                    .get_api_key_with_source_token(&assistant.provider, true);
                let rejected = self
                    .provider_target
                    .read()
                    .expect("provider target lock")
                    .as_ref()
                    .and_then(|target| target.api_key.clone());
                if source.api_key == rejected {
                    if let Some(token) = source.source_token {
                        registry.auth.mark_auth_source_stale(token.clone());
                        let mut retained = self
                            .role_candidate_state
                            .lock()
                            .expect("role candidate lock");
                        if !retained.stale_sources.contains(&token) {
                            retained.stale_sources.push(token);
                        }
                    }
                }
            }
            let models = registry.get_executable_models().await;
            let mut next = None;
            for (candidate_index, candidate) in candidates.iter().enumerate().skip(index + 1) {
                if candidate.selector.eq_ignore_ascii_case(&current) {
                    continue;
                }
                anyhow::ensure!(
                    candidate.runtime == RlmRuntimeKind::Native,
                    "Native model fallback cannot change child runtime"
                );
                let Some(model) = models.iter().find(|model| {
                    format!("{}/{}", model.provider, model.id)
                        .eq_ignore_ascii_case(&candidate.selector)
                }) else {
                    continue;
                };
                crate::model_allowlist::assert_allowed(
                    &crate::model_allowlist::load(&self.cwd(), &self.config.agent_dir),
                    &candidate.selector,
                )?;
                let auth = registry.get_api_key_and_headers(model, None);
                if auth.ok {
                    next = Some((candidate_index, candidate, model.clone(), auth));
                    break;
                }
            }
            let Some((candidate_index, candidate, mut model, auth)) = next else {
                return Ok(outcome);
            };
            let level = candidate.thinking_level.unwrap_or_else(|| {
                pa_types::ai::clamp_thinking_level(
                    &model,
                    pa_core::session_engine::provider_adapter::model_thinking_level(
                        state.thinking_level,
                    ),
                )
            });
            if candidate.thinking_level.is_some() && model.reasoning {
                model
                    .thinking_level_map
                    .get_or_insert_with(std::collections::BTreeMap::new)
                    .insert(level, Some(level.wire_name().into()));
            }
            let core = self.session.lock().await.clone().ok_or_else(|| {
                anyhow::anyhow!("Child session was retired during model fallback")
            })?;
            let journal = self
                .act_record_sink
                .lock()
                .expect("worker journal sink lock")
                .clone();
            if let Some(journal) = journal {
                journal(
                    "model_change",
                    serde_json::json!({"provider":model.provider,"modelId":model.id}),
                )?;
                journal(
                    "thinking_level_change",
                    serde_json::json!({"thinkingLevel":level.wire_name()}),
                )?;
            }
            let entries = {
                let persistence = core.session.shared_persistence();
                let mut session = persistence.lock().await;
                session.append_model_change(&model.provider, &model.id)?;
                session.append_thinking_level_change(level.wire_name())?;
                session
                    .active_branch_entries()
                    .into_iter()
                    .cloned()
                    .collect::<Vec<_>>()
            };
            if state.model.provider != model.provider {
                let context = pa_core::session::build_session_context(&entries, None);
                let messages = context
                    .messages
                    .into_iter()
                    .map(|message| serde_json::from_value(serde_json::to_value(message)?))
                    .collect::<Result<Vec<_>, serde_json::Error>>()?;
                agent.set_messages(messages).await;
            }
            drop_trailing_assistant(agent).await;
            agent
                .set_model_and_thinking_level(
                    json_round_trip(&model)
                        .ok_or_else(|| anyhow::anyhow!("Fallback model conversion failed"))?,
                    map_thinking_level(level),
                )
                .await;
            {
                let mut selection = self.selection.write().expect("model selection lock");
                selection.provider = Some(model.provider.clone());
                selection.model = Some(model.id.clone());
                selection.thinking = Some(level);
            }
            *self
                .effective_thinking
                .write()
                .expect("effective thinking lock") = Some(level);
            let tier = {
                let mut tier = self.service_tier.write().expect("service tier lock");
                if *tier == Some(pa_types::ai::ServiceTier::Priority)
                    && !pa_types::ai::supports_fast_mode(&model)
                {
                    *tier = Some(pa_types::ai::ServiceTier::Default);
                }
                *tier
            };
            *self.provider_target.write().expect("provider target lock") = Some(ProviderTarget {
                model: model.clone(),
                api_key: auth.api_key,
                headers: auth.headers,
                service_tier: tier,
            });
            core.update_model_facts(&model);
            if let Some(children) = &self.children {
                children.set_model(format!("{}/{}", model.provider, model.id));
                children.set_service_tier(tier);
            }
            advances += 1;
            current_index = Some(candidate_index);
            self.role_candidate_state
                .lock()
                .expect("role candidate lock")
                .current_index = current_index;
            if !emit(retry_event_to_engine_event(AutoRetryEvent::Start {
                attempt: advances,
                max_attempts: u32::try_from(candidates.len() - 1).unwrap_or(u32::MAX),
                delay_ms: 0,
                error_message: assistant.error_message.clone().unwrap_or_default(),
                reason: RetryStartReason::Backup {
                    backup_model: format!("{}/{}", model.provider, model.id),
                },
            })) {
                return Ok(TurnOnce::Aborted);
            }
            first = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_engine::AgentEngineConfig;
    use crate::engine::SessionEngine;
    use pa_agent::types::{
        AssistantContent, AssistantMessage, AssistantMessageDiagnostic, TextContent,
        ThinkingContent, Usage,
    };
    use pa_ai::faux::{
        faux_assistant_text_message, register_faux_provider, FauxAssistantMessageOptions,
        FauxResponseStep, RegisterFauxProviderOptions,
    };
    use pa_types::session::FileEntry;

    fn failure(kind: &str) -> AssistantMessage {
        AssistantMessage {
            content: Vec::new(),
            api: "faux".into(),
            provider: "faux".into(),
            model: "primary".into(),
            response_model: None,
            response_id: None,
            diagnostics: Some(vec![AssistantMessageDiagnostic {
                kind: "provider_stream_failure".into(),
                timestamp: 0,
                error: None,
                details: Some(serde_json::json!({"kind": kind, "status": 503})),
            }]),
            usage: Usage::zero(),
            stop_reason: StopReason::Error,
            stop_reason_raw: None,
            error_message: Some("provider unavailable".into()),
            timestamp: 0,
        }
    }

    #[test]
    fn fallback_requires_a_safe_failure_before_any_output() {
        assert!(can_advance(&failure("server_error"), 128_000));
        for kind in ["refusal", "safety", "invalid_request"] {
            assert!(!can_advance(&failure(kind), 128_000), "{kind}");
        }
        let mut message = failure("server_error");
        message.content.push(AssistantContent::Text(TextContent {
            text: "partial answer".into(),
            text_signature: None,
        }));
        assert!(!can_advance(&message, 128_000));
        message.content = vec![AssistantContent::Thinking(ThinkingContent {
            thinking: "partial reasoning".into(),
            thinking_signature: None,
            redacted: None,
        })];
        assert!(!can_advance(&message, 128_000));
        message.content.clear();
        message.stop_reason = StopReason::Aborted;
        assert!(!can_advance(&message, 128_000));
        message.stop_reason = StopReason::Error;
        message.error_message = Some("prompt is too long: 213462 tokens > 128000 maximum".into());
        assert!(!can_advance(&message, 128_000));
    }

    #[test]
    fn role_failure_switches_immediately_and_persists_without_changing_defaults() {
        let _guard = crate::agent_engine::tests::FAUX_TEST_LOCK.lock().unwrap();
        let dir = tempfile::TempDir::new().unwrap();
        let agent_dir = dir.path().join("agent");
        std::fs::create_dir_all(&agent_dir).unwrap();
        let api = "faux-role-fallback";
        let registration = register_faux_provider(RegisterFauxProviderOptions {
            api: Some(api.into()),
            provider: Some("role-battery".into()),
            ..Default::default()
        });
        let observed = Arc::new(std::sync::Mutex::new(Vec::new()));
        let served = observed.clone();
        let backup_served = observed.clone();
        let failed: pa_ai::types::AssistantMessage =
            serde_json::from_value(serde_json::to_value(failure("server_error")).unwrap()).unwrap();
        registration.set_responses(vec![
            FauxResponseStep::Factory(Arc::new(move |_, _, _, model| {
                served.lock().unwrap().push(model.id.clone());
                Ok(failed.clone())
            })),
            FauxResponseStep::Factory(Arc::new(move |_, _, _, model| {
                backup_served.lock().unwrap().push(model.id.clone());
                Ok(faux_assistant_text_message(
                    "fallback answer",
                    FauxAssistantMessageOptions::default(),
                ))
            })),
        ]);
        std::fs::write(
            agent_dir.join("models.json"),
            serde_json::json!({
                "providers": {"role-battery": {
                    "api": api, "apiKey": "local-test-key", "baseUrl": "http://127.0.0.1:9",
                    "models": [
                        {"id":"primary", "reasoning":true, "thinkingLevelMap":{"high":null}, "contextWindow":128_000,"maxTokens":4096},
                        {"id":"backup", "contextWindow":128_000,"maxTokens":4096}
                    ]
                }}
            })
            .to_string(),
        )
        .unwrap();
        let settings =
            serde_json::json!({"defaultProvider":"role-battery", "defaultModel":"primary", "retry":{"enabled":true,"maxRetries":0}})
                .to_string();
        std::fs::write(agent_dir.join("settings.json"), &settings).unwrap();
        let engine = AgentSessionEngine::new(AgentEngineConfig {
            cwd: dir.path().to_path_buf(),
            agent_dir: agent_dir.clone(),
            provider: Some("role-battery".into()),
            model: Some("primary".into()),
            api_key: None,
            thinking: Some(pa_types::ai::ModelThinkingLevel::High),
            session_dir: Some(dir.path().join("sessions")),
            session_file: None,
            faux_script: None,
            supervisor_link: None,
            telemetry_disabled: Some(true),
            cron_store: None,
            queued_steering_probe: None,
        })
        .unwrap();
        engine
            .create_resources
            .write()
            .unwrap()
            .rlm_model_candidates = vec![
            "role-battery/primary:high".into(),
            "role-battery/primary:high".into(),
            "role-battery/backup".into(),
            "role-battery/primary:high".into(),
        ];
        assert_eq!(
            engine.effective_thinking(),
            pa_types::ai::ModelThinkingLevel::High
        );
        engine
            .create_resources
            .write()
            .unwrap()
            .runtime_policy
            .rlm_max_depth_ceiling = Some(1);
        assert_eq!(engine.rlm_max_depth_status()["maxDepth"], 1);
        let durable = Arc::new(std::sync::Mutex::new(Vec::new()));
        let writes = durable.clone();
        *engine.act_record_sink.lock().unwrap() = Some(Arc::new(move |kind, fields| {
            writes.lock().unwrap().push((kind.to_string(), fields));
            Ok(())
        }));
        let mut events = Vec::new();
        let start = std::time::Instant::now();
        crate::agent_engine::tests::admit(&engine, "answer".into(), &mut events);
        assert!(start.elapsed() < std::time::Duration::from_secs(10));
        assert_eq!(registration.call_count(), 2);
        assert_eq!(*observed.lock().unwrap(), ["primary", "backup"]);
        let writes = durable.lock().unwrap();
        assert_eq!(writes[0].0, "model_change");
        assert_eq!(writes[0].1["modelId"], "backup");
        assert_eq!(writes[1].0, "thinking_level_change");
        drop(writes);
        assert_eq!(
            engine.selection.read().unwrap().model.as_deref(),
            Some("backup")
        );
        assert_eq!(
            std::fs::read_to_string(agent_dir.join("settings.json")).unwrap(),
            settings
        );
        let core = engine.session.blocking_lock().clone().unwrap();
        let entries = engine
            .runtime
            .block_on(async { core.session.shared_persistence().lock().await.get_entries() });
        assert!(entries.iter().any(
            |entry| matches!(entry, FileEntry::ModelChange { payload, .. }
            if payload.model_id == "backup")
        ));
        for (fails, text) in [(true, ""), (false, "primary recovered"), (true, "")] {
            let observed = observed.clone();
            registration.append_responses(vec![FauxResponseStep::Factory(Arc::new(
                move |_, _, _, model| {
                    observed.lock().unwrap().push(model.id.clone());
                    if fails {
                        let mut failed: pa_ai::types::AssistantMessage = serde_json::from_value(
                            serde_json::to_value(failure("server_error")).unwrap(),
                        )
                        .unwrap();
                        failed.provider = model.provider.clone();
                        failed.model = model.id.clone();
                        Ok(failed)
                    } else {
                        Ok(faux_assistant_text_message(
                            text,
                            FauxAssistantMessageOptions::default(),
                        ))
                    }
                },
            ))]);
        }
        crate::agent_engine::tests::admit(&engine, "recover".into(), &mut events);
        crate::agent_engine::tests::admit(&engine, "exhaust candidates".into(), &mut events);
        assert_eq!(registration.call_count(), 5);
        assert_eq!(
            *observed.lock().unwrap(),
            ["primary", "backup", "backup", "primary", "primary"]
        );
        assert_eq!(
            engine.selection.read().unwrap().model.as_deref(),
            Some("primary")
        );
        assert_eq!(
            std::fs::read_to_string(agent_dir.join("settings.json")).unwrap(),
            settings
        );
        registration.unregister();
    }
}
