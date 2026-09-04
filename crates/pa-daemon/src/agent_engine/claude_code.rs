use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use pa_core::session_engine::claude_code::family::family_mcp_handler;
use pa_core::session_engine::claude_code::runtime::{
    ClaudeCodeRuntime, RuntimeSnapshot, RuntimeStatus,
};
use pa_core::session_engine::claude_code::transport::{start_query, QueryOptions};
use pa_core::session_engine::claude_code::{
    native_tools, ClaudeCodeUsage, COORDINATION_PROMPT, FAMILY_TOOLS,
};
use serde_json::{json, Value};
use tokio::task::JoinHandle;

use super::{AgentSessionEngine, EngineEvent, LinkAgentMessageController, PromptRequest};

pub(super) struct ClaudeQuery {
    model: String,
    thinking: Option<pa_types::ai::ModelThinkingLevel>,
    pub(super) runtime: Arc<ClaudeCodeRuntime>,
    task: Option<JoinHandle<()>>,
}

impl Drop for ClaudeQuery {
    fn drop(&mut self) {
        self.runtime.dispose();
    }
}

impl AgentSessionEngine {
    fn saved_claude_session_id(&self) -> Result<Option<String>> {
        let path = self
            .session_file
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let Some(path) = path.filter(|path| path.exists()) else {
            return Ok(None);
        };
        let store = crate::session_store::SessionFile::open(&path)?;
        Ok(store.branch().into_iter().rev().find_map(|entry| {
            (entry.type_ == "custom_message" && entry.fields["customType"] == "claude_code_session")
                .then(|| {
                    entry.fields["details"]["sessionId"]
                        .as_str()
                        .map(str::to_owned)
                })
                .flatten()
        }))
    }
    pub(super) fn claude_code_model(&self) -> Result<pa_types::ai::Model> {
        let selection = self.current_selection();
        let model = selection
            .model
            .as_deref()
            .context("Claude Code model is required")?;
        let id = model.strip_prefix("claude-code/").unwrap_or(model);
        anyhow::ensure!(!id.trim().is_empty(), "Claude Code model is required");
        serde_json::from_value(json!({"id":id,"name":format!("Claude Code {id}"),"api":"claude-code","provider":"claude-code","baseUrl":"",
            "reasoning":true,"thinkingLevelMap":{"off":null,"minimal":null,"low":"low","medium":"medium","high":"high","xhigh":"xhigh","max":"max"},
            "input":["text"],"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0},"contextWindow":0,"maxTokens":0})).map_err(Into::into)
    }
    pub(crate) fn is_claude_code_selection(&self) -> bool {
        let selection = self.current_selection();
        selection.provider.as_deref() == Some("claude-code")
            || selection
                .model
                .as_deref()
                .is_some_and(|model| model.starts_with("claude-code/"))
    }

    pub(super) fn abort_claude_query(&self, reason: &str) {
        if let Some(query) = self
            .claude_query
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
        {
            query.runtime.abort(reason.into());
        }
    }

    pub(super) async fn dispose_claude_query(&self) {
        let query = self
            .claude_query
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(mut query) = query {
            query.runtime.dispose();
            if let Some(mut task) = query.task.take() {
                if tokio::time::timeout(Duration::from_secs(5), &mut task)
                    .await
                    .is_err()
                {
                    task.abort();
                    let _ = task.await;
                }
            }
        }
    }

    fn create_claude_query(
        &self,
        prompt: String,
        resume_session_id: Option<String>,
    ) -> Result<Arc<ClaudeCodeRuntime>> {
        let selection = self.current_selection();
        let model = selection
            .model
            .as_deref()
            .context("Claude Code model is required")?
            .strip_prefix("claude-code/")
            .unwrap_or(selection.model.as_deref().unwrap());
        anyhow::ensure!(!model.trim().is_empty(), "Claude Code model is required");
        let settings =
            pa_core::settings::SettingsManager::create(self.cwd(), &self.config.agent_dir);
        let executable = settings
            .get_claude_code_executable()
            .context("Claude Code has no configured executable")?;
        let config = self
            .config
            .supervisor_link
            .as_ref()
            .context("Claude Code children require a daemon-backed family")?;
        let mailbox = self
            .mailbox_provider
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .context("Claude Code family mailbox unavailable")?;
        let controller = Arc::new(LinkAgentMessageController::new(
            self.link.clone(),
            config.active_session_id.clone(),
            config.worker_token.clone(),
            self.own_summary.clone(),
            self.children.clone(),
        ));
        let required_tools = FAMILY_TOOLS
            .iter()
            .map(|tool| (*tool).to_string())
            .collect::<Vec<_>>();
        let runtime = Arc::new(ClaudeCodeRuntime::new(
            prompt,
            model.into(),
            required_tools.clone(),
        ));
        let options = QueryOptions {
            executable: executable.into(),
            cwd: self.cwd(),
            model: model.into(),
            resume_session_id: resume_session_id.or(self.saved_claude_session_id()?),
            effort: selection
                .thinking
                .map(|level| level.wire_name().to_string()),
            append_system_prompt: Some(COORDINATION_PROMPT.into()),
            tools: native_tools(&["ipython".into()]),
            required_tools,
            mcp_handler: Some(family_mcp_handler(controller, mailbox)),
        };
        let task = self
            .runtime
            .block_on(async { start_query(runtime.clone(), options) })?;
        *self
            .claude_query
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(ClaudeQuery {
            model: model.into(),
            thinking: selection.thinking,
            runtime: runtime.clone(),
            task: Some(task),
        });
        Ok(runtime)
    }

    pub(super) fn run_claude_code_prompt(
        &self,
        request: PromptRequest,
        aborted: &dyn Fn() -> bool,
        emit: &mut dyn FnMut(EngineEvent) -> bool,
    ) {
        let outcome = self.run_claude_prompt_inner(request, aborted, emit);
        match outcome {
            Ok(true) => {
                emit(EngineEvent::DoneAborted);
            }
            Ok(false) => {
                emit(EngineEvent::Done(Ok(())));
            }
            Err(error) => {
                emit(EngineEvent::Done(Err(format!("{error:#}"))));
            }
        }
    }

    fn run_claude_prompt_inner(
        &self,
        request: PromptRequest,
        aborted: &dyn Fn() -> bool,
        emit: &mut dyn FnMut(EngineEvent) -> bool,
    ) -> Result<bool> {
        anyhow::ensure!(
            request.images.is_empty() && request.batch.iter().all(|row| row.images.is_empty()),
            "Claude Code child input does not support image attachments"
        );
        if aborted() {
            return Ok(true);
        }
        let accepted = match &request.custom_message {
            Some(message) => EngineEvent::CustomMessage(message.clone()),
            None => EngineEvent::UserMessage(
                json!({"role":"user","content":request.message,"timestamp":crate::util::now_ms()}),
            ),
        };
        if !emit(accepted) {
            return Ok(true);
        }
        let selection = self.current_selection();
        let selected_model = selection
            .model
            .as_deref()
            .unwrap_or_default()
            .strip_prefix("claude-code/")
            .unwrap_or(selection.model.as_deref().unwrap_or_default());
        let selection_changed = self
            .claude_query
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .is_some_and(|query| {
                query.model != selected_model || query.thinking != selection.thinking
            });
        let resume_session_id = if selection_changed {
            self.claude_query
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_ref()
                .and_then(|query| query.runtime.snapshot().session_id)
        } else {
            None
        };
        if selection_changed {
            self.runtime.block_on(self.dispose_claude_query());
        }
        let existing = self
            .claude_query
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .map(|query| query.runtime.clone());
        let before = existing
            .as_ref()
            .map(|runtime| runtime.snapshot().usage)
            .unwrap_or_default();
        let mut text = request.message;
        for row in request.batch {
            if !emit(EngineEvent::UserMessage(
                json!({"role":"user","content":row.text,"timestamp":crate::util::now_ms()}),
            )) {
                return Ok(true);
            }
            text.push_str("\n\n");
            text.push_str(&row.text);
        }
        let new_query = existing.is_none();
        let runtime = match existing {
            Some(runtime) => {
                runtime.deliver(text)?;
                runtime
            }
            None => self
                .create_claude_query(format!("[task from parent]\n\n{text}"), resume_session_id)?,
        };
        let mut snapshots = runtime.subscribe();
        let mut previous_text = runtime.snapshot().answer_preview;
        let mut persisted_identity = !new_query;
        self.runtime.block_on(async {
            loop {
                if aborted() {runtime.abort("RLM child cancelled".into()); return Ok(true);}
                let snapshot = snapshots.borrow_and_update().clone();
                if !persisted_identity {
                    if let Some(session_id) = &snapshot.session_id {
                        if !emit(EngineEvent::CustomMessage(json!({"role":"custom","customType":"claude_code_session","content":"","display":false,
                            "details":{"sessionId":session_id,"model":snapshot.model},"timestamp":crate::util::now_ms()}))) {
                            runtime.abort("RLM child cancelled".into()); return Ok(true);
                        }
                        persisted_identity=true;
                    }
                }
                if snapshot.answer_preview != previous_text {
                    previous_text.clone_from(&snapshot.answer_preview);
                    if !emit(EngineEvent::AssistantUpdate {message:crate::engine::AssistantSnapshot::Wire(assistant_value(&snapshot,&ClaudeCodeUsage::default())),stream_event:None}) {
                        runtime.abort("RLM child cancelled".into()); return Ok(true);
                    }
                }
                match snapshot.status {
                    RuntimeStatus::Done if snapshot.turn_idle => {
                        let usage = usage_delta(&snapshot.usage,&before);
                        if !emit(EngineEvent::AssistantMessage(assistant_value(&snapshot,&usage))) {return Ok(true);}
                        return Ok(false);
                    }
                    RuntimeStatus::Cancelled => return Ok(true),
                    RuntimeStatus::Error => anyhow::bail!("{}",snapshot.error.as_deref().unwrap_or("Claude Code query failed")),
                    _ => {}
                }
                tokio::select! {
                    result=snapshots.changed() => result.context("Claude Code runtime closed")?,
                    ()=tokio::time::sleep(Duration::from_millis(20)) => {},
                }
            }
        })
    }
}

fn usage_delta(total: &ClaudeCodeUsage, before: &ClaudeCodeUsage) -> ClaudeCodeUsage {
    ClaudeCodeUsage {
        input: total.input.saturating_sub(before.input),
        output: total.output.saturating_sub(before.output),
        cache_read: total.cache_read.saturating_sub(before.cache_read),
        cache_write: total.cache_write.saturating_sub(before.cache_write),
        total_tokens: total.total_tokens.saturating_sub(before.total_tokens),
        cost: (total.cost - before.cost).max(0.0),
        requests: total.requests.saturating_sub(before.requests),
    }
}

fn assistant_value(snapshot: &RuntimeSnapshot, usage: &ClaudeCodeUsage) -> Value {
    json!({"role":"assistant","content":[{"type":"text","text":snapshot.answer_preview.as_deref().unwrap_or("")}],
        "api":"claude-code","provider":"claude-code","model":snapshot.model,"stopReason":"stop","timestamp":crate::util::now_ms(),
        "usage":{"input":usage.input,"output":usage.output,"cacheRead":usage.cache_read,"cacheWrite":usage.cache_write,"totalTokens":usage.total_tokens,
        "cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":usage.cost}}})
}

#[cfg(test)]
mod tests;
