use super::*;
use crate::session::manager::SessionManager;
use pa_types::ai::Usage;

#[test]
fn compacted_windows_keep_act_identity_and_billable_usage_on_cold_and_warm_reads() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    let usage = Usage {
        input: 7,
        output: 3,
        total_tokens: 10,
        ..Default::default()
    };
    let rows = [
        serde_json::json!({"type":"session", "version":3,"id":"session","timestamp":"t","cwd":"/tmp"}),
        serde_json::json!({"type":"act_start","id":"start","parentId":null,"timestamp":"t","actId":"assignment","depth":1,"outerToolCallId":"caller","usageBaseline":Usage::default()}),
        serde_json::json!({"type":"act_terminal","id":"terminal","parentId":"start","timestamp":"t","actId":"assignment","depth":1,"status":"done","usage":usage}),
        serde_json::json!({"type":"message","id":"kept","parentId":"terminal","timestamp":"t","message":{"role":"user","content":"continuation","timestamp":1}}),
        serde_json::json!({"type":"compaction","id":"boundary","parentId":"kept","timestamp":"t","summary":"summary","firstKeptEntryId":"kept","tokensBefore":100}),
    ];
    let text = rows
        .iter()
        .map(serde_json::to_string)
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
        .join("\n");
    std::fs::write(&path, format!("{text}\n")).unwrap();
    for warm in [false, true] {
        let window = WindowedSessionStore::open(&path).unwrap().unwrap();
        assert_eq!(window.read_stats().cache_hit, warm);
        assert_eq!(window.older_path_stats().input, 7);
        assert_eq!(window.older_path_stats().output, 3);
        let mut manager = SessionManager::in_memory(dir.path());
        manager.adopt_window(window);
        let acts = manager.act_records();
        assert_eq!(acts.len(), 2);
        let FileEntry::ActStart { payload, .. } = &acts[0] else {
            panic!("start")
        };
        assert_eq!(payload.outer_tool_call_id.as_deref(), Some("caller"));
        let FileEntry::ActTerminal { payload, .. } = &acts[1] else {
            panic!("terminal")
        };
        assert_eq!(payload.usage, usage);
    }
}
