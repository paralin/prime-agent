use pa_core::models::merge_gateway::{fetch_merge_gateway_models, parse_merge_gateway_models};
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};

fn route(input: Value, controls: Value, efforts: Value, disable: bool) -> Value {
    json!({"availability_status":"available", "context_window":262144,"max_output_tokens":65536,
        "capabilities":{"input":input,"output":["text"],"supports_tool_calling":true,"supports_reasoning":true,
            "reasoning":{"controls":controls,"effort_values":efforts,"disable_supported":disable}}})
}

#[test]
fn legacy_catalog_reuses_merge_pricing_and_flash_effort_map() {
    let bootstrap: Vec<_> = pa_ai::fork_catalog::get_models("merge-gateway")
        .into_iter()
        .cloned()
        .collect();
    let id = "zai/glm-5.3-flash";
    let models = parse_merge_gateway_models(
        &json!({"data":[{"id":id},{"id":"unknown/model"}]}),
        &bootstrap,
    )
    .unwrap();
    let source = bootstrap.iter().find(|model| model.id == id).unwrap();
    assert_eq!(models[0].cost, source.cost);
    assert_eq!(
        models[0].base_url,
        "https://api-gateway.merge.dev/v1/ai-sdk"
    );
    let map = serde_json::to_value(&models[0].thinking_level_map).unwrap();
    assert_eq!(map["low"], "low");
    assert!(map["medium"].is_null());
    assert_eq!(models[1].context_window, 128000);
    assert_eq!(models[1].name, "Model");
    let compat = models[0].compat.as_ref().unwrap();
    assert!(compat.kind().is_ok());
    assert_eq!(
        compat.raw["sendSessionAffinityHeaders"],
        json!(["x-session-affinity", "X-Session-Id"])
    );
}

#[test]
fn rich_catalog_intersects_routes_and_filters_unavailable_or_toolless_vendors() {
    let mut text = route(
        json!(["text"]),
        json!(["thinking.budget_tokens"]),
        json!([]),
        false,
    );
    text["context_window"] = json!(128000);
    let vision = route(
        json!(["text", "image"]),
        json!(["thinking", "reasoning_effort"]),
        json!(["low", "high"]),
        true,
    );
    let mut excluded = vision.clone();
    excluded["capabilities"]["supports_tool_calling"] = json!(false);
    let mut unavailable = vision.clone();
    unavailable["availability_status"] = json!("unavailable");
    let models = parse_merge_gateway_models(&json!({"data":[
        {"model":"shared/model","display_name":"Live Model","vendors":{"vision":vision,"text":text,"excluded":excluded.clone()}},
        {"model":"tool-less","vendors":{"excluded":excluded}},
        {"model":"unavailable","vendors":{"unavailable":unavailable}}
    ]}), &[]).unwrap();
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].name, "Live Model");
    assert_eq!(
        serde_json::to_value(&models[0].input).unwrap(),
        json!(["text"])
    );
    assert_eq!(models[0].context_window, 128000);
    assert!(models[0].reasoning);
    assert_eq!(
        models[0].compat.as_ref().unwrap().raw["supportsReasoningEffort"],
        false
    );
    assert_eq!(
        serde_json::to_value(&models[0].thinking_level_map).unwrap(),
        json!({"off":null})
    );
}

#[test]
fn live_flash_thinking_controls_restrict_effort_levels() {
    let vendor = route(
        json!(["text", "image"]),
        json!(["thinking", "reasoning_effort"]),
        json!(["low", "high", "max"]),
        false,
    );
    let models = parse_merge_gateway_models(
        &json!({"data":[{"model":"zai/glm-5.3-flash","vendors":{"fast":vendor}}]}),
        &[],
    )
    .unwrap();
    let map = serde_json::to_value(&models[0].thinking_level_map).unwrap();
    assert_eq!(map["max"], "max");
    assert!(map["off"].is_null());
    assert!(map["xhigh"].is_null());
    assert_eq!(
        models[0].compat.as_ref().unwrap().raw["supportsReasoningEffort"],
        true
    );
}

#[test]
fn malformed_catalogs_fail() {
    for payload in [
        json!({"models":[]}),
        json!({"data":[null]}),
        json!({"data":[{"model":"bad","vendors":[]} ]}),
    ] {
        assert!(parse_merge_gateway_models(&payload, &[]).is_err());
    }
}

fn server(
    pages: Vec<(u16, Value)>,
) -> (String, Arc<Mutex<Vec<String>>>, std::thread::JoinHandle<()>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let requests = Arc::new(Mutex::new(Vec::new()));
    let captured = requests.clone();
    let handle = std::thread::spawn(move || {
        for (status, page) in pages {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(10)))
                .unwrap();
            let mut bytes = Vec::new();
            let mut buffer = [0u8; 1024];
            while !bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                let count = stream.read(&mut buffer).unwrap();
                if count == 0 {
                    break;
                }
                bytes.extend_from_slice(&buffer[..count]);
            }
            captured
                .lock()
                .unwrap()
                .push(String::from_utf8(bytes).unwrap());
            let body = page.to_string();
            write!(stream,"HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
        }
    });
    (url, requests, handle)
}

#[tokio::test]
async fn pages_catalog_with_auth_and_rejects_repeated_cursor() {
    let (url, requests, handle) = server(vec![
        (
            200,
            json!({"data":[{"id":"first/model"}],"has_more":true,"next_cursor":"first/model"}),
        ),
        (200, json!({"data":[{"id":"second/model"}]})),
    ]);
    let models = fetch_merge_gateway_models(&url, "test-key", &[])
        .await
        .unwrap();
    handle.join().unwrap();
    assert_eq!(models.len(), 2);
    let requests = requests.lock().unwrap();
    assert!(requests[0].starts_with("GET /models?limit=500 "));
    assert!(requests[0]
        .to_ascii_lowercase()
        .contains("authorization: bearer test-key"));
    assert!(requests[1].starts_with("GET /models?limit=500&cursor=first%2Fmodel "));
    drop(requests);
    let page = json!({"data":[],"has_more":true,"next_cursor":"same"});
    let (url, _, handle) = server(vec![(200, page.clone()), (200, page)]);
    assert!(fetch_merge_gateway_models(&url, "test-key", &[])
        .await
        .unwrap_err()
        .to_string()
        .contains("repeated"));
    handle.join().unwrap();
}

#[tokio::test]
async fn registry_retains_bootstrap_and_reuses_discovery_in_fresh_instances() {
    let dir = tempfile::tempdir().unwrap();
    let mut auth = pa_core::auth::AuthStorage::in_memory_without_env(
        &pa_core::auth::AuthStorageData::default(),
        Arc::new(pa_core::auth::NoOAuth),
    );
    auth.set_runtime_api_key("merge-gateway", "test-key".to_string());
    let mut registry = pa_core::models::ModelRegistry::create(auth, dir.path().join("models.json"));
    let (url, _, handle) = server(vec![(200, json!({"data":[{"id":"discovered/model"}]}))]);
    registry.refresh_merge_gateway_models(&url).await;
    handle.join().unwrap();
    assert!(registry
        .get_all()
        .iter()
        .any(|model| model.id == "discovered/model"));
    assert!(registry.get_all().iter().any(
        |model| model.id == "anthropic/claude-sonnet-4-6" && model.provider == "merge-gateway"
    ));
    registry.refresh();
    assert!(registry
        .get_all()
        .iter()
        .any(|model| model.id == "discovered/model"));
    let mut fresh_auth = pa_core::auth::AuthStorage::in_memory_without_env(
        &pa_core::auth::AuthStorageData::default(),
        Arc::new(pa_core::auth::NoOAuth),
    );
    fresh_auth.set_runtime_api_key("merge-gateway", "test-key".to_string());
    let fresh = pa_core::models::ModelRegistry::create(fresh_auth, dir.path().join("models.json"));
    assert!(fresh
        .get_all()
        .iter()
        .any(|model| model.id == "discovered/model"));
    registry
        .auth
        .set_runtime_api_key("merge-gateway", "other-key".to_string());
    registry.refresh();
    assert!(!registry
        .get_all()
        .iter()
        .any(|model| model.id == "discovered/model"));
    let (url, _, handle) = server(vec![(503, json!({}))]);
    registry.refresh_merge_gateway_models(&url).await;
    handle.join().unwrap();
    assert!(registry
        .get_all()
        .iter()
        .any(|model| model.provider == "merge-gateway"));
}

#[tokio::test]
async fn codex_catalog_pages_and_rejects_invalid_cursor() {
    use pa_core::models::codex_catalog::{fetch_codex_model_ids, parse_codex_model_page};
    let (url, requests, handle) = server(vec![
        (
            200,
            json!({"models":[{"slug":"first"}],"next_cursor":"next/page"}),
        ),
        (
            200,
            json!({"models":[{"slug":"second"}],"next_cursor":null}),
        ),
    ]);
    let ids = fetch_codex_model_ids(
        &format!("{url}/codex/responses"),
        "codex-key",
        "account",
        None,
    )
    .await
    .unwrap();
    handle.join().unwrap();
    assert_eq!(ids.len(), 2);
    let requests = requests.lock().unwrap();
    assert!(requests[0].starts_with("GET /codex/models?client_version=0.147.0 "));
    assert!(requests[1].starts_with("GET /codex/models?client_version=0.147.0&cursor=next%2Fpage "));
    assert!(requests[0]
        .to_ascii_lowercase()
        .contains("chatgpt-account-id: account"));
    assert!(parse_codex_model_page(&json!({"models":[{}]})).is_err());
    assert!(parse_codex_model_page(&json!({"models":[],"next_cursor":3})).is_err());
}

#[tokio::test]
async fn codex_catalog_rejects_repeated_cursor() {
    let page = json!({"models":[],"next_cursor":"same"});
    let (url, _, handle) = server(vec![(200, page.clone()), (200, page)]);
    let error = pa_core::models::codex_catalog::fetch_codex_model_ids(&url, "key", "account", None)
        .await
        .unwrap_err();
    handle.join().unwrap();
    assert!(error.to_string().contains("repeated"));
}
