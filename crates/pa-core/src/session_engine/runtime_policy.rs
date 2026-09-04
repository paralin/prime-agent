use crate::kernel::shared::HostRequestHandlers;

/// Launch restrictions survive rebuilds and can only become stricter.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimePolicy {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rlm_max_depth_ceiling: Option<u32>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub disable_rlm_act: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub rpc_only: bool,
}

impl RuntimePolicy {
    #[must_use]
    pub fn merge(self, other: Self) -> Self {
        Self {
            rlm_max_depth_ceiling: match (self.rlm_max_depth_ceiling, other.rlm_max_depth_ceiling) {
                (Some(left), Some(right)) => Some(left.min(right)),
                (left, right) => left.or(right),
            },
            disable_rlm_act: self.disable_rlm_act || other.disable_rlm_act,
            rpc_only: self.rpc_only || other.rpc_only,
        }
    }

    #[must_use]
    pub fn max_depth(self, requested: u32) -> u32 {
        if self.rpc_only {
            0
        } else {
            self.rlm_max_depth_ceiling
                .map_or(requested, |ceiling| requested.min(ceiling))
        }
    }

    #[must_use]
    pub fn act_enabled(self) -> bool {
        !self.rpc_only && !self.disable_rlm_act
    }

    pub fn restrict_handlers(self, handlers: &mut HostRequestHandlers) {
        handlers.retain(|name| {
            (self.act_enabled() || name != "rlm.act")
                && (!self.rpc_only
                    || !(name.starts_with("goal.")
                        || name.starts_with("rlm_heartbeat.")
                        || name.starts_with("agent_message.")
                        || name.starts_with("session.external_event.")
                        || name.starts_with("refine.")))
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kernel::shared::host_handler;

    #[test]
    fn restrictions_cannot_be_relaxed_by_rebuild_overrides() {
        let restricted = RuntimePolicy {
            rlm_max_depth_ceiling: Some(2),
            disable_rlm_act: true,
            rpc_only: false,
        };
        let merged = restricted.merge(RuntimePolicy {
            rlm_max_depth_ceiling: Some(8),
            ..Default::default()
        });
        assert_eq!(merged.max_depth(10), 2);
        assert_eq!(merged.max_depth(1), 1);
        assert!(!merged.act_enabled());
        let harness = merged.merge(RuntimePolicy {
            rpc_only: true,
            ..Default::default()
        });
        assert_eq!(harness.max_depth(10), 0);
        let mut handlers = HostRequestHandlers::default();
        for name in [
            "goal.get",
            "rlm_heartbeat.create",
            "agent_message.send",
            "session.external_event.emit",
            "refine.run",
            "rlm.run",
            "compact.run",
            "model.info",
        ] {
            handlers.register(
                name,
                host_handler(|_| async { Ok(serde_json::Value::Null) }),
            );
        }
        handlers.register_duplex(
            "rlm.act",
            crate::kernel::host_channel::duplex_host_handler(|_, _| async {
                Ok(serde_json::Value::Null)
            }),
        );
        let unrestricted = handlers.clone();
        harness.restrict_handlers(&mut handlers);
        assert!(handlers.get_duplex("rlm.act").is_none());
        assert!(unrestricted.get_duplex("rlm.act").is_some());
        assert_eq!(handlers.len(), 3);
        assert!(handlers.get("rlm.run").is_some());
        assert!(handlers.get("compact.run").is_some());
        assert!(handlers.get("model.info").is_some());
    }
}
