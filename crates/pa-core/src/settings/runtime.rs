use super::{CompactionStrategy, ModelRoleSelector, SettingsManager};
use anyhow::{bail, Context, Result};
use pa_types::ai::ServiceTier;
use std::collections::BTreeMap;

impl SettingsManager {
    #[must_use]
    pub fn get_rlm_allowed_service_tiers(&self) -> Vec<ServiceTier> {
        self.settings()
            .rlm_allowed_service_tiers
            .clone()
            .unwrap_or_else(|| vec![self.get_default_service_tier()])
    }

    /// # Errors
    /// Returns an error if the configured depth is not a non-negative integer.
    pub fn get_rlm_act_max_depth(&self) -> Result<u64> {
        self.settings()
            .rlm_act_max_depth
            .as_ref()
            .map_or(Ok(1), |value| {
                value
                    .as_u64()
                    .filter(|depth| *depth <= 9_007_199_254_740_991)
                    .context("rlmActMaxDepth must be a non-negative integer")
            })
    }

    /// # Errors
    /// Returns an error for invalid depth, selector shape, or empty array entries.
    pub fn get_rlm_act_default_model(&self, depth: usize) -> Result<Option<String>> {
        if depth == 0 {
            bail!("Act depth must be a positive integer");
        }
        let Some(value) = &self.settings().rlm_act_default_model else {
            return Ok(None);
        };
        if let Some(selector) = value.as_str() {
            return Ok(
                (depth == 1 && !selector.trim().is_empty()).then(|| selector.trim().to_string())
            );
        }
        let values = value
            .as_array()
            .context("rlmActDefaultModel must be a string or array of strings")?;
        let values: Vec<String> = values
            .iter()
            .map(|value| {
                value
                    .as_str()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string)
                    .context("rlmActDefaultModel entries must be non-empty strings")
            })
            .collect::<Result<_>>()?;
        Ok(values.get(depth - 1).cloned())
    }

    /// # Errors
    /// Returns an error when global Codex homes are not non-empty directory strings.
    pub fn get_codex_homes(&self) -> Result<Vec<String>> {
        let Some(value) = &self.global_settings().codex_homes else {
            return Ok(Vec::new());
        };
        value
            .as_array()
            .context("codexHomes must be an array of directory paths")?
            .iter()
            .map(|value| {
                value
                    .as_str()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string)
                    .context("codexHomes entries must be non-empty strings")
            })
            .collect()
    }

    #[must_use]
    pub fn get_model_roles(&self) -> BTreeMap<String, ModelRoleSelector> {
        self.settings().model_roles.clone().unwrap_or_default()
    }
    #[must_use]
    pub fn get_model_role(&self, role: &str) -> Option<ModelRoleSelector> {
        self.settings().model_roles.as_ref()?.get(role).cloned()
    }

    /// # Errors
    /// Returns an error if the global settings document cannot be saved.
    pub fn set_model_roles(&mut self, roles: BTreeMap<String, ModelRoleSelector>) -> Result<()> {
        self.global_mut().model_roles = Some(roles);
        self.save_global_scope()
    }

    #[must_use]
    pub fn get_claude_code_executable(&self) -> Option<String> {
        self.settings()
            .claude_code
            .as_ref()?
            .executable
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    }
    #[must_use]
    pub fn get_open_router_responses(&self) -> bool {
        self.settings()
            .open_router
            .as_ref()
            .and_then(|value| value.responses)
            .unwrap_or(false)
    }
    #[must_use]
    pub fn get_session_elapsed_time_enabled(&self) -> bool {
        self.settings()
            .session_elapsed_time
            .as_ref()
            .and_then(|value| value.enabled)
            .unwrap_or(false)
    }
    #[must_use]
    pub fn get_agents_view_usage_enabled(&self) -> bool {
        self.settings()
            .agents_view_usage
            .as_ref()
            .and_then(|value| value.enabled)
            .unwrap_or(false)
    }
    #[must_use]
    pub fn get_compaction_native(&self) -> bool {
        self.settings()
            .compaction
            .as_ref()
            .and_then(|value| value.native)
            .unwrap_or(true)
    }
    #[must_use]
    pub fn get_compaction_strategy(&self) -> CompactionStrategy {
        self.settings()
            .compaction
            .as_ref()
            .and_then(|value| value.strategy)
            .unwrap_or(CompactionStrategy::Default)
    }
    #[must_use]
    pub fn get_compaction_trigger_context_tokens(&self) -> Option<f64> {
        self.settings()
            .compaction
            .as_ref()?
            .trigger_context_tokens
            .filter(|value| value.is_finite() && *value > 0.0)
    }
    #[must_use]
    pub fn get_scratch_handoff_settings(&self) -> (bool, String) {
        let value = self.settings().scratch_handoff.as_ref();
        (
            value.and_then(|value| value.enabled).unwrap_or(false),
            value
                .and_then(|value| value.root_dir.as_deref())
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .unwrap_or("agent")
                .to_string(),
        )
    }
}
