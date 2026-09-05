use super::{compact_session::CompactOutcome, AgentSession};
use std::sync::atomic::Ordering;

impl AgentSession {
    /// Recover once from a failed reasoning-only turn through the configured scratch route.
    ///
    /// # Errors
    /// Returns closeout or durable compaction failures; original context remains retained.
    #[tracing::instrument(skip_all)]
    pub async fn recover_reasoning_exhaustion(
        &self,
        model: &pa_types::ai::Model,
        api_key: Option<String>,
    ) -> anyhow::Result<bool> {
        let Some(pa_types::session::AgentMessage::Assistant(message)) =
            self.last_assistant_message().await
        else {
            return Ok(false);
        };
        if message.stop_reason != pa_types::ai::StopReason::Error
            || message.provider != model.provider || message.model != model.id
            || !message.diagnostics.as_ref().is_some_and(|entries| entries.iter().any(|entry| entry.type_ == "provider_warning" && entry.error.as_ref().is_some_and(|error| matches!(error.code.as_ref(), Some(pa_types::ai::DiagnosticCode::Str(code)) if code == "reasoning_exhausted"))))
            || !self.auto_compaction_enabled() { return Ok(false); }
        let scratch = self
            .scratch_handoff
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if !scratch.is_some_and(|settings| {
            settings.enabled
                && settings.strategy != crate::settings::types::CompactionStrategy::Default
        }) || !model.input.contains(&pa_types::ai::ModelInput::Image)
            || self
                .reasoning_recovery_attempted
                .swap(true, Ordering::AcqRel)
        {
            return Ok(false);
        }
        let result = self.compact(Some("Reasoning exhausted: save the active task checkpoint using the current model, then resume once from compacted context. Do not repeat completed actions."), model, api_key, None).await?;
        Ok(matches!(result, CompactOutcome::Ran(_)))
    }
}
