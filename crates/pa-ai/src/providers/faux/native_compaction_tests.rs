use std::sync::Arc;

use serde_json::json;

use super::{register_faux_provider, FauxModelDefinition, RegisterFauxProviderOptions};
use crate::types::{Context, ProviderNativeCompactionOptions, ProviderNativeCompactionResult};

#[tokio::test]
async fn registered_compactor_receives_context_and_instructions() {
    let registration = register_faux_provider(RegisterFauxProviderOptions {
        models: Some(vec![FauxModelDefinition {
            thinking_level_map: Some(serde_json::from_value(json!({"high":"custom"})).unwrap()),
            ..Default::default()
        }]),
        compact: Some(Arc::new(|model, context, options| {
            Box::pin(async move {
                assert_eq!(context.system_prompt.as_deref(), Some("system"));
                assert_eq!(options.instructions, "compact");
                let item = json!({"type":"compaction","encrypted_content":"opaque"});
                Ok(ProviderNativeCompactionResult {
                    provider: model.provider.clone(),
                    replacement_history: vec![item.clone()],
                    compaction_item: item,
                })
            })
        })),
        ..Default::default()
    });
    let model = registration.get_model();
    assert!(model.thinking_level_map.is_some());
    let result = crate::compact(
        &model,
        &Context {
            system_prompt: Some("system".into()),
            messages: vec![],
            tools: None,
        },
        &ProviderNativeCompactionOptions {
            instructions: "compact".into(),
            base: Default::default(),
        },
    )
    .await
    .unwrap();
    assert_eq!(result.replacement_history, vec![result.compaction_item]);
    registration.unregister();
}

#[tokio::test]
async fn absent_compactor_reports_unsupported() {
    let registration = register_faux_provider(RegisterFauxProviderOptions::default());
    let error = crate::compact(
        &registration.get_model(),
        &Context {
            system_prompt: None,
            messages: vec![],
            tools: None,
        },
        &ProviderNativeCompactionOptions {
            instructions: String::new(),
            base: Default::default(),
        },
    )
    .await
    .unwrap_err();
    assert!(error
        .to_string()
        .contains("does not support native compaction"));
    registration.unregister();
}
