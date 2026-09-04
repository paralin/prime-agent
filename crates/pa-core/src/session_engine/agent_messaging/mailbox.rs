//! Durable mailbox projection. Acceptance survives retries and consumption is scoped to a target.
use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub mod runtime;

use super::AGENT_MESSAGE_SOURCE;

pub const ACCEPTED_CUSTOM_TYPE: &str = "agent_message.accepted";
pub const CONSUMED_CUSTOM_TYPE: &str = "agent_message.consumed";
pub const HANDOFF_CUSTOM_TYPE: &str = "agent_message.handoff";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MailboxEnvelope {
    pub id: String,
    pub source: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_relationship: Option<String>,
    pub target: Value,
    pub accepted_at: String,
    pub sequence: u64,
}

impl MailboxEnvelope {
    #[must_use]
    pub fn target_session_id(&self) -> &str {
        self.target["sessionId"].as_str().unwrap_or_default()
    }

    #[must_use]
    pub fn matches(&self, filter: &MailboxFilter) -> bool {
        filter.sender.as_deref().is_none_or(|sender| {
            self.from.as_ref().is_some_and(|from| {
                ["sessionId", "activeSessionId", "sessionName", "clientId"]
                    .iter()
                    .any(|field| from[*field].as_str() == Some(sender))
            })
        }) && filter
            .reply_to
            .as_ref()
            .is_none_or(|reply_to| self.reply_to.as_ref() == Some(reply_to))
    }
}

#[derive(Debug, Clone, Default)]
pub struct MailboxFilter {
    pub sender: Option<String>,
    pub reply_to: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MailboxHandoff {
    pub message_id: String,
    pub target_session_id: String,
    pub handoff: String,
    pub delivery_status: String,
    pub handed_off_at: String,
}

fn custom_details<'a>(entry: &'a Value, kind: &str) -> Option<&'a Value> {
    (entry["type"] == "custom_message" && entry["customType"] == kind)
        .then(|| entry.get("details"))
        .flatten()
        .filter(|details| details.is_object())
}

fn acceptance(entry: &Value, target: Option<&str>) -> Option<MailboxEnvelope> {
    let details = custom_details(entry, ACCEPTED_CUSTOM_TYPE)?;
    let envelope: MailboxEnvelope =
        serde_json::from_value(details.get("envelope")?.clone()).ok()?;
    (envelope.source == AGENT_MESSAGE_SOURCE
        && envelope.target["sessionId"].is_string()
        && target.is_none_or(|target| envelope.target_session_id() == target))
    .then_some(envelope)
}

#[must_use]
pub fn project(entries: &[Value], target: Option<&str>) -> Vec<MailboxEnvelope> {
    let mut accepted = Vec::new();
    let mut seen = HashSet::new();
    let mut consumed_ids = HashSet::new();
    let mut consumed_targets = HashSet::new();
    for entry in entries {
        if let Some(envelope) = acceptance(entry, target) {
            let key = (
                envelope.target_session_id().to_string(),
                envelope.id.clone(),
            );
            if seen.insert(key) {
                accepted.push(envelope);
            }
        }
        if let Some(details) = custom_details(entry, CONSUMED_CUSTOM_TYPE) {
            let Some(id) = details["messageId"].as_str() else {
                continue;
            };
            if let Some(target) = details["targetSessionId"].as_str() {
                consumed_targets.insert((target.to_string(), id.to_string()));
            } else {
                consumed_ids.insert(id.to_string());
            }
        }
    }
    accepted.retain(|envelope| {
        !consumed_ids.contains(&envelope.id)
            && !consumed_targets.contains(&(
                envelope.target_session_id().to_string(),
                envelope.id.clone(),
            ))
    });
    accepted.sort_by_key(|envelope| envelope.sequence);
    accepted
}

#[must_use]
pub fn find_acceptance(
    entries: &[Value],
    message_id: &str,
    target: Option<&str>,
) -> Option<MailboxEnvelope> {
    entries
        .iter()
        .filter_map(|entry| acceptance(entry, target))
        .find(|envelope| envelope.id == message_id)
}

/// # Errors
/// Returns an error when the sequence has reached the representable maximum.
pub fn next_sequence(entries: &[Value], target: Option<&str>) -> anyhow::Result<u64> {
    entries
        .iter()
        .filter_map(|entry| acceptance(entry, target))
        .map(|envelope| envelope.sequence)
        .max()
        .unwrap_or(0)
        .checked_add(1)
        .ok_or_else(|| anyhow::anyhow!("agent mailbox sequence exhausted"))
}

#[must_use]
pub fn find_handoff(entries: &[Value], message_id: &str, target: &str) -> Option<MailboxHandoff> {
    entries.iter().find_map(|entry| {
        let details = custom_details(entry, HANDOFF_CUSTOM_TYPE)?;
        let handoff: MailboxHandoff = serde_json::from_value(details.clone()).ok()?;
        (handoff.message_id == message_id
            && handoff.target_session_id == target
            && matches!(handoff.handoff.as_str(), "waiter" | "context" | "queue")
            && matches!(handoff.delivery_status.as_str(), "delivered" | "queued"))
        .then_some(handoff)
    })
}

/// # Errors
/// Returns an error unless the supplied limit is an integer between 1 and 100.
pub fn normalize_limit(value: Option<&Value>) -> anyhow::Result<usize> {
    let Some(value) = value else { return Ok(20) };
    let limit = value.as_u64().filter(|limit| (1..=100).contains(limit));
    limit.map(|limit| limit as usize).ok_or_else(|| {
        anyhow::anyhow!("agent_message.inbox limit must be an integer between 1 and 100")
    })
}

/// # Errors
/// Returns an error unless the supplied timeout is an integer between 1 and 300000.
pub fn normalize_timeout(value: Option<&Value>) -> anyhow::Result<u64> {
    let Some(value) = value else {
        return Ok(30_000);
    };
    value
        .as_u64()
        .filter(|timeout| (1..=300_000).contains(timeout))
        .ok_or_else(|| {
            anyhow::anyhow!("agent_message.wait timeout_ms must be an integer between 1 and 300000")
        })
}

/// # Errors
/// Returns an error when a supplied sender or reply filter is not a nonempty string.
pub fn normalize_filter(value: &Value) -> anyhow::Result<MailboxFilter> {
    fn optional(value: Option<&Value>, name: &str) -> anyhow::Result<Option<String>> {
        match value {
            None | Some(Value::Null) => Ok(None),
            Some(value) => value
                .as_str()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(|value| Some(value.to_string()))
                .ok_or_else(|| anyhow::anyhow!("agent_message {name} must be a non-empty string")),
        }
    }
    Ok(MailboxFilter {
        sender: optional(value.get("sender"), "sender")?,
        reply_to: optional(value.get("reply_to"), "reply_to")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn accepted(id: &str, target: &str, sequence: u64) -> Value {
        json!({"type":"custom_message", "customType":ACCEPTED_CUSTOM_TYPE,
            "details":{"envelope":{"id":id,"source":AGENT_MESSAGE_SOURCE,"message":id,
                "target":{"sessionId":target}, "acceptedAt":"now", "sequence":sequence,
                "from":{"sessionName":"worker", "sessionId":"sender"}, "replyTo":"task"}}})
    }

    #[test]
    fn reload_deduplicates_target_ids_and_orders_unconsumed_acceptances() {
        let entries = vec![
            accepted("same", "a", 4),
            accepted("second", "a", 3),
            accepted("same", "a", 7),
            accepted("same", "b", 2),
            json!({"type":"custom_message", "customType":CONSUMED_CUSTOM_TYPE,
                "details":{"messageId":"same", "targetSessionId":"a"}}),
            json!({"type":"custom_message", "customType":ACCEPTED_CUSTOM_TYPE,
                "details":{"envelope":{"source":"other"}}}),
        ];
        let messages = project(&entries, None);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].target_session_id(), "b");
        assert_eq!(messages[1].id, "second");
        assert_eq!(project(&entries, Some("a")).len(), 1);
        assert_eq!(
            find_acceptance(&entries, "same", Some("a"))
                .unwrap()
                .sequence,
            4
        );
        assert_eq!(next_sequence(&entries, Some("a")).unwrap(), 8);
    }

    #[test]
    fn legacy_consumption_and_sender_reply_filters_match_after_reload() {
        let mut entries = vec![accepted("first", "a", 1), accepted("first", "b", 1)];
        let message = project(&entries, None).remove(0);
        assert!(message
            .matches(&normalize_filter(&json!({"sender":" worker ","reply_to":"task"})).unwrap()));
        assert!(message.matches(&normalize_filter(&json!({"sender":"sender"})).unwrap()));
        assert!(!message.matches(&normalize_filter(&json!({"reply_to":"other"})).unwrap()));
        entries.push(
            json!({"type":"custom_message", "customType":CONSUMED_CUSTOM_TYPE,
            "details":{"messageId":"first"}}),
        );
        assert!(project(&entries, None).is_empty());
    }

    #[test]
    fn handoff_recovery_ignores_retry_and_invalid_status() {
        let row = |handoff, status| {
            json!({"type":"custom_message", "customType":HANDOFF_CUSTOM_TYPE,
            "details":{"messageId":"m", "targetSessionId":"a", "handoff":handoff,
                "deliveryStatus":status, "handedOffAt":"now"}})
        };
        let entries = vec![
            row("retry", "queued"),
            row("queue", "invalid"),
            row("waiter", "delivered"),
        ];
        assert_eq!(find_handoff(&entries, "m", "a").unwrap().handoff, "waiter");
        assert!(find_handoff(&entries, "m", "b").is_none());
    }

    #[test]
    fn bounds_reject_fractional_negative_null_and_empty_filters() {
        assert_eq!(normalize_limit(None).unwrap(), 20);
        assert_eq!(normalize_timeout(None).unwrap(), 30_000);
        for value in [json!(0), json!(-1), json!(1.5), Value::Null, json!("1")] {
            assert!(normalize_limit(Some(&value)).is_err());
            assert!(normalize_timeout(Some(&value)).is_err());
        }
        assert!(normalize_limit(Some(&json!(101))).is_err());
        assert!(normalize_timeout(Some(&json!(300_001))).is_err());
        assert!(normalize_filter(&json!({"sender":" "})).is_err());
        assert!(normalize_filter(&json!({"reply_to":1})).is_err());
        assert_eq!(normalize_limit(Some(&json!(100))).unwrap(), 100);
        assert_eq!(normalize_timeout(Some(&json!(300_000))).unwrap(), 300_000);
        assert!(next_sequence(&[accepted("last", "a", u64::MAX)], None).is_err());
    }
}
