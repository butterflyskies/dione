//! Manual MCP JSON-RPC server over stdio.
//!
//! We implement the MCP protocol by hand rather than using rmcp's stdio
//! transport. This gives us full control of stdout so we can interleave
//! custom `notifications/channel` JSON-RPC notifications alongside normal
//! tool responses without fighting rmcp's transport ownership.
//!
//! Protocol: line-delimited JSON-RPC 2.0.
//! - Request  → `{"jsonrpc":"2.0","id":N,"method":"...","params":{...}}`
//! - Response → `{"jsonrpc":"2.0","id":N,"result":{...}}`
//! - Error    → `{"jsonrpc":"2.0","id":N,"error":{"code":-32000,"message":"..."}}`
//! - Notification (no id) → `{"jsonrpc":"2.0","method":"...","params":{...}}`

use std::sync::Arc;
use std::time::{Duration, Instant};

use camino::Utf8PathBuf;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{Mutex, mpsc};
use tokio_util::sync::CancellationToken;

use crate::delivery_buffer::{BufferResult, DeliveryBuffer};
use crate::discord::events::NotificationEvent;
use crate::mcp::dispatch::call_tool;
use crate::mcp::notifications::event_to_notification;
use crate::mcp::protocol::{initialize_response, tools_list};
use crate::mcp::tools::{
    access::AccessCtx,
    bot_state::{BotStateCtx, DiscordCommand},
    diagnostics::DiagnosticsCtx,
    introspection::IntrospectionCtx,
    management::ManagementCtx,
    messaging::MessagingCtx,
};
use crate::rate_limiter::{ChannelRef, ParticipantId, RateLimitDecision, RateLimiter};
pub use crate::tracing_channel::TraceLevelController;

// ── Server struct ─────────────────────────────────────────────────────────────

/// Runtime context for the MCP server.
pub struct DioneServer {
    pub state: crate::state::State,
    pub queue: Arc<Mutex<crate::queue::AccessQueue>>,
    pub http: Arc<serenity::http::Http>,
    pub state_dir: Utf8PathBuf,
    pub notification_tx: mpsc::Sender<Value>,
    pub discord_cmd_tx: Option<mpsc::Sender<DiscordCommand>>,
    pub trace_controller: TraceLevelController,
}

// ── Context factory methods ───────────────────────────────────────────────────

impl DioneServer {
    pub(crate) fn messaging_ctx(&self, config: Arc<crate::config::LoadedConfig>) -> MessagingCtx {
        MessagingCtx {
            http: self.http.clone(),
            state: self.state.clone(),
            config,
            state_dir: self.state_dir.clone(),
        }
    }

    pub(crate) fn introspection_ctx(
        &self,
        config: Arc<crate::config::LoadedConfig>,
    ) -> IntrospectionCtx {
        IntrospectionCtx {
            http: self.http.clone(),
            config,
        }
    }

    pub(crate) fn management_ctx(&self, config: Arc<crate::config::LoadedConfig>) -> ManagementCtx {
        ManagementCtx {
            http: self.http.clone(),
            state: self.state.clone(),
            config,
        }
    }

    pub(crate) fn access_ctx(&self, config: Arc<crate::config::LoadedConfig>) -> AccessCtx {
        AccessCtx {
            queue: self.queue.clone(),
            config,
            state_dir: self.state_dir.clone(),
        }
    }

    pub(crate) fn bot_state_ctx(&self, config: Arc<crate::config::LoadedConfig>) -> BotStateCtx {
        BotStateCtx {
            http: self.http.clone(),
            discord_cmd_tx: self.discord_cmd_tx.clone(),
            state: self.state.clone(),
            config,
        }
    }

    pub(crate) fn diagnostics_ctx(&self) -> DiagnosticsCtx<'_> {
        DiagnosticsCtx {
            trace_controller: &self.trace_controller,
        }
    }
}

// ── Entry point ───────────────────────────────────────────────────────────────

/// Runs the MCP server: reads JSON-RPC from stdin, writes to stdout.
///
/// Also spawns a task that converts [`NotificationEvent`]s from Discord into
/// MCP notifications and writes them to stdout.
pub async fn run(
    server: DioneServer,
    event_rx: mpsc::Receiver<NotificationEvent>,
    cancel: CancellationToken,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let stdin = BufReader::new(tokio::io::stdin());
    let stdout = Arc::new(Mutex::new(tokio::io::stdout()));

    // Rate limiter config is snapshot at startup. Changes to [rate_limit]
    // in config.toml require a server restart to take effect. Delivery buffer
    // delays are live-reloadable (read from ArcSwap config cache per event).
    let config = crate::config::load_config(&server.state_dir);
    let rate_limit_config = config.rate_limit.clone().into_runtime();
    let mut rate_limiter = RateLimiter::new(rate_limit_config);
    let mut delivery_buffer = DeliveryBuffer::new();

    let state_dir_notif = server.state_dir.clone();

    // Notification forwarding task.
    // Exits on cancellation or when the event channel closes.
    let stdout_notif = stdout.clone();
    let cancel_notif = cancel.clone();
    let notif_task = tokio::spawn(async move {
        let mut rx = event_rx;

        loop {
            let flush_deadline = delivery_buffer.next_flush_deadline();

            tokio::select! {
                biased;

                // Cancellation takes priority — break to drain path.
                _ = cancel_notif.cancelled() => {
                    tracing::debug!("notif_task: cancellation received, draining buffer");
                    break;
                }

                // Flush deadline fires — drain buffered events.
                _ = async {
                    match flush_deadline {
                        Some(deadline) => tokio::time::sleep_until(deadline).await,
                        None => std::future::pending::<()>().await,
                    }
                } => {
                    let now = tokio::time::Instant::now();
                    let flushed = delivery_buffer.flush_ready(now);
                    for event in flushed {
                        let notification = event_to_notification(event);
                        write_line(&stdout_notif, &notification).await;
                    }
                }

                // New event arrives from Discord.
                event = rx.recv() => {
                    let Some(event) = event else { break };

                    // Rate-limit check for message events.
                    if let NotificationEvent::Message { ref user_id, ref chat_id, .. } = event {
                        let sender = ParticipantId::new(user_id.as_str());
                        let channel = ChannelRef::new(chat_id.as_str());
                        let now = Instant::now();
                        match rate_limiter.check_message(&sender, &channel, &[], now) {
                            RateLimitDecision::Allowed { remaining, .. } => {
                                tracing::trace!(
                                    user_id,
                                    chat_id,
                                    remaining,
                                    "rate limiter: message allowed"
                                );
                            }
                            RateLimitDecision::Denied { retry_after, overflow: _ } => {
                            // All denied messages are dropped for now.
                            // OverflowPolicy::Buffer is accepted by config but not
                            // yet implemented — see #79 for sender class wiring.
                                tracing::info!(
                                    user_id,
                                    chat_id,
                                    retry_after_ms = retry_after.as_millis() as u64,
                                    "rate limiter: message denied, dropping"
                                );
                                continue;
                            }
                        }
                    }

                    // Delivery buffer: coalesce message events per channel.
                    let cfg = crate::config::load_config(&state_dir_notif);
                    let delay_ms = extract_delay_ms(&event, &cfg);

                    match delivery_buffer.buffer_event(event, delay_ms) {
                        BufferResult::Immediate(event) => {
                            let notification = event_to_notification(event);
                            write_line(&stdout_notif, &notification).await;
                        }
                        BufferResult::Buffered => {
                            // Will be flushed when the deadline fires.
                        }
                    }
                }
            }
        }

        // Channel closed — flush any remaining buffered events.
        let remaining = delivery_buffer.flush_all();
        for event in remaining {
            let notification = event_to_notification(event);
            write_line(&stdout_notif, &notification).await;
        }
    });

    // Main request loop.
    let mut lines = stdin.lines();
    loop {
        tokio::select! {
            biased;

            _ = cancel.cancelled() => {
                tracing::info!("MCP server shutting down");
                break;
            }

            line = lines.next_line() => {
                match line {
                    Ok(Some(text)) => {
                        let trimmed = text.trim();
                        if trimmed.is_empty() {
                            continue;
                        }
                        match serde_json::from_str::<Value>(trimmed) {
                            Ok(req) => {
                                let resp = handle_request(&server, req).await;
                                if let Some(resp_value) = resp {
                                    write_line(&stdout, &resp_value).await;
                                }
                            }
                            Err(e) => {
                                tracing::warn!(error = %e, "failed to parse JSON-RPC line");
                                let err_resp = json!({
                                    "jsonrpc": "2.0",
                                    "id": null,
                                    "error": { "code": -32700, "message": "parse error" }
                                });
                                write_line(&stdout, &err_resp).await;
                            }
                        }
                    }
                    Ok(None) => {
                        // EOF — client disconnected.
                        tracing::info!("stdin EOF, MCP server exiting");
                        break;
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "stdin read error");
                        break;
                    }
                }
            }
        }
    }

    // Cancellation signal already sent — notif_task will break out of its
    // loop and flush_all() any buffered events. Give it a short window.
    drop(server);
    let _ = tokio::time::timeout(Duration::from_millis(500), notif_task).await;

    Ok(())
}

// ── Request dispatch ──────────────────────────────────────────────────────────

async fn handle_request(server: &DioneServer, req: Value) -> Option<Value> {
    let id = req.get("id").cloned();
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    let params = req.get("params").cloned().unwrap_or(json!({}));

    // Notifications (no id) from client — process but don't respond.
    let is_notification = id.is_none();

    let result = dispatch(server, method, params).await;

    if is_notification {
        return None;
    }

    let id = id.unwrap();
    Some(match result {
        Ok(v) => json!({ "jsonrpc": "2.0", "id": id, "result": v }),
        Err(msg) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": -32000, "message": msg }
        }),
    })
}

async fn dispatch(server: &DioneServer, method: &str, params: Value) -> Result<Value, String> {
    match method {
        // ── MCP lifecycle ─────────────────────────────────────────────────────
        "initialize" => Ok(initialize_response()),
        "notifications/initialized" => Ok(json!({})),

        // ── Tool discovery ────────────────────────────────────────────────────
        "tools/list" => Ok(tools_list()),

        // ── Tool invocation ───────────────────────────────────────────────────
        "tools/call" => {
            let tool_name = params
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| "missing tool name".to_string())?;
            let arguments = params.get("arguments").cloned().unwrap_or(json!({}));
            call_tool(server, tool_name, arguments).await
        }

        // ── Permission relay (inbound from Claude Code) ──────────────────────
        "notifications/claude/channel/permission_request" => {
            let request_id = params
                .get("request_id")
                .and_then(Value::as_str)
                .unwrap_or("");
            if request_id.is_empty() {
                tracing::warn!("permission_request missing request_id, ignoring");
                return Ok(json!({}));
            }
            let tool_name = params
                .get("tool_name")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let description = params
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("");
            let input_preview = params
                .get("input_preview")
                .and_then(Value::as_str)
                .unwrap_or("");

            let config = crate::config::load_config(&server.state_dir);
            if let Err(e) = crate::permissions::send_permission_request(
                &server.http,
                &config,
                &server.state,
                request_id,
                tool_name,
                description,
                input_preview,
            )
            .await
            {
                tracing::warn!(error = %e, "failed to relay permission request");
            }
            Ok(json!({}))
        }

        other => {
            tracing::debug!(method = other, "unknown MCP method");
            Err(format!("method not found: {other}"))
        }
    }
}

// ── I/O helpers ───────────────────────────────────────────────────────────────

async fn write_line(stdout: &Arc<Mutex<tokio::io::Stdout>>, value: &Value) {
    let mut line = serde_json::to_string(value).unwrap_or_else(|_| "{}".to_string());
    line.push('\n');
    let mut out = stdout.lock().await;
    if let Err(e) = out.write_all(line.as_bytes()).await {
        tracing::warn!(error = %e, "failed to write MCP response to stdout");
    }
    if let Err(e) = out.flush().await {
        tracing::warn!(error = %e, "failed to flush stdout");
    }
}

// ── Notification helpers ─────────────────────────────────────────────────────

/// Extract the delivery delay (ms) for an event based on its channel ID.
fn extract_delay_ms(event: &NotificationEvent, config: &crate::config::LoadedConfig) -> u64 {
    let chat_id = match event {
        NotificationEvent::Message { chat_id, .. }
        | NotificationEvent::MessageEdit { chat_id, .. }
        | NotificationEvent::MessageDelete { chat_id, .. } => Some(chat_id.as_str()),
        _ => None,
    };
    match chat_id.and_then(|id| id.parse::<u64>().ok()) {
        Some(channel_id) => config.delivery_delay_ms(channel_id),
        None => 0,
    }
}

// ── Test helpers ─────────────────────────────────────────────────────────────

/// Test helpers that expose internal functions through the crate's public API.
///
/// Always compiled (zero-cost: only re-exports existing functions for
/// testability).
pub mod test_helpers {
    use super::*;

    /// Exposes `event_to_notification` for unit testing notification format.
    pub fn make_notification(event: NotificationEvent) -> Value {
        crate::mcp::notifications::event_to_notification(event)
    }

    /// Exposes `tools_list` for unit testing tool discovery.
    pub fn get_tools_list() -> Value {
        crate::mcp::protocol::tools_list()
    }

    /// Exposes `initialize_response` for unit testing the handshake.
    pub fn get_initialize_response() -> Value {
        crate::mcp::protocol::initialize_response()
    }

    /// Exposes `handle_request` for unit testing request dispatch.
    pub async fn dispatch_request(server: &DioneServer, req: Value) -> Option<Value> {
        handle_request(server, req).await
    }
}
