use super::AgentSession;
use serde_json::json;
use std::sync::atomic::Ordering;

pub(super) const REASONING_OUTPUT_NUDGE_CUSTOM_TYPE: &str = "reasoning_output_nudge";
pub(super) const REASONING_OUTPUT_NUDGE_PROMPT: &str = "Collect more information or decide how to combine together existing information you collected before you try to think through what to do next. Once you are better informed you can decide what the next step is.";

impl AgentSession {
    /// Remove failed thinking from live context and admit one evidence nudge until progress.
    /// The durable failure remains available in scrollback.
    ///
    /// # Errors
    /// Returns conversion errors when loop and session message schemas disagree.
    #[tracing::instrument(skip_all)]
    pub async fn recover_reasoning_exhaustion(
        &self,
        model: &pa_types::ai::Model,
        _api_key: Option<String>,
    ) -> anyhow::Result<bool> {
        if self.auxiliary_model.as_ref().is_some_and(|context| {
            !crate::settings::SettingsManager::create(&context.cwd, &context.agent_dir)
                .get_provider_retry_policy()
                .enabled
        }) {
            return Ok(false);
        }
        let Some(pa_types::session::AgentMessage::Assistant(message)) =
            self.last_assistant_message().await
        else {
            return Ok(false);
        };
        if message.provider != model.provider
            || message.model != model.id
            || !pa_ai::utils::diagnostics::is_reasoning_exhausted_response(&message)
        {
            return Ok(false);
        }
        let failed: pa_agent::types::AssistantMessage =
            serde_json::from_value(serde_json::to_value(message)?)?;
        self.agent.mutate_messages(|messages| messages.retain(|message| !matches!(super::standard_message(message), Some(pa_agent::types::Message::Assistant(assistant)) if assistant == &failed))).await;
        if self
            .reasoning_recovery_attempted
            .swap(true, Ordering::AcqRel)
        {
            return Ok(false);
        }
        self.agent.steer(pa_agent::types::AgentMessage::Custom(pa_agent::types::CustomAgentMessage {
            role: "custom".into(), payload: json!({"customType":REASONING_OUTPUT_NUDGE_CUSTOM_TYPE,"content":REASONING_OUTPUT_NUDGE_PROMPT,"display":true,"timestamp":super::now_millis()})
        }));
        if let Some(telemetry) = &self.skill_telemetry {
            telemetry.note_feature_outcome("reasoning_recovery", "completed", None);
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::manager::SessionManager;
    use pa_agent::agent::{Agent, AgentInitialState, AgentOptions};
    use std::sync::Arc;

    #[tokio::test]
    async fn evidence_nudge_is_bounded_and_preserves_the_durable_failure() {
        let registration = pa_ai::faux::register_faux_provider(
            pa_ai::faux::RegisterFauxProviderOptions::default(),
        );
        let model = registration.get_model();
        let failed = json!({"role":"assistant","content":[{"type":"thinking","thinking":"failed deliberation"}],"api":model.api,"provider":model.provider,"model":model.id,"usage":pa_agent::types::Usage::zero(),"stopReason":"error","timestamp":12,"diagnostics":[{"type":"provider_warning","timestamp":12,"error":{"code":"reasoning_exhausted","message":"reasoning exhausted"}}]});
        let mut store = SessionManager::in_memory(std::path::Path::new("."));
        store
            .append_message(serde_json::from_value(failed.clone()).unwrap())
            .unwrap();
        let agent_model: pa_agent::types::Model =
            serde_json::from_value(serde_json::to_value(&model).unwrap()).unwrap();
        let provider = Arc::new(pa_agent::scripted::ScriptedProvider::new(
            agent_model.clone(),
        ));
        provider.push_text_turn("recovered");
        let agent = Arc::new(Agent::new(AgentOptions {
            initial_state: AgentInitialState {
                model: Some(agent_model),
                messages: Some(vec![serde_json::from_value(failed).unwrap()]),
                ..Default::default()
            },
            stream_fn: Some(provider.stream_fn()),
            convert_to_llm: Some(super::super::messages::engine_convert_to_llm()),
            ..Default::default()
        }));
        let session = AgentSession::new(agent.clone(), store, vec![])
            .await
            .unwrap();
        let durable = session.entries().await;
        assert!(session
            .recover_reasoning_exhaustion(&model, /*_api_key*/ None)
            .await
            .unwrap());
        assert!(!session
            .recover_reasoning_exhaustion(&model, /*_api_key*/ None)
            .await
            .unwrap());
        assert_eq!(agent.steering_previews().len(), 1);
        assert!(agent.state().await.messages.is_empty());
        assert_eq!(session.entries().await, durable);
        agent.continue_run().await.unwrap();
        let calls = provider.calls();
        assert_eq!(calls.len(), 1);
        let context = serde_json::to_string(&calls[0].messages).unwrap();
        assert!(context.contains(REASONING_OUTPUT_NUDGE_PROMPT));
    }
}
