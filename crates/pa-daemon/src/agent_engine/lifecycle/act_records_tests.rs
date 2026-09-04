use std::sync::{Arc, Mutex};

use crate::agent_engine::tests::{faux_engine_with_settings, FAUX_TEST_LOCK};
use crate::engine::SessionEngine;
use crate::session_store::SessionFile;

#[test]
fn interrupted_act_is_closed_durably_after_worker_history_adoption() {
    let _registry = FAUX_TEST_LOCK.lock().unwrap();
    let (engine, dir) = faux_engine_with_settings(&serde_json::json!({"responses":[]}), 100);
    let path = dir.path().join("root.jsonl");
    let mut store = SessionFile::create(&dir.path().display().to_string(), None, 0);
    store.set_path(path.clone());
    store
        .persist_entry(
            "act_start",
            serde_json::json!({
                "actId":"crashed", "depth":1, "sessionKey":"faux/model", "outerToolCallId":"outer",
                "usageBaseline":pa_types::ai::Usage::default(),
            }),
        )
        .unwrap();
    let durable = Arc::new(Mutex::new(store));
    let sink = durable.clone();
    *engine.act_record_sink.lock().unwrap() = Some(Arc::new(move |kind, fields| {
        sink.lock().unwrap().persist_entry(kind, fields)?;
        Ok(())
    }));
    engine.set_session_file(path.clone());
    let model = engine.resolve_model().unwrap();
    engine.ensure_core_session(&model).unwrap();
    let restored = SessionFile::open(&path).unwrap();
    let terminals: Vec<_> = restored
        .entries()
        .iter()
        .filter(|entry| entry.type_ == "act_terminal")
        .collect();
    assert_eq!(terminals.len(), 1);
    assert_eq!(terminals[0].fields["actId"], "crashed");
    assert_eq!(terminals[0].fields["status"], "interrupted");
    engine.ensure_core_session(&model).unwrap();
    assert_eq!(
        durable
            .lock()
            .unwrap()
            .entries()
            .iter()
            .filter(|entry| entry.type_ == "act_terminal")
            .count(),
        1
    );
    engine.runtime.block_on(engine.dispose_kernel());
}
