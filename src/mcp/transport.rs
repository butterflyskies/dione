//! Transport mode abstraction: how notifications are delivered to the harness.
//!
//! - **ClaudeCode**: Async `notifications/claude/channel` JSON-RPC notifications
//!   pushed to stdout alongside normal tool responses.
//! - **Codex**: Notifications are queued internally and delivered via a
//!   `wait_for_push` tool call (elicitation/long-poll pattern). The harness
//!   calls `wait_for_push` to block until a notification arrives.

use clap::ValueEnum;
use serde_json::Value;
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::AsyncWriteExt,
    sync::{Mutex as AsyncMutex, Notify},
    time::Instant,
};

/// Transport mode selector — determines how channel notifications reach the harness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Default)]
pub enum TransportMode {
    /// Claude Code: async notifications via `notifications/claude/channel` on stdout.
    #[default]
    ClaudeCode,
    /// Codex: notifications queued, served via `wait_for_push` tool.
    Codex,
}

/// A bounded, non-blocking queue for Codex push notifications.
///
/// Insertion never waits for a consumer. When the queue is full, the oldest
/// notification is discarded before the new notification is appended. Clones
/// refer to the same queue.
#[derive(Clone)]
pub struct NotificationQueue {
    inner: Arc<NotificationQueueInner>,
}

struct NotificationQueueInner {
    capacity: usize,
    state: Mutex<NotificationQueueState>,
    available: Notify,
}

struct NotificationQueueState {
    notifications: VecDeque<Value>,
    closed: bool,
    overflow_warning_emitted: bool,
}

pub(crate) enum QueueDrain {
    Notifications(Vec<Value>),
    TimedOut,
    Closed,
}

impl NotificationQueue {
    /// Creates a queue that retains at most `capacity` notifications.
    ///
    /// # Panics
    ///
    /// Panics when `capacity` is zero.
    pub fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "notification queue capacity must be nonzero");
        Self {
            inner: Arc::new(NotificationQueueInner {
                capacity,
                state: Mutex::new(NotificationQueueState {
                    notifications: VecDeque::with_capacity(capacity),
                    closed: false,
                    overflow_warning_emitted: false,
                }),
                available: Notify::new(),
            }),
        }
    }

    /// Adds a notification without waiting for consumer drainage.
    ///
    /// Returns `false` when the queue has been closed and no notification was
    /// added. On overflow, the oldest queued notification is discarded.
    pub fn push(&self, notification: Value) -> bool {
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.closed {
            return false;
        }

        let warn_on_overflow = if state.notifications.len() == self.inner.capacity {
            state.notifications.pop_front();
            let should_warn = !state.overflow_warning_emitted;
            state.overflow_warning_emitted = true;
            should_warn
        } else {
            false
        };
        state.notifications.push_back(notification);
        drop(state);
        if warn_on_overflow {
            tracing::warn!(
                capacity = self.inner.capacity,
                dropped_notifications = 1_u64,
                further_warnings_suppressed_until_drain = true,
                "notification queue full; dropped oldest notification"
            );
        }
        self.inner.available.notify_one();
        true
    }

    /// Closes the queue and wakes pending consumers.
    ///
    /// Already queued notifications remain available; subsequent insertions
    /// are rejected.
    pub fn close(&self) {
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.closed = true;
        drop(state);
        self.inner.available.notify_waiters();
    }

    pub(crate) async fn wait_and_drain(&self, timeout: Duration) -> QueueDrain {
        let deadline = Instant::now() + timeout;

        loop {
            // Register before inspecting state so a producer cannot enqueue
            // between the empty check and the waiter becoming visible.
            let notified = self.inner.available.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();

            if let Some(result) = self.drain_ready() {
                return result;
            }

            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                // Check once more: an enqueue at the deadline wins over the
                // timeout and must not be stranded until the next poll.
                return self.drain_ready().unwrap_or(QueueDrain::TimedOut);
            }
        }
    }

    fn drain_ready(&self) -> Option<QueueDrain> {
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !state.notifications.is_empty() {
            let notifications = std::mem::take(&mut state.notifications);
            state.overflow_warning_emitted = false;
            return Some(QueueDrain::Notifications(
                notifications.into_iter().collect(),
            ));
        }
        state.closed.then_some(QueueDrain::Closed)
    }
}

/// Where coalesced notification values are delivered.
///
/// In Claude Code mode, notifications are written directly to stdout as
/// JSON-RPC notification lines. In Codex mode, they are inserted into a
/// bounded queue that `wait_for_push` drains.
pub(crate) enum NotificationSink {
    /// Write JSON-RPC notification lines to stdout.
    Stdout(Arc<AsyncMutex<tokio::io::Stdout>>),
    /// Queue notifications for `wait_for_push` consumption.
    Queue(NotificationQueue),
}

impl NotificationSink {
    /// Deliver a notification value to the appropriate sink.
    pub(crate) async fn deliver(&self, value: &Value) {
        match self {
            NotificationSink::Stdout(stdout) => {
                let mut line = serde_json::to_string(value).unwrap_or_else(|_| "{}".to_string());
                line.push('\n');
                let mut out = stdout.lock().await;
                if let Err(e) = out.write_all(line.as_bytes()).await {
                    tracing::warn!(error = %e, "failed to write notification to stdout");
                }
                if let Err(e) = out.flush().await {
                    tracing::warn!(error = %e, "failed to flush stdout");
                }
            }
            NotificationSink::Queue(queue) => {
                if !queue.push(value.clone()) {
                    tracing::warn!("notification queue closed; discarded notification");
                }
            }
        }
    }

    /// Close transports with an explicit consumer lifecycle.
    pub(crate) fn close(&self) {
        if let NotificationSink::Queue(queue) = self {
            queue.close();
        }
    }
}
