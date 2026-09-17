use std::sync::Arc;

use anyhow::{Context, Result};
use pa_agent::abort::AbortSignal;
use serde_json::{json, Value};

use crate::kernel::shared::{HostRequestHandlers, HostRequestPayload};
use crate::session_engine::agent_messaging::mailbox::runtime::MailboxProvider;
use crate::session_engine::agent_messaging::mailbox::{
    normalize_filter, normalize_limit, normalize_timeout,
};
use crate::session_engine::agent_messaging::{
    create_agent_session_message_id, register_agent_message_host_handlers, AgentMessageController,
};

use super::transport::McpHandler;

const PROTOCOL_VERSIONS: &[&str] = &[
    "2025-11-25",
    "2025-06-18",
    "2025-03-26",
    "2024-11-05",
    "2024-10-07",
];

pub fn family_mcp_handler<C: AgentMessageController + 'static>(
    controller: Arc<C>,
    mailbox: MailboxProvider,
) -> McpHandler {
    let mut handlers = HostRequestHandlers::default();
    register_agent_message_host_handlers(controller, &mut handlers);
    Arc::new(move |request, signal| {
        let handlers = handlers.clone();
        let mailbox = mailbox.clone();
        Box::pin(async move {
            let id = request.get("id").cloned().unwrap_or(Value::Null);
            if request["method"] == "notifications/initialized"
                || request["method"] == "notifications/cancelled"
            {
                return Ok(Value::Null);
            }
            let result = match request["method"].as_str() {
                Some("initialize") => {
                    let version = request["params"]["protocolVersion"]
                        .as_str()
                        .filter(|version| PROTOCOL_VERSIONS.contains(version))
                        .unwrap_or(PROTOCOL_VERSIONS[0]);
                    json!({"protocolVersion":version, "capabilities":{"tools":{}}, "serverInfo":{"name":"prime","version":"1.0.0"},
                        "instructions":"Prime Agent coordinates the family. Use these tools only for your parent and siblings; replies correlate with message id and replyTo."})
                }
                Some("ping") => json!({}),
                Some("tools/list") => json!({"tools":tool_definitions()}),
                Some("tools/call") => {
                    let call = call_tool(&handlers, &mailbox, &request["params"], &signal);
                    let value = tokio::select! {
                        biased;
                        () = signal.aborted() => return Err(pa_agent::abort::aborted_error()),
                        result = call => result,
                    };
                    match value {
                        Ok(value) => {
                            json!({"content":[{"type":"text", "text":serde_json::to_string(&value)?}]})
                        }
                        Err(error) => {
                            json!({"isError":true, "content":[{"type":"text", "text":error.to_string()}]})
                        }
                    }
                }
                _ => {
                    return Ok(
                        json!({"jsonrpc":"2.0", "id":id, "error":{"code":-32601,"message":"Method not found"}}),
                    )
                }
            };
            Ok(json!({"jsonrpc":"2.0", "id":id, "result":result}))
        })
    })
}

async fn call_tool(
    handlers: &HostRequestHandlers,
    mailbox: &MailboxProvider,
    params: &Value,
    signal: &AbortSignal,
) -> Result<Value> {
    let name = params["name"]
        .as_str()
        .context("Prime family tool name is required")?;
    let mut args = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let fields = args
        .as_object_mut()
        .context("Prime family tool arguments must be an object")?;
    let allowed: &[&str] = match name {
        "family_list" => &[],
        "family_send" => &[
            "receiver_role",
            "receiver_name",
            "message",
            "message_id",
            "reply_to",
        ],
        "family_inbox" => &["limit", "consume", "sender", "reply_to"],
        "family_wait" => &["timeout_ms", "sender", "reply_to"],
        _ => anyhow::bail!("Unknown Prime family tool"),
    };
    anyhow::ensure!(
        fields.keys().all(|key| allowed.contains(&key.as_str())),
        "Unsupported Prime family tool argument"
    );
    match name {
        "family_list" => {
            let handler = handlers
                .get("agent_message.list_agents")
                .context("Prime family roster unavailable")?;
            let mut value = handler(HostRequestPayload {
                data: args,
                cell_source_code: None,
            })
            .await?;
            if let Some(entries) = value["entries"].as_array_mut() {
                entries.retain(|entry| {
                    matches!(entry["relationship"].as_str(), Some("parent" | "sibling"))
                });
            }
            Ok(value)
        }
        "family_send" => {
            anyhow::ensure!(
                matches!(
                    fields.get("receiver_role").and_then(Value::as_str),
                    Some("parent" | "sibling")
                ),
                "receiver_role must be parent or sibling"
            );
            let id = fields
                .remove("message_id")
                .unwrap_or_else(|| json!(create_agent_session_message_id()));
            fields.insert("id".into(), id);
            let handler = handlers
                .get("agent_message.send")
                .context("Prime family delivery unavailable")?;
            handler(HostRequestPayload {
                data: args,
                cell_source_code: None,
            })
            .await
        }
        "family_inbox" => {
            let filter = normalize_filter(&args)?;
            let limit = normalize_limit(args.get("limit"))?;
            let consume = match args.get("consume") {
                None => false,
                Some(Value::Bool(value)) => *value,
                _ => anyhow::bail!("consume must be a boolean"),
            };
            Ok(json!({"messages":mailbox()?.inbox(&filter,limit,consume)?}))
        }
        "family_wait" => {
            let filter = normalize_filter(&args)?;
            let timeout = normalize_timeout(args.get("timeout_ms"))?;
            let message = mailbox()?.wait(&filter, timeout, signal).await?;
            Ok(message.map_or_else(|| json!({}), |message| json!({"message":message})))
        }
        _ => unreachable!(),
    }
}

fn tool_definitions() -> Vec<Value> {
    let filter = json!({"sender":{"type":"string","minLength":1}, "reply_to":{"type":"string","minLength":1}});
    let mut inbox = filter.clone();
    inbox["limit"] = json!({"type":"integer","minimum":1,"maximum":100,"default":20});
    inbox["consume"] = json!({"type":"boolean","default":false});
    let mut wait = filter;
    wait["timeout_ms"] = json!({"type":"integer","minimum":1,"maximum":300_000,"default":30_000});
    [
        ("family_list", "List this Claude child's parent and siblings.", json!({}), vec![]),
        ("family_send", "Send one correlated message to the parent or one named sibling.", json!({
            "receiver_role":{"type":"string","enum":["parent","sibling"]}, "receiver_name":{"type":"string","minLength":1},
            "message":{"type":"string","minLength":1}, "message_id":{"type":"string","minLength":1}, "reply_to":{"type":"string","minLength":1}
        }), vec!["receiver_role","message"]),
        ("family_inbox", "Peek or consume retained family messages in oldest-first order.", inbox, vec![]),
        ("family_wait", "Wait for and consume the oldest matching family message.", wait, vec![]),
    ].into_iter().map(|(name,description,properties,required)| json!({"name":name, "description":description,
        "inputSchema":{"type":"object","properties":properties,"required":required,"additionalProperties":false}})).collect()
}

#[cfg(test)]
mod tests;
