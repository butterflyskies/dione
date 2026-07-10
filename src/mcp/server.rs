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

pub use crate::tracing_channel::TraceLevelController;
use crate::{
    coalesce::{CoalesceResult, coalesce},
    codex::{CodexEventSender, TransportMode},
    delivery_buffer::{BufferResult, DeliveryBuffer},
    discord::events::{MessageEvent, NotificationEvent},
    mcp::{
        dispatch::call_tool,
        notifications::IntoNotification,
        protocol::{initialize_response, tools_list},
        tools::{
            access::AccessCtx,
            bot_state::{BotStateCtx, DiscordCommand},
            diagnostics::DiagnosticsCtx,
            introspection::IntrospectionCtx,
            management::ManagementCtx,
            messaging::MessagingCtx,
        },
    },
    rate_limiter::{ChannelRef, ParticipantId, RateLimitDecision, RateLimiter},
};
use camino::Utf8PathBuf;
use serde_json::{Value, json};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    sync::{Mutex, mpsc},
};
use tokio_util::sync::CancellationToken;

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
    pub mode: TransportMode,
    pub codex_event_tx: Option<CodexEventSender>,
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

    // Both rate limiter and delivery buffer configs are live-reloadable
    // via the ArcSwap config cache. Rate limiter config is refreshed per
    // event; existing bucket state is preserved across config changes.
    let config = crate::config::load_config(&server.state_dir);
    let mut rate_limiter = RateLimiter::new(config.rate_limit_runtime().clone());
    let mut delivery_buffer = DeliveryBuffer::new();

    // Resolve timezone once at startup so `deliver_flushed` doesn't need to
    // load config just for the tz. Updated opportunistically when we already
    // load config per-event for the rate limiter.
    let initial_tz = config.tz;

    let state_dir_notif = server.state_dir.clone();

    // Notification forwarding task.
    // Exits on cancellation or when the event channel closes.
    let notification_sink =
        NotificationSink::new(server.mode, stdout.clone(), server.codex_event_tx.clone())
            .map_err(std::io::Error::other)?;
    let cancel_notif = cancel.clone();
    let notif_task = tokio::spawn(async move {
        let mut rx = event_rx;
        let mut events_since_prune: u64 = 0;
        let mut tz = initial_tz;
        const PRUNE_INTERVAL: u64 = 100;

        loop {
            let flush_deadline = delivery_buffer.next_flush_deadline();

            tokio::select! {
                biased;

                // Cancellation takes priority — break to drain path.
                _ = cancel_notif.cancelled() => {
                    tracing::debug!("notif_task: cancellation received, draining buffer");
                    break;
                }

                // Flush deadline fires — drain and coalesce buffered events.
                _ = async {
                    match flush_deadline {
                        Some(deadline) => tokio::time::sleep_until(deadline).await,
                        None => std::future::pending::<()>().await,
                    }
                } => {
                    let now = tokio::time::Instant::now();
                    let flushed = delivery_buffer.flush_ready(now);
                    deliver_flushed(&notification_sink, flushed, tz).await;
                }

                // New event arrives from Discord.
                event = rx.recv() => {
                    let Some(event) = event else { break };

                    // Reload config from ArcSwap (cheap Arc pointer load).
                    let cfg = crate::config::load_config(&state_dir_notif);

                    // Keep tz in sync with config changes so flushes use
                    // the current value without a separate config load.
                    tz = cfg.tz;

                    // Live-reload rate limiter config before the check so
                    // changes apply to the current event, not the next one.
                    let new_rl_config = cfg.rate_limit_runtime();
                    if new_rl_config != rate_limiter.config_ref() {
                        tracing::info!("rate limiter config changed, applying");
                        rate_limiter.update_config(new_rl_config.clone());
                    }

                    // Rate-limit check for message events.
                    if let NotificationEvent::Message(MessageEvent { ref user_id, ref chat_id, .. }) = event {
                        let user_id_str = user_id.get().to_string();
                        let chat_id_str = chat_id.get().to_string();
                        let sender = ParticipantId::new(&user_id_str);
                        let channel = ChannelRef::new(&chat_id_str);
                        let now = Instant::now();
                        match rate_limiter.check_message(&sender, &channel, &[], now) {
                            RateLimitDecision::Allowed { remaining, .. } => {
                                tracing::trace!(
                                    user_id = user_id.get(),
                                    chat_id = chat_id.get(),
                                    remaining,
                                    "rate limiter: message allowed"
                                );
                            }
                            RateLimitDecision::Denied { retry_after, overflow: _ } => {
                                // All denied messages are dropped for now.
                                // OverflowPolicy::Buffer is accepted by config but not
                                // yet implemented — see #79 for sender class wiring.
                                tracing::info!(
                                    user_id = user_id.get(),
                                    chat_id = chat_id.get(),
                                    retry_after_ms = retry_after.as_millis() as u64,
                                    "rate limiter: message denied, dropping"
                                );
                                continue;
                            }
                        }
                    }

                    // Delivery buffer: coalesce channel events per channel.
                    let delay_ms = extract_delay_ms(&event, &cfg);

                    match delivery_buffer.buffer_event(event, delay_ms) {
                        BufferResult::Immediate(event) => {
                            let notification = (*event).into_notification();
                            notification_sink.deliver(&notification).await;
                        }
                        BufferResult::Buffered => {
                            // Will be flushed when the deadline fires.
                        }
                    }

                    // Periodically prune idle rate limiter buckets to bound memory.
                    events_since_prune += 1;
                    if events_since_prune >= PRUNE_INTERVAL {
                        events_since_prune = 0;
                        rate_limiter.prune_idle(Instant::now());
                    }
                }
            }
        }

        // Channel closed — flush any remaining buffered events.
        let remaining = delivery_buffer.flush_all();
        deliver_flushed(&notification_sink, remaining, tz).await;
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
        "initialize" => Ok(initialize_response(server.mode)),
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
        "notifications/claude/channel/permission_request"
            if server.mode == TransportMode::ClaudeCode =>
        {
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

#[derive(Clone)]
enum NotificationSink {
    ClaudeCode(Arc<Mutex<tokio::io::Stdout>>),
    Codex(CodexEventSender),
}

impl NotificationSink {
    fn new(
        mode: TransportMode,
        stdout: Arc<Mutex<tokio::io::Stdout>>,
        codex_event_tx: Option<CodexEventSender>,
    ) -> Result<Self, &'static str> {
        match mode {
            // Claude Code must always retain the original MCP stdout path,
            // even if a stray Codex sender is present in the server fixture.
            TransportMode::ClaudeCode => Ok(Self::ClaudeCode(stdout)),
            TransportMode::Codex => codex_event_tx
                .map(Self::Codex)
                .ok_or("Codex mode requires a Codex delivery worker"),
        }
    }

    async fn deliver(&self, value: &Value) {
        match self {
            Self::ClaudeCode(stdout) => write_line(stdout, value).await,
            Self::Codex(tx) => {
                if let Err(error) = tx.persist(value.clone()).await {
                    tracing::warn!(error = %error, "failed to persist Codex event");
                }
            }
        }
    }
}

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

/// Deliver flushed events, coalescing multiple events into a single envelope.
///
/// Single events pass through as individual notifications. Multiple events
/// are coalesced into a single batched notification so the LLM receives one
/// prompt injection per batch window instead of N.
async fn deliver_flushed(
    sink: &NotificationSink,
    events: Vec<NotificationEvent>,
    tz: Option<chrono_tz::Tz>,
) {
    if events.is_empty() {
        return;
    }

    let event_count = events.len();

    match coalesce(events, tz) {
        Some(CoalesceResult::Single(event)) => {
            let notification = event.into_notification();
            sink.deliver(&notification).await;
        }
        Some(CoalesceResult::Coalesced(notification)) => {
            tracing::debug!(
                event_count,
                "coalesced {event_count} events into single delivery"
            );
            sink.deliver(&notification).await;
        }
        None => {
            // Empty — nothing to deliver.
        }
    }
}

/// Extract the delivery delay (ms) for an event based on its channel ID.
///
/// Returns the configured delay for channel events (Message, MessageEdit,
/// MessageDelete, Reaction). Non-channel events always return 0.
fn extract_delay_ms(event: &NotificationEvent, config: &crate::config::LoadedConfig) -> u64 {
    let channel_id = match event {
        NotificationEvent::Message(msg) => Some(msg.chat_id.get()),
        NotificationEvent::MessageEdit { chat_id, .. }
        | NotificationEvent::MessageDelete { chat_id, .. }
        | NotificationEvent::Reaction { chat_id, .. } => Some(chat_id.get()),
        _ => None,
    };
    match channel_id {
        Some(id) => config.delivery_delay_ms(id),
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

    /// Exposes [`IntoNotification::into_notification`] for unit testing notification format.
    pub fn make_notification(event: NotificationEvent) -> Value {
        use crate::mcp::notifications::IntoNotification;
        event.into_notification()
    }

    /// Exposes `tools_list` for unit testing tool discovery.
    pub fn get_tools_list() -> Value {
        crate::mcp::protocol::tools_list()
    }

    /// Exposes `initialize_response` for unit testing the handshake.
    pub fn get_initialize_response() -> Value {
        crate::mcp::protocol::initialize_response(TransportMode::ClaudeCode)
    }

    /// Exposes the Codex initialize response for protocol tests.
    pub fn get_codex_initialize_response() -> Value {
        crate::mcp::protocol::initialize_response(TransportMode::Codex)
    }

    /// Exposes `handle_request` for unit testing request dispatch.
    pub async fn dispatch_request(server: &DioneServer, req: Value) -> Option<Value> {
        handle_request(server, req).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::{ChannelConfig, Config, LoadedConfig},
        delivery_buffer::{BufferResult, DeliveryBuffer},
        mcp::notifications::IntoNotification,
        timestamp::Timestamp,
    };
    use serenity::model::id::{ChannelId, MessageId, UserId};

    fn config_with_channel_delay(channel_id: u64, delay_ms: u64) -> LoadedConfig {
        let mut raw = Config::default();
        raw.channels.push(ChannelConfig {
            id: channel_id.to_string(),
            delivery_delay_ms: Some(delay_ms),
            ..Default::default()
        });
        LoadedConfig::from_raw(raw)
    }

    #[test]
    fn claude_code_mode_always_selects_mcp_stdout() {
        let (codex_tx, _codex_rx) = crate::codex::event_channel(1);
        let sink = NotificationSink::new(
            TransportMode::ClaudeCode,
            Arc::new(Mutex::new(tokio::io::stdout())),
            Some(codex_tx),
        )
        .unwrap();
        assert!(matches!(sink, NotificationSink::ClaudeCode(_)));
    }

    #[test]
    fn codex_mode_requires_durable_delivery_worker() {
        let result = NotificationSink::new(
            TransportMode::Codex,
            Arc::new(Mutex::new(tokio::io::stdout())),
            None,
        );
        assert!(result.is_err());
    }

    #[test]
    fn codex_mode_selects_durable_event_sender() {
        let (codex_tx, _codex_rx) = crate::codex::event_channel(1);
        let sink = NotificationSink::new(
            TransportMode::Codex,
            Arc::new(Mutex::new(tokio::io::stdout())),
            Some(codex_tx),
        )
        .unwrap();
        assert!(matches!(sink, NotificationSink::Codex(_)));
    }

    fn config_with_global_delay(delay_ms: u64) -> LoadedConfig {
        let mut raw = Config::default();
        raw.delivery.delivery_delay_ms = delay_ms;
        LoadedConfig::from_raw(raw)
    }

    fn message_event(channel: u64) -> NotificationEvent {
        NotificationEvent::Message(MessageEvent {
            chat_id: ChannelId::new(channel),
            message_id: MessageId::new(1),
            user: "u".into(),
            user_id: UserId::new(1),
            content: "c".into(),
            timestamp: Timestamp::parse("2026-01-01T00:00:00Z").unwrap(),
            attachments: vec![],
            is_voice_message: false,
            thread_parent_id: None,
            reply_to_message_id: None,
            reply_to_user_id: None,
            reply_to_user: None,
            reply_to_content_preview: None,
        })
    }

    #[test]
    fn extract_delay_message_uses_channel_config() {
        let config = config_with_channel_delay(42, 500);
        assert_eq!(extract_delay_ms(&message_event(42), &config), 500);
    }

    #[test]
    fn extract_delay_reaction_uses_channel_config() {
        let config = config_with_channel_delay(7, 200);
        let event = NotificationEvent::Reaction {
            chat_id: ChannelId::new(7),
            message_id: MessageId::new(1),
            user: "u".into(),
            user_id: UserId::new(1),
            emoji: "👍".into(),
        };
        assert_eq!(extract_delay_ms(&event, &config), 200);
    }

    #[test]
    fn extract_delay_message_edit_uses_channel_config() {
        let config = config_with_channel_delay(99, 300);
        let event = NotificationEvent::MessageEdit {
            chat_id: ChannelId::new(99),
            message_id: MessageId::new(1),
            user: "u".into(),
            user_id: UserId::new(1),
            new_content: "edited".into(),
            timestamp: Timestamp::parse("2026-01-01T00:00:00Z").unwrap(),
            thread_parent_id: None,
            reply_to_message_id: None,
        };
        assert_eq!(extract_delay_ms(&event, &config), 300);
    }

    #[test]
    fn extract_delay_message_delete_uses_channel_config() {
        let config = config_with_channel_delay(55, 100);
        let event = NotificationEvent::MessageDelete {
            chat_id: ChannelId::new(55),
            message_id: MessageId::new(1),
            thread_parent_id: None,
        };
        assert_eq!(extract_delay_ms(&event, &config), 100);
    }

    #[test]
    fn extract_delay_non_channel_event_is_zero() {
        let config = config_with_channel_delay(1, 999);
        let event = NotificationEvent::ConfigError {
            error: "oops".into(),
        };
        assert_eq!(extract_delay_ms(&event, &config), 0);
    }

    #[test]
    fn extract_delay_unconfigured_channel_falls_back_to_global() {
        let config = config_with_global_delay(750);
        assert_eq!(extract_delay_ms(&message_event(9999), &config), 750);
    }

    #[test]
    fn immediate_buffer_result_dereferences_to_notification() {
        // server.rs consumes a Immediate(Box<event>) as (*event).into_notification().
        // Verify the dereference-then-consume pattern produces a well-formed notification
        // with the correct channel ID.
        let mut buf = DeliveryBuffer::new();
        let result = buf.buffer_event(message_event(42), 0);
        let notification = match result {
            BufferResult::Immediate(event) => (*event).into_notification(),
            BufferResult::Buffered => panic!("expected Immediate for delay=0"),
        };
        assert_eq!(notification["jsonrpc"], "2.0");
        assert_eq!(notification["method"], "notifications/claude/channel");
        assert_eq!(notification["params"]["meta"]["chat_id"], "42");
    }
}
