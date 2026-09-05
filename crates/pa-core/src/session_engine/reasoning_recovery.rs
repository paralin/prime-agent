use std::sync::atomic::Ordering;
use serde_json::json;
use super::AgentSession;

pub(super) const REASONING_OUTPUT_NUDGE_CUSTOM_TYPE: &str = "reasoning_output_nudge";
pub(super) const REASONING_OUTPUT_NUDGE_PROMPT: &str = "Collect more information or decide how to combine together existing information you collected before you try to think through what to do next. Once you are better informed you can decide what the next step is.";

impl AgentSession {
    /// Remove failed thinking from live context and admit one evidence nudge until progress.
    /// The durable failure remains available in scrollback.
    ///
    /// # Errors
    /// Returns conversion errors when loop and session message schemas disagree.
    #[tracing::instrument(skip_all)]
    pub async fn recover_reasoning_exhaustion(&self, model: &pa_types::ai::Model, _api_key: Option<String>) -> anyhow::Result<bool> {
        let Some(pa_types::session::AgentMessage::Assistant(message)) = self.last_assistant_message().await else { return Ok(false); };
        if message.provider != model.provider || message.model != model.id || !pa_ai::utils::diagnostics::is_reasoning_exhausted_response(&message) { return Ok(false); }
        let failed: pa_agent::types::AssistantMessage = serde_json::from_value(serde_json::to_value(message)?)?;
        self.agent.mutate_messages(|messages| messages.retain(|message| !matches!(super::standard_message(message), Some(pa_agent::types::Message::Assistant(assistant)) if assistant == &failed))).await;
        if self.reasoning_recovery_attempted.swap(true, Ordering::AcqRel) { return Ok(false); }
        self.agent.steer(pa_agent::types::AgentMessage::Custom(pa_agent::types::CustomAgentMessage {
            role: "custom".into(), payload: json!({"customType":REASONING_OUTPUT_NUDGE_CUSTOM_TYPE,"content":REASONING_OUTPUT_NUDGE_PROMPT,"display":true,"timestamp":super::now_millis()})
        }));
        Ok(true)
    }
}
