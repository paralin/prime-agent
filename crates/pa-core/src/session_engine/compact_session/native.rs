use pa_ai::types::{ProviderNativeCompactionOptions, StreamOptions};
use pa_types::ai::{Context, Message, UserContent, UserMessage};
use serde_json::{json, Value};

use super::{
    compaction_entry_for, context_tokens, details_for, file_ops_block, message_from_entry,
    prepare_compaction, AgentMessage, CompactOptions, CompactOutcome, CompactRun, CompactionResult,
    FileEntry, SessionManager,
};

pub const PROVIDER_NATIVE_COMPACTION_SUMMARY: &str =
    "Provider-native compaction preserved opaque history for this session.";

fn prior_native_history(entry: &FileEntry, provider: &str) -> Option<Value> {
    let FileEntry::Compaction { base, payload } = entry else {
        return None;
    };
    let native = payload
        .provider_native_compaction
        .as_ref()
        .or_else(|| base.rest.get("providerNativeCompaction"))?;
    (native["provider"] == provider && native["replacementHistory"].is_array()).then(|| {
        json!({"type":"openaiResponsesHistory", "provider":provider,
            "items":native["replacementHistory"]})
    })
}

/// Compact the prepared prefix using the active provider and persist its opaque history.
///
/// # Errors
/// Returns provider, cancellation, invalid-history, or persistence errors without committing.
pub async fn execute_native_compaction(
    session: &mut SessionManager,
    options: &CompactOptions<'_>,
    headers: Option<std::collections::BTreeMap<String, String>>,
) -> anyhow::Result<CompactOutcome> {
    let started = std::time::Instant::now();
    pa_agent::abort::throw_if_aborted_signal(options.abort)?;
    let entries = session.retained_entries().to_vec();
    let prepared = match prepare_compaction(&entries, options.settings.keep_recent_tokens) {
        Ok(prepared) => prepared,
        Err(skip) => return Ok(CompactOutcome::Skipped(skip.user_message())),
    };
    let end = prepared.cut.first_kept_entry_index;
    let previous = entries[..end]
        .iter()
        .rposition(|entry| matches!(entry, FileEntry::Compaction { .. }));
    let previous_payload =
        previous.and_then(|index| prior_native_history(&entries[index], &options.model.provider));
    let mut messages = Vec::new();
    if let Some(payload) = previous_payload {
        messages.push(AgentMessage::User(UserMessage {
            content: UserContent::Text(PROVIDER_NATIVE_COMPACTION_SUMMARY.into()),
            timestamp: 0,
            rest: [("providerPayload".into(), payload)].into_iter().collect(),
        }));
    } else if let Some(summary) = &prepared.previous_summary {
        messages.push(AgentMessage::User(UserMessage {
            content: UserContent::Text(format!(
                "{}{}{}",
                super::super::messages::COMPACTION_SUMMARY_PREFIX,
                summary,
                super::super::messages::COMPACTION_SUMMARY_SUFFIX
            )),
            timestamp: 0,
            rest: serde_json::Map::default(),
        }));
    }
    let summarized: Vec<_> = entries[prepared.boundary_start..end]
        .iter()
        .filter_map(|entry| match entry {
            FileEntry::Message { message, .. } => Some(message.clone()),
            _ => message_from_entry(entry),
        })
        .collect();
    let details = details_for(&summarized, &entries, previous);
    messages.extend(summarized);
    let context = Context {
        system_prompt: None,
        messages: super::super::messages::convert_to_llm(&messages)
            .iter()
            .filter_map(super::super::provider_adapter::json_round_trip::<_, Message>)
            .collect(),
        tools: None,
    };
    let signal = tokio_util::sync::CancellationToken::new();
    let request = ProviderNativeCompactionOptions {
        base: StreamOptions {
            api_key: options.api_key.clone(),
            headers: headers.map(|headers| headers.into_iter().collect()),
            signal: Some(signal.clone()),
            session_id: Some(session.get_session_id().to_string()),
            ..Default::default()
        },
        instructions: format!(
            "{}\n\n{}",
            super::super::compaction_utils::SUMMARIZATION_SYSTEM_PROMPT,
            super::super::compaction::build_summarization_prompt(None, None)
        ),
    };
    let operation = pa_ai::compact(&options.model, &context, &request);
    let native = if let Some(abort) = options.abort {
        tokio::select! {
            biased;
            () = abort.aborted() => {
                signal.cancel();
                return Err(pa_agent::abort::aborted_error());
            }
            result = operation => result?,
        }
    } else {
        operation.await?
    };
    pa_agent::abort::throw_if_aborted_signal(options.abort)?;
    anyhow::ensure!(
        native.provider == options.model.provider,
        "Native compaction returned history for a different provider"
    );
    anyhow::ensure!(
        !native.replacement_history.is_empty()
            && native.replacement_history.last() == Some(&native.compaction_item),
        "Native compaction returned incomplete history"
    );
    let result = CompactionResult {
        summary: format!(
            "{PROVIDER_NATIVE_COMPACTION_SUMMARY}{}",
            file_ops_block(&details.read_files, &details.modified_files)
        ),
        first_kept_entry_id: entries
            .get(end)
            .and_then(FileEntry::id)
            .unwrap_or_default()
            .into(),
        tokens_before: context_tokens(&entries, session.get_leaf_id()),
        usage: None,
    };
    let (digest, fingerprint) = options
        .harness_digest
        .as_ref()
        .map(|inputs| {
            let rendered = inputs.render_with_fingerprint();
            (Some(rendered.digest), Some(rendered.state_fingerprint))
        })
        .unwrap_or_default();
    let mut entry = compaction_entry_for(&result, &details, None, digest, fingerprint);
    entry.provider_native_compaction = Some(json!({
        "provider":native.provider, "replacementHistory":native.replacement_history,
        "compactionItem":native.compaction_item,
    }));
    session.append_compaction(entry.clone())?;
    if let Some(sink) = &options.summary_delta {
        sink(&result.summary);
    }
    Ok(CompactOutcome::Ran(Box::new(CompactRun {
        continuation: None,
        result,
        entry,
        duration_ms: started.elapsed().as_millis() as u64,
        ipython_state: None,
    })))
}

#[cfg(test)]
mod tests;
