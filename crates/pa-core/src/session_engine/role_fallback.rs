use pa_agent::types::{AssistantContent, AssistantMessage, StopReason};

use super::provider_retry::{
    is_agent_lifecycle_failure, is_context_overflow_failure, is_faux_provider_queue_exhausted,
    provider_stream_failure_kind,
};

/// Whether a role may advance without discarding emitted output or changing refusal semantics.
#[must_use]
pub fn can_advance_role_candidate(message: &AssistantMessage, window: u64) -> bool {
    message.stop_reason == StopReason::Error
        && !is_agent_lifecycle_failure(message)
        && !is_faux_provider_queue_exhausted(message)
        && !is_context_overflow_failure(message, window)
        && !matches!(
            provider_stream_failure_kind(message).as_deref(),
            Some("refusal" | "safety" | "invalid_request")
        )
        && !message.content.iter().any(|content| match content {
            AssistantContent::Text(text) => !text.text.trim().is_empty(),
            AssistantContent::Thinking(thinking) => !thinking.thinking.trim().is_empty(),
            AssistantContent::ToolCall(_) => true,
        })
}
