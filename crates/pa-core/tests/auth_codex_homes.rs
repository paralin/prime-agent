use pa_core::auth::{
    AuthCredential, AuthSource, AuthStorage, AuthStorageData, NoOAuth,
    RuntimeApiKeyChainCredential, RuntimeApiKeyChainState,
};
use std::sync::Arc;

fn auth() -> AuthStorage {
    AuthStorage::in_memory_without_env(&AuthStorageData::default(), Arc::new(NoOAuth))
}
fn credentials() -> Vec<RuntimeApiKeyChainCredential> {
    ["first", "second"]
        .into_iter()
        .map(|key| RuntimeApiKeyChainCredential {
            key: key.to_string(),
            label: Some(format!("{key}-home")),
        })
        .collect()
}

#[test]
fn shared_rotation_exhausts_without_fallback() {
    let state = Arc::new(RuntimeApiKeyChainState::default());
    let mut first = auth();
    let mut second = auth();
    first.set_runtime_api_key_chain_state(state.clone());
    second.set_runtime_api_key_chain_state(state);
    first
        .set_runtime_api_key_chain("openai-codex", credentials())
        .unwrap();
    second.set_fallback_resolver(Arc::new(|_| Some("fallback".to_string())));
    assert_eq!(first.get_api_key("openai-codex").as_deref(), Some("first"));
    let token = first
        .get_api_key_with_source_token("openai-codex", true)
        .source_token
        .unwrap();
    assert!(first.mark_auth_source_stale(token.clone()));
    assert!(!second.mark_auth_source_stale(token));
    assert_eq!(
        second.get_api_key("openai-codex").as_deref(),
        Some("second")
    );
    assert!(second.mark_auth_stale("openai-codex"));
    assert!(first.get_api_key("openai-codex").is_none());
    assert_eq!(
        first.get_auth_status("openai-codex").source,
        Some(AuthSource::Stale)
    );
    first.remove_runtime_api_key_chain("openai-codex");
    assert_eq!(
        second.get_api_key("openai-codex").as_deref(),
        Some("fallback")
    );
}

#[test]
fn explicit_override_wins_without_consuming_chain() {
    let mut auth = auth();
    auth.set_runtime_api_key_chain("openai-codex", credentials())
        .unwrap();
    auth.set_runtime_api_key("openai-codex", "explicit".to_string());
    assert_eq!(
        auth.get_api_key("openai-codex").as_deref(),
        Some("explicit")
    );
    auth.remove_runtime_api_key("openai-codex");
    assert_eq!(auth.get_api_key("openai-codex").as_deref(), Some("first"));
    let status = auth.get_auth_status("openai-codex");
    assert!(!status.configured);
    assert_eq!(status.label.as_deref(), Some("first-home"));
}

#[test]
fn invalid_chain_leaves_existing_credentials_intact() {
    let mut auth = auth();
    auth.set_runtime_api_key_chain("openai-codex", credentials())
        .unwrap();
    assert!(auth
        .set_runtime_api_key_chain(
            "openai-codex",
            vec![RuntimeApiKeyChainCredential {
                key: " ".to_string(),
                label: None
            }]
        )
        .is_err());
    assert_eq!(auth.get_api_key("openai-codex").as_deref(), Some("first"));
}

#[test]
fn homes_preserve_order_ignore_malformed_and_do_not_write() {
    let dir = tempfile::tempdir().unwrap();
    let first = dir.path().join("first");
    let bad = dir.path().join("bad");
    let second = dir.path().join("second");
    for path in [&first, &bad, &second] {
        std::fs::create_dir(path).unwrap();
    }
    let content = r#"{"tokens":{"access_token":" first "}}"#;
    std::fs::write(first.join("auth.json"), content).unwrap();
    std::fs::write(bad.join("auth.json"), "invalid").unwrap();
    std::fs::write(
        second.join("auth.json"),
        r#"{"tokens":{"access_token":"second"}}"#,
    )
    .unwrap();
    let paths = [&first, &bad, &first.join("."), &second]
        .into_iter()
        .map(|path| path.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    let mut auth = auth();
    let result = auth.apply_codex_homes(&paths).unwrap();
    assert_eq!(
        result.configured_homes,
        vec![first.clone(), bad, second.clone()]
    );
    assert_eq!(result.loaded_homes, vec![first.clone(), second]);
    assert!(auth.mark_runtime_api_key_chain_stale("openai-codex", "first"));
    auth.apply_codex_homes(&paths).unwrap();
    assert_eq!(auth.get_api_key("openai-codex").as_deref(), Some("second"));
    assert_eq!(
        std::fs::read_to_string(first.join("auth.json")).unwrap(),
        content
    );
}

#[test]
fn existing_storage_observes_external_replacement() {
    let dir = tempfile::tempdir().unwrap();
    let mut auth = AuthStorage::create_with_oauth(dir.path(), Arc::new(NoOAuth));
    auth.set(
        "openai-codex",
        AuthCredential::ApiKey {
            key: "old".to_string(),
            prime_team: None,
        },
    );
    assert!(auth.mark_auth_stale("openai-codex"));
    std::fs::write(
        dir.path().join("auth.json"),
        r#"{"openai-codex":{"type":"api_key","key":"new"}}"#,
    )
    .unwrap();
    assert_eq!(
        auth.get_auth_status("openai-codex").source,
        Some(AuthSource::Stored)
    );
    assert_eq!(auth.get_api_key("openai-codex").as_deref(), Some("new"));
}

#[test]
fn production_storage_shares_homes_and_reload_revives_changed_tokens() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("codex");
    std::fs::create_dir(&home).unwrap();
    std::fs::write(
        home.join("auth.json"),
        r#"{"tokens":{"access_token":"first"}}"#,
    )
    .unwrap();
    std::fs::write(
        dir.path().join("settings.json"),
        serde_json::json!({"codexHomes": [home]}).to_string(),
    )
    .unwrap();
    let mut first = AuthStorage::create_with_oauth(dir.path(), Arc::new(NoOAuth));
    assert_eq!(first.get_api_key("openai-codex").as_deref(), Some("first"));
    first.mark_auth_stale("openai-codex");
    let mut second = AuthStorage::create_with_oauth(dir.path(), Arc::new(NoOAuth));
    assert!(second.get_api_key("openai-codex").is_none());
    std::fs::write(
        home.join("auth.json"),
        r#"{"tokens":{"access_token":"new"}}"#,
    )
    .unwrap();
    let mut third = AuthStorage::create_with_oauth(dir.path(), Arc::new(NoOAuth));
    assert_eq!(third.get_api_key("openai-codex").as_deref(), Some("new"));
}

#[test]
fn exhausted_chain_blocks_models_json_fallback() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("models.json");
    std::fs::write(
        &path,
        r#"{"providers":{"openai-codex":{"apiKey":"fallback"}}}"#,
    )
    .unwrap();
    let mut auth = auth();
    auth.set_runtime_api_key_chain("openai-codex", credentials())
        .unwrap();
    let mut registry = pa_core::models::ModelRegistry::create(auth, path);
    let model = registry
        .get_all()
        .iter()
        .find(|model| model.provider == "openai-codex")
        .unwrap()
        .clone();
    assert!(registry.has_configured_auth(&model));
    registry.auth.mark_auth_stale("openai-codex");
    registry.auth.mark_auth_stale("openai-codex");
    assert!(!registry.has_configured_auth(&model));
    assert!(registry
        .get_api_key_and_headers(&model, None)
        .api_key
        .is_none());
}
