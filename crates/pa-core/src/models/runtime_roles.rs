use crate::settings::ModelRoleSelector;
use anyhow::{bail, Context, Result};
use pa_types::ai::ModelThinkingLevel;
use std::collections::BTreeMap;

pub const CLAUDE_CODE_RUNTIME_PREFIX: &str = "claude-code/";
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RlmRuntimeKind {
    Native,
    ClaudeCode,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RlmRuntimeCandidate {
    pub runtime: RlmRuntimeKind,
    pub selector: String,
    pub model_reference: String,
    pub thinking_level: Option<ModelThinkingLevel>,
}

/// Parse a runtime selector without looking up its model.
///
/// # Errors
/// Returns an error for empty models or unqualified native selectors.
pub fn parse_rlm_runtime_candidate(raw: &str) -> Result<RlmRuntimeCandidate> {
    let selector = raw.trim();
    if selector.is_empty() {
        bail!("RLM model role candidates must be non-empty strings");
    }
    let claude = selector.starts_with(CLAUDE_CODE_RUNTIME_PREFIX);
    let reference = selector
        .strip_prefix(CLAUDE_CODE_RUNTIME_PREFIX)
        .unwrap_or(selector)
        .trim();
    let (reference, thinking_level) = reference
        .rsplit_once(':')
        .and_then(|(reference, level)| {
            let level = match level {
                "off" if !claude => ModelThinkingLevel::Off,
                "minimal" if !claude => ModelThinkingLevel::Minimal,
                "low" => ModelThinkingLevel::Low,
                "medium" => ModelThinkingLevel::Medium,
                "high" => ModelThinkingLevel::High,
                "xhigh" => ModelThinkingLevel::Xhigh,
                "max" => ModelThinkingLevel::Max,
                _ => return None,
            };
            (!reference.is_empty()).then_some((reference, Some(level)))
        })
        .unwrap_or((reference, None));
    let reference = reference.trim();
    if claude && reference.is_empty() {
        bail!("Claude runtime selector \"{selector}\" names no model");
    }
    if !claude
        && !reference
            .split_once('/')
            .is_some_and(|(provider, model)| !provider.is_empty() && !model.is_empty())
    {
        bail!("RLM model role candidate \"{selector}\" must be provider-qualified");
    }
    Ok(RlmRuntimeCandidate {
        runtime: if claude {
            RlmRuntimeKind::ClaudeCode
        } else {
            RlmRuntimeKind::Native
        },
        selector: if claude {
            format!("{CLAUDE_CODE_RUNTIME_PREFIX}{reference}")
        } else {
            reference.into()
        },
        model_reference: reference.into(),
        thinking_level,
    })
}

/// Resolve the ordered candidates for one role and reject mixed runtimes.
///
/// # Errors
/// Returns an error for missing, empty, malformed, or mixed-runtime roles.
pub fn resolve_rlm_role_candidates(
    role: &str,
    roles: &BTreeMap<String, ModelRoleSelector>,
) -> Result<Vec<RlmRuntimeCandidate>> {
    let value = roles
        .get(role)
        .with_context(|| format!("Unknown RLM model role \"@{role}\""))?;
    let selectors: Vec<&str> = match value {
        ModelRoleSelector::Single(value) => vec![value],
        ModelRoleSelector::Ordered(values) => values.iter().map(String::as_str).collect(),
    };
    if selectors.is_empty() {
        bail!("RLM model role \"@{role}\" has no candidates");
    }
    let candidates: Vec<_> = selectors
        .into_iter()
        .map(parse_rlm_runtime_candidate)
        .collect::<Result<_>>()?;
    if candidates
        .iter()
        .any(|candidate| candidate.runtime != candidates[0].runtime)
    {
        bail!("RLM model role \"@{role}\" mixes native and claude-code runtime candidates");
    }
    Ok(candidates)
}

/// Resolve configured CLI roles to native candidates with configured credentials, in preference order.
/// Returns an error for a provider override, a Claude Code role, or an unavailable role.
pub fn resolve_cli_role(provider: Option<&str>, selector: &str, registry: &super::ModelRegistry, roles: &BTreeMap<String, ModelRoleSelector>) -> Result<Vec<super::ScopedModel>> {
    if provider.is_some() { bail!("--provider cannot override a named model role"); }
    let candidates = resolve_rlm_role_candidates(selector.trim_start_matches('@'), roles)?;
    if candidates[0].runtime != RlmRuntimeKind::Native { bail!("Model role \"{selector}\" requires the Claude Code subagent runtime"); }
    let models: Vec<_> = candidates.into_iter().filter_map(|candidate| {
        super::find_exact_model_reference_match(&candidate.model_reference, registry.get_all()).map(|model| super::ScopedModel { model: model.clone(), thinking_level: candidate.thinking_level.map(|level| serde_json::from_value(serde_json::json!(level)).expect("matching thinking level vocabulary")) })
    }).collect();
    let first = models.iter().position(|candidate| registry.has_configured_auth(&candidate.model)).with_context(|| format!("Model role \"{selector}\" has no available candidates"))?;
    Ok(models.into_iter().skip(first).collect())
}
