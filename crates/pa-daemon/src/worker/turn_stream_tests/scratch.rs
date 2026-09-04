use super::*;

#[tokio::test]
async fn scratch_boundary_keeps_kernel_notice_and_write_failures_settle_waiters() {
    for fail_write in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let path = if fail_write {
            dir.path().to_path_buf()
        } else {
            dir.path().join("session.jsonl")
        };
        let continuation = json!({"role":"user","content":"scratch continuation","timestamp":1});
        let engine: Arc<dyn SessionEngine> = Arc::new(GateProbeEngine {
            frames: vec![
                EngineEvent::CompactionStart {
                    event: json!({"type":"compaction_start","reason":"requested"}),
                },
                EngineEvent::CustomMessage(
                    json!({"role":"custom","customType":"ipython_state","content":"live bindings","display":true,"timestamp":1}),
                ),
                EngineEvent::Compaction {
                    entry: json!({"summary":"summary","firstKeptEntryId":"engine-id","tokensBefore":100}),
                    event: json!({"type":"compaction_end","result":{"summary":"summary"}}),
                    continuation: Some(continuation),
                },
                EngineEvent::Done(Ok(())),
            ],
        });
        let runner = burst_runner(engine.clone());
        let mut store = SessionFile::create("/tmp", None, 0);
        store.set_path(path.clone());
        if !fail_write {
            store.rewrite().unwrap();
        }
        runner.core.lock().unwrap().store = Some(store);
        let (done, outcome) = oneshot::channel();
        runner
            .run_turn(
                engine,
                vec![QueuedItem {
                    message: "assignment".into(),
                    priority: QueuePriority::Human,
                    preview: None,
                    custom_message: None,
                    agent_message: None,
                    queue_key: None,
                    admission_id: None,
                    images: Vec::new(),
                    done: Some(done),
                    queue_visible: true,
                    policy: TurnPolicy::Queued,
                    forced_batch: false,
                }],
            )
            .await;
        let settled = tokio::time::timeout(std::time::Duration::from_secs(1), outcome)
            .await
            .unwrap()
            .unwrap();
        if fail_write {
            assert!(settled
                .wire_error()
                .unwrap()
                .contains("Scratch handoff persistence failed"));
            assert!(runner
                .core
                .lock()
                .unwrap()
                .store
                .as_ref()
                .unwrap()
                .entries()
                .is_empty());
        } else {
            assert!(settled.wire_error().is_none());
            let restored = SessionFile::open(&path).unwrap();
            let branch = restored.branch();
            assert_eq!(
                branch
                    .iter()
                    .map(|entry| entry.type_.as_str())
                    .collect::<Vec<_>>(),
                ["message", "compaction", "custom_message"]
            );
            assert_eq!(branch[2].fields["customType"], "ipython_state");
            assert_eq!(branch[1].fields["firstKeptEntryId"], branch[0].id);
        }
        assert!(!runner.core.lock().unwrap().busy);
    }
}
