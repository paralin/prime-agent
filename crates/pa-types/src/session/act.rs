use serde::{Deserialize, Serialize};

use crate::ai::Usage;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActModel {
    pub provider: String,
    pub id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActTerminalStatus {
    Done,
    Cancelled,
    Error,
    Interrupted,
}

fn root_depth() -> u32 {
    1
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActStartEntry {
    pub act_id: String,
    #[serde(default = "root_depth")]
    pub depth: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_act_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outer_tool_call_id: Option<String>,
    pub usage_baseline: Usage,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActTerminalEntry {
    pub act_id: String,
    #[serde(default = "root_depth")]
    pub depth: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_act_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_key: Option<String>,
    pub status: ActTerminalStatus,
    pub usage: Usage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ActModel>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::FileEntry;

    #[test]
    fn old_act_rows_restore_default_depth_and_terminal_metadata() {
        let usage = serde_json::to_value(Usage::default()).unwrap();
        let start: FileEntry = serde_json::from_value(serde_json::json!({"type":"act_start","id":"start","parentId":null,"timestamp":"t","actId":"act","usageBaseline":usage})).unwrap();
        let FileEntry::ActStart { payload, .. } = &start else {
            panic!("typed start")
        };
        assert_eq!(payload.depth, 1);
        assert_eq!(start.id(), Some("start"));
        let terminal: FileEntry = serde_json::from_value(serde_json::json!({"type":"act_terminal","id":"end","parentId":"start","timestamp":"t","actId":"act","status":"interrupted","usage":usage,"model":{"provider":"faux","id":"model"}})).unwrap();
        let FileEntry::ActTerminal { payload, .. } = &terminal else {
            panic!("typed terminal")
        };
        assert_eq!(payload.status, ActTerminalStatus::Interrupted);
        assert_eq!(payload.depth, 1);
        assert_eq!(terminal.parent_id(), start.id());
        let roundtrip: FileEntry =
            serde_json::from_str(&serde_json::to_string(&terminal).unwrap()).unwrap();
        assert_eq!(roundtrip, terminal);
    }
}
