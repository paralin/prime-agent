use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use pa_agent::stream::AssistantMessageEvent;
use pa_agent::types::{AgentEvent, Model, ThinkingLevel};
use serde_json::{json, Value};

pub type ActEventSink = Arc<dyn Fn(Value) + Send + Sync>;

pub struct ActProjection {
    identity: Value,
    summary: Mutex<Value>,
    sequence: AtomicU64,
    sink: ActEventSink,
}

fn bounded(text: &str, max_units: usize) -> (String, bool) {
    let mut units = 0;
    let value: String = text
        .chars()
        .take_while(|character| {
            units += character.len_utf16();
            units <= max_units
        })
        .collect();
    let truncated = value.len() != text.len();
    (value, truncated)
}

impl ActProjection {
    #[must_use]
    pub fn new(
        identity: Value,
        prompt: &str,
        model: &Model,
        thinking: ThinkingLevel,
        sink: ActEventSink,
    ) -> Arc<Self> {
        let (prompt, truncated) = bounded(prompt, 16_384);
        Arc::new(Self {
            identity,
            summary: Mutex::new(json!({"prompt":prompt,"promptTruncated":truncated,
                "model":{"provider":model.provider,"id":model.id,"name":model.name},
                "thinkingLevel":format!("{thinking:?}").to_lowercase(),
                "cancellationCapability":if cfg!(unix) {"posix-managed"} else {"cooperative-only"}})),
            sequence: AtomicU64::new(0),
            sink,
        })
    }

    fn emit(&self, kind: &str, data: Value) {
        let mut event = self.identity.clone();
        let Some(fields) = event.as_object_mut() else {
            return;
        };
        fields.insert("type".into(), json!("act_event"));
        fields.insert("event".into(), json!(kind));
        fields.insert(
            "sequence".into(),
            json!(self.sequence.fetch_add(1, Ordering::Relaxed) + 1),
        );
        if let Value::Object(data) = data {
            fields.extend(data);
        }
        (self.sink)(event);
    }

    pub(crate) fn set_model(&self, model: &Model, thinking: ThinkingLevel) {
        let mut summary = self
            .summary
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        summary["model"] = json!({"provider":model.provider,"id":model.id,"name":model.name});
        summary["thinkingLevel"] = json!(format!("{thinking:?}").to_lowercase());
    }

    pub fn start(&self) {
        self.emit(
            "start",
            self.summary
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone(),
        );
    }

    pub fn terminal(&self, status: &str, usage: pa_types::ai::Usage, error: Option<&str>) {
        let mut summary = self
            .summary
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        summary["status"] = json!(status);
        summary["usage"] = json!(usage);
        if let Some(error) = error {
            let (error, truncated) = bounded(error, 4096);
            summary["error"] = json!(error);
            summary["errorTruncated"] = json!(truncated);
        }
        self.emit("terminal", summary);
    }

    pub fn agent_event(&self, event: &AgentEvent) {
        match event {
            AgentEvent::MessageUpdate {
                assistant_message_event,
                ..
            } => {
                let (stream, delta) = match assistant_message_event.as_ref() {
                    AssistantMessageEvent::TextDelta { delta, .. } => ("text", delta),
                    AssistantMessageEvent::ThinkingDelta { delta, .. } => ("thinking", delta),
                    _ => return,
                };
                let (text, truncated) = bounded(delta, 65_536);
                self.emit(
                    "assistant_delta",
                    json!({"stream":stream,"text":text,"textTruncated":truncated}),
                );
            }
            AgentEvent::ToolExecutionStart {
                tool_call_id,
                tool_name,
                args,
            } if tool_name == "shared_ipython" => {
                let (code, truncated) = bounded(args["code"].as_str().unwrap_or_default(), 65_536);
                self.emit(
                    "cell_start",
                    json!({"cellId":tool_call_id,"code":code,"codeTruncated":truncated}),
                );
            }
            AgentEvent::ToolExecutionEnd {
                tool_call_id,
                tool_name,
                result,
                is_error,
            } if tool_name == "shared_ipython" => {
                let details = &result.details;
                let mut data = json!({"cellId":tool_call_id,"status":if details["status"] == "aborted" {"cancelled"} else if *is_error || details["status"] == "error" || details["error"].as_str().is_some_and(|error| !error.is_empty()) {"error"} else {"ok"}});
                for field in ["stdout", "stderr", "result", "error"] {
                    let text = details[field].as_str().unwrap_or_default();
                    let (text, truncated) =
                        bounded(text, if field == "error" { 4096 } else { 65_536 });
                    data[field] = json!(text);
                    data[format!("{field}Truncated")] = json!(truncated);
                }
                if let Some(duration) = details.get("durationMs") {
                    data["durationMs"] = duration.clone();
                }
                self.emit("cell_terminal", data);
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn projection_preserves_assignment_identity_sequence_and_utf16_bounds() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let captured = events.clone();
        let projection = ActProjection::new(
            json!({"actId":"nested","depth":2,"parentActId":"parent","outerToolCallId":"cell"}),
            &"😀".repeat(9000),
            &Model::unknown(),
            ThinkingLevel::High,
            Arc::new(move |event| captured.lock().unwrap().push(event)),
        );
        projection.start();
        let mut backup = Model::unknown();
        backup.provider = "backup-provider".into();
        backup.id = "backup-model".into();
        backup.name = "Backup".into();
        projection.set_model(&backup, ThinkingLevel::Low);
        projection.terminal(
            "cancelled",
            pa_types::ai::Usage::default(),
            Some(&"😀".repeat(3000)),
        );
        let events = events.lock().unwrap();
        assert_eq!(
            events[1]["model"],
            json!({"provider":"backup-provider","id":"backup-model","name":"Backup"})
        );
        assert_eq!(events[1]["thinkingLevel"], "low");
        assert_eq!(
            events[0]["prompt"].as_str().unwrap().encode_utf16().count(),
            16_384
        );
        assert_eq!(
            events[1]["error"].as_str().unwrap().encode_utf16().count(),
            4096
        );
        for (index, event) in events.iter().enumerate() {
            assert_eq!(event["sequence"], index + 1);
            assert_eq!(event["actId"], "nested");
            assert_eq!(event["parentActId"], "parent");
            assert_eq!(event["depth"], 2);
            assert_eq!(event["outerToolCallId"], "cell");
        }
    }
}
