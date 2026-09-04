//! Wire changes introduced with protocol 8 / schema revision 31.

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
    ("attach", UNKNOWN_STOP_REASON),
    ("get_messages", UNKNOWN_STOP_REASON),
    ("get_session_context", UNKNOWN_STOP_REASON),
    ("get_session_tree", UNKNOWN_STOP_REASON),
];

/// Events whose messages can contain the extended assistant stop reason.
pub const EVENT_COMPATIBILITY: &[(&str, WireCompatibility)] = &[
    ("session_event", UNKNOWN_STOP_REASON),
    ("side_question_event", UNKNOWN_STOP_REASON),
    ("session_replaced", UNKNOWN_STOP_REASON),
    ("session_resynced", UNKNOWN_STOP_REASON),
    ("session_attached", UNKNOWN_STOP_REASON),
    ("session_snapshot_chunk", UNKNOWN_STOP_REASON),
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::DAEMON_PROTOCOL_VERSION;

    #[test]
    fn message_shapes_require_the_current_protocol() {
        for (_, compatibility) in COMMAND_COMPATIBILITY.iter().chain(EVENT_COMPATIBILITY) {
            assert_eq!(
                *compatibility,
                WireCompatibility::Incompatible {
                    minimum_protocol: DAEMON_PROTOCOL_VERSION,
                }
            );
        }
    }
}
