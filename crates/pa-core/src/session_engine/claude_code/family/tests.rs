use super::*;
use crate::session_engine::agent_messaging::mailbox::runtime::DurableMailbox;
use crate::session_engine::agent_messaging::mailbox::{MailboxEnvelope, MailboxFilter};
use crate::session_engine::agent_messaging::{
    AgentFamilyMember, AgentMessageReceipt, AgentMessageSendInput,
};

struct Controller;
impl AgentMessageController for Controller {
    fn roster(&self) -> impl std::future::Future<Output = Result<Value>> + Send {
        std::future::ready(Ok(
            json!({"current":{"id":"own"},"entries":[{"relationship":"parent"},{"relationship":"sibling"},{"relationship":"child"}]}),
        ))
    }
    fn family(&self) -> impl std::future::Future<Output = Result<Vec<AgentFamilyMember>>> + Send {
        std::future::ready(Ok(vec![]))
    }
    fn send_agent_message(
        &self,
        _: AgentMessageSendInput,
    ) -> impl std::future::Future<Output = Result<AgentMessageReceipt>> + Send {
        std::future::ready(Err(anyhow::anyhow!("unexpected send")))
    }
}

#[tokio::test]
async fn family_server_exposes_only_four_tools_and_restricts_send_targets() {
    let mailbox = Arc::new(DurableMailbox::new(
        "own".into(),
        vec![],
        Arc::new(|_| Ok(())),
    ));
    let handler = family_mcp_handler(Arc::new(Controller), Arc::new(move || Ok(mailbox.clone())));
    let response = handler(
        json!({"id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}),
        AbortSignal::never(),
    )
    .await
    .unwrap();
    assert_eq!(response["result"]["protocolVersion"], "2025-06-18");
    let response = handler(json!({"id":2,"method":"tools/list"}), AbortSignal::never())
        .await
        .unwrap();
    let tools = response["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 4);
    assert!(tools.iter().all(|tool| super::super::FAMILY_TOOLS
        .contains(&format!("mcp__prime__{}", tool["name"].as_str().unwrap()).as_str())));
    let call = |name, arguments| json!({"id":3,"method":"tools/call","params":{"name":name,"arguments":arguments}});
    let response = handler(call("family_list", json!({})), AbortSignal::never())
        .await
        .unwrap();
    let roster: Value =
        serde_json::from_str(response["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(roster["entries"].as_array().unwrap().len(), 2);
    for arguments in [
        json!({"receiver_role":"child","message":"no"}),
        json!({"receiver_role":"parent","receiver_name":"named","message":"no"}),
        json!({"receiver_role":"sibling","message":"no"}),
        json!({"target":"all","message":"no"}),
    ] {
        let response = handler(call("family_send", arguments), AbortSignal::never())
            .await
            .unwrap();
        assert_eq!(response["result"]["isError"], true);
        assert!(!response["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("unexpected send"));
    }
}

#[tokio::test]
async fn cancelled_family_wait_does_not_consume_later_delivery() {
    let mailbox = Arc::new(DurableMailbox::new(
        "own".into(),
        vec![],
        Arc::new(|_| Ok(())),
    ));
    let handler = family_mcp_handler(
        Arc::new(Controller),
        Arc::new({
            let mailbox = mailbox.clone();
            move || Ok(mailbox.clone())
        }),
    );
    let controller = pa_agent::abort::AbortController::new();
    let waiter = tokio::spawn({
        let signal = controller.signal();
        let handler = handler.clone();
        async move {
            handler(json!({"id":1,"method":"tools/call","params":{"name":"family_wait","arguments":{"timeout_ms":1000}}}), signal).await
        }
    });
    tokio::task::yield_now().await;
    controller.abort();
    assert!(pa_agent::abort::is_abort_error(
        &waiter.await.unwrap().unwrap_err()
    ));
    mailbox
        .accept(MailboxEnvelope {
            id: "retained".into(),
            source: "agent_message".into(),
            message: "hello".into(),
            reply_to: Some("task".into()),
            from: None,
            from_relationship: None,
            target: json!({"sessionId":"own"}),
            accepted_at: "now".into(),
            sequence: 0,
        })
        .unwrap();
    let response = handler(json!({"id":2,"method":"tools/call","params":{"name":"family_inbox","arguments":{"reply_to":"task","consume":true}}}), AbortSignal::never()).await.unwrap();
    let inbox: Value =
        serde_json::from_str(response["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(inbox["messages"][0]["id"], "retained");
    assert!(mailbox
        .inbox(&MailboxFilter::default(), 20, false)
        .unwrap()
        .is_empty());
}
