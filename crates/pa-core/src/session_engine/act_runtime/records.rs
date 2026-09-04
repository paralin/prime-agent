use std::collections::HashSet;

use pa_types::ai::Usage;
use pa_types::session::{ActModel, ActStartEntry, ActTerminalEntry, ActTerminalStatus, FileEntry};
use sha2::{Digest, Sha256};

use super::{lock, ActLaneResult, ActRuntime, Arc, Mutex, ParentSession};

pub(super) struct ActRecordIdentity {
    pub act_id: String,
    pub depth: u32,
    pub parent_act_id: Option<String>,
    pub session_key: String,
    pub model: ActModel,
}

type Baseline = Arc<Mutex<Option<Usage>>>;

pub(in crate::session_engine) fn usage_delta(after: Usage, before: Usage) -> Usage {
    let mut delta = after;
    delta.input = after.input.saturating_sub(before.input);
    delta.output = after.output.saturating_sub(before.output);
    delta.cache_read = after.cache_read.saturating_sub(before.cache_read);
    delta.cache_write = after.cache_write.saturating_sub(before.cache_write);
    delta.total_tokens = after.total_tokens.saturating_sub(before.total_tokens);
    delta.cost.input =
        pa_types::JsNumber((after.cost.input.as_f64() - before.cost.input.as_f64()).max(0.0));
    delta.cost.output =
        pa_types::JsNumber((after.cost.output.as_f64() - before.cost.output.as_f64()).max(0.0));
    delta.cost.cache_read = pa_types::JsNumber(
        (after.cost.cache_read.as_f64() - before.cost.cache_read.as_f64()).max(0.0),
    );
    delta.cost.cache_write = pa_types::JsNumber(
        (after.cost.cache_write.as_f64() - before.cost.cache_write.as_f64()).max(0.0),
    );
    delta.cost.total =
        pa_types::JsNumber((after.cost.total.as_f64() - before.cost.total.as_f64()).max(0.0));
    delta
}

fn bounded_error(error: &str) -> String {
    let mut units = 0;
    error
        .chars()
        .take_while(|character| {
            units += character.len_utf16();
            units <= 4096
        })
        .collect()
}

impl ActRuntime {
    pub(super) fn start_hook(
        &self,
        act_id: String,
        depth: u32,
        parent_act_id: Option<String>,
        outer_tool_call_id: Option<String>,
        session_key: String,
    ) -> (super::super::act_lane::ActStartHook, Baseline) {
        let baseline: Baseline = Arc::default();
        let captured = baseline.clone();
        let session = lock(&self.parent_session).clone();
        let sink = lock(&self.record_sink).clone();
        let hook: super::super::act_lane::ActStartHook = Arc::new(move |usage| {
            let session = session.as_ref().and_then(std::sync::Weak::upgrade);
            let baseline = captured.clone();
            let sink = sink.clone();
            let payload = ActStartEntry {
                act_id: act_id.clone(),
                depth,
                parent_act_id: parent_act_id.clone(),
                session_key: Some(session_key.clone()),
                outer_tool_call_id: outer_tool_call_id.clone(),
                usage_baseline: usage,
            };
            Box::pin(async move {
                if let Some(sink) = sink {
                    sink("act_start", serde_json::to_value(&payload)?)?;
                }
                if let Some(session) = session {
                    session.lock().await.append_act_start(payload)?;
                }
                *lock(&baseline) = Some(usage);
                Ok(())
            })
        });
        (hook, baseline)
    }

    pub(super) async fn finish_record(
        &self,
        identity: ActRecordIdentity,
        result: &anyhow::Result<ActLaneResult>,
        cancelled: bool,
        baseline: Baseline,
        after: Usage,
    ) -> anyhow::Result<()> {
        let Some(before) = *lock(&baseline) else {
            return Ok(());
        };
        let session = lock(&self.parent_session)
            .as_ref()
            .and_then(std::sync::Weak::upgrade);
        let Some(session) = session else {
            return Ok(());
        };
        let (status, error) = match result {
            Ok(ActLaneResult::Done) => (ActTerminalStatus::Done, None),
            Ok(ActLaneResult::Cancelled) => (ActTerminalStatus::Cancelled, None),
            Ok(ActLaneResult::Text(_)) => (
                ActTerminalStatus::Error,
                Some("Act ended without calling rlm.done()".into()),
            ),
            Err(error) => (
                if cancelled {
                    ActTerminalStatus::Cancelled
                } else {
                    ActTerminalStatus::Error
                },
                Some(bounded_error(&error.to_string())),
            ),
        };
        let payload = ActTerminalEntry {
            act_id: identity.act_id,
            depth: identity.depth,
            parent_act_id: identity.parent_act_id,
            session_key: Some(identity.session_key),
            status,
            usage: usage_delta(after, before),
            model: Some(identity.model),
            error,
        };
        self.persist_terminal(&mut *session.lock().await, payload)?;
        Ok(())
    }

    pub(super) async fn recover_interrupted(
        &self,
        session: &Arc<ParentSession>,
    ) -> anyhow::Result<()> {
        let mut session = session.lock().await;
        let branch = session.act_records();
        let terminal_ids: HashSet<_> = branch
            .iter()
            .filter_map(|row| match row {
                FileEntry::ActTerminal { payload, .. } => Some(payload.act_id.as_str()),
                _ => None,
            })
            .collect();
        let starts: Vec<_> = branch
            .iter()
            .filter_map(|row| match row {
                FileEntry::ActStart { payload, .. }
                    if !terminal_ids.contains(payload.act_id.as_str()) =>
                {
                    Some(payload.clone())
                }
                _ => None,
            })
            .collect();
        for start in starts {
            let (usage, model) = self.persisted_state(start.depth, start.session_key.as_deref())?;
            self.persist_terminal(
                &mut session,
                ActTerminalEntry {
                    act_id: start.act_id,
                    depth: start.depth,
                    parent_act_id: start.parent_act_id,
                    session_key: start.session_key,
                    status: ActTerminalStatus::Interrupted,
                    usage: usage_delta(usage, start.usage_baseline),
                    model,
                    error: Some("Act was interrupted before the root session restarted".into()),
                },
            )?;
        }
        Ok(())
    }

    fn persist_terminal(
        &self,
        session: &mut crate::session::manager::SessionManager,
        payload: ActTerminalEntry,
    ) -> anyhow::Result<()> {
        let sink = lock(&self.record_sink).clone();
        if let Some(sink) = sink {
            sink("act_terminal", serde_json::to_value(&payload)?)?;
        }
        session.append_act_terminal(payload)?;
        Ok(())
    }

    pub(super) fn persisted_session(
        &self,
        depth: u32,
        key: Option<&str>,
    ) -> anyhow::Result<Option<crate::session::manager::SessionManager>> {
        let Some(root) = &self.artifact_dir else {
            return Ok(None);
        };
        let name = if depth == 1 {
            "act".into()
        } else {
            format!("act-depth-{depth}")
        };
        let mut directory = root.join(&name);
        if let Some(key) = key {
            let marker = directory.join("model-key");
            let uses_base = if marker.exists() {
                std::fs::read_to_string(&marker)?.trim() == key
            } else if directory.join("session.jsonl").is_file() {
                let manager = crate::session::manager::SessionManager::open(
                    &self.cwd,
                    &directory,
                    &directory.join("session.jsonl"),
                );
                manager
                    .get_branch(None)
                    .iter()
                    .rev()
                    .find_map(|entry| match entry {
                        FileEntry::ModelChange { payload, .. } => {
                            Some(format!("{}/{}", payload.provider, payload.model_id) == key)
                        }
                        _ => None,
                    })
                    .unwrap_or(false)
            } else {
                true
            };
            if !uses_base {
                let hash = format!("{:x}", Sha256::digest(key.as_bytes()));
                directory = root.join(format!("{name}-model-{}", &hash[..16]));
            }
            let marker = directory.join("model-key");
            if marker.exists() {
                anyhow::ensure!(
                    std::fs::read_to_string(marker)?.trim() == key,
                    "Act model session key collision"
                );
            }
        }
        let file = directory.join("session.jsonl");
        if !file.is_file() {
            return Ok(None);
        }
        Ok(Some(crate::session::manager::SessionManager::open(
            &self.cwd, &directory, &file,
        )))
    }

    fn persisted_state(
        &self,
        depth: u32,
        key: Option<&str>,
    ) -> anyhow::Result<(Usage, Option<ActModel>)> {
        let Some(manager) = self.persisted_session(depth, key)? else {
            return Ok((Usage::default(), None));
        };
        let usage = super::super::act_lane::retained_usage(&manager);
        let mut model = None;
        for entry in manager.get_entries() {
            if let FileEntry::ModelChange { payload, .. } = entry {
                model = Some(ActModel {
                    provider: payload.provider.clone(),
                    id: payload.model_id.clone(),
                });
            }
        }
        Ok((usage, model))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::manager::SessionManager;

    #[tokio::test]
    async fn denied_role_candidate_does_not_fall_back_or_start_a_provider() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("auth.json"),
            r#"{"openai":{"type":"api_key","key":"test-only-unused"}}"#,
        )
        .unwrap();
        let mut settings = crate::settings::SettingsManager::create(dir.path(), dir.path());
        settings
            .set_model_roles(std::collections::BTreeMap::from([(
                "task".into(),
                crate::settings::ModelRoleSelector::Ordered(vec![
                    "openai/gpt-4".into(),
                    "openai/gpt-4".into(),
                ]),
            )]))
            .unwrap();
        let runtime = ActRuntime::new(dir.path().into(), dir.path().into());
        let checked = Arc::new(Mutex::new(Vec::new()));
        let captured = checked.clone();
        runtime.set_model_gate(Arc::new(move |selector| {
            lock(&captured).push(selector.to_owned());
            anyhow::bail!("model denied")
        }));
        let result = runtime
            .resolve_target(
                &settings,
                "@task",
                1,
                2,
                pa_agent::types::ThinkingLevel::Off,
            )
            .await;
        assert_eq!(result.err().unwrap().to_string(), "model denied");
        assert_eq!(*lock(&checked), ["openai/gpt-4"]);
    }

    #[tokio::test]
    async fn act_explicit_effort_overrides_catalog_levels_without_mutating_the_catalog() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory.path().join("models.json"),
            serde_json::json!({
                "providers":{"act-effort-test":{"api":"openai-completions","apiKey":"test-only",
                    "baseUrl":"http://127.0.0.1:9","models":[{"id":"primary","reasoning":true,
                        "thinkingLevelMap":{"high":null},"contextWindow":128_000,"maxTokens":4096}]}}
            })
            .to_string(),
        )
        .unwrap();
        let settings = crate::settings::SettingsManager::create(directory.path(), directory.path());
        let runtime = ActRuntime::new(directory.path().into(), directory.path().into());
        let (target, _, model) = runtime
            .resolve_target(
                &settings,
                "act-effort-test/primary:high",
                1,
                1,
                pa_agent::types::ThinkingLevel::Off,
            )
            .await
            .unwrap();
        assert_eq!(target.thinking_level, pa_agent::types::ThinkingLevel::High);
        assert_eq!(
            model
                .thinking_level_map
                .as_ref()
                .unwrap()
                .get(&pa_types::ai::ModelThinkingLevel::High),
            Some(&Some("high".into()))
        );
        let (target, _, _) = runtime
            .resolve_target(
                &settings,
                "act-effort-test/primary",
                1,
                1,
                pa_agent::types::ThinkingLevel::High,
            )
            .await
            .unwrap();
        assert_eq!(
            target.thinking_level,
            pa_agent::types::ThinkingLevel::Medium
        );
    }

    #[tokio::test]
    async fn interrupted_assignment_recovers_usage_once_from_its_private_session() {
        let dir = tempfile::tempdir().unwrap();
        let private_dir = dir.path().join("act");
        std::fs::create_dir_all(&private_dir).unwrap();
        std::fs::write(private_dir.join("model-key"), "faux/model\n").unwrap();
        let mut private = SessionManager::in_memory(dir.path());
        private.append_model_change("faux", "model").unwrap();
        let usage = Usage {
            input: 70,
            output: 5,
            total_tokens: 75,
            ..Usage::default()
        };
        let assistant = serde_json::from_value(serde_json::json!({"role":"assistant", "content":[],"api":"faux","provider":"faux","model":"model","usage":usage,"stopReason":"stop","timestamp":1})).unwrap();
        private.append_message(assistant).unwrap();
        let first_kept_entry_id = private.get_leaf_id().unwrap().to_owned();
        private
            .append_compaction(pa_types::session::CompactionEntry {
                summary: "Retained private work".into(),
                first_kept_entry_id,
                tokens_before: 75,
                usage: Some(Usage {
                    input: 10,
                    output: 4,
                    total_tokens: 14,
                    ..Default::default()
                }),
                ..Default::default()
            })
            .unwrap();
        let lines = private
            .get_all_entries()
            .iter()
            .map(serde_json::to_string)
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
            .join("\n");
        std::fs::write(private_dir.join("session.jsonl"), format!("{lines}\n")).unwrap();
        let root = Arc::new(tokio::sync::Mutex::new(SessionManager::in_memory(
            dir.path(),
        )));
        root.lock()
            .await
            .append_act_start(ActStartEntry {
                act_id: "interrupted".into(),
                depth: 1,
                parent_act_id: None,
                session_key: Some("faux/model".into()),
                outer_tool_call_id: Some("caller-cell".into()),
                usage_baseline: Usage {
                    input: 20,
                    output: 2,
                    total_tokens: 22,
                    ..Default::default()
                },
            })
            .unwrap();
        let runtime = ActRuntime::with_persistence(
            dir.path().into(),
            dir.path().into(),
            Some(dir.path().into()),
        );
        runtime.bind_session(&root).await.unwrap();
        runtime.bind_session(&root).await.unwrap();
        let store = root.lock().await;
        let terminals: Vec<_> = store
            .get_branch(None)
            .into_iter()
            .filter_map(|row| match row {
                FileEntry::ActTerminal { payload, .. } => Some(payload),
                _ => None,
            })
            .collect();
        assert_eq!(terminals.len(), 1);
        assert_eq!(terminals[0].status, ActTerminalStatus::Interrupted);
        assert_eq!(terminals[0].usage.input, 60);
        assert_eq!(terminals[0].usage.output, 7);
        assert_eq!(terminals[0].model.as_ref().unwrap().provider, "faux");
        assert_eq!(
            terminals[0].error.as_deref(),
            Some("Act was interrupted before the root session restarted")
        );
    }

    #[tokio::test]
    async fn start_and_terminal_keep_the_assignment_identity_and_usage_delta() {
        let dir = tempfile::tempdir().unwrap();
        let root = Arc::new(tokio::sync::Mutex::new(SessionManager::in_memory(
            dir.path(),
        )));
        let runtime = ActRuntime::new(dir.path().into(), dir.path().into());
        runtime.bind_session(&root).await.unwrap();
        let (hook, baseline) = runtime.start_hook(
            "nested".into(),
            2,
            Some("parent".into()),
            Some("cell".into()),
            "faux/model".into(),
        );
        hook(Usage {
            input: 50,
            total_tokens: 50,
            ..Default::default()
        })
        .await
        .unwrap();
        runtime
            .finish_record(
                ActRecordIdentity {
                    act_id: "nested".into(),
                    depth: 2,
                    parent_act_id: Some("parent".into()),
                    session_key: "faux/model".into(),
                    model: ActModel {
                        provider: "faux".into(),
                        id: "model".into(),
                    },
                },
                &Ok(ActLaneResult::Text("no done".into())),
                false,
                baseline,
                Usage {
                    input: 65,
                    total_tokens: 65,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let store = root.lock().await;
        let branch = store.get_branch(None);
        let FileEntry::ActStart { payload: start, .. } = branch[0] else {
            panic!("start record")
        };
        let FileEntry::ActTerminal { payload: end, .. } = branch[1] else {
            panic!("terminal record")
        };
        assert_eq!(start.outer_tool_call_id.as_deref(), Some("cell"));
        assert_eq!(end.act_id, start.act_id);
        assert_eq!(end.parent_act_id.as_deref(), Some("parent"));
        assert_eq!(end.status, ActTerminalStatus::Error);
        assert_eq!(end.usage.input, 15);
        assert_eq!(
            end.error.as_deref(),
            Some("Act ended without calling rlm.done()")
        );
    }

    #[tokio::test]
    async fn rejected_worker_write_leaves_no_start_or_baseline() {
        let dir = tempfile::tempdir().unwrap();
        let root = Arc::new(tokio::sync::Mutex::new(SessionManager::in_memory(
            dir.path(),
        )));
        let runtime = ActRuntime::new(dir.path().into(), dir.path().into());
        runtime.bind_session(&root).await.unwrap();
        runtime.set_record_sink(Arc::new(|_, _| anyhow::bail!("worker write failed")));
        let (hook, baseline) = runtime.start_hook(
            "assignment".into(),
            1,
            None,
            Some("cell".into()),
            "faux/model".into(),
        );
        assert_eq!(
            hook(Usage::default()).await.unwrap_err().to_string(),
            "worker write failed"
        );
        assert!(lock(&baseline).is_none());
        assert!(root.lock().await.get_branch(None).is_empty());
    }

    #[test]
    fn usage_deltas_and_error_bounds_never_wrap_or_split_characters() {
        let delta = usage_delta(
            Usage {
                input: 1,
                ..Default::default()
            },
            Usage {
                input: 100,
                ..Default::default()
            },
        );
        assert_eq!(delta.input, 0);
        let bounded = bounded_error(&"😀".repeat(3000));
        assert_eq!(bounded.encode_utf16().count(), 4096);
    }

    #[test]
    fn recovery_does_not_attribute_a_different_models_private_usage() {
        let dir = tempfile::tempdir().unwrap();
        let private_dir = dir.path().join("act");
        std::fs::create_dir_all(&private_dir).unwrap();
        let mut private = SessionManager::in_memory(dir.path());
        private.append_model_change("faux", "other-model").unwrap();
        let lines = private
            .get_all_entries()
            .iter()
            .map(serde_json::to_string)
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
            .join("\n");
        std::fs::write(private_dir.join("session.jsonl"), format!("{lines}\n")).unwrap();
        let runtime = ActRuntime::with_persistence(
            dir.path().into(),
            dir.path().into(),
            Some(dir.path().into()),
        );
        let (usage, model) = runtime.persisted_state(1, Some("faux/model")).unwrap();
        assert_eq!(usage, Usage::default());
        assert!(model.is_none());
        assert!(!private_dir.join("model-key").exists());
    }
}
