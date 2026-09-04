//! Fork catalog updates layered over the upstream compiled transport templates.

use std::collections::BTreeMap;
use std::sync::LazyLock;

use serde::Deserialize;
use serde_json::Value;

use crate::types::Model;

#[derive(Deserialize)]
struct CatalogPatch {
    models: Vec<ModelPatch>,
    removed: Vec<ModelKey>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ModelPatch {
    model: Model,
    changed_fields: Vec<String>,
}

#[derive(Deserialize)]
struct ModelKey {
    provider: String,
    id: String,
}

type Catalog = BTreeMap<String, BTreeMap<String, Model>>;

fn patch() -> &'static CatalogPatch {
    static PATCH: LazyLock<CatalogPatch> = LazyLock::new(|| {
        serde_json::from_str(include_str!("models_fork.json"))
            .expect("compiled fork catalog is valid")
    });
    &PATCH
}

fn catalog() -> &'static Catalog {
    static CATALOG: LazyLock<Catalog> = LazyLock::new(|| {
        let mut catalog = Catalog::new();
        for provider in crate::models_generated::get_providers() {
            for model in crate::models_generated::get_models(provider) {
                catalog
                    .entry(provider.into())
                    .or_default()
                    .insert(model.id.clone(), model.clone());
            }
        }
        for patch in &patch().models {
            let models = catalog.entry(patch.model.provider.clone()).or_default();
            let model = if let Some(existing) = models.get(&patch.model.id) {
                let mut merged = serde_json::to_value(existing).expect("model serializes");
                let updated = serde_json::to_value(&patch.model).expect("model serializes");
                for field in &patch.changed_fields {
                    if let Some(value) = updated.get(field) {
                        merged[field] = value.clone();
                    } else if let Value::Object(fields) = &mut merged {
                        fields.remove(field);
                    }
                }
                serde_json::from_value(merged).expect("merged model is valid")
            } else {
                patch.model.clone()
            };
            models.insert(model.id.clone(), model);
        }
        for removed in &patch().removed {
            if let Some(models) = catalog.get_mut(&removed.provider) {
                models.remove(&removed.id);
            }
        }
        catalog
    });
    &CATALOG
}

/// Fill models absent from cached or bundled catalogs while keeping their newer entries.
#[must_use]
pub fn fill_missing_models(mut models: Vec<Model>) -> Vec<Model> {
    for row in &patch().models {
        if !models
            .iter()
            .any(|model| model.provider == row.model.provider && model.id == row.model.id)
        {
            if let Some(model) = get_model(&row.model.provider, &row.model.id) {
                models.push(model.clone());
            }
        }
    }
    models.retain(|model| {
        !patch()
            .removed
            .iter()
            .any(|removed| removed.provider == model.provider && removed.id == model.id)
    });
    models
}

#[must_use]
pub fn get_model(provider: &str, id: &str) -> Option<&'static Model> {
    catalog().get(provider)?.get(id)
}

#[must_use]
pub fn get_providers() -> Vec<&'static str> {
    catalog().keys().map(String::as_str).collect()
}

#[must_use]
pub fn get_models(provider: &str) -> Vec<&'static Model> {
    catalog()
        .get(provider)
        .map(|models| models.values().collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fork_providers_and_upstream_transports_are_available() {
        for provider in ["runinfra", "venice", "merge-gateway"] {
            assert!(!get_models(provider).is_empty(), "{provider}");
        }
        for provider in crate::models_generated::get_providers() {
            assert!(get_providers().contains(&provider), "{provider}");
        }
        assert!(get_model("openai", "gpt-4").is_some());
        assert!(get_model("openrouter", "ibm-granite/granite-4.1-8b").is_none());
    }

    #[test]
    fn compiled_models_have_valid_routes_and_budgets() {
        for provider in get_providers() {
            for model in get_models(provider) {
                assert_eq!(model.provider, provider);
                assert!(!model.api.is_empty());
                assert!(!model.base_url.is_empty() || model.api == "azure-openai-responses");
                assert!(model.context_window > 0);
                assert!(model.max_tokens > 0);
            }
        }
    }
}
