use pa_core::models::{parse_rlm_runtime_candidate, resolve_rlm_role_candidates, RlmRuntimeKind};
use pa_core::settings::{ModelRoleSelector, Settings, SettingsManager, WatchedSettingsManager};
use pa_types::ai::{ModelThinkingLevel, ServiceTier};
use serde_json::json;
use std::collections::BTreeMap;
use std::fs;
use std::time::{Duration, Instant};

#[test]
fn yaml_merges_roles_and_saves_without_losing_external_changes_or_runtime_overrides() {
    let dir = tempfile::tempdir().unwrap();
    let agent = dir.path().join("agent");
    let project = dir.path().join(".prime/agent");
    fs::create_dir_all(&agent).unwrap();
    fs::create_dir_all(&project).unwrap();
    let path = agent.join("settings.yml");
    fs::write(
        &path,
        "theme: old\nmodelRoles:\n  task: global/task\n  review: global/review\n",
    )
    .unwrap();
    fs::write(
        project.join("settings.yaml"),
        "modelRoles:\n  task: [project/missing, 'project/task:max']\n",
    )
    .unwrap();
    let mut manager = SettingsManager::create(dir.path(), &agent);
    assert!(manager.errors().is_empty());
    assert_eq!(manager.get_model_roles().len(), 2);
    assert_eq!(
        manager.get_model_role("task"),
        Some(ModelRoleSelector::Ordered(vec![
            "project/missing".into(),
            "project/task:max".into()
        ]))
    );
    manager.apply_overrides(&serde_json::from_value(json!({"theme":"runtime"})).unwrap());
    fs::write(
        &path,
        "theme: external\nexternallyAdded: retained\nmodelRoles:\n  task: global/task\n",
    )
    .unwrap();
    manager.set_default_provider("venice".into()).unwrap();
    let saved = fs::read_to_string(&path).unwrap();
    assert!(saved.contains("externallyAdded: retained"));
    assert!(saved.contains("theme: external"));
    assert!(!agent.join("settings.json").exists());
    assert_eq!(manager.get_theme(), Some("runtime"));
    manager.reload().unwrap();
    assert_eq!(manager.get_theme(), Some("runtime"));
    manager
        .set_model_roles(BTreeMap::from([(
            "new".into(),
            ModelRoleSelector::Single("provider/model".into()),
        )]))
        .unwrap();
    let saved: serde_json::Value =
        serde_yaml::from_str(&fs::read_to_string(path).unwrap()).unwrap();
    assert_eq!(saved["modelRoles"], json!({"new":"provider/model"}));
}

#[test]
fn ambiguous_and_invalid_documents_are_preserved() {
    for invalid in ["[]\n", "null\n", "broken: [\n"] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.yml");
        fs::write(&path, invalid).unwrap();
        let mut manager = SettingsManager::create(dir.path(), dir.path());
        assert!(!manager.errors().is_empty());
        assert!(manager.set_default_provider("test".into()).is_err());
        assert_eq!(fs::read_to_string(path).unwrap(), invalid);
    }
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("settings.json"), "{\"theme\":\"dark\"}").unwrap();
    fs::write(dir.path().join("settings.yml"), "theme: light\n").unwrap();
    let mut manager = SettingsManager::create(dir.path(), dir.path());
    assert!(manager.errors()[0]
        .message
        .contains("Multiple settings files"));
    assert!(manager.set_default_provider("test".into()).is_err());
    assert_eq!(
        fs::read_to_string(dir.path().join("settings.yml")).unwrap(),
        "theme: light\n"
    );
}

#[test]
fn runtime_settings_defaults_validation_and_global_codex_homes() {
    let manager = SettingsManager::in_memory(&Settings::default());
    assert_eq!(manager.get_rlm_act_max_depth().unwrap(), 1);
    assert_eq!(
        manager.get_rlm_allowed_service_tiers(),
        vec![ServiceTier::Default]
    );
    assert!(manager.get_compaction_native());
    assert!(!manager.get_open_router_responses());
    assert_eq!(
        manager.get_scratch_handoff_settings(),
        (false, "agent".into())
    );
    for depth in [json!(-1), json!(1.5), json!("2")] {
        let manager = SettingsManager::in_memory(
            &serde_json::from_value(json!({"rlmActMaxDepth":depth})).unwrap(),
        );
        assert!(manager.get_rlm_act_max_depth().is_err());
    }
    let manager = SettingsManager::in_memory(&serde_json::from_value(json!({"rlmActMaxDepth":0,"rlmActDefaultModel":[" @luna ","@task"],"rlmAllowedServiceTiers":["default","priority"],"claudeCode":{"executable":" /bin/claude "},"openRouter":{"responses":true},"compaction":{"native":false,"triggerContextTokens":256_000}})).unwrap());
    assert_eq!(manager.get_rlm_act_max_depth().unwrap(), 0);
    assert_eq!(
        manager.get_rlm_act_default_model(1).unwrap().as_deref(),
        Some("@luna")
    );
    assert_eq!(manager.get_rlm_act_default_model(3).unwrap(), None);
    assert!(manager.get_rlm_act_default_model(0).is_err());
    assert_eq!(
        manager.get_claude_code_executable().as_deref(),
        Some("/bin/claude")
    );
    assert!(!manager.get_compaction_native());
    assert!(manager.get_open_router_responses());
    assert_eq!(
        manager.get_compaction_trigger_context_tokens(),
        Some(256_000.0)
    );
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".prime/agent")).unwrap();
    fs::write(
        dir.path().join("settings.json"),
        "{\"codexHomes\":[\" /global \"]}",
    )
    .unwrap();
    fs::write(
        dir.path().join(".prime/agent/settings.json"),
        "{\"codexHomes\":[\"/project\"]}",
    )
    .unwrap();
    let manager = SettingsManager::create(dir.path(), dir.path());
    assert_eq!(manager.get_codex_homes().unwrap(), vec!["/global"]);
    assert!(manager
        .errors()
        .iter()
        .any(|error| error.message.contains("global-only")));
}

#[test]
fn runtime_roles_preserve_nested_ids_and_enforce_runtime_consistency() {
    let candidate = parse_rlm_runtime_candidate(" openrouter/deepseek/model:max ").unwrap();
    assert_eq!(candidate.model_reference, "openrouter/deepseek/model");
    assert_eq!(candidate.thinking_level, Some(ModelThinkingLevel::Max));
    assert_eq!(
        parse_rlm_runtime_candidate("openrouter/missing:model")
            .unwrap()
            .model_reference,
        "openrouter/missing:model"
    );
    let candidate = parse_rlm_runtime_candidate("claude-code/claude-opus:high").unwrap();
    assert_eq!(candidate.runtime, RlmRuntimeKind::ClaudeCode);
    assert_eq!(candidate.model_reference, "claude-opus");
    assert!(parse_rlm_runtime_candidate("unqualified").is_err());
    assert!(parse_rlm_runtime_candidate("claude-code/").is_err());
    let roles = BTreeMap::from([
        ("empty".into(), ModelRoleSelector::Ordered(vec![])),
        (
            "mixed".into(),
            ModelRoleSelector::Ordered(vec!["openai/model".into(), "claude-code/opus".into()]),
        ),
    ]);
    assert!(resolve_rlm_role_candidates("missing", &roles).is_err());
    assert!(resolve_rlm_role_candidates("empty", &roles).is_err());
    assert!(resolve_rlm_role_candidates("mixed", &roles).is_err());
}

#[test]
fn watched_manager_reloads_atomic_replacements_and_stops_on_disposal() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.yml");
    fs::write(&path, "theme: old\n").unwrap();
    let mut manager = WatchedSettingsManager::create(dir.path(), dir.path());
    manager.with_mut(|manager| {
        manager.apply_overrides(
            &serde_json::from_value(json!({"defaultProvider":"runtime"})).unwrap(),
        );
    });
    let replacement = dir.path().join("replacement.yml");
    fs::write(
        &replacement,
        "theme: new\nopenRouter:\n  responses: true\nmodelRoles:\n  task: provider/new\n",
    )
    .unwrap();
    fs::rename(replacement, &path).unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    while !manager.with(SettingsManager::get_open_router_responses) {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        manager
            .with(|manager| manager.get_theme().map(str::to_string))
            .as_deref(),
        Some("new")
    );
    assert_eq!(
        manager
            .with(|manager| manager.get_default_provider().map(str::to_string))
            .as_deref(),
        Some("runtime")
    );
    manager.dispose();
    fs::write(path, "theme: ignored\n").unwrap();
    std::thread::sleep(Duration::from_millis(150));
    assert_eq!(
        manager
            .with(|manager| manager.get_theme().map(str::to_string))
            .as_deref(),
        Some("new")
    );
}
