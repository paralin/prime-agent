//! Host request handling: execute-side requests answered by the host
//! (harness/goal/etc.) and their settle/exit waits.

use crate::kernel::cancellation::AbortSignal;
use crate::kernel::host_channel::{ChannelInterrupt, ChannelSend, HostRequestChannel};
use std::sync::atomic::Ordering;

use super::{
    anyhow, json, lock, Arc, Duration, HostRequestPayload, Inner, Value,
    MAX_HANDLED_HOST_REQUEST_IDS,
};

/// The cell source attached to a host request is capped at this many
/// characters (TS #2475: `MAX_CELL_SOURCE_CHARS`, repl-manager.ts:81-82):
/// the spawning cell's source rides on every host request it triggers, and
/// an uncapped multi-KB cell re-shipped per progress note (and persisted
/// per child as spawn code) dwarfs the request it tags.
const MAX_CELL_SOURCE_CHARS: usize = 2 * 1024;

/// Cap a cell source for host-request attachment: a source within the cap
/// passes verbatim; a longer one keeps the first `MAX_CELL_SOURCE_CHARS`
/// characters and carries the truncation marker so the consumer knows the
/// prefix is partial (TS repl-manager.ts:110-111's cap helper).
fn cap_cell_source(code: &str) -> String {
    if code.chars().count() <= MAX_CELL_SOURCE_CHARS {
        code.to_string()
    } else {
        let head: String = code.chars().take(MAX_CELL_SOURCE_CHARS).collect();
        format!("{head}\n[... cell source truncated at {MAX_CELL_SOURCE_CHARS} chars ...]")
    }
}

// ---------------------------------------------------------------------------
// Host requests
// ---------------------------------------------------------------------------

impl Inner {
    /// Dispatch one typed request from kernel code to the registered handler
    /// and reply over the protocol. Unhandled requests answer with an error.
    pub(crate) fn start_host_request(self: &Arc<Self>, request_id: &str, data: Value) {
        {
            let mut g = lock(&self.guarded);
            let (seen, order) = &mut g.handled_host_request_ids;
            if seen.contains(request_id) {
                return;
            }
            seen.insert(request_id.to_string());
            order.push_back(request_id.to_string());
            while seen.len() > MAX_HANDLED_HOST_REQUEST_IDS {
                if let Some(oldest) = order.pop_front() {
                    seen.remove(&oldest);
                } else {
                    break;
                }
            }
        }
        let (execution, generation) = {
            let guarded = lock(&self.guarded);
            (guarded.active_execution.clone(), guarded.start_generation)
        };
        let duplex = data
            .get("type")
            .and_then(Value::as_str)
            .is_some_and(|kind| self.options.host_handlers.get_duplex(kind).is_some());
        if duplex {
            if let Some(execution) = &execution {
                execution
                    .cooperative_host_request
                    .store(true, Ordering::Release);
            }
        }
        let signal = AbortSignal::new();
        let interrupt_signal = execution
            .as_ref()
            .and_then(|execution| execution.opts.signal.clone());
        let outer_tool_call_id = execution
            .as_ref()
            .and_then(|execution| execution.opts.outer_tool_call_id.clone());
        let weak = Arc::downgrade(self);
        let outbound_id = request_id.to_string();
        let send: ChannelSend = Arc::new(move |mut data| {
            let weak = weak.clone();
            let id = outbound_id.clone();
            Box::pin(async move {
                let inner = weak.upgrade().ok_or_else(|| anyhow!("kernel has closed"))?;
                anyhow::ensure!(!inner.start_stale(generation), "kernel has closed");
                data["status"] = json!("event");
                inner
                    .write_line(&json!({"type":"host_message","id":id,"data":data}))
                    .await
            })
        });
        let weak = Arc::downgrade(self);
        let interrupt: ChannelInterrupt = Arc::new(move |grace_ms| {
            let weak = weak.clone();
            let execution = execution.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(grace_ms)).await;
                let Some(inner) = weak.upgrade() else {
                    return;
                };
                let Some(execution) = execution else {
                    return;
                };
                inner.interrupt_execution_once(&execution).await;
            });
        });
        let (channel, sender) = HostRequestChannel::new(
            signal,
            interrupt_signal.clone(),
            outer_tool_call_id,
            send,
            interrupt,
        );
        lock(&self.guarded)
            .host_channels
            .insert(request_id.to_string(), (channel.clone(), sender));
        if let Some(parent) = interrupt_signal {
            let channel = channel.clone();
            tokio::spawn(async move {
                tokio::select! {
                    () = parent.cancelled() => channel.close(),
                    () = channel.signal.cancelled() => {},
                }
            });
        }
        let inner = Arc::clone(self);
        let request_id = request_id.to_string();
        let task = tokio::spawn(async move {
            let result = if duplex {
                inner.handle_host_request(&data, channel.clone()).await
            } else {
                tokio::select! {
                    biased;
                    () = channel.signal.cancelled() => Err(anyhow!("host request cancelled")),
                    result = inner.handle_host_request(&data, channel.clone()) => result,
                }
            };
            let reply = match result {
                Ok(result) => json!({ "status": "ok", "result": result }),
                Err(error) => {
                    inner.append_diagnostic(&format!(
                        "host request failed for {request_id}: {error:#}"
                    ));
                    json!({ "status": "error", "error": format!("{error:#}") })
                }
            };
            let frame = json!({ "type": "host_reply", "id": request_id, "data": reply });
            if inner.start_stale(generation) {
                channel.close();
                return;
            }
            if let Err(error) = inner.write_line(&frame).await {
                inner.append_diagnostic(&format!(
                    "failed to send host request reply for {request_id}: {error:#}"
                ));
            }
            channel.close();
            lock(&inner.guarded).host_channels.remove(&request_id);
        });
        let mut g = lock(&self.guarded);
        // Completed task handles are dropped so the inflight set stays bounded.
        g.host_inflight.retain(|handle| !handle.is_finished());
        g.host_inflight.push(task);
    }

    async fn handle_host_request(
        &self,
        data: &Value,
        channel: Arc<HostRequestChannel>,
    ) -> anyhow::Result<Value> {
        let Some(obj) = data.as_object() else {
            return Err(anyhow!("host request payload must be an object"));
        };
        let request_type = obj
            .get("type")
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty())
            .ok_or_else(|| anyhow!("host request payload must have a string type"))?;
        // Tag the request with the cell that triggered it. A blocking call is
        // still the in-flight execution; detached spawns fire after the
        // scheduling cell goes idle, so fall back to that last cell's source.
        let cell_source_code = {
            let g = lock(&self.guarded);
            g.active_execution
                .as_ref()
                .map(|e| cap_cell_source(&e.code))
                .or_else(|| g.last_cell_code.as_deref().map(cap_cell_source))
        };
        let mut payload = obj.clone();
        if let Some(code) = &cell_source_code {
            payload.insert("cellSourceCode".to_string(), Value::String(code.clone()));
        }
        let payload = HostRequestPayload {
            data: Value::Object(payload),
            cell_source_code,
        };
        if let Some(handler) = self.options.host_handlers.get_duplex(request_type) {
            return handler(payload, channel).await;
        }
        let handler = self
            .options
            .host_handlers
            .get(request_type)
            .ok_or_else(|| {
                anyhow!("host request type \"{request_type}\" is not available in this session")
            })?;
        handler(payload).await
    }

    /// Wait (bounded) for the in-flight host request tasks to settle.
    pub(crate) async fn wait_for_host_requests_to_settle(
        &self,
        tasks: Vec<tokio::task::JoinHandle<()>>,
        timeout_ms: u64,
    ) {
        let all = async {
            for task in tasks {
                let _ = task.await;
            }
        };
        if tokio::time::timeout(Duration::from_millis(timeout_ms), all)
            .await
            .is_err()
        {
            self.append_diagnostic(&format!(
                "timed out waiting {timeout_ms}ms for host request task(s) during shutdown"
            ));
        }
    }

    pub(crate) async fn wait_for_kernel_exit(&self) {
        let exit_rx = match lock(&self.child).as_ref() {
            Some(child) => child.exit_rx.clone(),
            None => return,
        };
        let mut exit_rx = exit_rx;
        if exit_rx.borrow().is_some() {
            return;
        }
        loop {
            if exit_rx.borrow().is_some() {
                return;
            }
            if exit_rx.changed().await.is_err() {
                return;
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    fn marker() -> String {
        format!("\n[... cell source truncated at {MAX_CELL_SOURCE_CHARS} chars ...]")
    }

    #[test]
    fn a_source_within_the_cap_attaches_verbatim() {
        let code = "x".repeat(MAX_CELL_SOURCE_CHARS);
        assert_eq!(cap_cell_source(&code), code);
    }

    #[test]
    fn an_oversized_source_keeps_the_prefix_and_carries_the_marker() {
        let code = "x".repeat(MAX_CELL_SOURCE_CHARS + 1);
        let capped = cap_cell_source(&code);
        let expected = format!("{}{}", "x".repeat(MAX_CELL_SOURCE_CHARS), marker());
        assert_eq!(capped, expected);
    }

    #[test]
    fn a_short_source_is_untouched() {
        let code = "print(1)";
        assert_eq!(cap_cell_source(code), code);
    }

    #[test]
    fn the_cap_lands_on_a_character_boundary() {
        // A multi-byte tail: the cap must slice by characters, never split one.
        let code = format!("{}{}", "é".repeat(MAX_CELL_SOURCE_CHARS), "é");
        let capped = cap_cell_source(&code);
        assert!(capped.starts_with(&"é".repeat(MAX_CELL_SOURCE_CHARS)));
        assert!(capped.ends_with(&marker()));
    }
}
