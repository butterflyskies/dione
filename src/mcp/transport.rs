//! Transport mode abstraction: how notifications are delivered to the harness.
//!
//! - **ClaudeCode**: Async `notifications/claude/channel` JSON-RPC notifications
//!   pushed to stdout alongside normal tool responses.
//! - **Codex**: Notifications are queued internally and delivered via a
//!   `wait_for_push` tool call (elicitation/long-poll pattern). The harness
//!   calls `wait_for_push` to block until a notification arrives.

use clap::ValueEnum;
use serde_json::Value;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio::sync::{Mutex, mpsc};

/// Transport mode selector — determines how channel notifications reach the harness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Default)]
pub enum TransportMode {
    /// Claude Code: async notifications via `notifications/claude/channel` on stdout.
    #[default]
    ClaudeCode,
    /// Codex: notifications queued, served via `wait_for_push` tool.
    Codex,
}

/// Where coalesced notification values are delivered.
///
/// In Claude Code mode, notifications are written directly to stdout as
/// JSON-RPC notification lines. In Codex mode, they are sent to an internal
/// channel that `wait_for_push` drains.
pub(crate) enum NotificationSink {
    /// Write JSON-RPC notification lines to stdout.
    Stdout(Arc<Mutex<tokio::io::Stdout>>),
    /// Queue notifications for `wait_for_push` consumption.
    Queue(mpsc::Sender<Value>),
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
            NotificationSink::Queue(tx) => {
                if let Err(e) = tx.send(value.clone()).await {
                    tracing::warn!(error = %e, "failed to queue notification for wait_for_push");
                }
            }
        }
    }
}
