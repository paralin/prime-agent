use std::collections::HashSet;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use pa_types::ai::Model;
use serde_json::{json, Value};

pub const CATALOG_BASE_URL: &str = "https://api-gateway.merge.dev/v1";
const CHAT_BASE_URL: &str = "https://api-gateway.merge.dev/v1/ai-sdk";
const EFFORTS: [&str; 6] = ["minimal", "low", "medium", "high", "xhigh", "max"];

fn contains(value: &Value, item: &str) -> bool {
    value
        .as_array()
        .is_some_and(|items| items.iter().any(|value| value.as_str() == Some(item)))
}

fn minimum(routes: &[&Value], field: &str) -> Option<u64> {
    routes
        .iter()
        .filter_map(|route| route[field].as_u64().filter(|value| *value > 0))
        .min()
}

fn name(id: &str) -> String {
    id.rsplit('/')
        .next()
        .unwrap_or(id)
        .split('-')
        .map(|part| {
            let mut chars = part.chars();
            chars.next().map_or_else(String::new, |first| {
                first.to_uppercase().collect::<String>() + chars.as_str()
            })
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Parse legacy ids and the current vendor catalog, intersecting route capabilities.
///
/// # Errors
/// Returns an error for malformed catalogs or model records.
pub fn parse_merge_gateway_models(payload: &Value, known: &[Model]) -> Result<Vec<Model>> {
    let entries = payload
        .get("data")
        .and_then(Value::as_array)
        .context("Merge Gateway model catalog response is invalid")?;
    let mut models = Vec::new();
    for entry in entries {
        let legacy = entry.get("id").and_then(Value::as_str);
        let id = legacy
            .or_else(|| entry.get("model").and_then(Value::as_str))
            .context("Merge Gateway model catalog response is invalid")?;
        let routes = if legacy.is_some() {
            None
        } else {
            let vendors = entry
                .get("vendors")
                .and_then(Value::as_object)
                .context("Merge Gateway model catalog response is invalid")?;
            let routes: Vec<_> = vendors
                .values()
                .filter(|vendor| {
                    vendor["availability_status"].as_str() != Some("unavailable")
                        && vendor["capabilities"]["supports_tool_calling"] == true
                        && contains(&vendor["capabilities"]["input"], "text")
                        && contains(&vendor["capabilities"]["output"], "text")
                })
                .collect();
            if routes.is_empty() {
                continue;
            }
            Some(routes)
        };
        let template = known
            .iter()
            .find(|model| model.id == id && model.provider == "merge-gateway")
            .or_else(|| known.iter().find(|model| model.id == id));
        let mut model = json!({
            "id": id, "name": template.map_or_else(|| name(id), |model| model.name.clone()),
            "api": "openai-completions", "provider": "merge-gateway", "baseUrl": CHAT_BASE_URL,
            "reasoning": template.is_none_or(|model| model.reasoning),
            "input": template.map_or_else(|| json!(["text"]), |model| json!(model.input)),
            "cost": template.map_or_else(|| json!({"input":0,"output":0,"cacheRead":0,"cacheWrite":0}), |model| json!(model.cost)),
            "contextWindow": template.map_or(128_000, |model| model.context_window),
            "maxTokens": template.map_or(16_384, |model| model.max_tokens),
            "compat": {"reasoningField":"thinking","requireFinishReason":true,"supportsStore":true,
                "supportsDeveloperRole":false,"supportsReasoningEffort":true,"maxTokensField":"max_tokens",
                "thinkingFormat":"merge","sendSessionAffinityHeaders":["x-session-affinity","X-Session-Id"]}
        });
        let mut thinking = template
            .filter(|model| model.provider == "merge-gateway")
            .and_then(|model| model.thinking_level_map.as_ref())
            .map(|map| json!(map));
        let normalized = id.to_ascii_lowercase();
        if thinking.is_none()
            && (normalized.contains("glm-5.3-flash") || normalized.contains("glm-5-3-flash"))
        {
            thinking = Some(
                json!({"off":null,"minimal":null,"low":"low","medium":null,"high":"high","xhigh":null,"max":"max"}),
            );
        }
        if let Some(routes) = &routes {
            if let Some(display) = entry.get("display_name").and_then(Value::as_str) {
                model["name"] = json!(display);
            }
            let reasoning = routes.iter().all(|route| route["capabilities"]["supports_reasoning"] == true);
            let thinking_budget = reasoning && routes.iter().all(|route| {
                let controls = &route["capabilities"]["reasoning"]["controls"];
                contains(controls, "thinking") || contains(controls, "thinking.budget_tokens")
            });
            model["compat"]["thinkingFormat"] = json!(if thinking_budget { "merge" } else { "openai" });
            let disable = thinking_budget
                && routes
                    .iter()
                    .all(|route| route["capabilities"]["reasoning"]["disable_supported"] == true);
            let efforts: Vec<_> = EFFORTS
                .into_iter()
                .filter(|effort| {
                    reasoning
                        && routes.iter().all(|route| {
                            let reasoning = &route["capabilities"]["reasoning"];
                            contains(&reasoning["controls"], "reasoning_effort")
                                && contains(&reasoning["effort_values"], effort)
                        })
                })
                .collect();
            model["reasoning"] = json!(reasoning);
            model["input"] = if routes
                .iter()
                .all(|route| contains(&route["capabilities"]["input"], "image"))
            {
                json!(["text", "image"])
            } else {
                json!(["text"])
            };
            if let Some(value) = minimum(routes, "context_window") {
                model["contextWindow"] = json!(value);
            }
            if let Some(value) = minimum(routes, "max_output_tokens") {
                model["maxTokens"] = json!(value);
            }
            model["compat"]["supportsReasoningEffort"] = json!(!efforts.is_empty());
            if !reasoning {
                thinking = None;
            } else if thinking.is_some() || !efforts.is_empty() || !disable {
                let map = thinking.get_or_insert_with(|| json!({}));
                if !disable && map.get("off").is_none() {
                    map["off"] = Value::Null;
                }
                if !efforts.is_empty() {
                    for effort in EFFORTS {
                        let mapped = map.get(effort).and_then(Value::as_str).unwrap_or(effort);
                        map[effort] = if efforts.contains(&mapped) {
                            json!(mapped)
                        } else {
                            Value::Null
                        };
                    }
                    if map
                        .get("off")
                        .and_then(Value::as_str)
                        .is_some_and(|mapped| !efforts.contains(&mapped))
                    {
                        map["off"] = Value::Null;
                    }
                }
            }
        }
        if let Some(thinking) = thinking {
            model["thinkingLevelMap"] = thinking;
        }
        models.push(serde_json::from_value(model)?);
    }
    Ok(models)
}

/// Fetch every catalog page with a five-second timeout for each request.
///
/// # Errors
/// Returns an error for transport failures, HTTP failures, malformed pages, or repeated cursors.
pub async fn fetch_merge_gateway_models(
    base_url: &str,
    api_key: &str,
    known: &[Model],
) -> Result<Vec<Model>> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()?;
    let mut cursor: Option<String> = None;
    let mut seen = HashSet::new();
    let mut models = Vec::new();
    loop {
        let mut url = url::Url::parse(&format!("{}/models", base_url.trim_end_matches('/')))?;
        url.query_pairs_mut().append_pair("limit", "500");
        if let Some(cursor) = &cursor {
            url.query_pairs_mut().append_pair("cursor", cursor);
        }
        let response = client.get(url).bearer_auth(api_key).send().await?;
        if !response.status().is_success() {
            bail!(
                "Merge Gateway model discovery failed with HTTP {}",
                response.status()
            );
        }
        let bytes = response.bytes().await?;
        anyhow::ensure!(
            bytes.len() <= 2 * 1024 * 1024,
            "Merge Gateway catalog response is too large"
        );
        let payload: Value = serde_json::from_slice(&bytes)?;
        models.extend(parse_merge_gateway_models(&payload, known)?);
        if payload["has_more"] != true {
            break;
        }
        let next = payload["next_cursor"].as_str().map_or("", str::trim);
        if next.is_empty() {
            break;
        }
        anyhow::ensure!(
            seen.insert(next.to_string()),
            "Merge Gateway model catalog response repeated its pagination cursor"
        );
        cursor = Some(next.to_string());
    }
    Ok(models)
}

#[derive(Clone)]
struct CachedCatalog {
    models: Vec<Model>,
    refreshed: std::time::Instant,
}

type CatalogCache = std::sync::Mutex<std::collections::HashMap<String, CachedCatalog>>;
fn cache() -> &'static CatalogCache {
    static CACHE: std::sync::OnceLock<CatalogCache> = std::sync::OnceLock::new();
    CACHE.get_or_init(CatalogCache::default)
}

pub(super) fn cache_key(scope: Option<&std::path::Path>, key: &str) -> String {
    use sha2::{Digest, Sha256};
    format!(
        "{}:{:x}",
        scope.map_or_else(String::new, |path| path.to_string_lossy().into_owned()),
        Sha256::digest(key.as_bytes())
    )
}

pub(super) fn cached(key: &str) -> Option<Vec<Model>> {
    cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(key)
        .map(|entry| entry.models.clone())
}

pub(super) fn fresh(key: &str) -> bool {
    cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(key)
        .is_some_and(|entry| entry.refreshed.elapsed() < Duration::from_secs(300))
}

pub(super) fn store(key: String, models: Vec<Model>) {
    cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(
            key,
            CachedCatalog {
                models,
                refreshed: std::time::Instant::now(),
            },
        );
}
