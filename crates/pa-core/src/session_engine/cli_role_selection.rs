use super::{provider_adapter::ProviderTarget, AgentSession};
use crate::models::{ModelRegistry, ScopedModel};
use std::sync::{Arc, RwLock};

#[derive(Clone)]
pub(super) struct CliRoleSelection {
    candidates: Vec<ScopedModel>,
    cursor: usize,
    target: Arc<RwLock<Option<ProviderTarget>>>,
    stale_sources: Vec<crate::auth::AuthSourceToken>,
}

impl AgentSession {
    /// Keep a named CLI role's ordered candidates and serving target for the session lifetime.
    pub fn configure_cli_role_candidates(
        &self,
        candidates: Vec<ScopedModel>,
        target: Arc<RwLock<Option<ProviderTarget>>>,
    ) {
        *self
            .cli_role_selection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(CliRoleSelection {
            candidates,
            cursor: 0,
            target,
            stale_sources: vec![],
        });
    }

    /// The request target currently serving a configured CLI role, including an admitted fallback.
    #[must_use]
    pub fn cli_role_target(&self) -> Option<ProviderTarget> {
        self.cli_role_selection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .and_then(|role| {
                role.target
                    .read()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .as_ref()
                    .cloned()
            })
    }

    /// Advance an empty failed request to the next executable named-role candidate.
    /// Failed rows remain durable; only the live request context removes them.
    /// # Errors
    /// Returns model conversion, allowlist, or session persistence failures.
    #[tracing::instrument(skip_all)]
    pub async fn advance_cli_role_candidate(&self) -> anyhow::Result<bool> {
        let Some(mut role) = self
            .cli_role_selection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
        else {
            return Ok(false);
        };
        let Some(context) = &self.auxiliary_model else {
            return Ok(false);
        };
        let settings = crate::settings::SettingsManager::create(&context.cwd, &context.agent_dir);
        if !settings.get_provider_retry_policy().enabled {
            return Ok(false);
        }
        let state = self.agent.state().await;
        let Some(pa_types::session::AgentMessage::Assistant(message)) =
            self.last_assistant_message().await
        else {
            return Ok(false);
        };
        let failed: pa_agent::types::AssistantMessage =
            serde_json::from_value(serde_json::to_value(message)?)?;
        if failed.provider != state.model.provider
            || failed.model != state.model.id
            || !super::role_fallback::can_advance_role_candidate(
                &failed,
                state.model.context_window,
            )
        {
            return Ok(false);
        }
        let current = role
            .target
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let Some(current) = current else {
            return Ok(false);
        };
        if current.model.provider != failed.provider || current.model.id != failed.model {
            return Ok(false);
        }
        let mut registry = ModelRegistry::create(
            crate::auth::AuthStorage::create(&context.agent_dir),
            context.agent_dir.join("models.json"),
        );
        registry.load_private_authorization_from_cache();
        for token in &role.stale_sources {
            registry.auth.mark_auth_source_stale(token.clone());
        }
        if super::provider_retry::provider_stream_failure_kind(&failed).as_deref() == Some("auth")
            && matches!(
                super::provider_retry::provider_stream_failure_status(&failed),
                Some(401 | 403)
            )
        {
            let source = registry
                .auth
                .get_api_key_with_source_token(&failed.provider, /*include_fallback*/ true);
            if let Some(token) = source
                .source_token
                .filter(|_| source.api_key == current.api_key)
            {
                registry.auth.mark_auth_source_stale(token.clone());
                if !role.stale_sources.contains(&token) {
                    role.stale_sources.push(token);
                }
            }
        }
        let executable = registry.get_executable_models().await;
        let mut next = None;
        for (index, candidate) in role.candidates.iter().enumerate().skip(role.cursor + 1) {
            if candidate.model.provider == failed.provider && candidate.model.id == failed.model {
                continue;
            }
            let Some(model) = executable.iter().find(|model| {
                model.provider == candidate.model.provider && model.id == candidate.model.id
            }) else {
                continue;
            };
            let selector = format!("{}/{}", model.provider, model.id);
            anyhow::ensure!(
                settings
                    .get_allowed_models()
                    .is_none_or(|allowlist| crate::models::model_allowed(&selector, &allowlist)),
                "Model {selector} is not allowed"
            );
            let auth = registry.get_api_key_and_headers(model, model.headers.as_ref());
            if auth.ok {
                next = Some((index, candidate.clone(), model.clone(), auth));
                break;
            }
        }
        let Some((index, candidate, mut model, auth)) = next else {
            *self
                .cli_role_selection
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(role);
            return Ok(false);
        };
        let level = candidate.thinking_level.unwrap_or(state.thinking_level);
        let shared_level = super::provider_adapter::model_thinking_level(level);
        let shared_level = if candidate.thinking_level.is_some() {
            shared_level
        } else {
            pa_types::ai::clamp_thinking_level(&model, shared_level)
        };
        if candidate.thinking_level.is_some() && model.reasoning {
            model
                .thinking_level_map
                .get_or_insert_with(std::collections::BTreeMap::new)
                .insert(shared_level, Some(shared_level.wire_name().into()));
        }
        let level = serde_json::from_value(serde_json::to_value(shared_level)?)?;
        let agent_model = serde_json::from_value(serde_json::to_value(&model)?)?;
        {
            let mut store = self.session.lock().await;
            store.append_model_change(&model.provider, &model.id)?;
            store.append_thinking_level_change(shared_level.wire_name())?;
        }
        self.agent.mutate_messages(|messages| messages.retain(|message| !matches!(super::standard_message(message), Some(pa_agent::types::Message::Assistant(assistant)) if assistant == &failed))).await;
        self.agent
            .set_model_and_thinking_level(agent_model, level)
            .await;
        let tier = if current.service_tier == Some(pa_types::ai::ServiceTier::Priority)
            && !pa_types::ai::supports_fast_mode(&model)
        {
            Some(pa_types::ai::ServiceTier::Default)
        } else {
            current.service_tier
        };
        *role
            .target
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(ProviderTarget {
            model,
            api_key: auth.api_key,
            headers: auth.headers,
            service_tier: tier,
        });
        role.cursor = index;
        *self
            .cli_role_selection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(role);
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pa_agent::agent::{Agent, AgentInitialState, AgentOptions};
    use serde_json::json;

    #[tokio::test]
    async fn role_advances_once_and_preserves_the_durable_failure() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("models.json"), json!({"providers":{"role-fixture":{"baseUrl":"http://127.0.0.1:1","apiKey":"fixture-key","api":"openai-completions","models":[{"id":"primary"},{"id":"backup"}]}}}).to_string()).unwrap();
        let registry = ModelRegistry::create(
            crate::auth::AuthStorage::create(dir.path()),
            dir.path().join("models.json"),
        );
        let candidates = ["primary", "backup"]
            .map(|id| ScopedModel {
                model: registry
                    .get_all()
                    .iter()
                    .find(|model| model.provider == "role-fixture" && model.id == id)
                    .unwrap()
                    .clone(),
                thinking_level: Some(pa_agent::types::ThinkingLevel::Off),
            })
            .to_vec();
        let failed = json!({"role":"assistant","content":[],"api":"openai-completions","provider":"role-fixture","model":"primary","usage":pa_agent::types::Usage::zero(),"stopReason":"error","errorMessage":"temporary provider failure","timestamp":12});
        let mut store = crate::session::manager::SessionManager::in_memory(dir.path());
        store
            .append_message(serde_json::from_value(failed.clone()).unwrap())
            .unwrap();
        let agent = Arc::new(Agent::new(AgentOptions {
            initial_state: AgentInitialState {
                model: Some(
                    serde_json::from_value(serde_json::to_value(&candidates[0].model).unwrap())
                        .unwrap(),
                ),
                messages: Some(vec![serde_json::from_value(failed).unwrap()]),
                ..Default::default()
            },
            ..Default::default()
        }));
        let mut session = AgentSession::new(agent.clone(), store, vec![])
            .await
            .unwrap();
        session.auxiliary_model = Some(super::super::auxiliary_model::AuxiliaryModelContext {
            cwd: dir.path().into(),
            agent_dir: dir.path().into(),
        });
        let original = session.entries().await;
        let expected = candidates[1].model.clone();
        session.configure_cli_role_candidates(
            candidates.clone(),
            Arc::new(RwLock::new(Some(ProviderTarget {
                model: candidates[0].model.clone(),
                api_key: Some("fixture-key".into()),
                headers: None,
                service_tier: Some(pa_types::ai::ServiceTier::Priority),
            }))),
        );
        assert!(session.advance_cli_role_candidate().await.unwrap());
        assert!(!session.advance_cli_role_candidate().await.unwrap());
        assert!(agent.state().await.messages.is_empty());
        let target = session.cli_role_target().unwrap();
        assert_eq!(
            (target.model, target.api_key, target.service_tier),
            (
                expected,
                Some("fixture-key".into()),
                Some(pa_types::ai::ServiceTier::Default)
            )
        );
        assert_eq!(
            &session.entries().await[..original.len()],
            original.as_slice()
        );
    }
}
