use std::collections::{HashMap, VecDeque};
use std::fmt::Write;

use serde_json::{json, Value};

use super::{AcpSessionUpdate, AcpToolKind, AcpToolStatus};
use crate::acp::meta::PRIME_AGENT_META_NAMESPACE;
use crate::acp::types::ToolCallContent;

const CONTENT_MAX: usize = 32_768;
const TERMINAL_RESERVE: usize = 4352;
const TRUNCATION_MARKER: &str = "\n[Act progress truncated]";
const PROGRESS_MAX: usize = CONTENT_MAX - TERMINAL_RESERVE - TRUNCATION_MARKER.len() - 1;

#[derive(Debug)]
struct ActiveAct {
    start: Value,
    sequence: u64,
    text: String,
    units: usize,
    pending: usize,
    updates: usize,
    truncated: bool,
    stream: String,
}

#[derive(Debug, Default)]
pub(super) struct ActMappingState {
    active: HashMap<String, ActiveAct>,
    closed: VecDeque<String>,
}

fn bounded(text: &str, maximum: usize) -> String {
    let mut units = 0;
    text.chars()
        .take_while(|character| {
            units += character.len_utf16();
            units <= maximum
        })
        .collect()
}

fn status(event: &Value) -> AcpToolStatus {
    if event["status"] == "done" {
        AcpToolStatus::Completed
    } else {
        AcpToolStatus::Failed
    }
}

fn metadata(event: &Value, start: &Value, truncated: bool) -> Value {
    let mut act = json!({
        "actId":event["actId"], "depth":event["depth"], "parentActId":event["parentActId"],
        "outerToolCallId":event["outerToolCallId"], "sequence":event["sequence"], "event":event["event"],
        "model":start["model"], "cancellationCapability":start["cancellationCapability"],
        "contentTruncated":truncated, "contentMaxChars":CONTENT_MAX,
    });
    match event["event"].as_str() {
        Some("terminal") => {
            act["terminalStatus"] = event["status"].clone();
            act["usage"] = event["usage"].clone();
        }
        Some("assistant_delta") => act["stream"] = event["stream"].clone(),
        Some("cell_start" | "cell_terminal") => {
            act["cellId"] = event["cellId"].clone();
            act["cellStatus"] = if event["event"] == "cell_start" {
                json!("start")
            } else {
                event["status"].clone()
            };
        }
        _ => {}
    }
    json!({PRIME_AGENT_META_NAMESPACE:{"act":act}})
}

impl ActiveAct {
    fn append(&mut self, text: &str) -> bool {
        if self.truncated || text.is_empty() {
            return false;
        }
        let retained = bounded(text, PROGRESS_MAX.saturating_sub(self.units));
        let retained_units = retained.encode_utf16().count();
        self.units += retained_units;
        self.pending += retained_units;
        self.text.push_str(&retained);
        let truncated_now = retained.len() < text.len();
        self.truncated = truncated_now;
        retained_units > 0 || truncated_now
    }

    fn content(&self) -> String {
        if self.truncated {
            format!("{}{TRUNCATION_MARKER}", self.text)
        } else {
            self.text.clone()
        }
    }
}

impl ActMappingState {
    fn remember_closed(&mut self, id: String) {
        self.closed.push_back(id);
        while self.closed.len() > 256 {
            self.closed.pop_front();
        }
    }

    pub(super) fn updates(&mut self, event: &Value) -> Vec<AcpSessionUpdate> {
        let Some(id) = event["actId"].as_str().filter(|id| !id.is_empty()) else {
            return vec![];
        };
        let Some(sequence) = event["sequence"].as_u64() else {
            return vec![];
        };
        if self.closed.iter().any(|closed| closed == id) {
            return vec![];
        }
        let kind = event["event"].as_str().unwrap_or_default();
        let tool_call_id = format!("prime-agent-act-{id}");
        if kind == "start" {
            if self.active.contains_key(id) || self.active.len() >= 128 {
                return vec![];
            }
            self.active.insert(
                id.into(),
                ActiveAct {
                    start: event.clone(),
                    sequence,
                    text: String::new(),
                    units: 0,
                    pending: 0,
                    updates: 0,
                    truncated: false,
                    stream: String::new(),
                },
            );
            return vec![AcpSessionUpdate::ToolCall {
                tool_call_id,
                title: format!(
                    "Act ({})",
                    event["model"]["id"].as_str().unwrap_or_default()
                ),
                kind: AcpToolKind::Execute,
                status: AcpToolStatus::InProgress,
                raw_input: json!({"prompt":event["prompt"]}),
                content: None,
                meta: Some(metadata(event, event, false)),
            }];
        }
        if kind == "terminal" {
            if self
                .active
                .get(id)
                .is_some_and(|active| sequence <= active.sequence)
            {
                return vec![];
            }
            let active = self.active.remove(id);
            self.remember_closed(id.into());
            let mut text = active.as_ref().map(ActiveAct::content).unwrap_or_default();
            if !text.is_empty() {
                text.push('\n');
            }
            let terminal = format!(
                "Act {}.{}",
                event["status"].as_str().unwrap_or("error"),
                event["error"]
                    .as_str()
                    .map(|error| format!("\n{error}"))
                    .unwrap_or_default()
            );
            let terminal_bounded = bounded(&terminal, TERMINAL_RESERVE);
            let truncated = active.as_ref().is_some_and(|active| active.truncated)
                || terminal_bounded.len() < terminal.len();
            text.push_str(&terminal_bounded);
            let content = Some(vec![ToolCallContent::new(text)]);
            let start = active.as_ref().map_or(event, |active| &active.start);
            let meta = Some(metadata(event, start, truncated));
            return vec![if active.is_some() {
                AcpSessionUpdate::ToolCallUpdate {
                    tool_call_id,
                    status: Some(status(event)),
                    content,
                    meta,
                }
            } else {
                AcpSessionUpdate::ToolCall {
                    tool_call_id,
                    title: format!(
                        "Act ({})",
                        event["model"]["id"].as_str().unwrap_or_default()
                    ),
                    kind: AcpToolKind::Execute,
                    status: status(event),
                    raw_input: json!({"prompt":event["prompt"]}),
                    content,
                    meta,
                }
            }];
        }
        let Some(active) = self.active.get_mut(id) else {
            return vec![];
        };
        if sequence <= active.sequence {
            return vec![];
        }
        let mut text = String::new();
        let force = matches!(kind, "cell_start" | "cell_terminal");
        match kind {
            "assistant_delta" => {
                let stream = event["stream"].as_str().unwrap_or("text");
                if stream != active.stream {
                    let _ = write!(text, "\n[{stream}]\n");
                    active.stream = stream.into();
                }
                text.push_str(event["text"].as_str().unwrap_or_default());
            }
            "cell_start" => {
                let _ = write!(
                    text,
                    "\nCell {} start:\n{}",
                    event["cellId"].as_str().unwrap_or_default(),
                    event["code"].as_str().unwrap_or_default()
                );
            }
            "cell_terminal" => {
                let _ = write!(
                    text,
                    "\nCell {} {}:",
                    event["cellId"].as_str().unwrap_or_default(),
                    event["status"].as_str().unwrap_or_default()
                );
                for field in ["stdout", "stderr", "result", "error"] {
                    if let Some(value) = event[field].as_str() {
                        let _ = write!(text, "\n{field}:\n{value}");
                    }
                }
            }
            _ => return vec![],
        }
        active.sequence = sequence;
        let was_truncated = active.truncated;
        if !active.append(&text) || active.updates >= 32 {
            return vec![];
        }
        if active.updates > 0
            && !force
            && active.pending < 1024
            && (was_truncated || !active.truncated)
        {
            return vec![];
        }
        active.updates += 1;
        active.pending = 0;
        vec![AcpSessionUpdate::ToolCallUpdate {
            tool_call_id,
            status: Some(AcpToolStatus::InProgress),
            content: Some(vec![ToolCallContent::new(active.content())]),
            meta: Some(metadata(event, &active.start, active.truncated)),
        }]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::wire_events::{wire_updates, WireMappingState};

    fn event(id: &str, kind: &str, sequence: u64) -> Value {
        json!({"type":"act_event","actId":id,"event":kind,"sequence":sequence,"depth":2,
            "parentActId":"parent", "outerToolCallId":"outer", "model":{"provider":"test","id":"model"},
            "prompt":"use retained kernel", "cancellationCapability":"posix-managed",
            "stream":"thinking", "text":"inspect state", "status":"done", "usage":{"input":3}})
    }

    #[test]
    fn standard_tool_lifecycle_is_capability_gated_and_replays_are_ignored() {
        let mut legacy = WireMappingState::default();
        assert!(wire_updates(&event("act", "start", 1), &mut legacy).is_empty());
        let mut state = WireMappingState::with_act_projection(true);
        let mut values = vec![];
        for (kind, sequence) in [("start", 1), ("assistant_delta", 2), ("terminal", 3)] {
            let current = event("act", kind, sequence);
            values.extend(
                wire_updates(&current, &mut state)
                    .iter()
                    .map(AcpSessionUpdate::to_bare_value),
            );
            assert!(wire_updates(&current, &mut state).is_empty());
        }
        assert_eq!(values.len(), 3);
        for value in &values {
            assert_eq!(value["toolCallId"], "prime-agent-act-act");
        }
        assert_eq!(values[0]["sessionUpdate"], "tool_call");
        assert_eq!(values[0]["status"], "in_progress");
        assert_eq!(values[2]["sessionUpdate"], "tool_call_update");
        assert_eq!(values[2]["status"], "completed");
        assert_eq!(
            values[2]["_meta"][PRIME_AGENT_META_NAMESPACE]["act"]["usage"]["input"],
            3
        );
        assert_eq!(
            values[2]["_meta"][PRIME_AGENT_META_NAMESPACE]["act"]["parentActId"],
            "parent"
        );
        assert!(values[2]["content"][0]["content"]["text"]
            .as_str()
            .unwrap()
            .contains("inspect state"));
        assert!(wire_updates(&event("act", "assistant_delta", 4), &mut state).is_empty());
    }

    #[test]
    fn terminal_reserve_and_update_bound_keep_nested_progress_bounded() {
        let mut state = ActMappingState::default();
        state.updates(&event("parent", "start", 1));
        state.updates(&event("child", "start", 1));
        let mut published = 0;
        for sequence in 2..100 {
            let mut current = event("child", "assistant_delta", sequence);
            current["text"] = json!("😀".repeat(1024));
            published += state.updates(&current).len();
        }
        assert!(published <= 32);
        let mut terminal = event("child", "terminal", 100);
        terminal["error"] = json!("😀".repeat(5000));
        terminal["status"] = json!("error");
        let value = state.updates(&terminal)[0].to_bare_value();
        let text = value["content"][0]["content"]["text"].as_str().unwrap();
        assert!(text.encode_utf16().count() <= CONTENT_MAX);
        assert!(text.contains("[Act progress truncated]"));
        assert!(text.contains("Act error."));
        assert_eq!(value["status"], "failed");
        assert!(state.active.contains_key("parent"));
        assert!(!state.active.contains_key("child"));
        let detached = state.updates(&event("late", "terminal", 1));
        assert_eq!(detached[0].to_bare_value()["sessionUpdate"], "tool_call");
        for index in 0..300 {
            state.updates(&event(&format!("terminal-{index}"), "terminal", 1));
        }
        assert_eq!(state.closed.len(), 256);
    }
}
