use super::*;
use pa_core::session_engine::agent_messaging::mailbox::MailboxFilter;
use pa_types::platform::transport::bind_transport;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

async fn server(
    socket: &std::path::Path,
    capable: bool,
    data: Value,
) -> tokio::task::JoinHandle<Option<Value>> {
    let listener = bind_transport(socket).await.unwrap();
    tokio::spawn(async move {
        let stream = listener.accept().await.unwrap();
        let (reader, mut writer) = stream.split();
        let hello = json!({"type":"daemon_hello", "capabilities":if capable { vec!["agent_message_mailbox"] } else { vec![] }});
        writer
            .write_all(format!("{hello}\n").as_bytes())
            .await
            .unwrap();
        let mut reader = BufReader::new(reader);
        let mut line = String::new();
        if reader.read_line(&mut line).await.unwrap() == 0 {
            return None;
        }
        let request: Value = serde_json::from_str(&line).unwrap();
        let response = crate::protocol::response_success(
            request["id"].as_str(),
            request["command"]["type"].as_str().unwrap(),
            Some(data),
        );
        writer
            .write_all(format!("{}\n", serde_json::to_string(&response).unwrap()).as_bytes())
            .await
            .unwrap();
        Some(request["command"].clone())
    })
}

fn controller(socket: std::path::PathBuf) -> LinkAgentMessageController {
    LinkAgentMessageController::new(
        Arc::new(SupervisorLink::new(socket)),
        "own".into(),
        "token".into(),
        Arc::default(),
        None,
    )
}

#[tokio::test]
async fn new_client_does_not_send_mailbox_commands_to_an_old_daemon() {
    for wait in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("old.sock");
        let server = server(&socket, false, json!({})).await;
        let client = controller(socket);
        let result = if wait {
            client.wait(MailboxFilter::default(), 1000).await
        } else {
            client.inbox(MailboxFilter::default(), 20, false).await
        };
        assert_eq!(
            result.unwrap_err().to_string(),
            "agent mailbox is not supported by this daemon"
        );
        assert!(server.await.unwrap().is_none());
    }
}

#[tokio::test]
async fn capable_daemon_receives_filters_and_returns_mailbox_payloads() {
    for wait in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("new.sock");
        let data = if wait {
            json!({"message":{"id":"answer", "replyTo":"question"}})
        } else {
            json!({"messages":[{"id":"answer", "replyTo":"question"}]})
        };
        let server = server(&socket, true, data.clone()).await;
        let client = controller(socket);
        let filter = MailboxFilter {
            sender: Some("parent".into()),
            reply_to: Some("question".into()),
        };
        let received = if wait {
            client.wait(filter, 1234).await
        } else {
            client.inbox(filter, 7, true).await
        }
        .unwrap();
        assert_eq!(received, data);
        let command = server.await.unwrap().unwrap();
        assert_eq!(command["activeSessionId"], "own");
        assert_eq!(command["sender"], "parent");
        assert_eq!(command["replyTo"], "question");
        if wait {
            assert_eq!(command["type"], "agent_message_wait");
            assert_eq!(command["timeoutMs"], 1234);
        } else {
            assert_eq!(command["type"], "agent_message_inbox");
            assert_eq!(command["limit"], 7);
            assert_eq!(command["consume"], true);
        }
    }
}

#[test]
fn mailbox_receipt_retains_original_identity_and_rejects_missing_requested_metadata() {
    let input = AgentMessageSendInput {
        target: "peer".into(),
        message: "retry body".into(),
        receiver_role: None,
        message_id: Some("stable".into()),
        reply_to: Some("question".into()),
    };
    let wire = json!({"id":"stable", "message":"original", "target":{"activeSessionId":"peer"},
        "replyTo":"question", "deliveryStatus":"queued", "handoff":"retry", "acceptedAt":"accepted", "targetSequence":3});
    let receipt = receipt_from_wire(&wire, &input).unwrap();
    assert_eq!(receipt.id, "stable");
    assert_eq!(receipt.message, "original");
    assert_eq!(receipt.mailbox_metadata.unwrap()["handoff"], "retry");
    let mut legacy = wire;
    legacy.as_object_mut().unwrap().remove("replyTo");
    assert!(receipt_from_wire(&legacy, &input).is_none());
    legacy["replyTo"] = json!("question");
    legacy["id"] = json!("fresh");
    assert!(receipt_from_wire(&legacy, &input).is_none());
}
