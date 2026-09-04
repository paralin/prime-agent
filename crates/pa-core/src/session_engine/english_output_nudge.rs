use pa_agent::agent::Agent;
use pa_agent::agent_loop::FilterAssistantMessageFn;
use pa_agent::types::{AgentMessage, AssistantContent, AssistantMessage, CustomAgentMessage};
use serde_json::json;
use std::sync::{Arc, Mutex, OnceLock, Weak};

pub const ENGLISH_OUTPUT_NUDGE_CUSTOM_TYPE: &str = "english_output_nudge";
pub const ENGLISH_OUTPUT_NUDGE_PROMPT: &str = "Continue the user's active task from the latest tool result. Use English for subsequent user-facing explanations. This is a language reminder, not a new task: do not reconstruct the conversation or repeat completed work. No reply to this notice is needed.";

/// # Panics
/// Panics if the regex engine no longer supports the `Unified_Ideograph` property.
#[must_use]
pub fn text_has_chinese(text: &str) -> bool {
    static HAN: OnceLock<fancy_regex::Regex> = OnceLock::new();
    let regex = HAN.get_or_init(|| {
        fancy_regex::Regex::new(r"\p{Unified_Ideograph}").expect("Han Unicode property")
    });
    let total = text.chars().count();
    let han = regex.find_iter(text).filter_map(Result::ok).count();
    han >= 2 && han.saturating_mul(20) >= total
}

#[must_use]
pub fn needs_english_output_nudge(message: &AssistantMessage) -> bool {
    message.content.iter().any(|block| matches!(block, AssistantContent::Text(text) if text_has_chinese(&text.text)))
}

#[derive(Default)]
pub struct EnglishOutputNudgeRuntime {
    agent: Mutex<Option<Weak<Agent>>>,
    admission: Mutex<Option<super::tool_error_nudge::NudgeAdmissionSink>>,
}

impl EnglishOutputNudgeRuntime {
    pub fn set_admission_sink(&self, sink: super::tool_error_nudge::NudgeAdmissionSink) {
        *self
            .admission
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(sink);
    }
    pub fn bind(&self, agent: &Arc<Agent>) {
        *self
            .agent
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Arc::downgrade(agent));
    }

    #[must_use]
    pub fn filter_hook(self: &Arc<Self>) -> FilterAssistantMessageFn {
        let runtime = self.clone();
        Arc::new(move |message| {
            if !needs_english_output_nudge(&message) {
                return Ok(Some(message));
            }
            let agent = runtime
                .agent
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_ref()
                .and_then(Weak::upgrade);
            let notice = AgentMessage::Custom(CustomAgentMessage {
                role: "custom".into(),
                payload: json!({"customType":ENGLISH_OUTPUT_NUDGE_CUSTOM_TYPE, "content":ENGLISH_OUTPUT_NUDGE_PROMPT, "display":true, "timestamp":super::now_millis()}),
            });
            let sink = runtime
                .admission
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            if let Some(sink) = sink {
                sink(notice)?;
            } else if let Some(agent) = agent {
                if !agent
                    .steering_previews()
                    .iter()
                    .any(|preview| preview == ENGLISH_OUTPUT_NUDGE_PROMPT)
                {
                    agent.steer(notice);
                }
            }
            Ok(Some(message))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::manager::SessionManager;
    use crate::session_engine::AgentSession;
    use pa_agent::agent::{AgentInitialState, AgentOptions};
    use pa_agent::scripted::ScriptedProvider;
    use pa_agent::types::{Message, Model};

    #[test]
    fn density_ignores_stray_glyphs_and_preserves_thinking_and_tools() {
        assert!(!text_has_chinese("中"));
        assert!(!text_has_chinese(&format!("{}中文", "x".repeat(39))));
        assert!(text_has_chinese(&format!("{}中文", "x".repeat(38))));
        assert!(text_has_chinese("𠀀𠀁"));
        let message: AssistantMessage = serde_json::from_value(json!({
            "content":[{"type":"thinking","thinking":"中文"},{"type":"text","text":"中文"},{"type":"text","text":"English"},
                {"type":"toolCall","id":"1","name":"ipython","arguments":{"code":"print('中文')"}}],
            "api":"faux","provider":"faux","model":"faux","usage":pa_agent::types::Usage::zero(),"stopReason":"toolUse","timestamp":0
        })).unwrap();
        assert!(needs_english_output_nudge(&message));

    }

    #[test]
    fn filtering_propagates_failed_notice_admission() {
        let runtime = Arc::new(EnglishOutputNudgeRuntime::default());
        runtime.set_admission_sink(Arc::new(|_| anyhow::bail!("journal write failed")));
        let message: AssistantMessage = serde_json::from_value(json!({
            "content":[{"type":"text","text":"中文回答"}], "api":"faux",
            "provider":"faux", "model":"faux", "usage":pa_agent::types::Usage::zero(),
            "stopReason":"stop", "timestamp":0
        }))
        .unwrap();
        assert_eq!(
            runtime.filter_hook()(message).unwrap_err().to_string(),
            "journal write failed"
        );
    }

    #[tokio::test]
    async fn live_session_preserves_output_and_continues_after_language_notice() {
        let provider = Arc::new(ScriptedProvider::new(Model::unknown()));
        provider.push_text_turn("中文回答");
        provider.push_text_turn("English answer");
        let runtime = Arc::new(EnglishOutputNudgeRuntime::default());
        let agent = Arc::new(Agent::new(AgentOptions {
            initial_state: AgentInitialState::default(),
            filter_assistant_message: Some(runtime.filter_hook()),
            stream_fn: Some(provider.stream_fn()),
            convert_to_llm: Some(super::super::messages::engine_convert_to_llm()),
            ..Default::default()
        }));
        runtime.bind(&agent);
        let session = AgentSession::new(
            agent.clone(),
            SessionManager::in_memory(std::path::Path::new(".")),
            Vec::new(),
        )
        .await
        .unwrap();
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            session.prompt("answer", crate::session_engine::PromptOptions::default()),
        )
        .await
        .unwrap()
        .unwrap();
        let rows = agent.state().await.messages;
        assert_eq!(
            rows.iter()
                .filter(|row| matches!(row, AgentMessage::Standard(Message::Assistant(_))))
                .count(),
            2
        );
        assert!(!serde_json::to_string(&session.entries().await)
            .unwrap()
            .contains("中文回答"));
        let calls = provider.calls();
        assert_eq!(calls.len(), 2);
        let context = serde_json::to_string(&calls[1].messages).unwrap();
        assert!(context.contains(ENGLISH_OUTPUT_NUDGE_PROMPT));
        assert!(context.contains("<system-notice>"));
        assert!(!context.contains("中文回答"));
    }
}
