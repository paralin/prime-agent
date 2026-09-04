use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;
use tokio::sync::{mpsc, Mutex};

use super::cancellation::AbortSignal;
use super::shared::{HostHandlerFuture, HostRequestPayload};

pub type ChannelSend =
    Arc<dyn Fn(Value) -> Pin<Box<dyn Future<Output = Result<()>> + Send>> + Send + Sync>;
pub type ChannelInterrupt = Arc<dyn Fn(u64) + Send + Sync>;
pub type HostDuplexHandlerFn =
    Arc<dyn Fn(HostRequestPayload, Arc<HostRequestChannel>) -> HostHandlerFuture + Send + Sync>;

pub struct HostRequestChannel {
    pub signal: AbortSignal,
    pub interrupt_signal: Option<AbortSignal>,
    pub outer_tool_call_id: Option<String>,
    messages: Mutex<mpsc::Receiver<Value>>,
    send: ChannelSend,
    interrupt: ChannelInterrupt,
}

impl HostRequestChannel {
    pub fn new(
        signal: AbortSignal,
        interrupt_signal: Option<AbortSignal>,
        outer_tool_call_id: Option<String>,
        send: ChannelSend,
        interrupt: ChannelInterrupt,
    ) -> (Arc<Self>, mpsc::Sender<Value>) {
        let (tx, rx) = mpsc::channel(128);
        (
            Arc::new(Self {
                signal,
                interrupt_signal,
                outer_tool_call_id,
                messages: Mutex::new(rx),
                send,
                interrupt,
            }),
            tx,
        )
    }

    /// # Errors
    /// Returns an error if closed or the outbound event cannot be delivered.
    pub async fn send(&self, event: Value) -> Result<()> {
        anyhow::ensure!(!self.signal.is_aborted(), "host request channel closed");
        anyhow::ensure!(
            event.is_object(),
            "host request channel message must be an object"
        );
        tokio::select! {
            biased;
            () = self.signal.cancelled() => anyhow::bail!("host request channel closed"),
            result = (self.send)(event) => result,
        }
    }

    /// # Errors
    /// Returns an error on cancellation or when the channel closes.
    pub async fn receive(&self, signal: Option<&AbortSignal>) -> Result<Value> {
        let cancel = async {
            match signal {
                Some(signal) => signal.cancelled().await,
                None => std::future::pending().await,
            }
        };
        tokio::pin!(cancel);
        let mut messages = tokio::select! {
            biased;
            () = self.signal.cancelled() => anyhow::bail!("host request channel closed"),
            () = &mut cancel => anyhow::bail!("host request channel receive aborted"),
            messages = self.messages.lock() => messages,
        };
        tokio::select! {
            biased;
            () = self.signal.cancelled() => anyhow::bail!("host request channel closed"),
            () = &mut cancel => anyhow::bail!("host request channel receive aborted"),
            message = messages.recv() => message.ok_or_else(|| anyhow::anyhow!("host request channel closed")),
        }
    }

    pub fn interrupt_after_grace(&self, grace_ms: Option<u64>) {
        (self.interrupt)(grace_ms.unwrap_or(100));
    }

    pub fn close(&self) {
        self.signal.abort();
    }
}

pub fn duplex_host_handler<F, Fut>(handler: F) -> HostDuplexHandlerFn
where
    F: Fn(HostRequestPayload, Arc<HostRequestChannel>) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<Value>> + Send + 'static,
{
    Arc::new(move |payload, channel| Box::pin(handler(payload, channel)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::time::Duration;

    fn channel() -> (Arc<HostRequestChannel>, mpsc::Sender<Value>) {
        HostRequestChannel::new(
            AbortSignal::new(),
            None,
            None,
            Arc::new(|_| Box::pin(async { Ok(()) })),
            Arc::new(|_| {}),
        )
    }

    #[tokio::test]
    async fn delivers_messages_and_rejects_closed_operations() {
        let (channel, sender) = channel();
        sender.send(json!({ "type": "cell_result" })).await.unwrap();
        assert_eq!(channel.receive(None).await.unwrap()["type"], "cell_result");
        assert!(channel.send(Value::Null).await.is_err());
        channel.close();
        assert!(channel.send(json!({})).await.is_err());
        assert!(channel.receive(None).await.is_err());
    }

    #[tokio::test]
    async fn cancellation_interrupts_a_receiver_waiting_for_another_receiver() {
        let (channel, _sender) = channel();
        let receiver_lock = channel.messages.lock().await;
        let signal = AbortSignal::aborted();
        let error = tokio::time::timeout(Duration::from_secs(1), channel.receive(Some(&signal)))
            .await
            .expect("cancelled receive must not wait for the receiver lock")
            .unwrap_err();
        assert_eq!(error.to_string(), "host request channel receive aborted");
        drop(receiver_lock);
    }

    #[tokio::test]
    async fn closing_interrupts_a_receiver_waiting_for_another_receiver() {
        let (channel, _sender) = channel();
        let receiver_lock = channel.messages.lock().await;
        channel.close();
        assert!(
            tokio::time::timeout(Duration::from_secs(1), channel.receive(None))
                .await
                .expect("closed receive must not wait for the receiver lock")
                .is_err()
        );
        drop(receiver_lock);
    }
}
