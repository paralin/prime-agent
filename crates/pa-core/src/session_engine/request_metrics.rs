use pa_agent::types::{
    AgentEvent, AgentMessage, AssistantContent, AssistantMessage, AssistantMessageDiagnostic,
    Message,
};
use serde_json::json;

#[derive(Default)]
pub(super) struct RequestMetrics {
    first_tool_call_ms: Option<u64>,
}

impl RequestMetrics {
    // The diagnostic ratio mirrors JavaScript number arithmetic for token counts.
    #[allow(clippy::cast_precision_loss)]
    pub(super) fn observe(
        &mut self,
        event: &mut AgentEvent,
        phase: &str,
    ) -> Option<(AssistantMessage, AssistantMessage)> {
        match event {
            AgentEvent::MessageStart {
                message: AgentMessage::Standard(Message::Assistant(_)),
            } => self.first_tool_call_ms = None,
            AgentEvent::MessageUpdate { message, .. } => {
                if let AgentMessage::Standard(Message::Assistant(assistant)) = message.as_ref() {
                    if assistant.content.iter().any(|part| matches!(part, AssistantContent::ToolCall(call) if !call.name.is_empty())) {
                        self.first_tool_call_ms.get_or_insert(super::now_millis().saturating_sub(u64::try_from(assistant.timestamp).unwrap_or_default()));
                    }
                }
            }
            AgentEvent::MessageEnd {
                message: AgentMessage::Standard(Message::Assistant(assistant)),
            } => {
                let prior = assistant.clone();
                let usage = &assistant.usage;
                let total = usage.input + usage.cache_read + usage.cache_write;
                let now = super::now_millis();
                assistant.diagnostics.get_or_insert_with(Vec::new).push(AssistantMessageDiagnostic {
                    kind: "agent_request_metrics".into(), timestamp: i64::try_from(now).unwrap_or(i64::MAX), error: None,
                    details: Some(json!({"phase":phase,"durationMs":now.saturating_sub(u64::try_from(assistant.timestamp).unwrap_or_default()),"firstToolCallMs":self.first_tool_call_ms,"uncachedInputTokens":usage.input,"cacheReadTokens":usage.cache_read,"cacheWriteTokens":usage.cache_write,"cacheReadRatio":(total>0).then(|| usage.cache_read as f64 / total as f64),"outputTokens":usage.output,"toolCalls":assistant.content.iter().filter(|part|matches!(part,AssistantContent::ToolCall(_))).count(),"thinkingChars":assistant.content.iter().map(|part|match part { AssistantContent::Thinking(thinking)=>thinking.thinking.encode_utf16().count(), AssistantContent::Text(_) | AssistantContent::ToolCall(_)=>0 }).sum::<usize>()})),
                });
                return Some((prior, assistant.clone()));
            }
            AgentEvent::AgentStart
            | AgentEvent::AgentEnd { .. }
            | AgentEvent::TurnStart
            | AgentEvent::TurnEnd { .. }
            | AgentEvent::ToolExecutionStart { .. }
            | AgentEvent::ToolExecutionUpdate { .. }
            | AgentEvent::ToolExecutionEnd { .. }
            | AgentEvent::MessageStart { .. }
            | AgentEvent::MessageEnd { .. } => {}
        }
        None
    }
}
