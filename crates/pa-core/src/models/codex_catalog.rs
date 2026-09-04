use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use pa_types::ai::Model;
use serde_json::Value;

/// # Errors
/// Returns an error for malformed model ids or cursors.
pub fn parse_codex_model_page(payload: &Value) -> Result<(HashSet<String>, Option<String>)> {
    let models = payload
        .get("models")
        .and_then(Value::as_array)
        .context("Invalid OpenAI Codex model catalog")?;
    let ids = models
        .iter()
        .map(|model| {
            model
                .get("slug")
                .and_then(Value::as_str)
                .map(str::to_string)
                .context("Invalid OpenAI Codex model catalog")
        })
        .collect::<Result<_>>()?;
    let cursor = match payload.get("next_cursor") {
        None | Some(Value::Null) => None,
        Some(Value::String(cursor)) => {
            (!cursor.trim().is_empty()).then(|| cursor.trim().to_string())
        }
        _ => anyhow::bail!("Invalid OpenAI Codex model catalog cursor"),
    };
    Ok((ids, cursor))
}

/// # Errors
/// Returns an error for invalid URLs, HTTP failures, malformed pages, or repeated cursors.
pub async fn fetch_codex_model_ids(
    base_url: &str,
    api_key: &str,
    account_id: &str,
    headers: Option<&std::collections::BTreeMap<String, String>>,
) -> Result<HashSet<String>> {
    let base = base_url.trim_end_matches('/');
    let path = if base.ends_with("/codex/responses") {
        format!("{}/models", base.trim_end_matches("/responses"))
    } else if base.ends_with("/codex") {
        format!("{base}/models")
    } else {
        format!("{base}/codex/models")
    };
    let mut url = url::Url::parse(&path)?;
    url.query_pairs_mut()
        .append_pair("client_version", "0.147.0");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()?;
    let mut ids = HashSet::new();
    let mut cursors = HashSet::new();
    loop {
        let mut request = client.get(url.clone());
        if let Some(headers) = headers {
            for (name, value) in headers {
                request = request.header(name, value);
            }
        }
        let response = request
            .bearer_auth(api_key)
            .header("chatgpt-account-id", account_id)
            .header("originator", "pi")
            .send()
            .await?;
        anyhow::ensure!(
            response.status().is_success(),
            "OpenAI Codex model discovery failed with HTTP {}",
            response.status()
        );
        let bytes = response.bytes().await?;
        anyhow::ensure!(
            bytes.len() <= 2 * 1024 * 1024,
            "OpenAI Codex catalog response is too large"
        );
        let (page, cursor) = parse_codex_model_page(&serde_json::from_slice(&bytes)?)?;
        ids.extend(page);
        let Some(cursor) = cursor else {
            break;
        };
        anyhow::ensure!(
            cursors.insert(cursor.clone()),
            "OpenAI Codex model discovery repeated its pagination cursor"
        );
        url.query_pairs_mut()
            .clear()
            .append_pair("client_version", "0.147.0")
            .append_pair("cursor", &cursor);
    }
    Ok(ids)
}

struct CachedIds {
    ids: HashSet<String>,
    refreshed: Instant,
}
fn cache() -> &'static Mutex<HashMap<String, CachedIds>> {
    static CACHE: OnceLock<Mutex<HashMap<String, CachedIds>>> = OnceLock::new();
    CACHE.get_or_init(Mutex::default)
}

pub(super) async fn executable_models(
    models: Vec<Model>,
    key: Option<String>,
    headers: Option<&std::collections::BTreeMap<String, String>>,
) -> Vec<Model> {
    let Some(seed) = models.iter().find(|model| model.provider == "openai-codex") else {
        return models;
    };
    let Some(key) = key else {
        return filter(models, None);
    };
    let cache_key =
        super::merge_gateway::cache_key(Some(std::path::Path::new(&seed.base_url)), &key);
    let cached = cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&cache_key)
        .filter(|entry| entry.refreshed.elapsed() < Duration::from_secs(300))
        .map(|entry| entry.ids.clone());
    if let Some(ids) = cached {
        return filter(models, Some(&ids));
    }
    let Ok(account) = pa_ai::oauth::read_openai_codex_account_id(&key) else {
        return filter(models, None);
    };
    match fetch_codex_model_ids(&seed.base_url, &key, &account, headers).await {
        Ok(ids) => {
            cache()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(
                    cache_key,
                    CachedIds {
                        ids: ids.clone(),
                        refreshed: Instant::now(),
                    },
                );
            filter(models, Some(&ids))
        }
        Err(_) => filter(models, None),
    }
}

fn filter(models: Vec<Model>, ids: Option<&HashSet<String>>) -> Vec<Model> {
    models
        .into_iter()
        .filter(|model| {
            model.provider != "openai-codex"
                || ids.is_some_and(|ids| ids.is_empty() || ids.contains(&model.id))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_catalog_keeps_bootstrap_but_missing_auth_filters_codex() {
        let models: Vec<_> = pa_ai::fork_catalog::get_models("openai-codex")
            .into_iter()
            .cloned()
            .collect();
        assert!(!models.is_empty());
        assert_eq!(
            filter(models.clone(), Some(&HashSet::new())).len(),
            models.len()
        );
        assert!(filter(models.clone(), None).is_empty());
        let selected = HashSet::from([models[0].id.clone()]);
        assert_eq!(filter(models, Some(&selected)).len(), 1);
    }
}
