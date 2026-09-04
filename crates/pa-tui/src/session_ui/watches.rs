use std::collections::HashSet;

use serde::Deserialize;
use serde_json::Value;

use super::{AgentView, SessionUi};
use crate::info_commands::ClientSpan;
use crate::info_panel::{InfoContent, InfoPanel};

#[derive(Debug, Clone, Deserialize)]
pub(super) struct Watch {
    id: String,
    label: String,
    status: String,
    command: Option<String>,
    ssh: Option<String>,
    pid: Option<i64>,
}

impl Watch {
    pub(super) fn running(&self) -> bool {
        self.status == "running"
    }
}

pub(super) fn parse(value: Option<&Value>) -> Vec<Watch> {
    let Some(rows) = value.and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut seen = HashSet::new();
    rows.iter()
        .take(128)
        .filter_map(|row| serde_json::from_value::<Watch>(row.clone()).ok())
        .filter(|watch| {
            !watch.id.is_empty()
                && watch.id.encode_utf16().count() <= 512
                && watch.label.encode_utf16().count() <= 128
                && matches!(
                    watch.status.as_str(),
                    "running" | "completed" | "failed" | "timed_out" | "cancelled"
                )
                && seen.insert(watch.id.clone())
        })
        .collect()
}

fn plain(text: &str) -> String {
    text.chars()
        .filter(|ch| !ch.is_control())
        .take(2000)
        .collect()
}

fn content(watches: &[Watch]) -> InfoContent {
    let mut rows = Vec::new();
    if watches.is_empty() {
        rows.push(vec![ClientSpan {
            text: "No external event watches".into(),
            color: None,
        }]);
    }
    for watch in watches {
        rows.push(vec![ClientSpan {
            text: format!(
                "{}: {}",
                plain(&watch.label),
                watch.status.replace('_', " ")
            ),
            color: None,
        }]);
        for text in [
            watch.command.as_deref().map(plain),
            watch
                .ssh
                .as_deref()
                .map(|ssh| format!("SSH: {}", plain(ssh))),
            watch.pid.map(|pid| format!("PID: {pid}")),
        ]
        .into_iter()
        .flatten()
        {
            rows.push(vec![ClientSpan { text, color: None }]);
        }
    }
    InfoContent::Rows(rows)
}

impl SessionUi {
    pub(super) fn open_watches_panel(&mut self, view: &mut AgentView) {
        if !self
            .client
            .supports_server_capability("external_event_watches")
        {
            self.note(
                "External event watches are unavailable on this daemon",
                view,
            );
            return;
        }
        self.open_info_panel(
            view,
            Some("External event watches".into()),
            content(&self.external_watches),
        );
        self.watches_panel_open = true;
        self.subagents_focused = false;
        self.update_subagent_summary(view);
    }

    pub(super) fn apply_external_watches(&mut self, value: Option<&Value>, view: &mut AgentView) {
        self.external_watches = parse(value);
        if self.watches_panel_open && view.info_panel.is_some() {
            view.info_panel = Some(InfoPanel::new(
                Some("External event watches".into()),
                content(&self.external_watches),
            ));
        }
        self.sync_activity_dock(view);
        self.dirty = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn legacy_snapshots_malformed_rows_and_duplicate_jobs_degrade_locally() {
        assert!(parse(None).is_empty());
        let watches = parse(Some(&json!([
            {"id":"job", "label":"training", "status":"running", "command":"echo\u{1b}hello"},
            {"id":"job", "label":"duplicate", "status":"completed"},
            {"id":"invalid", "label":"invalid", "status":"unknown"},
            {"id":"done", "label":"finished", "status":"completed"},
        ])));
        assert_eq!(watches.len(), 2);
        assert_eq!(watches.iter().filter(|watch| watch.running()).count(), 1);
        let InfoContent::Rows(rows) = content(&watches) else {
            panic!("watch rows")
        };
        assert_eq!(rows[1][0].text, "echohello");
        let dock = crate::chrome::ActivityDock {
            watches: 2,
            watches_running: 1,
            ..Default::default()
        };
        assert!(dock
            .groups()
            .contains(&crate::chrome::ActivityGroup::Watches));
        let quiet = crate::chrome::ActivityDock::default();
        assert!(!quiet
            .groups()
            .contains(&crate::chrome::ActivityGroup::Watches));
    }
}
