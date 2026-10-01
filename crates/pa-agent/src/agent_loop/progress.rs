use crate::types::{AssistantContent, AssistantMessage, StopReason, ToolResultMessage};
use serde_json::{json, Value};

#[derive(Default)]
pub(super) struct TurnProgress {
    incomplete: usize,
    signature: Option<String>,
    result_signature: Option<String>,
    batches: usize,
}

pub(super) fn incomplete(message: &AssistantMessage) -> bool {
    message.stop_reason == StopReason::Unknown
        || (matches!(message.stop_reason, StopReason::Stop | StopReason::Length)
            && !message.content.iter().any(
                |part| matches!(part, AssistantContent::Text(text) if !text.text.trim().is_empty()),
            )
            && message.tool_calls().is_empty())
}

impl TurnProgress {
    pub fn reset_tools(&mut self) {
        self.signature = None;
        self.result_signature = None;
        self.batches = 0;
    }
    pub fn observe_results(&mut self, results: &[ToolResultMessage]) {
        let signature = canonical(&json!(results.iter().map(|result| json!({"toolName":result.tool_name,"content":result.content,"isError":result.is_error})).collect::<Vec<_>>()));
        if self
            .result_signature
            .as_ref()
            .is_some_and(|previous| previous != &signature)
        {
            self.signature = None;
            self.batches = 0;
        }
        self.result_signature = Some(signature);
    }
    pub fn finalize(
        &mut self,
        mut message: AssistantMessage,
        repetition: Option<&super::RepetitionLoopConfig>,
    ) -> AssistantMessage {
        let calls = message.tool_calls();
        if calls.is_empty() {
            self.signature = None;
            self.batches = 0;
        } else {
            let mut signatures: Vec<_> = calls
                .iter()
                .map(|call| format!("{}:{}", call.name, canonical(&call.arguments)))
                .collect();
            signatures.sort();
            let signature = signatures.join("\n");
            if self.signature.as_ref() == Some(&signature) {
                self.batches += 1;
            } else {
                self.batches = 1;
                self.signature = Some(signature);
            }
            if self.batches >= 3 && repetition.is_none_or(|policy| policy.enabled != Some(false)) {
                let error = "Repetition loop detected: the model repeated the same tool call batch 3 times; the repeated tools were not executed";
                message.stop_reason = StopReason::Error;
                message.error_message = Some(error.into());
                message.diagnostics.get_or_insert_with(Vec::new).push(
                    crate::types::assistant_message_diagnostic(
                        "agent_repetition_loop",
                        &anyhow::anyhow!(error),
                        Some(json!({"threshold":3,"kind":"tool_call_batch"})),
                    ),
                );
            }
        }
        if matches!(message.stop_reason, StopReason::Error | StopReason::Aborted) {
            return message;
        }
        if incomplete(&message) {
            if message.stop_reason == StopReason::Length
                && message.diagnostics.as_ref().is_some_and(|entries| {
                    entries.iter().any(|entry| {
                        entry.kind == "provider_warning"
                            && entry
                                .error
                                .as_ref()
                                .and_then(|error| error.get("code"))
                                .and_then(Value::as_str)
                                == Some("reasoning_exhausted")
                    })
                })
            {
                message.stop_reason = StopReason::Error;
                message.error_message = Some("Provider exhausted the output budget on reasoning without producing an answer; increase the output budget or lower the reasoning effort".into());
                return message;
            }
            self.incomplete += 1;
            if self.incomplete >= 3 {
                message.stop_reason = StopReason::Error;
                message.error_message =
                    Some("Provider closed 3 consecutive streams without final content".into());
            }
        } else {
            self.incomplete = 0;
        }
        message
    }
}
fn canonical(value: &Value) -> String {
    match value {
        Value::Object(values) => {
            let mut keys: Vec<_> = values.keys().collect();
            keys.sort();
            format!(
                "{{{}}}",
                keys.into_iter()
                    .map(|key| format!("{}:{}", json!(key), canonical(&values[key])))
                    .collect::<Vec<_>>()
                    .join(",")
            )
        }
        Value::Array(values) => format!(
            "[{}]",
            values.iter().map(canonical).collect::<Vec<_>>().join(",")
        ),
        _ => value.to_string(),
    }
}
