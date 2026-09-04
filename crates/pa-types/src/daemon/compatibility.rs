//! Wire changes introduced with protocol 8 / schema revisions 31–34.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireCompatibility {
    BackwardCompatible,
    CapabilityGated(&'static str),
    Incompatible { minimum_protocol: u64 },
}

const UNKNOWN_STOP_REASON: WireCompatibility = WireCompatibility::Incompatible {
    minimum_protocol: 8,
};

/// Commands whose responses can contain the extended assistant stop reason.
pub const COMMAND_COMPATIBILITY: &[(&str, WireCompatibility)] = &[
    (
        "create.config.rlmModelCandidates",
        WireCompatibility::CapabilityGated("runtime_launch_policy"),
    ),
    (
        "create.config.runtimeLaunchPolicy",
        WireCompatibility::CapabilityGated("runtime_launch_policy"),
    ),
    (
        "get_context_tree.children.act",
        WireCompatibility::BackwardCompatible,
    ),
    (
        "get_state.activeToolNames.claude-code",
        WireCompatibility::BackwardCompatible,
    ),
    (
        "get_state.externalEventWatches",
        WireCompatibility::BackwardCompatible,
    ),
    (
        "attach.state.externalEventWatches",
        WireCompatibility::BackwardCompatible,
    ),
    (
        "create.config.provider.claude-code",
        WireCompatibility::CapabilityGated("claude_code_children"),
    ),
    (
        "agent_message_inbox",
        WireCompatibility::CapabilityGated("agent_message_mailbox"),
    ),
    (
        "agent_message_wait",
        WireCompatibility::CapabilityGated("agent_message_mailbox"),
    ),
    (
        "send_message.messageId",
        WireCompatibility::CapabilityGated("agent_message_mailbox"),
    ),
    (
        "send_message.replyTo",
        WireCompatibility::CapabilityGated("agent_message_mailbox"),
    ),
    (
        "worker_deliver_message.messageId",
        WireCompatibility::CapabilityGated("agent_message_mailbox"),
    ),
    (
        "worker_deliver_message.replyTo",
        WireCompatibility::CapabilityGated("agent_message_mailbox"),
    ),
    (
        "send_message.receipt.acceptedAt",
        WireCompatibility::BackwardCompatible,
    ),
    (
        "send_message.receipt.targetSequence",
        WireCompatibility::BackwardCompatible,
    ),
    (
        "send_message.receipt.handoff",
        WireCompatibility::BackwardCompatible,
    ),
    (
        "send_message.receipt.replyTo",
        WireCompatibility::BackwardCompatible,
    ),
    ("attach", UNKNOWN_STOP_REASON),
    ("get_messages", UNKNOWN_STOP_REASON),
    ("get_session_context", UNKNOWN_STOP_REASON),
    ("get_session_tree", UNKNOWN_STOP_REASON),
    (
        "attach.messages.providerPayload",
        WireCompatibility::BackwardCompatible,
    ),
    (
        "get_messages.providerPayload",
        WireCompatibility::BackwardCompatible,
    ),
    (
        "get_session_context.providerPayload",
        WireCompatibility::BackwardCompatible,
    ),
    (
        "get_session_tree.providerPayload",
        WireCompatibility::BackwardCompatible,
    ),
    (
        "compact.providerNativeCompaction",
        WireCompatibility::BackwardCompatible,
    ),
    (
        "compact.details.scratchHandoff",
        WireCompatibility::BackwardCompatible,
    ),
    (
        "get_session_tree.entries.providerNativeCompaction",
        WireCompatibility::BackwardCompatible,
    ),
    (
        "get_session_tree.entries.details.scratchHandoff",
        WireCompatibility::BackwardCompatible,
    ),
    (
        "get_session_tree.entries.act_start",
        WireCompatibility::BackwardCompatible,
    ),
    (
        "get_session_tree.entries.act_terminal",
        WireCompatibility::BackwardCompatible,
    ),
];

/// Events whose messages can contain the extended assistant stop reason.
pub const EVENT_COMPATIBILITY: &[(&str, WireCompatibility)] = &[
    (
        "session_event.external_event_watches_changed",
        WireCompatibility::CapabilityGated("external_event_watches"),
    ),
    (
        "session_event.act_event",
        WireCompatibility::CapabilityGated("act_projection"),
    ),
    (
        "session_event.message.claude_code_session",
        WireCompatibility::BackwardCompatible,
    ),
    (
        "session_event.message.details.replyTo",
        WireCompatibility::BackwardCompatible,
    ),
    ("session_event", UNKNOWN_STOP_REASON),
    ("side_question_event", UNKNOWN_STOP_REASON),
    ("session_replaced", UNKNOWN_STOP_REASON),
    ("session_resynced", UNKNOWN_STOP_REASON),
    ("session_attached", UNKNOWN_STOP_REASON),
    ("session_snapshot_chunk", UNKNOWN_STOP_REASON),
    (
        "session_event.message.providerPayload",
        WireCompatibility::BackwardCompatible,
    ),
    (
        "session_snapshot_chunk.messages.providerPayload",
        WireCompatibility::BackwardCompatible,
    ),
    (
        "session_attached.messages.providerPayload",
        WireCompatibility::BackwardCompatible,
    ),
    (
        "session_replaced.messages.providerPayload",
        WireCompatibility::BackwardCompatible,
    ),
    (
        "session_resynced.messages.providerPayload",
        WireCompatibility::BackwardCompatible,
    ),
    (
        "session_event.compaction_end.result.providerNativeCompaction",
        WireCompatibility::BackwardCompatible,
    ),
    (
        "session_event.compaction_end.result.details.scratchHandoff",
        WireCompatibility::BackwardCompatible,
    ),
];

/// An older daemon must never silently ignore an explicit launch restriction.
#[must_use]
pub fn requires_runtime_launch_policy(config: &serde_json::Value) -> bool {
    config
        .get("rlmModelCandidates")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|candidates| !candidates.is_empty())
        || config
            .get("rlmMaxDepthCeiling")
            .is_some_and(|value| !value.is_null())
        || config
            .get("disableRlmAct")
            .and_then(serde_json::Value::as_bool)
            == Some(true)
        || config.get("rpcOnly").and_then(serde_json::Value::as_bool) == Some(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::DAEMON_PROTOCOL_VERSION;

    #[test]
    fn ordered_role_candidates_require_negotiated_launch_support() {
        #[derive(serde::Deserialize)]
        struct LegacyCreateConfig {
            provider: String,
        }
        assert!(!requires_runtime_launch_policy(&serde_json::json!({})));
        assert!(!requires_runtime_launch_policy(
            &serde_json::json!({"rlmModelCandidates":[]})
        ));
        assert!(requires_runtime_launch_policy(&serde_json::json!({
            "rlmModelCandidates":["provider/primary", "provider/backup"]
        })));
        assert!(COMMAND_COMPATIBILITY.contains(&(
            "create.config.rlmModelCandidates",
            WireCompatibility::CapabilityGated("runtime_launch_policy")
        )));
        let old: LegacyCreateConfig = serde_json::from_value(serde_json::json!({
            "provider":"provider", "rlmModelCandidates":["provider/primary"]
        }))
        .unwrap();
        assert_eq!(old.provider, "provider");
    }

    #[test]
    fn message_shapes_require_the_current_protocol() {
        for (_, compatibility) in COMMAND_COMPATIBILITY
            .iter()
            .chain(EVENT_COMPATIBILITY)
            .filter(|(shape, compatibility)| {
                !shape.contains('.')
                    && matches!(compatibility, WireCompatibility::Incompatible { .. })
            })
        {
            assert_eq!(
                *compatibility,
                WireCompatibility::Incompatible {
                    minimum_protocol: DAEMON_PROTOCOL_VERSION,
                }
            );
        }
    }

    #[test]
    fn native_history_is_optional_for_new_clients_with_old_daemons() {
        let message: crate::session::AgentMessage = serde_json::from_value(serde_json::json!({
            "role":"compactionSummary", "summary":"portable display", "tokensBefore":100, "timestamp":1,
        })).unwrap();
        let crate::session::AgentMessage::CompactionSummary(summary) = message else {
            panic!("summary role")
        };
        assert!(summary.provider_payload.is_none());
        for (_, compatibility) in COMMAND_COMPATIBILITY
            .iter()
            .chain(EVENT_COMPATIBILITY)
            .filter(|(shape, _)| shape.contains('.'))
        {
            match compatibility {
                WireCompatibility::CapabilityGated(capability) => {
                    assert!(matches!(
                        *capability,
                        "agent_message_mailbox"
                            | "claude_code_children"
                            | "act_projection"
                            | "external_event_watches"
                            | "runtime_launch_policy"
                    ));
                }
                WireCompatibility::BackwardCompatible | WireCompatibility::Incompatible { .. } => {
                    assert_eq!(*compatibility, WireCompatibility::BackwardCompatible);
                }
            }
        }
    }

    #[test]
    fn old_clients_can_read_new_daemon_compaction_summaries() {
        #[derive(serde::Deserialize)]
        struct LegacySummary {
            summary: String,
            #[serde(rename = "tokensBefore")]
            tokens_before: u64,
            timestamp: u64,
        }
        let wire = serde_json::json!({
            "role":"compactionSummary", "summary":"portable display", "tokensBefore":100, "timestamp":1,
            "providerPayload":{"type":"openaiResponsesHistory", "provider":"openai-codex", "items":[{"type":"compaction", "encrypted_content":"opaque"}]},
        });
        let summary: LegacySummary = serde_json::from_value(wire).unwrap();
        assert_eq!(summary.summary, "portable display");
        assert_eq!(summary.tokens_before, 100);
        assert_eq!(summary.timestamp, 1);
    }

    #[test]
    fn optional_compaction_metadata_works_with_old_and_new_clients() {
        #[derive(serde::Deserialize)]
        struct LegacyResult {
            summary: String,
        }
        let legacy = serde_json::json!({"summary":"portable", "firstKeptEntryId":"kept", "tokensBefore":100});
        let entry: crate::session::CompactionEntry =
            serde_json::from_value(legacy.clone()).unwrap();
        assert!(entry.provider_native_compaction.is_none());
        let mut current = legacy;
        current["providerNativeCompaction"] =
            serde_json::json!({"provider":"openai-codex", "items":[]});
        current["details"] =
            serde_json::json!({"scratchHandoff":{"version":1,"path":"checkpoint.org"}});
        let old: LegacyResult = serde_json::from_value(current.clone()).unwrap();
        assert_eq!(old.summary, "portable");
        let new: crate::session::CompactionEntry = serde_json::from_value(current).unwrap();
        assert!(new.provider_native_compaction.is_some());
        assert_eq!(new.details.unwrap()["scratchHandoff"]["version"], 1);
    }

    #[test]
    fn context_tree_readers_accept_optional_act_nodes_in_both_directions() {
        #[derive(serde::Deserialize)]
        struct LegacyNode {
            id: String,
            #[serde(default)]
            children: Vec<LegacyNode>,
        }
        let old: LegacyNode =
            serde_json::from_value(serde_json::json!({"id":"root", "children":[]})).unwrap();
        assert!(old.children.is_empty());
        let current: LegacyNode = serde_json::from_value(serde_json::json!({"id":"root", "children":[{
            "id":"act", "depth":1,"status":"done", "cancellationCapability":"posix-managed",
            "ownUsage":crate::ai::Usage::default(),"totalUsage":crate::ai::Usage::default(),"children":[],
        }]})).unwrap();
        assert_eq!(current.children[0].id, "act");
        assert!(COMMAND_COMPATIBILITY.contains(&(
            "get_context_tree.children.act",
            WireCompatibility::BackwardCompatible
        )));
    }

    #[test]
    fn legacy_tree_readers_preserve_optional_act_records() {
        #[derive(serde::Deserialize, serde::Serialize)]
        struct LegacyUnknown {
            #[serde(flatten)]
            fields: serde_json::Map<String, serde_json::Value>,
        }
        for kind in ["act_start", "act_terminal"] {
            let wire = serde_json::json!({
                "type":kind, "id":"record", "parentId":null, "timestamp":"t",
                "actId":"assignment", "usageBaseline":crate::ai::Usage::default(),
                "usage":crate::ai::Usage::default(), "status":"done",
            });
            let legacy: LegacyUnknown = serde_json::from_value(wire.clone()).unwrap();
            assert_eq!(serde_json::to_value(legacy).unwrap(), wire);
            let current: crate::session::FileEntry = serde_json::from_value(wire).unwrap();
            assert_eq!(current.id(), Some("record"));
        }
        let old: crate::session::FileEntry = serde_json::from_value(serde_json::json!({
            "type":"custom", "id":"old", "parentId":null, "timestamp":"t", "customType":"other",
        }))
        .unwrap();
        assert_eq!(old.id(), Some("old"));
    }

    #[test]
    fn old_clients_ignore_optional_mailbox_receipt_metadata() {
        #[derive(serde::Deserialize)]
        struct LegacyReceipt {
            id: String,
            #[serde(rename = "deliveryStatus")]
            delivery_status: String,
        }
        let current = serde_json::json!({"id":"stable", "deliveryStatus":"queued", "replyTo":"task",
            "acceptedAt":"accepted", "targetSequence":2, "handoff":"queue"});
        let old: LegacyReceipt = serde_json::from_value(current).unwrap();
        assert_eq!(old.id, "stable");
        assert_eq!(old.delivery_status, "queued");
        for command in [
            "agent_message_inbox",
            "agent_message_wait",
            "send_message.messageId",
            "send_message.replyTo",
        ] {
            assert!(COMMAND_COMPATIBILITY.contains(&(
                command,
                WireCompatibility::CapabilityGated("agent_message_mailbox")
            )));
        }
    }

    #[test]
    fn legacy_custom_message_readers_accept_native_claude_identity_rows() {
        #[derive(serde::Deserialize)]
        struct LegacyCustomMessage {
            role: String,
            #[serde(rename = "customType")]
            custom_type: String,
            content: String,
            display: bool,
        }
        let row = serde_json::json!({"role":"custom", "customType":"claude_code_session", "content":"", "display":false,
            "details":{"sessionId":"sdk-session", "model":"sonnet"}, "timestamp":1});
        let legacy: LegacyCustomMessage = serde_json::from_value(row).unwrap();
        assert_eq!(legacy.role, "custom");
        assert_eq!(legacy.custom_type, "claude_code_session");
        assert!(legacy.content.is_empty());
        assert!(!legacy.display);
        assert!(COMMAND_COMPATIBILITY.contains(&(
            "create.config.provider.claude-code",
            WireCompatibility::CapabilityGated("claude_code_children")
        )));
        assert!(EVENT_COMPATIBILITY.contains(&(
            "session_event.message.claude_code_session",
            WireCompatibility::BackwardCompatible
        )));
    }
}
