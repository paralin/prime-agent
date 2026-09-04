use pa_agent::types::{
    AgentMessage, AssistantContent, AssistantMessage, Message, ToolResultContent, ToolResultMessage,
};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex, Weak};

pub type NudgeAdmissionSink = Arc<dyn Fn(AgentMessage) -> anyhow::Result<()> + Send + Sync>;

#[derive(Default)]
pub struct ToolErrorNudgeRuntime {
    agent: Mutex<Option<Weak<pa_agent::agent::Agent>>>,
    admission: Mutex<Option<NudgeAdmissionSink>>,
}

impl ToolErrorNudgeRuntime {
    pub fn set_admission_sink(&self, sink: NudgeAdmissionSink) {
        *self
            .admission
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(sink);
    }
    pub fn bind(&self, agent: &Arc<pa_agent::agent::Agent>) {
        *self
            .agent
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Arc::downgrade(agent));
    }

    #[must_use]
    pub fn after_turn_hook(
        self: &Arc<Self>,
        probe: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
    ) -> pa_agent::agent_loop::ShouldStopAfterTurnFn {
        let runtime = self.clone();
        Arc::new(move |context| {
            let runtime = runtime.clone();
            let probe = probe.clone();
            Box::pin(async move {
                if let Some(kind) = consecutive_tool_error_nudge(&context.context.messages) {
                    let sink = runtime
                        .admission
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .clone();
                    if let Some(sink) = sink {
                        sink(kind.message(super::now_millis()))?;
                        return Ok(true);
                    }
                    let agent = runtime
                        .agent
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .as_ref()
                        .and_then(Weak::upgrade);
                    if let Some(agent) = agent {
                        agent.steer(kind.message(super::now_millis()));
                    }
                }
                Ok(probe.is_some_and(|probe| probe()))
            })
        })
    }
}

pub const TOOL_ERROR_NUDGE_CUSTOM_TYPE: &str = "tool_error_nudge";
pub const BASH_ERROR_NUDGE_PROMPT: &str = "Your bash calls are malformed or have syntax errors or otherwise are returning errors. Carefully take a step back and think through the syntax and what you're trying to do before trying again.";
pub const PYTHON_SYNTAX_NUDGE_PROMPT: &str = "Your Python has invalid syntax. Carefully take a step back and think through the syntax and what you're trying to do before trying again.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolErrorNudgeKind {
    Bash,
    PythonSyntax,
}

impl ToolErrorNudgeKind {
    #[must_use]
    pub fn prompt(self) -> &'static str {
        match self {
            Self::Bash => BASH_ERROR_NUDGE_PROMPT,
            Self::PythonSyntax => PYTHON_SYNTAX_NUDGE_PROMPT,
        }
    }

    #[must_use]
    pub fn message(self, timestamp: u64) -> AgentMessage {
        AgentMessage::Custom(pa_agent::types::CustomAgentMessage {
            role: "custom".into(),
            payload: json!({
                "customType":TOOL_ERROR_NUDGE_CUSTOM_TYPE, "content":self.prompt(), "display":true,
                "details":{"kind":match self {Self::Bash => "bash", Self::PythonSyntax => "python-syntax"}}, "timestamp":timestamp,
            }),
        })
    }
}

fn matches(pattern: &str, text: &str) -> bool {
    fancy_regex::Regex::new(pattern).is_ok_and(|regex| regex.is_match(text).unwrap_or(false))
}

#[must_use]
pub fn classify_tool_error_nudge(
    result: &ToolResultMessage,
    arguments: &Value,
) -> Option<ToolErrorNudgeKind> {
    let details = result.details.as_ref().unwrap_or(&Value::Null);
    if details["status"] == "aborted" {
        return None;
    }
    let error = details["error"]["ename"]
        .as_str()
        .or_else(|| details["errorEname"].as_str());
    if matches!(error, Some("SyntaxError" | "IndentationError" | "TabError")) {
        return Some(ToolErrorNudgeKind::PythonSyntax);
    }
    if result.tool_name == "bash" {
        return result.is_error.then_some(ToolErrorNudgeKind::Bash);
    }
    if result.tool_name != "ipython"
        || !matches(
            r"\bbash\s*\(",
            arguments["code"].as_str().unwrap_or_default(),
        )
    {
        return None;
    }
    let text = ["stdout", "stderr", "result"]
        .iter()
        .filter_map(|field| details[*field].as_str())
        .chain(result.content.iter().filter_map(|block| match block {
            ToolResultContent::Text(text) => Some(text.text.as_str()),
            ToolResultContent::Image(_) => None,
        }))
        .collect::<Vec<_>>()
        .join("\n");
    let nonzero_exit = fancy_regex::Regex::new(r"\bexit_code=(-?\d+)\b")
        .ok()
        .and_then(|regex| {
            regex
                .captures(&text)
                .ok()
                .flatten()
                .and_then(|captures| captures.get(1).map(|value| value.as_str().to_owned()))
        })
        .is_some_and(|value| value.parse::<i64>() != Ok(0));
    (result.is_error
        || details["status"] == "error"
        || nonzero_exit
        || matches(r"\btransport_error=True\b", &text))
    .then_some(ToolErrorNudgeKind::Bash)
}

fn has_thinking(message: &AssistantMessage) -> bool {
    message.content.iter().any(|block| matches!(block, AssistantContent::Thinking(thinking) if !thinking.thinking.trim().is_empty()))
}

#[must_use]
pub fn consecutive_tool_error_nudge(messages: &[AgentMessage]) -> Option<ToolErrorNudgeKind> {
    let mut count = 0;
    let mut kind = None;
    for (index, message) in messages.iter().enumerate().rev() {
        match message {
            AgentMessage::Custom(custom)
                if custom.payload["customType"] == TOOL_ERROR_NUDGE_CUSTOM_TYPE =>
            {
                break
            }
            AgentMessage::Standard(Message::User(_)) => break,
            AgentMessage::Standard(Message::Assistant(assistant)) if has_thinking(assistant) => {
                break
            }
            AgentMessage::Standard(Message::ToolResult(result)) => {
                if !matches!(result.tool_name.as_str(), "ipython" | "bash") {
                    break;
                }
                let call = messages[..index]
                    .iter()
                    .rev()
                    .find_map(|message| match message {
                        AgentMessage::Standard(Message::Assistant(assistant)) => assistant
                            .tool_calls()
                            .into_iter()
                            .find(|call| call.id == result.tool_call_id)
                            .map(|call| (assistant, call)),
                        _ => None,
                    });
                let Some((assistant, call)) = call else {
                    break;
                };
                if has_thinking(assistant) {
                    break;
                }
                let Some(classified) = classify_tool_error_nudge(result, &call.arguments) else {
                    break;
                };
                if kind.is_some_and(|kind| kind != classified) {
                    break;
                }
                kind = Some(classified);
                count += 1;
                if count >= 3 {
                    return kind;
                }
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::manager::SessionManager;
    use crate::session_engine::AgentSession;
    use crate::tools::tool_definition::{ToolDefinition, ToolExecutionResult};
    use pa_agent::agent::{Agent, AgentInitialState, AgentOptions};
    use pa_agent::scripted::ScriptedProvider;

    #[tokio::test]
    async fn native_session_steers_one_durable_notice_after_three_syntax_failures() {
        let provider = Arc::new(ScriptedProvider::new(pa_agent::types::Model::unknown()));
        for id in ["1", "2", "3"] {
            provider.push_tool_call_turn(
                None,
                vec![(id, "ipython", json!({"code":format!("print({id}")}))],
            );
        }
        provider.push_text_turn("I will fix the syntax.");
        let tool = super::super::tool_bridge::bridge_tool(ToolDefinition {
            name: "ipython".into(),
            label: "ipython".into(),
            description: "test Python".into(),
            prompt_snippet: String::new(),
            parameters: json!({"type":"object", "properties":{"code":{"type":"string"}}, "required":["code"]}),
            execution_mode: None,
            prepare_arguments: None,
            execute: Arc::new(|_, _, _, _| {
                Box::pin(async {
                    let mut result = ToolExecutionResult::text("invalid syntax");
                    result.details = Some(json!({"status":"error", "errorEname":"SyntaxError"}));
                    Ok(result)
                })
            }),
        });
        let nudge_runtime = Arc::new(ToolErrorNudgeRuntime::default());
        let agent = Arc::new(Agent::new(AgentOptions {
            initial_state: AgentInitialState {
                tools: Some(vec![tool]),
                ..Default::default()
            },
            stream_fn: Some(provider.stream_fn()),
            convert_to_llm: Some(super::super::messages::engine_convert_to_llm()),
            should_stop_after_turn: Some(nudge_runtime.after_turn_hook(None)),
            ..Default::default()
        }));
        nudge_runtime.bind(&agent);
        let session = AgentSession::new(
            agent.clone(),
            SessionManager::in_memory(std::path::Path::new(".")),
            Vec::new(),
        )
        .await
        .unwrap();
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            session.prompt(
                "run the cells",
                crate::session_engine::PromptOptions::default(),
            ),
        )
        .await
        .unwrap()
        .unwrap();
        let rows = agent.state().await.messages;
        assert_eq!(rows.iter().filter(|row| matches!(row, AgentMessage::Custom(custom) if custom.payload["customType"] == TOOL_ERROR_NUDGE_CUSTOM_TYPE)).count(), 1);
        let entries = session.entries().await;
        assert_eq!(entries.iter().filter(|entry| matches!(entry, pa_types::session::FileEntry::CustomMessage { payload, .. } if payload.custom_type == TOOL_ERROR_NUDGE_CUSTOM_TYPE)).count(), 1);
        let calls = provider.calls();
        assert_eq!(calls.len(), 4);
        let context = serde_json::to_value(&calls[3].messages)
            .unwrap()
            .to_string();
        assert!(context.contains("<system-notice>"));
        assert!(context.contains(PYTHON_SYNTAX_NUDGE_PROMPT));
    }

    fn messages(error: &str) -> Vec<AgentMessage> {
        let mut messages = Vec::new();
        for id in ["1", "2", "3"] {
            messages.push(serde_json::from_value(json!({"role":"assistant", "content":[{"type":"toolCall", "id":id, "name":"ipython", "arguments":{"code":"await bash('ls')"}}],
                "api":"faux", "provider":"faux", "model":"faux", "usage":pa_types::ai::Usage::default(), "stopReason":"toolUse", "timestamp":0})).unwrap());
            messages.push(serde_json::from_value(json!({"role":"toolResult", "toolCallId":id, "toolName":"ipython", "content":[], "isError":true,
                "details":{"status":"error", "errorEname":error}, "timestamp":0})).unwrap());
        }
        messages
    }

    #[tokio::test]
    async fn embedding_admission_stops_the_run_and_propagates_durable_write_errors() {
        let runtime = Arc::new(ToolErrorNudgeRuntime::default());
        let rows = messages("SyntaxError");
        let AgentMessage::Standard(Message::Assistant(assistant)) = &rows[4] else {
            panic!("assistant")
        };
        let context = pa_agent::types::ShouldStopAfterTurnContext {
            message: assistant.clone(),
            tool_results: vec![],
            new_messages: vec![],
            context: pa_agent::types::AgentContext {
                system_prompt: String::new(),
                messages: rows.clone(),
                tools: vec![],
            },
        };
        let notices = Arc::new(Mutex::new(Vec::new()));
        let collected = notices.clone();
        runtime.set_admission_sink(Arc::new(move |message| {
            collected.lock().unwrap().push(message);
            Ok(())
        }));
        assert!(runtime.after_turn_hook(None)(context.clone())
            .await
            .unwrap());
        assert_eq!(notices.lock().unwrap().len(), 1);
        runtime.set_admission_sink(Arc::new(|_| anyhow::bail!("journal write failed")));
        assert_eq!(
            runtime.after_turn_hook(None)(context)
                .await
                .unwrap_err()
                .to_string(),
            "journal write failed"
        );
    }

    #[test]
    fn failed_cells_trigger_once_and_thinking_success_or_mixed_errors_reset_the_streak() {
        assert_eq!(
            consecutive_tool_error_nudge(&messages("TypeError")),
            Some(ToolErrorNudgeKind::Bash)
        );
        assert_eq!(
            consecutive_tool_error_nudge(&messages("SyntaxError")),
            Some(ToolErrorNudgeKind::PythonSyntax)
        );
        let mut rows = messages("TypeError");
        rows.push(ToolErrorNudgeKind::Bash.message(0));
        assert_eq!(consecutive_tool_error_nudge(&rows), None);
        let mut rows = messages("TypeError");
        if let AgentMessage::Standard(Message::ToolResult(result)) = &mut rows[5] {
            result.is_error = false;
            result.details = Some(json!({"status":"ok", "result":"BashResult(exit_code=0)"}));
        }
        assert_eq!(consecutive_tool_error_nudge(&rows), None);
        let mut rows = messages("TypeError");
        if let AgentMessage::Standard(Message::ToolResult(result)) = &mut rows[5] {
            result.details = Some(json!({"errorEname":"SyntaxError"}));
        }
        assert_eq!(consecutive_tool_error_nudge(&rows), None);
        let mut rows = messages("TypeError");
        if let AgentMessage::Standard(Message::Assistant(assistant)) = &mut rows[4] {
            assistant.content.push(
                serde_json::from_value(json!({"type":"thinking", "thinking":"reconsider"}))
                    .unwrap(),
            );
        }
        assert_eq!(consecutive_tool_error_nudge(&rows), None);
    }

    #[test]
    fn normal_python_exceptions_and_aborts_are_ignored_but_transport_and_exit_failures_count() {
        let mut result = ToolResultMessage {
            tool_call_id: "1".into(),
            tool_name: "ipython".into(),
            content: Vec::new(),
            details: Some(json!({"status":"error"})),
            is_error: true,
            timestamp: 0,
        };
        assert_eq!(
            classify_tool_error_nudge(&result, &json!({"code":"raise ValueError()"})),
            None
        );
        result.details = Some(json!({"status":"aborted", "errorEname":"SyntaxError"}));
        assert_eq!(
            classify_tool_error_nudge(&result, &json!({"code":"bash(ls)"})),
            None
        );
        result.is_error = false;
        for text in [
            "BashResult(exit_code=-2)",
            "BashResult(transport_error=True)",
        ] {
            result.details = Some(json!({"status":"ok", "result":text}));
            assert_eq!(
                classify_tool_error_nudge(&result, &json!({"code":"await bash('ls')"})),
                Some(ToolErrorNudgeKind::Bash)
            );
        }
    }
}
