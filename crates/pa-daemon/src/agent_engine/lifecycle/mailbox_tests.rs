use super::*;
use pa_core::kernel::cancellation::AbortSignal;
use pa_core::kernel::host_channel::HostRequestChannel;
use pa_core::kernel::shared::HostRequestPayload;
use pa_core::session_engine::agent_messaging::mailbox::runtime::DurableMailbox;
use pa_core::session_engine::agent_messaging::mailbox::{MailboxEnvelope, MailboxFilter};
use serde_json::json;

#[tokio::test]
async fn kernel_wait_uses_the_local_mailbox_and_cancellation_removes_its_waiter() {
    let dir = tempfile::tempdir().unwrap();
    let engine = AgentSessionEngine::new(AgentEngineConfig {
        cwd: dir.path().into(),
        agent_dir: dir.path().join("agent"),
        provider: None,
        model: None,
        api_key: None,
        thinking: None,
        session_dir: None,
        session_file: None,
        faux_script: None,
        supervisor_link: Some(crate::agent_engine::SupervisorLinkConfig {
            socket_path: dir.path().join("absent.sock"),
            active_session_id: "own".into(),
            worker_token: "token".into(),
        }),
        telemetry_disabled: Some(true),
        cron_store: None,
        queued_steering_probe: None,
    })
    .unwrap();
    let mailbox = Arc::new(DurableMailbox::new(
        "target".into(),
        vec![],
        Arc::new(|_| Ok(())),
    ));
    *engine.mailbox_provider.lock().unwrap() = Some(Arc::new({
        let mailbox = mailbox.clone();
        move || Ok(mailbox.clone())
    }));
    let handlers = engine.extra_host_handlers().unwrap();
    let request = |data| HostRequestPayload {
        data,
        cell_source_code: None,
    };
    let inbox = handlers.get("agent_message.inbox").unwrap().clone();
    assert_eq!(
        inbox(request(json!({}))).await.unwrap(),
        json!({"messages":[]})
    );
    let signal = AbortSignal::new();
    let (channel, _sender) = HostRequestChannel::new(
        signal.clone(),
        None,
        None,
        Arc::new(|_| Box::pin(async { Ok(()) })),
        Arc::new(|_| {}),
    );
    let wait = handlers.get_duplex("agent_message.wait").unwrap().clone();
    let waiter =
        tokio::spawn(async move { wait(request(json!({"timeout_ms":1000})), channel).await });
    tokio::task::yield_now().await;
    signal.abort();
    assert!(pa_agent::abort::is_abort_error(
        &waiter.await.unwrap().unwrap_err()
    ));
    mailbox
        .accept(MailboxEnvelope {
            id: "after-cancel".into(),
            source: "agent_message".into(),
            message: "retained".into(),
            reply_to: None,
            from: None,
            from_relationship: None,
            target: json!({"sessionId":"target"}),
            accepted_at: "now".into(),
            sequence: 0,
        })
        .unwrap();
    assert_eq!(
        mailbox.inbox(&MailboxFilter::default(), 20, false).unwrap()[0].id,
        "after-cancel"
    );
    assert_eq!(
        inbox(request(json!({}))).await.unwrap()["messages"][0]["id"],
        "after-cancel"
    );
}
