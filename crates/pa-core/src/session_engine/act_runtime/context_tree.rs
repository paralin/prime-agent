use std::collections::BTreeSet;

use pa_types::session::{ActModel, ActTerminalStatus, FileEntry};
use serde_json::{json, Value};

use super::{lock, ActRuntime, Arc};

impl ActRuntime {
    /// Retained lanes use root-branch billing rather than their reusable private history.
    ///
    /// # Panics
    /// The internal node builder always supplies an array for `children`.
    pub async fn context_tree_nodes(self: &Arc<Self>) -> Vec<Value> {
        let session = lock(&self.parent_session)
            .as_ref()
            .and_then(std::sync::Weak::upgrade);
        let Some(session) = session else {
            return Vec::new();
        };
        let records = session.lock().await.act_records();
        let lanes = lock(&self.state).lanes.clone();
        let depths: BTreeSet<_> = records
            .iter()
            .filter_map(|entry| match entry {
                FileEntry::ActStart { payload, .. } => Some(payload.depth),
                FileEntry::ActTerminal { payload, .. } => Some(payload.depth),
                _ => None,
            })
            .chain(lanes.keys().copied())
            .collect();
        let mut nodes = Vec::new();
        for depth in depths {
            if let Some(node) = self
                .context_tree_node(depth, &records, lanes.get(&depth))
                .await
            {
                nodes.push((depth, node));
            }
        }
        let mut roots = Vec::new();
        while let Some((depth, node)) = nodes.pop() {
            if let Some((_, parent)) = nodes
                .iter_mut()
                .find(|(parent_depth, _)| *parent_depth + 1 == depth)
            {
                parent["children"]
                    .as_array_mut()
                    .expect("lane children")
                    .insert(0, node);
            } else {
                roots.insert(0, node);
            }
        }
        roots
    }
    async fn context_tree_node(
        self: &Arc<Self>,
        depth: u32,
        records: &[FileEntry],
        lane: Option<&Arc<super::super::act_lane::ActLane>>,
    ) -> Option<Value> {
        let terminal = records.iter().rev().find_map(|entry| match entry {
            FileEntry::ActTerminal { payload, .. } if payload.depth == depth => Some(payload),
            _ => None,
        });
        let running = lane.is_some_and(|lane| lane.running());
        if !running && terminal.is_none() {
            return None;
        }
        let key = if running {
            records.iter().rev().find_map(|entry| match entry {
                FileEntry::ActStart { payload, .. } if payload.depth == depth => {
                    payload.session_key.clone()
                }
                _ => None,
            })
        } else {
            terminal.and_then(|entry| entry.session_key.clone())
        };
        let mut model = terminal.and_then(|entry| entry.model.clone());
        let context_usage;
        let snapshot = match (lane, key.as_deref()) {
            (Some(lane), Some(key)) => lane.context_snapshot(key).await,
            _ => None,
        };
        if let Some((live_model, window, entries)) = snapshot {
            if running || model.is_none() {
                model = Some(live_model);
            }
            context_usage = context_value(&entries, Some(window));
        } else if let Some((disk_model, usage)) = self.disk_context(depth, key, model.clone()).await
        {
            model = disk_model;
            context_usage = usage;
        } else {
            return None;
        }
        let mut usage = pa_types::ai::Usage::default();
        for entry in records {
            if let FileEntry::ActTerminal { payload, .. } = entry {
                if payload.depth == depth {
                    super::super::rlm_usage::add_assistant_usage(&mut usage, &payload.usage);
                }
            }
        }
        let status = if running {
            "running"
        } else {
            match terminal.map(|entry| entry.status) {
                Some(ActTerminalStatus::Error) => "error",
                Some(ActTerminalStatus::Cancelled | ActTerminalStatus::Interrupted) => "cancelled",
                _ => "done",
            }
        };
        let base_label = if cfg!(windows) {
            "Act lane (cooperative cancellation only)"
        } else {
            "Act lane"
        };
        let mut node = json!({
            "id": if depth == 1 { "act".into() } else { format!("act-depth-{depth}") },
            "label": if depth == 1 { base_label.into() } else { format!("{base_label} depth {depth}") },
            "status": status, "depth": depth,
            "cancellationCapability": if cfg!(windows) { "cooperative-only" } else { "posix-managed" },
            "ownUsage": usage, "totalUsage": usage, "children": [],
        });
        if let Some(model) = model {
            node["model"] = json!(model);
        }
        if let Some(usage) = context_usage {
            node["contextUsage"] = usage;
        }
        Some(node)
    }

    async fn disk_context(
        self: &Arc<Self>,
        depth: u32,
        key: Option<String>,
        saved_model: Option<ActModel>,
    ) -> Option<(Option<ActModel>, Option<Value>)> {
        let runtime = self.clone();
        tokio::task::spawn_blocking(move || {
            let Some(session) = runtime.persisted_session(depth, key.as_deref())? else {
                return Ok::<_, anyhow::Error>(None);
            };
            let entries: Vec<_> = session
                .active_branch_entries()
                .into_iter()
                .cloned()
                .collect();
            let model = saved_model.or_else(|| {
                entries.iter().rev().find_map(|entry| match entry {
                    FileEntry::ModelChange { payload, .. } => Some(ActModel {
                        provider: payload.provider.clone(),
                        id: payload.model_id.clone(),
                    }),
                    _ => None,
                })
            });
            let auth = crate::auth::AuthStorage::create(&runtime.agent_dir);
            let registry = crate::models::registry::ModelRegistry::create(
                auth,
                runtime.agent_dir.join("models.json"),
            );
            let window = model.as_ref().and_then(|model| {
                registry
                    .get_all()
                    .iter()
                    .find(|candidate| {
                        candidate.provider == model.provider && candidate.id == model.id
                    })
                    .map(|candidate| candidate.context_window)
            });
            Ok(Some((model, context_value(&entries, window))))
        })
        .await
        .ok()?
        .ok()?
    }
}

fn context_value(entries: &[FileEntry], window: Option<u64>) -> Option<Value> {
    super::super::turn_boundary::context_usage(entries, window).map(|usage| {
        json!({
            "tokens": usage.tokens, "contextWindow": usage.context_window, "percent": usage.percent,
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::manager::SessionManager;
    use pa_types::session::ActTerminalEntry;

    #[tokio::test]
    async fn completed_lanes_survive_restart_and_keep_nested_billing_separate() {
        let directory = tempfile::tempdir().unwrap();
        let root = Arc::new(tokio::sync::Mutex::new(SessionManager::in_memory(
            directory.path(),
        )));
        for depth in [1, 2, 4] {
            let name = if depth == 1 {
                "act".into()
            } else {
                format!("act-depth-{depth}")
            };
            let private_dir = directory.path().join(name);
            std::fs::create_dir_all(&private_dir).unwrap();
            std::fs::write(private_dir.join("model-key"), "openai/gpt-4\n").unwrap();
            let mut private = SessionManager::in_memory(directory.path());
            private.append_model_change("openai", "gpt-4").unwrap();
            private.append_message(serde_json::from_value(json!({
                "role": "assistant", "content": [], "api": "faux",
                "provider": "openai", "model": "gpt-4",
                "usage": pa_types::ai::Usage { input: 999, total_tokens: 999, ..Default::default() },
                "stopReason": "stop", "timestamp": 1,
            })).unwrap()).unwrap();
            let lines = private
                .get_all_entries()
                .iter()
                .map(serde_json::to_string)
                .collect::<Result<Vec<_>, _>>()
                .unwrap()
                .join("\n");
            std::fs::write(private_dir.join("session.jsonl"), format!("{lines}\n")).unwrap();
        }
        for (depth, tokens, status) in [
            (1, 10, ActTerminalStatus::Done),
            (2, 7, ActTerminalStatus::Error),
            (1, 20, ActTerminalStatus::Cancelled),
            (4, 3, ActTerminalStatus::Interrupted),
            (5, 100, ActTerminalStatus::Done),
        ] {
            root.lock()
                .await
                .append_act_terminal(ActTerminalEntry {
                    act_id: uuid::Uuid::new_v4().to_string(),
                    depth,
                    parent_act_id: None,
                    session_key: Some("openai/gpt-4".into()),
                    status,
                    usage: pa_types::ai::Usage {
                        input: tokens,
                        total_tokens: tokens,
                        ..Default::default()
                    },
                    model: Some(ActModel {
                        provider: "openai".into(),
                        id: "gpt-4".into(),
                    }),
                    error: None,
                })
                .unwrap();
        }
        let runtime = ActRuntime::with_persistence(
            directory.path().into(),
            directory.path().into(),
            Some(directory.path().into()),
        );
        runtime.bind_session(&root).await.unwrap();
        let nodes = runtime.context_tree_nodes().await;
        assert_eq!(nodes.len(), 2);
        assert_eq!(nodes[0]["id"], "act");
        assert_eq!(nodes[0]["status"], "cancelled");
        assert_eq!(nodes[0]["ownUsage"]["input"], 30);
        assert_eq!(nodes[0]["totalUsage"]["input"], 30);
        assert_eq!(nodes[0]["model"]["id"], "gpt-4");
        assert_eq!(nodes[0]["children"][0]["id"], "act-depth-2");
        assert_eq!(nodes[0]["children"][0]["status"], "error");
        assert_eq!(nodes[0]["children"][0]["ownUsage"]["input"], 7);
        assert_eq!(nodes[1]["id"], "act-depth-4");
        assert_eq!(nodes[1]["status"], "cancelled");
        assert_eq!(nodes[0]["contextUsage"]["tokens"], 999);
        assert!(nodes[0]["contextUsage"]["contextWindow"].as_u64().unwrap() > 0);
        assert_eq!(nodes[1]["children"], json!([]));
        let fresh_root = Arc::new(tokio::sync::Mutex::new(SessionManager::in_memory(
            directory.path(),
        )));
        runtime.bind_session(&fresh_root).await.unwrap();
        assert!(runtime.context_tree_nodes().await.is_empty());
    }
}
