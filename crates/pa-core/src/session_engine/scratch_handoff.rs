use std::path::{Path, PathBuf};

use pa_agent::types::{
    AgentMessage, ImageContent, Message, TextContent, UserContent, UserMessage, UserPart,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::settings::types::CompactionStrategy;
use crate::tools::path_utils::resolve_to_cwd;

use super::history_snapshot::{build_session_history_snapshot, HistorySnapshot};

mod execute;
pub(crate) use execute::execute_scratch_handoff;

#[derive(Debug, Clone)]
pub struct ScratchHandoffRuntimeSettings {
    pub strategy: CompactionStrategy,
    pub enabled: bool,
    pub root_dir: String,
    pub cwd: PathBuf,
}

pub const SCRATCH_HANDOFF_READ_CUSTOM_TYPE: &str = "scratch-handoff-read";
pub const SCRATCH_HANDOFF_PATH_CUSTOM_TYPE: &str = "scratch-handoff-path";
pub const SCRATCH_HANDOFF_WARNING_CUSTOM_TYPE: &str = "scratch-handoff-warning";
pub const SCRATCH_HANDOFF_CLOSEOUT_CUSTOM_TYPE: &str = "scratch-handoff-closeout";
pub const SCRATCH_HANDOFF_CONTINUE_INSTRUCTION: &str = "Keep this org file up to date as you continue the tasks within. When you finish a task or subtask, update it from TODO to DONE and move any notes to the daily log leaving behind a short org-link to the relevant daily log entry in the scratch file. If you are confused on what this means, read the daily-log skill. After marking a task as DONE, check if there are any parent headings to mark DONE, or any peer or child TODO headings to action next, and loop.";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScratchHandoffPath {
    pub display_path: String,
    pub absolute_path: PathBuf,
}

fn safe_id(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
                ch
            } else {
                '-'
            }
        })
        .collect()
}

#[must_use]
pub fn resolve_scratch_handoff_path(
    cwd: &Path,
    root_dir: Option<&str>,
    session_id: &str,
    agent_id: Option<&str>,
    scratch_file: Option<&str>,
    date: &str,
) -> ScratchHandoffPath {
    let display_path =
        if let Some(explicit) = scratch_file.map(str::trim).filter(|path| !path.is_empty()) {
            explicit.replace(std::path::MAIN_SEPARATOR, "/")
        } else {
            let root = root_dir
                .map(str::trim)
                .filter(|root| !root.is_empty())
                .unwrap_or("agent");
            let session = safe_id(session_id);
            let agent = agent_id
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .map_or_else(|| session.clone(), safe_id);
            let filename = if session == agent {
                format!("{session}.org")
            } else {
                format!("{agent}-{session}.org")
            };
            Path::new(root)
                .join(date)
                .join(filename)
                .to_string_lossy()
                .replace(std::path::MAIN_SEPARATOR, "/")
        };
    let absolute_path = PathBuf::from(resolve_to_cwd(&display_path, &cwd.to_string_lossy()));
    ScratchHandoffPath {
        display_path,
        absolute_path,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PersistedScratchHandoff {
    pub version: u32,
    pub path: String,
    pub history_text: String,
    pub message_count: usize,
    pub truncated: bool,
}

fn persisted(entry: &Value) -> Option<PersistedScratchHandoff> {
    let state: PersistedScratchHandoff =
        serde_json::from_value(entry["details"]["scratchHandoff"].clone()).ok()?;
    (state.version == 1 && !state.path.trim().is_empty()).then_some(state)
}

fn committed_path(entry: &Value) -> Option<String> {
    match entry["type"].as_str() {
        Some("compaction") => persisted(entry).map(|state| state.path),
        Some("custom_message") if entry["customType"] == SCRATCH_HANDOFF_READ_CUSTOM_TYPE => entry
            ["details"]["path"]
            .as_str()
            .filter(|path| !path.trim().is_empty())
            .map(str::to_owned),
        _ => None,
    }
}

#[must_use]
pub fn latest_persisted_scratch_handoff_path(entries: &[Value]) -> Option<String> {
    entries.iter().rev().find_map(|entry| {
        if entry["type"] == "custom" && entry["customType"] == SCRATCH_HANDOFF_PATH_CUSTOM_TYPE {
            entry["data"]["path"]
                .as_str()
                .filter(|path| !path.trim().is_empty())
                .map(str::to_owned)
        } else {
            committed_path(entry)
        }
    })
}

#[must_use]
pub fn has_committed_scratch_handoff(entries: &[Value], path: &str) -> bool {
    entries
        .iter()
        .any(|entry| committed_path(entry).as_deref() == Some(path))
}

/// # Errors
/// Returns an error when history PNG compression fails.
pub fn build_scratch_handoff_history(entries: &[Value]) -> std::io::Result<HistorySnapshot> {
    let previous = entries.iter().enumerate().rev().find_map(|(index, entry)| {
        (entry["type"] == "compaction")
            .then(|| persisted(entry))
            .flatten()
            .map(|state| (index + 1, state))
    });
    match previous {
        Some((start, state)) => build_session_history_snapshot(
            &entries[start..],
            Some(&HistorySnapshot {
                text: state.history_text,
                message_count: state.message_count,
                truncated: state.truncated,
                images: Vec::new(),
            }),
        ),
        None => build_session_history_snapshot(entries, None),
    }
}

#[must_use]
pub fn scratch_handoff_compaction_details(path: &str, history: &HistorySnapshot) -> Value {
    json!({"scratchHandoff":{"version":1, "path":path, "historyText":history.text,
        "messageCount":history.message_count, "truncated":history.truncated}})
}

fn escape_attribute(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[must_use]
pub fn build_scratch_handoff_continuation(
    path: &str,
    scratch_text: &str,
    images: &[ImageContent],
    timestamp: i64,
) -> AgentMessage {
    let mut content: Vec<_> = images.iter().cloned().map(UserPart::Image).collect();
    content.push(UserPart::Text(TextContent { text:format!("<scratch-handoff-file path=\"{}\">\n{scratch_text}\n</scratch-handoff-file>\n\n{SCRATCH_HANDOFF_CONTINUE_INSTRUCTION}", escape_attribute(path)), text_signature:None }));
    AgentMessage::Standard(Message::User(UserMessage {
        content: UserContent::Parts(content),
        timestamp,
        rest: serde_json::Map::default(),
    }))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScratchBoundaryReason {
    Manual,
    Overflow,
    Threshold,
    Requested,
}

#[derive(Debug, PartialEq, Eq)]
pub struct ScratchHandoffBoundary {
    pub requires_closeout: bool,
    pub warning: Option<&'static str>,
}

#[must_use]
pub fn resolve_scratch_handoff_boundary(
    strategy: CompactionStrategy,
    enabled: bool,
    native: bool,
    images: bool,
    reason: ScratchBoundaryReason,
) -> ScratchHandoffBoundary {
    if reason == ScratchBoundaryReason::Overflow {
        return ScratchHandoffBoundary {
            requires_closeout: false,
            warning: None,
        };
    }
    let wants = enabled
        && (strategy == CompactionStrategy::ScratchHandoff
            || (strategy == CompactionStrategy::NativeOrScratch && !native));
    ScratchHandoffBoundary { requires_closeout:wants && images,
        warning:(wants && !images).then_some("Scratch handoff requires a vision-capable model; using ordinary compaction for this boundary.") }
}

#[must_use]
pub fn render_scratch_handoff_closeout_message(path: &str, create: bool) -> String {
    if create {
        format!("Stop working for now; please create a .org file brain-dump of your ongoing work to {path}, use org-todo structure including TODO subheadings, subheadings of subheadings, TODOs on nested subheadings, and so on. It should be detailed enough to hand off this work to a colleague.")
    } else {
        format!("Stop working for now and make any final edits to {path} such that you can hand it to a colleague to continue this work.")
    }
}

/// # Errors
/// Returns a file read error except when the checkpoint does not exist.
pub async fn read_scratch_handoff_text(path: &Path) -> std::io::Result<Option<String>> {
    match tokio::fs::read_to_string(path).await {
        Ok(text) => Ok(Some(text.trim().into())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pins_survive_date_rollover_and_only_committed_boundaries_count() {
        let path = resolve_scratch_handoff_path(
            Path::new("/project"),
            None,
            "session/1",
            Some("worker 2"),
            None,
            "20261001",
        );
        assert_eq!(path.display_path, "agent/20261001/worker-2-session-1.org");
        let entries = vec![
            json!({"type":"custom", "customType":SCRATCH_HANDOFF_PATH_CUSTOM_TYPE, "data":{"path":path.display_path}}),
        ];
        let pinned = latest_persisted_scratch_handoff_path(&entries).unwrap();
        assert!(!has_committed_scratch_handoff(&entries, &pinned));
        let next = resolve_scratch_handoff_path(
            Path::new("/project"),
            None,
            "session/1",
            None,
            Some(&pinned),
            "20261002",
        );
        assert_eq!(next, path);
    }

    #[test]
    fn overflow_remains_ordinary_and_hybrid_prefers_native() {
        assert!(
            !resolve_scratch_handoff_boundary(
                CompactionStrategy::ScratchHandoff,
                true,
                false,
                true,
                ScratchBoundaryReason::Overflow
            )
            .requires_closeout
        );
        assert!(
            !resolve_scratch_handoff_boundary(
                CompactionStrategy::NativeOrScratch,
                true,
                true,
                true,
                ScratchBoundaryReason::Manual
            )
            .requires_closeout
        );
        let unsupported = resolve_scratch_handoff_boundary(
            CompactionStrategy::ScratchHandoff,
            true,
            false,
            false,
            ScratchBoundaryReason::Threshold,
        );
        assert!(!unsupported.requires_closeout);
        assert!(unsupported.warning.is_some());
        assert!(
            resolve_scratch_handoff_boundary(
                CompactionStrategy::NativeOrScratch,
                true,
                false,
                true,
                ScratchBoundaryReason::Requested
            )
            .requires_closeout
        );
    }

    #[test]
    fn generations_retain_prior_source_and_continuation_escapes_path() {
        let entries = vec![
            json!({"type":"compaction", "details":{"scratchHandoff":{"version":1, "path":"agent/work.org", "historyText":"prior", "messageCount":2, "truncated":false}}}),
            json!({"type":"message", "message":{"role":"user", "content":"next"}}),
        ];
        let history = build_scratch_handoff_history(&entries).unwrap();
        assert_eq!(history.message_count, 3);
        assert!(history.text.starts_with("prior\n\nUSER\nnext"));
        assert!(has_committed_scratch_handoff(&entries, "agent/work.org"));
        let message =
            build_scratch_handoff_continuation("a\"<&.org", "* TODO next", &history.images, 7);
        let value = serde_json::to_value(message).unwrap();
        assert_eq!(value["content"][0]["type"], "image");
        assert!(value["content"][1]["text"]
            .as_str()
            .unwrap()
            .contains("a&quot;&lt;&amp;.org"));
        assert_eq!(
            scratch_handoff_compaction_details("agent/work.org", &history)["scratchHandoff"]
                ["messageCount"],
            3
        );
    }

    #[tokio::test]
    async fn missing_checkpoint_is_optional_and_read_errors_propagate() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_scratch_handoff_text(&dir.path().join("missing.org"))
            .await
            .unwrap()
            .is_none());
        let path = dir.path().join("work.org");
        tokio::fs::write(&path, "  * TODO work\n").await.unwrap();
        assert_eq!(
            read_scratch_handoff_text(&path).await.unwrap().as_deref(),
            Some("* TODO work")
        );
        assert!(read_scratch_handoff_text(dir.path()).await.is_err());
    }
}
