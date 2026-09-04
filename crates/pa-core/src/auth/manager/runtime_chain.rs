use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

use super::{fingerprint, AuthSource, AuthSourceCandidate, AuthSourceToken, AuthStorage};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuntimeApiKeyChainCredential {
    pub key: String,
    pub label: Option<String>,
}

#[derive(Default)]
struct ChainData {
    credentials: HashMap<String, Vec<RuntimeApiKeyChainCredential>>,
    stale: HashMap<String, Vec<AuthSourceToken>>,
}

#[derive(Default)]
pub struct RuntimeApiKeyChainState {
    data: Mutex<ChainData>,
}

impl RuntimeApiKeyChainState {
    pub(super) fn has_chain(&self, provider: &str) -> bool {
        self.data
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .credentials
            .contains_key(provider)
    }

    pub(super) fn is_stale(&self, token: &AuthSourceToken) -> bool {
        self.data
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .stale
            .get(&token.provider)
            .is_some_and(|tokens| tokens.contains(token))
    }

    pub(super) fn mark_stale(&self, token: AuthSourceToken) -> bool {
        let mut data = self
            .data
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let tokens = data.stale.entry(token.provider.clone()).or_default();
        if tokens.contains(&token) {
            return false;
        }
        tokens.push(token);
        true
    }

    pub(super) fn clear_stale(&self, provider: &str) {
        self.data
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .stale
            .remove(provider);
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct CodexHomeAuthResult {
    pub configured_homes: Vec<PathBuf>,
    pub loaded_homes: Vec<PathBuf>,
}

impl AuthStorage {
    pub fn set_runtime_api_key_chain_state(&mut self, state: Arc<RuntimeApiKeyChainState>) {
        self.runtime_chain = state;
    }

    /// # Errors
    /// Returns an error if an entry has an empty key.
    pub fn set_runtime_api_key_chain(
        &mut self,
        provider: &str,
        credentials: Vec<RuntimeApiKeyChainCredential>,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            credentials.iter().all(|entry| !entry.key.trim().is_empty()),
            "runtime API key chain entries must contain a non-empty key"
        );
        let credentials: Vec<_> = credentials
            .into_iter()
            .filter_map(|credential| {
                let key = credential.key.trim().to_string();
                if key.is_empty() {
                    return None;
                }
                Some(RuntimeApiKeyChainCredential {
                    key,
                    label: credential
                        .label
                        .map(|label| label.trim().to_string())
                        .filter(|label| !label.is_empty()),
                })
            })
            .collect();
        let mut data = self
            .runtime_chain
            .data
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        data.stale.remove(provider);
        if credentials.is_empty() {
            data.credentials.remove(provider);
        } else {
            data.credentials.insert(provider.to_string(), credentials);
        }
        Ok(())
    }

    #[must_use]
    pub fn has_runtime_api_key_chain(&self, provider: &str) -> bool {
        self.runtime_chain.has_chain(provider)
    }

    pub fn remove_runtime_api_key_chain(&mut self, provider: &str) {
        let mut data = self
            .runtime_chain
            .data
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        data.credentials.remove(provider);
        data.stale.remove(provider);
    }

    pub fn mark_runtime_api_key_chain_stale(&mut self, provider: &str, rejected_key: &str) -> bool {
        let candidate =
            self.runtime_chain_candidates(provider)
                .into_iter()
                .find(|(credential, candidate)| {
                    credential.key == rejected_key && !self.is_stale(provider, candidate)
                });
        candidate
            .and_then(|(_, candidate)| Self::token_for(provider, &candidate))
            .is_some_and(|token| self.mark_auth_source_stale(token))
    }

    pub(super) fn runtime_chain_candidates(
        &self,
        provider: &str,
    ) -> Vec<(RuntimeApiKeyChainCredential, AuthSourceCandidate)> {
        let data = self
            .runtime_chain
            .data
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        data.credentials
            .get(provider)
            .into_iter()
            .flatten()
            .enumerate()
            .map(|(index, credential)| {
                let identity = format!(
                    "runtime-chain:{index}:{}",
                    credential.label.as_deref().unwrap_or_default()
                );
                (
                    credential.clone(),
                    AuthSourceCandidate {
                        source: AuthSource::RuntimeChain,
                        configured: false,
                        label: Some(
                            credential
                                .label
                                .clone()
                                .unwrap_or_else(|| format!("credential {}", index + 1)),
                        ),
                        identity_fingerprint: fingerprint(AuthSource::RuntimeChain, &identity),
                        value_fingerprint: Some(fingerprint(
                            AuthSource::RuntimeChain,
                            &credential.key,
                        )),
                        resolve_value_fingerprint: None,
                    },
                )
            })
            .collect()
    }

    /// Load access tokens without refreshing or changing Codex CLI files.
    ///
    /// # Errors
    /// Returns an error when the process working directory cannot be read.
    pub fn apply_codex_homes(&mut self, homes: &[String]) -> std::io::Result<CodexHomeAuthResult> {
        let cwd = std::env::current_dir()?;
        let home = pa_types::platform::home_dir();
        let mut configured_homes = Vec::new();
        let mut loaded_homes = Vec::new();
        let mut credentials = Vec::new();
        for value in homes {
            let value = value.trim();
            let path = if value == "~" {
                home.clone().unwrap_or_else(|| PathBuf::from(value))
            } else if let Some(suffix) = value.strip_prefix("~/") {
                home.as_ref()
                    .map_or_else(|| PathBuf::from(value), |home| home.join(suffix))
            } else {
                PathBuf::from(value)
            };
            let path = normalize_path(&if path.is_absolute() {
                path
            } else {
                cwd.join(path)
            });
            if configured_homes.contains(&path) {
                continue;
            }
            configured_homes.push(path.clone());
            let key = std::fs::read_to_string(path.join("auth.json"))
                .ok()
                .and_then(|content| serde_json::from_str::<serde_json::Value>(&content).ok())
                .and_then(|value| {
                    value
                        .get("tokens")?
                        .get("access_token")?
                        .as_str()
                        .map(str::trim)
                        .filter(|key| !key.is_empty())
                        .map(str::to_string)
                });
            if let Some(key) = key {
                loaded_homes.push(path.clone());
                credentials.push(RuntimeApiKeyChainCredential {
                    key,
                    label: Some(path.to_string_lossy().into_owned()),
                });
            }
        }
        let unchanged = self
            .runtime_chain
            .data
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .credentials
            .get("openai-codex")
            .is_some_and(|existing| *existing == credentials);
        if !unchanged {
            self.set_runtime_api_key_chain("openai-codex", credentials)
                .map_err(std::io::Error::other)?;
        }
        Ok(CodexHomeAuthResult {
            configured_homes,
            loaded_homes,
        })
    }
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                result.pop();
            }
            other => result.push(other.as_os_str()),
        }
    }
    result
}

pub(super) fn shared_chain(agent_dir: &Path) -> Arc<RuntimeApiKeyChainState> {
    static STATES: std::sync::OnceLock<Mutex<HashMap<PathBuf, Arc<RuntimeApiKeyChainState>>>> =
        std::sync::OnceLock::new();
    let states = STATES.get_or_init(Mutex::default);
    let path = std::fs::canonicalize(agent_dir).unwrap_or_else(|_| agent_dir.to_path_buf());
    states
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .entry(path)
        .or_default()
        .clone()
}
