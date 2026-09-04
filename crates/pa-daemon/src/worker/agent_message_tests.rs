//! Agent-message command tests (moved with the commands concern).
use super::*;

fn test_worker() -> Arc<Worker> {
    let dir = std::env::temp_dir().join(format!("pa-worker-am-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let config = WorkerConfig {
        socket_path: dir.join("worker.sock"),
        supervisor_socket_path: PathBuf::new(),
        token: "token".to_string(),
        worker_instance_id: String::new(),
        active_session_id: "target-session".to_string(),
        agent_dir: dir.join("agent"),
        recovery_journal_path: dir.join("recovery.jsonl"),
        telemetry_disabled: None,
        script: Some(json!({ "responses": ["ack"] })),
    };
    Arc::new(Worker::new(config, None))
}

async fn created_worker() -> Arc<Worker> {
    let worker = test_worker();
    let created = worker
        .dispatch(
            "create",
            &json!({ "noSession": true, "cwd": "/tmp", "name": "target" }),
        )
        .await;
    assert!(created.success, "create failed: {created:?}");
    worker
}

fn queue_texts(core: &Mutex<SessionCore>, lane: Lane) -> Vec<String> {
    let core = core.lock().unwrap();
    match lane {
        Lane::Steering => &core.steering,
        Lane::FollowUp => &core.follow_up,
    }
    .iter()
    .map(|item| item.message.clone())
    .collect()
}

#[tokio::test]
async fn stable_delivery_retry_preserves_the_original_envelope_and_queue() {
    let worker = created_worker().await;
    let input = json!({"messageId":"stable-message", "message":"original", "replyTo":"task",
        "sender":{"sessionId":"sender", "sessionName":"first"}});
    let first = worker.dispatch("worker_deliver_message", &input).await;
    assert!(first.success, "{first:?}");
    let mut retry = input;
    retry["message"] = json!("replacement");
    retry["sender"]["sessionName"] = json!("different");
    let second = worker.dispatch("worker_deliver_message", &retry).await;
    assert!(second.success, "{second:?}");
    let receipt = second.data.unwrap();
    assert_eq!(receipt["id"], "stable-message");
    assert_eq!(receipt["message"], "original");
    assert_eq!(receipt["replyTo"], "task");
    assert_eq!(receipt["from"]["sessionName"], "first");
    assert_eq!(receipt["handoff"], "retry");
    assert_eq!(
        queue_texts(&worker.core, Lane::Steering),
        vec!["[agent-message from first]\n\noriginal"]
    );
    let core = worker.core.lock().unwrap();
    let store = core.store.as_ref().unwrap();
    assert_eq!(
        store
            .entries()
            .iter()
            .filter(|entry| entry.fields["customType"] == "agent_message.accepted")
            .count(),
        1
    );
    assert_eq!(
        store
            .entries()
            .iter()
            .filter(|entry| entry.fields["customType"] == "agent_message.handoff")
            .count(),
        1
    );
    assert_eq!(
        core.steering
            .front()
            .unwrap()
            .custom_message
            .as_ref()
            .unwrap()["details"]["replyTo"],
        "task"
    );
}

#[tokio::test]
async fn worker_delivery_to_a_waiter_does_not_start_a_turn() {
    use pa_agent::abort::AbortSignal;
    use pa_core::session_engine::agent_messaging::mailbox::MailboxFilter;

    let worker = created_worker().await;
    let mailbox = worker.session_mailbox().unwrap();
    let waiter = tokio::spawn(async move {
        mailbox
            .wait(&MailboxFilter::default(), 1000, &AbortSignal::never())
            .await
    });
    tokio::task::yield_now().await;
    let response = worker
        .dispatch(
            "worker_deliver_message",
            &json!({"message":"answer", "messageId":"waiter-message"}),
        )
        .await;
    assert!(response.success, "{response:?}");
    assert_eq!(response.data.unwrap()["handoff"], "waiter");
    assert_eq!(waiter.await.unwrap().unwrap().unwrap().id, "waiter-message");
    assert!(queue_texts(&worker.core, Lane::Steering).is_empty());
    assert!(worker
        .session_mailbox()
        .unwrap()
        .inbox(&MailboxFilter::default(), 20, false)
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn mailbox_closure_settles_waiters_and_replacement_builds_a_fresh_mailbox() {
    use pa_agent::abort::AbortSignal;
    use pa_core::session_engine::agent_messaging::mailbox::MailboxFilter;

    let worker = created_worker().await;
    let mailbox = worker.session_mailbox().unwrap();
    let waiter = tokio::spawn({
        let mailbox = mailbox.clone();
        async move {
            mailbox
                .wait(&MailboxFilter::default(), 1000, &AbortSignal::never())
                .await
        }
    });
    tokio::task::yield_now().await;
    worker.teardown_for_replacement().await.unwrap();
    assert_eq!(
        waiter.await.unwrap().unwrap_err().to_string(),
        "Session was replaced"
    );
    assert!(worker.session_mailbox().is_err());
    worker.discard_closed_mailbox();
    let fresh = worker.session_mailbox().unwrap();
    assert!(!Arc::ptr_eq(&mailbox, &fresh));
    let waiter = tokio::spawn(async move {
        fresh
            .wait(&MailboxFilter::default(), 1000, &AbortSignal::never())
            .await
    });
    tokio::task::yield_now().await;
    assert!(worker.handle_shutdown().await.success);
    assert_eq!(
        waiter.await.unwrap().unwrap_err().to_string(),
        "Session is shutting down"
    );
    assert!(worker.session_mailbox().is_err());
    let fresh_worker = created_worker().await;
    fresh_worker.close_mailbox("closed before first access");
    assert!(fresh_worker.session_mailbox().is_err());
}

#[tokio::test]
async fn mailbox_commands_filter_peek_consume_and_wait_for_delivery() {
    let worker = created_worker().await;
    let first = worker.dispatch("worker_deliver_message", &json!({"message":"first", "messageId":"first", "replyTo":"question", "sender":{"sessionId":"parent"}})).await;
    assert!(first.success, "{first:?}");
    let inbox = json!({"sender":" parent ","replyTo":"question", "limit":1});
    let response = worker.dispatch("agent_message_inbox", &inbox).await;
    assert!(response.success, "{response:?}");
    assert_eq!(response.data.unwrap()["messages"][0]["id"], "first");
    let mut consume = inbox.clone();
    consume["consume"] = json!(true);
    assert_eq!(
        worker
            .dispatch("agent_message_inbox", &consume)
            .await
            .data
            .unwrap()["messages"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert!(worker
        .dispatch("agent_message_inbox", &inbox)
        .await
        .data
        .unwrap()["messages"]
        .as_array()
        .unwrap()
        .is_empty());
    let wait = tokio::spawn({
        let worker = worker.clone();
        async move {
            worker
                .dispatch(
                    "agent_message_wait",
                    &json!({"timeoutMs":1000, "sender":"parent", "replyTo":"next"}),
                )
                .await
        }
    });
    tokio::task::yield_now().await;
    let delivery = worker.dispatch("worker_deliver_message", &json!({"message":"second", "messageId":"second", "replyTo":"next", "sender":{"sessionId":"parent"}})).await;
    assert!(delivery.success, "{delivery:?}");
    assert_eq!(delivery.data.unwrap()["handoff"], "waiter");
    let waited = wait.await.unwrap();
    assert!(waited.success, "{waited:?}");
    assert_eq!(waited.data.unwrap()["message"]["id"], "second");
    let timeout = worker
        .dispatch("agent_message_wait", &json!({"timeoutMs":1}))
        .await;
    assert!(timeout.success, "{timeout:?}");
    assert_eq!(timeout.data.unwrap(), json!({}));
    assert!(
        !worker
            .dispatch("agent_message_inbox", &json!({"limit":101}))
            .await
            .success
    );
    assert!(
        !worker
            .dispatch("agent_message_wait", &json!({"timeoutMs":300_001}))
            .await
            .success
    );
}

/// Receipt shape (`createAgentSessionMessageReceipt`): id, source,
/// target endpoint, sender echo, delivered status and timestamp while
/// the session is idle, and the rendered prompt on the steering lane.
#[tokio::test]
async fn deliver_message_answers_the_ts_receipt_shape() {
    let worker = created_worker().await;
    let response = worker
        .dispatch(
            "worker_deliver_message",
            &json!({
                "targetActiveSessionId": "target-session",
                "message": "ping from the first session",
                "sender": {
                    "activeSessionId": "source-session",
                    "sessionId": "source-file",
                    "sessionName": "source-agent",
                    "runtimeKind": "top-level",
                    "clientId": "cli-1",
                },
            }),
        )
        .await;
    assert!(response.success, "deliver failed: {response:?}");
    assert_eq!(response.command, "worker_deliver_message");
    let data = response.data.expect("receipt data");
    assert!(
        data["id"]
            .as_str()
            .unwrap_or_default()
            .starts_with("agentmsg_"),
        "receipt id: {data}"
    );
    assert_eq!(data["source"], "agent_message");
    assert_eq!(data["message"], "ping from the first session");
    assert_eq!(data["deliveryStatus"], "delivered");
    assert_eq!(data["deliveryMode"], "steer");
    assert!(
        data["deliveredAt"].as_str().is_some(),
        "deliveredAt: {data}"
    );
    assert!(
        data.get("queuedAt").is_none(),
        "queuedAt on delivery: {data}"
    );
    assert_eq!(data["target"]["activeSessionId"], "target-session");
    assert_eq!(data["target"]["sessionName"], "target");
    assert!(!data["target"]["sessionId"]
        .as_str()
        .unwrap_or_default()
        .is_empty());
    assert_eq!(data["from"]["sessionName"], "source-agent");
    assert_eq!(
        queue_texts(&worker.core, Lane::Steering),
        vec!["[agent-message from source-agent]\n\nping from the first session"],
        "steering lane"
    );
    assert!(queue_texts(&worker.core, Lane::FollowUp).is_empty());
}

/// The queued delivery carries the `agent_message` custom row (TS
/// `acceptAgentSessionMessage` rides `acceptAgentMessagePrompt`'s
/// `customMessage`): the row's content is the rendered prompt, the
/// details carry the identity the collapsed card reads, and the
/// agent-message marker still targets `agent_messages_clear`/`pause`.
#[tokio::test]
async fn deliver_message_carries_the_agent_message_custom_row() {
    let worker = created_worker().await;
    let response = worker
        .dispatch(
            "worker_deliver_message",
            &json!({
                "targetActiveSessionId": "target-session",
                "message": "the research is done",
                "sender": {
                    "activeSessionId": "source-session",
                    "sessionId": "source-file",
                    "sessionName": "research-lane",
                    "runtimeKind": "subagent",
                    // The child label derives from this parent edge,
                    // never from the runtime kind alone.
                    "parentActiveSessionId": "target-session",
                },
            }),
        )
        .await;
    assert!(response.success, "deliver failed: {response:?}");
    let data = response.data.expect("receipt data");
    let prompt = "[agent-message from child:research-lane]\n\nthe research is done";
    let (custom, message, agent_message, preview) = {
        let core = worker.core.lock().unwrap();
        let item = core.steering.front().expect("the delivery queued");
        (
            item.custom_message
                .clone()
                .expect("the agent_message row rides the delivery"),
            item.message.clone(),
            item.agent_message.clone(),
            item.preview.clone(),
        )
    };
    // The queue strip serves the TS labeled preview.
    assert_eq!(
        preview.as_deref(),
        Some("Agent message received: the research is done")
    );
    assert_eq!(custom["role"], "custom");
    assert_eq!(custom["customType"], "agent_message");
    assert_eq!(custom["content"], prompt);
    assert_eq!(custom["display"], true);
    assert_eq!(custom["details"]["id"], data["id"]);
    assert_eq!(custom["details"]["message"], "the research is done");
    assert_eq!(
        custom["details"]["from"]["activeSessionId"],
        "source-session"
    );
    assert_eq!(custom["details"]["fromRelationship"], "child");
    assert_eq!(
        custom["details"]["target"]["activeSessionId"],
        "target-session"
    );
    // The turn still runs on the rendered prompt, and the marker the
    // clear/pause arms read is untouched.
    assert_eq!(message, prompt);
    assert_eq!(agent_message.as_deref(), Some("the research is done"));
}

/// An explicit `follow_up` delivery mode queues behind current work
/// instead of steering, and a subagent sender renders the relationship.
#[tokio::test]
async fn deliver_message_follow_up_lane_and_subagent_sender() {
    let worker = created_worker().await;
    let response = worker
        .dispatch(
            "worker_deliver_message",
            &json!({
                "targetActiveSessionId": "target-session",
                "message": "queue me",
                "sender": {
                    "activeSessionId": "source-session",
                    "sessionName": "source-agent",
                    "runtimeKind": "subagent",
                    // The child label derives from this parent edge,
                    // never from the runtime kind alone.
                    "parentActiveSessionId": "target-session",
                },
                "deliveryMode": "follow_up",
            }),
        )
        .await;
    assert!(response.success, "deliver failed: {response:?}");
    let data = response.data.expect("receipt data");
    assert_eq!(data["deliveryMode"], "follow_up");
    assert_eq!(
        queue_texts(&worker.core, Lane::FollowUp),
        vec!["[agent-message from child:source-agent]\n\nqueue me"],
        "follow-up lane"
    );
    assert!(queue_texts(&worker.core, Lane::Steering).is_empty());
}

/// A busy session reports `queued` with `queuedAt` (`queueIfBusy`).
#[tokio::test]
async fn deliver_message_while_busy_queues() {
    let worker = created_worker().await;
    worker.core.lock().unwrap().busy = true;
    let response = worker
        .dispatch(
            "worker_deliver_message",
            &json!({
                "targetActiveSessionId": "target-session",
                "message": "while busy",
                "sender": { "activeSessionId": "source-session" },
            }),
        )
        .await;
    assert!(response.success, "deliver failed: {response:?}");
    let data = response.data.expect("receipt data");
    assert_eq!(data["deliveryStatus"], "queued");
    assert!(data["queuedAt"].as_str().is_some(), "queuedAt: {data}");
    assert!(
        data.get("deliveredAt").is_none(),
        "deliveredAt while queued: {data}"
    );
}

/// The pending-capacity guard fails with the TS error string.
#[tokio::test]
async fn deliver_message_respects_the_pending_capacity() {
    let worker = created_worker().await;
    {
        let mut core = worker.core.lock().unwrap();
        for _ in 0..DEFAULT_AGENT_MESSAGE_MAX_PENDING_PER_SESSION {
            core.follow_up.push_back(QueuedItem {
                priority: QueuePriority::Human,
                preview: None,
                message: "occupied".to_string(),
                custom_message: None,
                agent_message: None,
                queue_key: None,
                admission_id: None,
                images: Vec::new(),
                done: None,
                queue_visible: true,
                policy: TurnPolicy::Injected,
                forced_batch: false,
            });
        }
    }
    let response = worker
        .dispatch(
            "worker_deliver_message",
            &json!({
                "targetActiveSessionId": "target-session",
                "message": "over the limit",
                "sender": { "activeSessionId": "source-session" },
            }),
        )
        .await;
    assert!(!response.success, "deliver should fail: {response:?}");
    assert_eq!(
        response.error.as_deref(),
        Some("Target session has too many pending messages: 20 unfinished, limit is 20")
    );
}

/// The attach response's wire shape is the TS `createAttachResult`
/// key order (`serde_json` keeps insertion order), for slim and plain
/// clients: the slim response carries the messages exactly once
/// (inside the snapshot) and the non-slim duplicate is byte-identical
/// to the snapshot's copy.
#[tokio::test]
async fn attach_response_wire_bytes_keep_the_ts_key_order() {
    let worker = created_worker().await;
    {
        let mut core = worker.core.lock().unwrap();
        let store = core.store.as_mut().expect("the created store");
        store.append_entry(
            "message",
            json!({ "message": { "role": "user", "content": "wire bytes" } }),
        );
        store.append_entry(
            "message",
            json!({ "message": { "role": "assistant", "content": "byte order" } }),
        );
    }
    for (capabilities, slim) in [(vec!["slim_attach"], true), (Vec::<&str>::new(), false)] {
        let response = worker
            .dispatch(
                "attach",
                &json!({
                    "clientId": "wire-client",
                    "capabilities": capabilities,
                }),
            )
            .await;
        assert!(response.success, "attach failed: {response:?}");
        let data = response.data.expect("attach carries data");
        let keys = data
            .as_object()
            .expect("attach data is an object")
            .keys()
            .cloned()
            .collect::<Vec<String>>();
        let expected = if slim {
            vec![
                "protocol".to_string(),
                "activeSessionId".to_string(),
                "snapshot".to_string(),
                "replay".to_string(),
                "lastEventSequence".to_string(),
                "lastEventCursor".to_string(),
                "client".to_string(),
            ]
        } else {
            vec![
                "protocol".to_string(),
                "activeSessionId".to_string(),
                "state".to_string(),
                "messages".to_string(),
                "snapshot".to_string(),
                "replay".to_string(),
                "lastEventSequence".to_string(),
                "lastEventCursor".to_string(),
                "client".to_string(),
            ]
        };
        assert_eq!(
            keys, expected,
            "the attach top-level keys keep the TS createAttachResult order"
        );
        let snapshot = data.get("snapshot").expect("the attach snapshot");
        let snapshot_keys = snapshot
            .as_object()
            .expect("the snapshot is an object")
            .keys()
            .cloned()
            .collect::<Vec<String>>();
        assert_eq!(
            snapshot_keys,
            vec![
                "activeSessionId".to_string(),
                "summary".to_string(),
                "state".to_string(),
                "messages".to_string(),
                "lastEventSequence".to_string(),
                "lastEventCursor".to_string(),
                "children".to_string(),
            ],
            "the snapshot keys keep the TS order"
        );
        let messages = snapshot.get("messages").expect("the snapshot messages");
        let content: Vec<String> = messages
            .as_array()
            .expect("messages are an array")
            .iter()
            .map(|row| {
                row.get("content")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string()
            })
            .collect();
        assert_eq!(
            content,
            vec!["wire bytes".to_string(), "byte order".to_string()],
            "the snapshot carries the store's transcript rows in order"
        );
        if slim {
            assert!(
                data.get("messages").is_none() && data.get("state").is_none(),
                "the slim attach duplicates no message tree at the top level"
            );
        } else {
            let top_messages = data.get("messages").expect("the top-level messages");
            assert_eq!(
                serde_json::to_string(top_messages).unwrap(),
                serde_json::to_string(messages).unwrap(),
                "the duplicated message trees serialize to identical bytes"
            );
            assert_eq!(
                data.get("state"),
                snapshot.get("summary"),
                "the top-level state is the summary the snapshot carries"
            );
        }
    }
}
