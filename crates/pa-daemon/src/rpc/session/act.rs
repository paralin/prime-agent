use std::sync::Arc;

use pa_core::session_engine::act_runtime::projection::ActEventSink;
use serde_json::Value;
use tokio::sync::{mpsc, oneshot, Mutex};

use crate::rpc::LineWriter;

enum Publication {
    Event(Value),
    Barrier(oneshot::Sender<()>),
}

pub(super) struct ActRelay {
    sender: mpsc::UnboundedSender<Publication>,
    task: tokio::task::JoinHandle<()>,
}

impl ActRelay {
    pub(super) fn new(pending: Arc<Mutex<Option<Vec<Value>>>>, writer: LineWriter) -> Arc<Self> {
        let (sender, mut receiver) = mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            while let Some(publication) = receiver.recv().await {
                match publication {
                    Publication::Event(event) => {
                        let mut pending = pending.lock().await;
                        if let Some(buffer) = pending.as_mut() {
                            buffer.push(event);
                        } else {
                            writer.write(event);
                        }
                    }
                    Publication::Barrier(done) => {
                        let _ = done.send(());
                    }
                }
            }
        });
        Arc::new(Self { sender, task })
    }

    pub(super) fn sink(&self) -> ActEventSink {
        let sender = self.sender.clone();
        Arc::new(move |event| {
            let _ = sender.send(Publication::Event(event));
        })
    }

    pub(super) async fn flush(&self) {
        let (done, wait) = oneshot::channel();
        if self.sender.send(Publication::Barrier(done)).is_ok() {
            let _ = wait.await;
        }
    }
}

impl Drop for ActRelay {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::AtomicUsize;

    #[tokio::test]
    async fn act_frames_buffer_in_order_and_retired_sinks_stop_publishing() {
        let (sender, mut frames) = mpsc::unbounded_channel();
        let writer = LineWriter {
            tx: sender,
            pending: Arc::new(AtomicUsize::new(0)),
        };
        let pending = Arc::new(Mutex::new(Some(Vec::new())));
        let relay = ActRelay::new(pending.clone(), writer.clone());
        let sink = relay.sink();
        for (kind, sequence) in [("start", 1), ("assistant_delta", 2), ("terminal", 3)] {
            sink(
                json!({"type":"act_event","actId":"nested","depth":2,"parentActId":"outer",
                "event":kind,"sequence":sequence}),
            );
        }
        relay.flush().await;
        assert!(frames.try_recv().is_err());
        writer.write(json!({"type":"response"}));
        for event in pending.lock().await.take().unwrap() {
            writer.write(event);
        }
        assert_eq!(frames.try_recv().unwrap()["type"], "response");
        for sequence in 1..=3 {
            let event = frames.try_recv().unwrap();
            assert_eq!(event["sequence"], sequence);
            assert_eq!(event["parentActId"], "outer");
        }
        sink(json!({"type":"act_event","event":"start"}));
        relay.flush().await;
        assert_eq!(frames.try_recv().unwrap()["event"], "start");
        drop(relay);
        sink(json!({"type":"act_event","event":"terminal"}));
        tokio::task::yield_now().await;
        assert!(frames.try_recv().is_err());
    }
}
