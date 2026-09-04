//! Claude Code child-runtime stream contract and restricted tool mapping.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub mod family;
pub mod input;
pub mod runtime;
pub mod transport;

pub const DENIED_TOOLS: &[&str] = &["Agent", "Task", "SendMessage"];
pub const FAMILY_TOOLS: &[&str] = &[
    "mcp__prime__family_list",
    "mcp__prime__family_send",
    "mcp__prime__family_inbox",
    "mcp__prime__family_wait",
];
pub const COORDINATION_PROMPT: &str = "You are a child agent in a Prime Agent session tree. Complete the assigned outcome through the simplest complete approach. Run the smallest check that exercises the claimed behavior. Report failed checks, conflicting evidence, uncertainty, and untested limits when they change the decision. Prime Agent controls child creation and messaging among parents, siblings, and direct children. Use the assigned tools and return the result through Prime Agent. Agent, Task, and SendMessage are unavailable.";

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaudeCodeUsage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub total_tokens: u64,
    pub cost: f64,
    pub requests: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ClaudeCodeEvent {
    Init {
        model: String,
        tools: Vec<String>,
        version: String,
        session_id: String,
    },
    Assistant {
        text: Option<String>,
        usage: ClaudeCodeUsage,
    },
    ToolProgress {
        tool_use_id: String,
        tool_name: String,
        elapsed_seconds: f64,
    },
    Result {
        is_error: bool,
        text: String,
        usage: ClaudeCodeUsage,
    },
    Error(String),
    Aborted(Option<String>),
    Close,
}

fn text_field(value: &Value, field: &str) -> anyhow::Result<String> {
    value[field]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| anyhow::anyhow!("Claude Code omitted string field {field}"))
}

fn token_field(value: &Value, field: &str, optional: bool) -> anyhow::Result<u64> {
    match value.get(field) {
        None | Some(Value::Null) if optional => Ok(0),
        Some(value) => value
            .as_u64()
            .ok_or_else(|| anyhow::anyhow!("Claude Code returned invalid {field}")),
        None => anyhow::bail!("Claude Code omitted {field}"),
    }
}

fn usage(message: &Value, result: bool) -> anyhow::Result<ClaudeCodeUsage> {
    let source = if result {
        &message["usage"]
    } else {
        &message["message"]["usage"]
    };
    let input = token_field(source, "input_tokens", false)?;
    let output = token_field(source, "output_tokens", false)?;
    let cache_read = token_field(source, "cache_read_input_tokens", true)?;
    let cache_write = token_field(source, "cache_creation_input_tokens", true)?;
    let total_tokens = input
        .checked_add(output)
        .and_then(|total| total.checked_add(cache_read))
        .and_then(|total| total.checked_add(cache_write))
        .ok_or_else(|| anyhow::anyhow!("Claude Code token usage overflowed"))?;
    let cost = if result {
        message["total_cost_usd"]
            .as_f64()
            .filter(|cost| cost.is_finite() && *cost >= 0.0)
            .ok_or_else(|| anyhow::anyhow!("Claude Code returned invalid total_cost_usd"))?
    } else {
        0.0
    };
    Ok(ClaudeCodeUsage {
        input,
        output,
        cache_read,
        cache_write,
        total_tokens,
        cost,
        requests: if result {
            token_field(message, "num_turns", false)?
        } else {
            1
        },
    })
}

/// Map one SDK-compatible stream message into a child-runtime event.
///
/// # Errors
/// Returns an error for malformed recognized frames or invalid usage.
pub fn map_sdk_message(message: &Value) -> anyhow::Result<Option<ClaudeCodeEvent>> {
    let event = match message["type"].as_str() {
        Some("system") if message["subtype"] == "init" => ClaudeCodeEvent::Init {
            model: text_field(message, "model")?,
            tools: serde_json::from_value(message["tools"].clone())?,
            version: text_field(message, "claude_code_version")?,
            session_id: text_field(message, "session_id")?,
        },
        Some("assistant") => {
            let blocks = message["message"]["content"]
                .as_array()
                .ok_or_else(|| anyhow::anyhow!("Claude Code assistant content is not an array"))?;
            let text: String = blocks
                .iter()
                .filter(|block| block["type"] == "text")
                .map(|block| text_field(block, "text"))
                .collect::<anyhow::Result<Vec<_>>>()?
                .concat();
            ClaudeCodeEvent::Assistant {
                text: (!text.is_empty()).then_some(text),
                usage: usage(message, false)?,
            }
        }
        Some("tool_progress") => ClaudeCodeEvent::ToolProgress {
            tool_use_id: text_field(message, "tool_use_id")?,
            tool_name: text_field(message, "tool_name")?,
            elapsed_seconds: message["elapsed_time_seconds"]
                .as_f64()
                .filter(|elapsed| elapsed.is_finite() && *elapsed >= 0.0)
                .ok_or_else(|| {
                    anyhow::anyhow!("Claude Code returned invalid elapsed_time_seconds")
                })?,
        },
        Some("result") => {
            let success = message["subtype"] == "success";
            let text = if success {
                text_field(message, "result")?
            } else {
                serde_json::from_value::<Vec<String>>(message["errors"].clone())?.join("\n")
            };
            ClaudeCodeEvent::Result {
                is_error: !success || message["is_error"] == true,
                text,
                usage: usage(message, true)?,
            }
        }
        _ => return Ok(None),
    };
    Ok(Some(event))
}

/// # Errors
/// Rejects incomplete identity, denied tools, and missing required Prime tools.
pub fn validate_init(event: &ClaudeCodeEvent, required_tools: &[String]) -> anyhow::Result<()> {
    let ClaudeCodeEvent::Init {
        model,
        tools,
        session_id,
        ..
    } = event
    else {
        anyhow::bail!("Claude Code emitted an event before init");
    };
    anyhow::ensure!(
        !model.is_empty() && !session_id.is_empty(),
        "Claude Code init omitted session identity or model"
    );
    let denied: Vec<_> = DENIED_TOOLS
        .iter()
        .filter(|tool| tools.iter().any(|actual| actual == **tool))
        .copied()
        .collect();
    anyhow::ensure!(
        denied.is_empty(),
        "Claude Code exposed denied tools: {}",
        denied.join(", ")
    );
    let missing: Vec<_> = required_tools
        .iter()
        .filter(|tool| !tools.contains(tool))
        .map(String::as_str)
        .collect();
    anyhow::ensure!(
        missing.is_empty(),
        "Claude Code omitted required Prime tools: {}",
        missing.join(", ")
    );
    Ok(())
}

#[must_use]
pub fn user_input(text: &str) -> Value {
    json!({"type":"user", "message":{"role":"user", "content":[{"type":"text","text":text}]},
        "parent_tool_use_id":null, "origin":{"kind":"coordinator"}, "priority":"next", "shouldQuery":true})
}

/// Map the parent's effective tools without granting additional built-ins.
#[must_use]
pub fn native_tools(active_prime_tools: &[String]) -> Vec<String> {
    let mut tools = Vec::new();
    for name in active_prime_tools {
        let mapped: &[&str] = match name.as_str() {
            "ipython" => &["Read", "Grep", "Glob", "Bash", "Edit", "Write", "WebSearch"],
            "read" => &["Read"],
            "grep" => &["Grep"],
            "glob" => &["Glob"],
            "bash" => &["Bash"],
            "edit" => &["Edit"],
            "write" => &["Write"],
            "web_search" => &["WebSearch"],
            _ => &[],
        };
        for tool in mapped {
            if !tools.iter().any(|existing| existing == tool) {
                tools.push((*tool).to_string());
            }
        }
    }
    tools
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_mapping_preserves_usage_text_tools_and_failure_results() {
        let counts = json!({"input_tokens":3,"output_tokens":4,"cache_read_input_tokens":5,"cache_creation_input_tokens":6});
        let assistant = map_sdk_message(&json!({"type":"assistant","message":{"usage":counts,
            "content":[{"type":"text","text":"a"},{"type":"thinking","thinking":"hidden"},{"type":"text","text":"b"}]}})).unwrap().unwrap();
        let ClaudeCodeEvent::Assistant { text, usage } = assistant else {
            panic!("assistant")
        };
        assert_eq!(text.as_deref(), Some("ab"));
        assert_eq!(usage.total_tokens, 18);
        assert_eq!(usage.requests, 1);
        assert_eq!(usage.cost, 0.0);
        for (subtype, expected) in [("success", false), ("error_max_turns", true)] {
            let result = map_sdk_message(&json!({"type":"result","subtype":subtype,"result":"done",
                "errors":["failed","limit"],"is_error":false,"usage":counts,"total_cost_usd":0.25,"num_turns":2})).unwrap().unwrap();
            let ClaudeCodeEvent::Result {
                is_error,
                text,
                usage,
            } = result
            else {
                panic!("result")
            };
            assert_eq!(is_error, expected);
            assert_eq!(text, if expected { "failed\nlimit" } else { "done" });
            assert_eq!(usage.requests, 2);
            assert_eq!(usage.cost, 0.25);
        }
        let event = map_sdk_message(&json!({"type":"tool_progress","tool_use_id":"call","tool_name":"Read","elapsed_time_seconds":1.5})).unwrap().unwrap();
        assert!(matches!(
            event,
            ClaudeCodeEvent::ToolProgress {
                elapsed_seconds: 1.5,
                ..
            }
        ));
        assert!(map_sdk_message(&json!({"type":"user"})).unwrap().is_none());
        assert!(map_sdk_message(&json!({"type":"assistant","message":{"content":[],"usage":{"input_tokens":-1,"output_tokens":0}}})).is_err());
    }

    #[test]
    fn admission_rejects_denied_tools_and_requires_family_tools_and_identity() {
        let required: Vec<_> = FAMILY_TOOLS.iter().map(|tool| (*tool).into()).collect();
        let init = |tools: Vec<String>, model: &str| ClaudeCodeEvent::Init {
            model: model.into(),
            tools,
            session_id: "session".into(),
            version: "version".into(),
        };
        validate_init(&init(required.clone(), "sonnet"), &required).unwrap();
        for denied in DENIED_TOOLS {
            let mut tools = required.clone();
            tools.push((*denied).into());
            assert!(validate_init(&init(tools, "sonnet"), &required)
                .unwrap_err()
                .to_string()
                .contains(denied));
        }
        assert!(validate_init(&init(vec![], "sonnet"), &required).is_err());
        assert!(validate_init(&init(required.clone(), ""), &required).is_err());
        assert!(validate_init(&ClaudeCodeEvent::Close, &required).is_err());
        let mapped = map_sdk_message(&json!({"type":"system","subtype":"init","model":"sonnet",
            "tools":required,"session_id":"session","claude_code_version":"version"}))
        .unwrap()
        .unwrap();
        validate_init(&mapped, &required).unwrap();
    }

    #[test]
    fn tool_mapping_is_restricted_and_user_input_keeps_coordinator_metadata() {
        let selected = native_tools(&[
            "read".into(),
            "bash".into(),
            "read".into(),
            "unknown".into(),
        ]);
        assert_eq!(selected, vec!["Read", "Bash"]);
        let python = native_tools(&["ipython".into()]);
        assert_eq!(python.len(), 7);
        assert!(DENIED_TOOLS
            .iter()
            .all(|tool| !python.iter().any(|actual| actual == tool)));
        let input = user_input("assignment");
        assert_eq!(input["message"]["content"][0]["text"], "assignment");
        assert_eq!(input["origin"]["kind"], "coordinator");
        assert_eq!(input["priority"], "next");
        assert_eq!(input["shouldQuery"], true);
    }
}
