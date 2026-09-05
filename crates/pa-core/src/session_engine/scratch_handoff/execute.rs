use std::time::{Instant, SystemTime, UNIX_EPOCH};

use pa_agent::agent::AgentPromptInput;
use pa_agent::types::{AgentMessage, CustomAgentMessage, Message, StopReason};
use pa_types::session::CompactionEntry;
use serde_json::json;

use super::{
    build_scratch_handoff_continuation, build_scratch_handoff_history,
    has_committed_scratch_handoff, latest_persisted_scratch_handoff_path,
    read_scratch_handoff_text, render_scratch_handoff_closeout_message,
    resolve_scratch_handoff_path, scratch_handoff_compaction_details,
    ScratchHandoffRuntimeSettings, SCRATCH_HANDOFF_CLOSEOUT_CUSTOM_TYPE,
    SCRATCH_HANDOFF_PATH_CUSTOM_TYPE,
};
use crate::session_engine::compact_session::{CompactOutcome, CompactRun};
use crate::session_engine::compaction_exec::CompactionResult;
use crate::session_engine::{compaction, provider_adapter, AgentSession};

struct CloseoutGuard(std::sync::Arc<std::sync::atomic::AtomicBool>);
impl Drop for CloseoutGuard {
    fn drop(&mut self) { self.0.store(false, std::sync::atomic::Ordering::Release); }
}

fn local_date() -> anyhow::Result<String> {
    let seconds = libc::time_t::try_from(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())?;
    let mut calendar = std::mem::MaybeUninit::<libc::tm>::uninit();
    // Both pointers remain valid for the call; success initializes the calendar.
    let result = unsafe { libc::localtime_r(&raw const seconds, calendar.as_mut_ptr()) };
    anyhow::ensure!(
        !result.is_null(),
        "Could not determine the local checkpoint date"
    );
    let calendar = unsafe { calendar.assume_init() };
    Ok(format!(
        "{:04}{:02}{:02}",
        calendar.tm_year + 1900,
        calendar.tm_mon + 1,
        calendar.tm_mday
    ))
}

pub(crate) async fn execute_scratch_handoff(
    session: &AgentSession,
    settings: &ScratchHandoffRuntimeSettings,
    abort: Option<&pa_agent::abort::AbortSignal>,
) -> anyhow::Result<CompactOutcome> {
    let started = Instant::now();
    pa_agent::abort::throw_if_aborted_signal(abort)?;
    let (path, history, create) = {
        let mut store = session.session.lock().await;
        let branch: Vec<_> = store
            .get_branch(None)
            .into_iter()
            .map(serde_json::to_value)
            .collect::<Result<_, _>>()?;
        let prior = latest_persisted_scratch_handoff_path(&branch);
        let path = resolve_scratch_handoff_path(
            &settings.cwd,
            Some(&settings.root_dir),
            store.get_session_id(),
            None,
            prior.as_deref(),
            &local_date()?,
        );
        let history = build_scratch_handoff_history(&branch)?;
        let create = !has_committed_scratch_handoff(&branch, &path.display_path);
        if prior.as_deref() != Some(&path.display_path) {
            store.append_custom_entry(
                SCRATCH_HANDOFF_PATH_CUSTOM_TYPE,
                Some(json!({"path":path.display_path})),
            )?;
        }
        (path, history, create)
    };
    let prompt = format!(
        "{}\n\n{}",
        render_scratch_handoff_closeout_message(&path.display_path, create),
        super::SCRATCH_HANDOFF_CLOSEOUT_GUIDANCE
    );
    let message = AgentMessage::Custom(CustomAgentMessage {
        role: "custom".into(),
        payload: json!({"customType":SCRATCH_HANDOFF_CLOSEOUT_CUSTOM_TYPE,"content":prompt,"display":true,
            "details":{"path":path.display_path,"phase":if create {"create"} else {"update"}},"timestamp":super::super::now_millis()}),
    });
    session.scratch_closeout_active.store(true, std::sync::atomic::Ordering::Release);
    let _closeout_guard = CloseoutGuard(session.scratch_closeout_active.clone());
    let _suppression = session.agent.suppress_continuations();
    let closeout = session
        .agent
        .prompt(AgentPromptInput::Messages(vec![message]));
    tokio::pin!(closeout);
    if let Some(abort) = abort {
        tokio::select! {
            biased;
            () = abort.aborted() => {
                session.agent.abort();
                let _ = closeout.await;
                return Err(pa_agent::abort::aborted_error());
            }
            result = &mut closeout => result?,
        }
    } else {
        closeout.await?;
    }
    pa_agent::abort::throw_if_aborted_signal(abort)?;
    let state = session.agent.state().await;
    let final_assistant = state
        .messages
        .iter()
        .rev()
        .find_map(|row| match row {
            AgentMessage::Standard(Message::Assistant(message)) => Some(message),
            _ => None,
        })
        .ok_or_else(|| {
            anyhow::anyhow!("Scratch handoff closeout did not produce an assistant response")
        })?;
    anyhow::ensure!(
        final_assistant.stop_reason == StopReason::Stop,
        "Scratch handoff closeout failed: {}",
        final_assistant
            .error_message
            .as_deref()
            .unwrap_or("provider request did not complete")
    );
    let text = read_scratch_handoff_text(&path.absolute_path)
        .await?
        .filter(|text| !text.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Scratch handoff closeout did not produce a non-empty Org checkpoint at {}",
                path.display_path
            )
        })?;
    let continuation = build_scratch_handoff_continuation(
        &path.display_path,
        &text,
        &history,
        i64::try_from(super::super::now_millis()).unwrap_or(i64::MAX),
    );
    let continuation: pa_types::session::AgentMessage =
        provider_adapter::json_round_trip(&continuation).ok_or_else(|| {
            anyhow::anyhow!("Scratch handoff continuation wire shapes do not match")
        })?;
    let digest = session
        .harness_digest_inputs()
        .await
        .map(|inputs| inputs.render_with_fingerprint());
    let mut store = session.session.lock().await;
    pa_agent::abort::throw_if_aborted_signal(abort)?;
    let tokens_before =
        compaction::estimate_context_tokens(&store.active_context().messages).tokens;
    let mut entry = CompactionEntry {
        summary: String::new(),
        first_kept_entry_id: String::new(),
        tokens_before,
        details: Some(scratch_handoff_compaction_details(
            &path.display_path,
            &history,
        )),
        from_hook: Some(false),
        harness_digest: digest.as_ref().map(|digest| digest.digest.clone()),
        harness_state_fingerprint: digest.map(|digest| digest.state_fingerprint),
        ..CompactionEntry::default()
    };
    let (kept_id, _) = store.append_message_compaction(continuation.clone(), entry.clone())?;
    entry.first_kept_entry_id.clone_from(&kept_id);
    Ok(CompactOutcome::Ran(Box::new(CompactRun {
        continuation: Some(continuation),
        result: CompactionResult {
            summary: format!(
                "Scratch handoff: rebuilt context around {}.",
                path.display_path
            ),
            first_kept_entry_id: kept_id,
            tokens_before,
            usage: None,
        },
        entry,
        duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        ipython_state: None,
    })))
}

#[cfg(test)]
mod tests;
