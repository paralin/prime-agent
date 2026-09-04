use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use tokio::sync::Notify;

pub const INPUT_CAPACITY: usize = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputDelivery {
    Queued,
    Woken,
}

struct InputState {
    queue: VecDeque<String>,
    in_flight: usize,
    closed: bool,
    consumer_created: bool,
}

pub struct InputMailbox {
    state: Mutex<InputState>,
    changed: Notify,
}

pub struct InputConsumer {
    mailbox: Arc<InputMailbox>,
}

impl InputMailbox {
    #[must_use]
    pub fn new(initial_prompt: String) -> Self {
        Self {
            state: Mutex::new(InputState {
                queue: VecDeque::from([initial_prompt]),
                in_flight: 0,
                closed: false,
                consumer_created: false,
            }),
            changed: Notify::new(),
        }
    }

    #[must_use]
    pub fn turn_idle(&self) -> bool {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.in_flight == 0 && state.queue.is_empty()
    }

    /// # Errors
    /// Returns an error if the mailbox is closed or its bounded input capacity is full.
    pub fn enqueue(&self, text: String) -> anyhow::Result<InputDelivery> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        anyhow::ensure!(!state.closed, "Claude Code input mailbox is closed");
        anyhow::ensure!(
            state.queue.len() + state.in_flight < INPUT_CAPACITY,
            "Claude Code input mailbox reached its {INPUT_CAPACITY}-message capacity"
        );
        let delivery = if state.queue.is_empty() && state.in_flight == 0 {
            InputDelivery::Woken
        } else {
            InputDelivery::Queued
        };
        state.queue.push_back(text);
        drop(state);
        self.changed.notify_one();
        Ok(delivery)
    }

    pub fn complete_turn(&self) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .in_flight = 0;
    }

    pub fn close(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.closed = true;
        state.queue.clear();
        state.in_flight = 0;
        drop(state);
        self.changed.notify_waiters();
    }

    /// # Errors
    /// Returns an error if the SDK input consumer has already been created.
    pub fn consumer(self: &Arc<Self>) -> anyhow::Result<InputConsumer> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        anyhow::ensure!(
            !state.consumer_created,
            "Claude Code input mailbox supports one SDK consumer"
        );
        state.consumer_created = true;
        Ok(InputConsumer {
            mailbox: self.clone(),
        })
    }
}

impl InputConsumer {
    pub async fn next(&mut self) -> Option<String> {
        loop {
            let changed = self.mailbox.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            {
                let mut state = self
                    .mailbox
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if let Some(text) = state.queue.pop_front() {
                    state.in_flight += 1;
                    return Some(text);
                }
                if state.closed {
                    return None;
                }
            }
            changed.await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn retained_input_wakes_after_completion_and_closes_pending_consumer() {
        let mailbox = Arc::new(InputMailbox::new("initial".into()));
        let mut consumer = mailbox.consumer().unwrap();
        assert!(mailbox.consumer().is_err());
        assert!(!mailbox.turn_idle());
        assert_eq!(consumer.next().await.as_deref(), Some("initial"));
        assert_eq!(
            mailbox.enqueue("queued".into()).unwrap(),
            InputDelivery::Queued
        );
        assert_eq!(consumer.next().await.as_deref(), Some("queued"));
        mailbox.complete_turn();
        assert!(mailbox.turn_idle());
        let pending = tokio::spawn(async move {
            let text = consumer.next().await;
            (text, consumer)
        });
        tokio::task::yield_now().await;
        assert_eq!(
            mailbox.enqueue("follow-up".into()).unwrap(),
            InputDelivery::Woken
        );
        let (text, mut consumer) = pending.await.unwrap();
        assert_eq!(text.as_deref(), Some("follow-up"));
        mailbox.complete_turn();
        let pending = tokio::spawn(async move { consumer.next().await });
        tokio::task::yield_now().await;
        mailbox.close();
        assert!(pending.await.unwrap().is_none());
        assert!(mailbox.enqueue("late".into()).is_err());
        assert!(mailbox.turn_idle());
    }

    #[tokio::test]
    async fn capacity_counts_both_pending_and_in_flight_inputs() {
        let mailbox = Arc::new(InputMailbox::new("initial".into()));
        let mut consumer = mailbox.consumer().unwrap();
        consumer.next().await.unwrap();
        for index in 1..INPUT_CAPACITY {
            mailbox.enqueue(index.to_string()).unwrap();
        }
        assert!(mailbox.enqueue("overflow".into()).is_err());
        consumer.next().await.unwrap();
        assert!(mailbox.enqueue("still full".into()).is_err());
        mailbox.complete_turn();
        mailbox.enqueue("room".into()).unwrap();
        mailbox.close();
        assert!(consumer.next().await.is_none());
    }
}
