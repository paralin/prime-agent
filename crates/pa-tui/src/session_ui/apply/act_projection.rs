use serde_json::{json, Value};
use std::collections::HashSet;
use std::fmt::Write;

use super::{AgentView, ChatEntry, SessionUi, ToolResultView};

fn retain_tail(text: &str) -> String {
    let mut units = 0;
    let chars: Vec<_> = text
        .chars()
        .rev()
        .take_while(|character| {
            units += character.len_utf16();
            units <= 65_536
        })
        .collect();
    chars.into_iter().rev().collect()
}

impl SessionUi {
    pub(super) fn apply_act_projection(&mut self, event: &Value, view: &mut AgentView) {
        apply_projection(event, view, &mut self.pending_tools, &self.aborted_tools);
    }
}

fn apply_projection(
    event: &Value,
    view: &mut AgentView,
    pending_tools: &mut HashSet<String>,
    aborted_tools: &HashSet<String>,
) {
    let Some(act_id) = event["actId"].as_str().filter(|id| !id.is_empty()) else {
        return;
    };
    let Some(sequence) = event["sequence"].as_u64() else {
        return;
    };
    let id = format!("act:{act_id}");
    if aborted_tools.contains(&id) {
        return;
    }
    let index = view
        .chat
        .iter()
        .rposition(|row| matches!(row,ChatEntry::Tool(card) if card.id == id));
    let index = if let Some(index) = index {
        index
    } else {
        crate::snapshot::apply_tool_execution_start(
            view,
            &id,
            "Act",
            json!({
                "prompt":event["prompt"],"model":event["model"],"depth":event["depth"],"parentActId":event["parentActId"]
            }),
        );
        pending_tools.insert(id.clone());
        view.chat.len() - 1
    };
    let Some(ChatEntry::Tool(card)) = view.chat.get(index) else {
        return;
    };
    if card.aborted || card.ended_at.is_some() {
        return;
    }
    let previous_sequence = card
        .result
        .as_ref()
        .and_then(|result| result.details["actSequence"].as_u64())
        .unwrap_or(0);
    if sequence <= previous_sequence {
        return;
    }
    let mut text = card
        .result
        .as_ref()
        .and_then(|result| result.content.first())
        .and_then(|block| block["text"].as_str())
        .unwrap_or_default()
        .to_string();
    let kind = event["event"].as_str().unwrap_or_default();
    match kind {
        "start" => text.push_str(event["prompt"].as_str().unwrap_or_default()),
        "assistant_delta" => text.push_str(event["text"].as_str().unwrap_or_default()),
        "cell_start" => {
            let _ = write!(
                text,
                "\n\n[Python]\n{}\n",
                event["code"].as_str().unwrap_or_default()
            );
        }
        "cell_terminal" => {
            for field in ["stdout", "stderr", "result", "error"] {
                if let Some(value) = event[field].as_str().filter(|text| !text.is_empty()) {
                    let _ = write!(text, "\n[{field}]\n{value}");
                }
            }
        }
        "terminal" => {
            let _ = write!(
                text,
                "\n\n[Act {}]",
                event["status"].as_str().unwrap_or("settled")
            );
            if let Some(error) = event["error"].as_str() {
                let _ = write!(text, "\n{error}");
            }
        }
        _ => return,
    }
    view.prepare_entry_mutation(index);
    if let Some(ChatEntry::Tool(card)) = view.chat.get_mut(index) {
        card.result = Some(ToolResultView {
            content: vec![json!({"type":"text","text":retain_tail(&text)})],
            details: json!({"actSequence":sequence,"actId":act_id,"depth":event["depth"],"status":event["status"],"usage":event["usage"]}),
            is_error: kind == "terminal" && event["status"] != "done",
        });
        card.result_partial = kind != "terminal";
        if kind == "terminal" {
            card.ended_at = Some(std::time::Instant::now());
            pending_tools.remove(&id);
        }
    }
    view.mark_entry_stale(index);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn act_output_tail_stays_bounded_without_splitting_unicode() {
        let text = format!("old{}new", "😀".repeat(40_000));
        let tail = retain_tail(&text);
        assert!(tail.encode_utf16().count() <= 65_536);
        assert!(tail.ends_with("new"));
        assert!(!tail.starts_with("old"));
    }

    #[test]
    fn nested_act_cards_stream_independently_ignore_replays_and_settle_once() {
        let mut view = AgentView::new(crate::theme::Theme::builtin(
            "prime",
            crate::theme::ColorMode::TrueColor,
        ));
        let mut pending = HashSet::new();
        let aborted = HashSet::new();
        let event = |act: &str, kind: &str, sequence: u64, text: &str| json!({"type":"act_event","actId":act,"depth":2,"parentActId":"parent", "event":kind,"sequence":sequence,"prompt":text,"text":text,"status":"done"});
        for act in ["parent", "child"] {
            apply_projection(
                &event(act, "start", 1, act),
                &mut view,
                &mut pending,
                &aborted,
            );
        }
        apply_projection(
            &event("child", "assistant_delta", 2, " answer"),
            &mut view,
            &mut pending,
            &aborted,
        );
        apply_projection(
            &event("child", "assistant_delta", 2, " duplicate"),
            &mut view,
            &mut pending,
            &aborted,
        );
        apply_projection(
            &event("child", "terminal", 3, ""),
            &mut view,
            &mut pending,
            &aborted,
        );
        apply_projection(
            &event("child", "assistant_delta", 4, " late"),
            &mut view,
            &mut pending,
            &aborted,
        );
        assert_eq!(view.chat.len(), 2);
        let ChatEntry::Tool(child) = &view.chat[1] else {
            panic!("Act card");
        };
        assert!(child.ended_at.is_some());
        assert!(!child.result_partial);
        assert_eq!(
            child.result.as_ref().unwrap().content[0]["text"],
            "child answer\n\n[Act done]"
        );
        assert!(pending.contains("act:parent"));
        assert!(!pending.contains("act:child"));
    }
}
