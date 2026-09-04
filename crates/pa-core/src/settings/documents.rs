use anyhow::{bail, Result};
use serde_json::Value;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsFileFormat {
    Json,
    Yaml,
}

/// Resolve the unique document; new settings use JSON.
///
/// # Errors
/// Returns an error if the directory contains multiple settings documents.
pub fn resolve_settings_file(directory: &Path) -> Result<(PathBuf, SettingsFileFormat)> {
    let mut existing = [
        ("settings.json", SettingsFileFormat::Json),
        ("settings.yml", SettingsFileFormat::Yaml),
        ("settings.yaml", SettingsFileFormat::Yaml),
    ]
    .into_iter()
    .filter_map(|(name, format)| {
        let path = directory.join(name);
        path.exists().then_some((path, format))
    });
    let first = existing.next();
    if existing.next().is_some() {
        bail!("Multiple settings files found in {}", directory.display());
    }
    Ok(first.unwrap_or_else(|| (directory.join("settings.json"), SettingsFileFormat::Json)))
}

/// Parse JSON or YAML with an object root.
///
/// # Errors
/// Returns an error for malformed documents or non-object roots.
pub fn parse_settings_document(content: &str) -> Result<Value> {
    let value: Value = serde_yaml::from_str(content)?;
    if !value.is_object() {
        bail!("Settings document must contain an object");
    }
    Ok(value)
}

/// Serialize a document in its existing format.
///
/// # Errors
/// Returns an error when serialization fails.
pub fn stringify_settings_document(value: &Value, format: SettingsFileFormat) -> Result<String> {
    Ok(match format {
        SettingsFileFormat::Json => serde_json::to_string_pretty(value)?,
        SettingsFileFormat::Yaml => serde_yaml::to_string(value)?,
    })
}

pub(crate) fn apply_settings_delta(current: &mut Value, previous: &Value, next: &Value) {
    if let (Some(previous), Some(next)) = (previous.as_object(), next.as_object()) {
        if !current.is_object() {
            *current = serde_json::json!({});
        }
        let current = current.as_object_mut().unwrap();
        for key in previous.keys().filter(|key| !next.contains_key(*key)) {
            current.remove(key);
        }
        for (key, value) in next {
            let before = previous.get(key).unwrap_or(&Value::Null);
            if before == value {
                continue;
            }
            if value.is_null() {
                current.remove(key);
            } else if key != "modelRoles" && before.is_object() && value.is_object() {
                apply_settings_delta(
                    current.entry(key.clone()).or_insert(Value::Null),
                    before,
                    value,
                );
            } else {
                current.insert(key.clone(), value.clone());
            }
        }
    } else {
        current.clone_from(next);
    }
}
