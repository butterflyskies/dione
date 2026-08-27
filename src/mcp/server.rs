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
    bell_rings::{BellEvaluator, render_bells},
    coalesce::{CoalesceResult, coalesce},
    codex::{CodexEventQueue, TransportMode},
    config::BellMode,
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
            no_rly::NoRlyCtx,
        },
    },
    no_rly::consent::ConsentGate,
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
    sync::{Mutex, mpsc, watch},
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
    pub presence: Option<crate::discord::events::SharedPresence>,
    pub trace_controller: TraceLevelController,
    pub mode: TransportMode,
    pub codex_queue: Option<CodexEventQueue>,
    pub codex_thread_binding: Option<watch::Sender<Option<crate::codex::CodexThreadId>>>,
    pub no_rly: Arc<ConsentGate>,
    pub event_tx: Option<mpsc::Sender<NotificationEvent>>,
    pub ingress_ledger: Arc<crate::ingress_ledger::IngressLedger>,
}

// ── Construction ──────────────────────────────────────────────────────────────

impl DioneServer {
    /// Core server with no gateway or codex wiring. Transports attach their
    /// optional channels via the `with_*` builders, so adding optional
    /// wiring never breaks existing constructions.
    #[expect(
        clippy::too_many_arguments,
        reason = "these are the always-required core dependencies; optional wiring goes through the with_* builders instead of widening this list"
    )]
    pub fn new(
        state: crate::state::State,
        queue: Arc<Mutex<crate::queue::AccessQueue>>,
        http: Arc<serenity::http::Http>,
        state_dir: Utf8PathBuf,
        notification_tx: mpsc::Sender<Value>,
        trace_controller: TraceLevelController,
        mode: TransportMode,
        no_rly: Arc<ConsentGate>,
        ingress_ledger: Arc<crate::ingress_ledger::IngressLedger>,
    ) -> Self {
        Self {
            state,
            queue,
            http,
            state_dir,
            notification_tx,
            discord_cmd_tx: None,
            presence: None,
            trace_controller,
            mode,
            codex_queue: None,
            codex_thread_binding: None,
            no_rly,
            event_tx: None,
            ingress_ledger,
        }
    }

    pub fn with_discord_cmd_tx(mut self, tx: Option<mpsc::Sender<DiscordCommand>>) -> Self {
        self.discord_cmd_tx = tx;
        self
    }

    pub fn with_presence(
        mut self,
        presence: Option<crate::discord::events::SharedPresence>,
    ) -> Self {
        self.presence = presence;
        self
    }

    pub fn with_codex_queue(mut self, codex_queue: Option<CodexEventQueue>) -> Self {
        self.codex_queue = codex_queue;
        self
    }

    pub fn with_codex_thread_binding(
        mut self,
        binding: Option<watch::Sender<Option<crate::codex::CodexThreadId>>>,
    ) -> Self {
        self.codex_thread_binding = binding;
        self
    }

    pub fn with_event_tx(mut self, event_tx: Option<mpsc::Sender<NotificationEvent>>) -> Self {
        self.event_tx = event_tx;
        self
    }
}

// ── Context factory methods ───────────────────────────────────────────────────

impl DioneServer {
    pub(crate) fn messaging_ctx(&self, config: Arc<crate::config::LoadedConfig>) -> MessagingCtx {
        let mut ctx = MessagingCtx::new(
            self.http.clone(),
            self.state.clone(),
            config,
            self.state_dir.clone(),
            self.no_rly.clone(),
            self.ingress_ledger.clone(),
        );
        ctx.event_tx = self.event_tx.clone();
        ctx
    }

    pub(crate) fn no_rly_ctx(&self, config: Arc<crate::config::LoadedConfig>) -> NoRlyCtx {
        NoRlyCtx {
            gate: self.no_rly.clone(),
            config,
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
            ingress_ledger: self.ingress_ledger.clone(),
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
        BotStateCtx::new(self.http.clone(), self.state.clone(), config)
            .with_discord_cmd_tx(self.discord_cmd_tx.clone())
            .with_presence(self.presence.clone())
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
    let bell_evaluator = Arc::new(BellEvaluator::new());

    // Lazy-init: providers connect on first message, no pre-warming.

    // Resolve timezone once at startup so `deliver_flushed` doesn't need to
    // load config just for the tz. Updated opportunistically when we already
    // load config per-event for the rate limiter.
    let initial_tz = config.tz;
    let initial_evidence_markers_enabled = config.delivery.evidence_markers_enabled;

    let state_dir_notif = server.state_dir.clone();

    // Expiry sweeper for the no_rly hold queue. Expiry is enforced lazily at
    // claim time too — this task just makes sure abandoned handles get their
    // journal record promptly instead of at the next claim. On shutdown it
    // drains everything still pending as expired, so the in-memory queue
    // never swallows a bounce without an audit trail.
    let no_rly_sweeper = server.no_rly.clone();
    let cancel_sweep = cancel.clone();
    let sweep_task = tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(15));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                biased;
                _ = cancel_sweep.cancelled() => break,
                _ = tick.tick() => {
                    let expired = no_rly_sweeper.expire_due(Instant::now()).await;
                    if expired > 0 {
                        tracing::info!(expired, "no_rly: expired abandoned handles");
                    }
                }
            }
        }
        let drained = no_rly_sweeper.drain_shutdown().await;
        if drained > 0 {
            tracing::info!(drained, "no_rly: drained pending handles at shutdown");
        }
    });

    // Notification forwarding task.
    // Exits on cancellation or when the event channel closes.
    let notification_sink =
        NotificationSink::new(server.mode, stdout.clone(), server.codex_queue.clone())
            .map_err(std::io::Error::other)?;
    let cancel_notif = cancel.clone();
    let notif_task = tokio::spawn(async move {
        let mut rx = event_rx;
        let mut events_since_prune: u64 = 0;
        let mut tz = initial_tz;
        let mut evidence_markers_enabled = initial_evidence_markers_enabled;
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
                    let outcome = deliver_flushed(
                        &notification_sink,
                        flushed,
                        tz,
                        evidence_markers_enabled,
                    ).await;
                    // Requeue undelivered events so flush_all picks them up.
                    delivery_buffer.requeue(outcome.undelivered);
                    if let Some(error) = outcome.error {
                        tracing::error!(error = %error, "inbound delivery failed; shutting down");
                        cancel_notif.cancel();
                        break;
                    }
                }

                // New event arrives from Discord.
                event = rx.recv() => {
                    let Some(mut event) = event else { break };

                    // Reload config from ArcSwap (cheap Arc pointer load).
                    let cfg = crate::config::load_config(&state_dir_notif);

                    // Keep tz in sync with config changes so flushes use
                    // the current value without a separate config load.
                    tz = cfg.tz;
                    evidence_markers_enabled = cfg.delivery.evidence_markers_enabled;

                    // Live-reload rate limiter config before the check so
                    // changes apply to the current event, not the next one.
                    let new_rl_config = cfg.rate_limit_runtime();
                    if new_rl_config != rate_limiter.config_ref() {
                        tracing::info!("rate limiter config changed, applying");
                        rate_limiter.update_config(new_rl_config.clone());
                    }


                    // MessageEvent.user_id is already the admitted effective
                    // participant, independent of transport.
                    if let Some(key) = message_rate_limit_key(&event) {
                        let now = Instant::now();
                        match rate_limiter.check_message(&key.participant, &key.channel, &[], now) {
                            RateLimitDecision::Allowed { remaining, .. } => {
                                tracing::trace!(
                                    user_id = key.user_id,
                                    chat_id = key.chat_id,
                                    remaining,
                                    "rate limiter: message allowed"
                                );
                            }
                            RateLimitDecision::Denied { retry_after, overflow: _ } => {
                                // All denied messages are dropped for now.
                                // OverflowPolicy::Buffer is accepted by config but not
                                // yet implemented — see #79 for sender class wiring.
                                tracing::info!(
                                    user_id = key.user_id,
                                    chat_id = key.chat_id,
                                    retry_after_ms = retry_after.as_millis() as u64,
                                    "rate limiter: message denied, dropping"
                                );
                                continue;
                            }
                        }
                    }

                    // Bell evaluation: in shadow mode, fire-and-forget off the
                    // critical path. In live mode, await inline and inject
                    // bells into the event before delivery.
                    match cfg.bell_rings.mode {
                        BellMode::Shadow => {
                            let evaluator = Arc::clone(&bell_evaluator);
                            let shadow_event = event.clone();
                            let shadow_config = cfg.bell_rings.clone();
                            tokio::spawn(async move {
                                let _ = evaluator.evaluate(shadow_event, &shadow_config).await;
                            });
                        }
                        BellMode::Live => {
                            let (returned_event, bells, status) = bell_evaluator.evaluate(event, &cfg.bell_rings).await;
                            event = returned_event;
                            if let NotificationEvent::Message(ref mut msg) = event {
                                if let Some(status) = status {
                                    msg.bells_status = Some(status);
                                }
                                if !bells.is_empty() {
                                    msg.bells = Some(render_bells(&bells));
                                }
                            }
                        }
                    }

                    // Delivery buffer: coalesce channel events per channel.
                    let delay_ms = extract_delay_ms(&event, &cfg);

                    match delivery_buffer.buffer_event_with_evidence(
                        event,
                        delay_ms,
                        evidence_markers_enabled,
                    ) {
                        BufferResult::Immediate(event) => {
                            let notification = (*event)
                                .into_notification_with_evidence(evidence_markers_enabled);
                            if let Err(error) = notification_sink.deliver(&notification).await {
                                tracing::error!(error = %error, "inbound delivery failed; shutting down");
                                cancel_notif.cancel();
                                break;
                            }
                        }
                        BufferResult::FlushThenImmediate { preceding, event } => {
                            let outcome = deliver_flushed(
                                &notification_sink,
                                preceding,
                                tz,
                                evidence_markers_enabled,
                            ).await;
                            delivery_buffer.requeue(outcome.undelivered);
                            if let Some(error) = outcome.error {
                                // Preserve the trigger event — it was never attempted.
                                delivery_buffer.requeue(vec![*event]);
                                tracing::error!(error = %error, "inbound FIFO flush failed; shutting down");
                                cancel_notif.cancel();
                                break;
                            }
                            let notification = (*event)
                                .into_notification_with_evidence(evidence_markers_enabled);
                            if let Err(error) = notification_sink.deliver(&notification).await {
                                tracing::error!(error = %error, "inbound delivery failed; shutting down");
                                cancel_notif.cancel();
                                break;
                            }
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
        let outcome =
            deliver_flushed(&notification_sink, remaining, tz, evidence_markers_enabled).await;
        if !outcome.undelivered.is_empty() {
            tracing::error!(
                dropped = outcome.undelivered.len(),
                "final flush: {} evidence events could not be delivered",
                outcome.undelivered.len()
            );
        }
        if let Some(error) = outcome.error {
            tracing::error!(error = %error, "failed to persist final inbound events");
            cancel_notif.cancel();
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
                        // EOF — the MCP client closed the pipe. This is the
                        // normal way the client dies, and it must trigger the
                        // same graceful shutdown as Ctrl-C: fire the cancel so
                        // the sweeper drains pending no_rly handles into the
                        // journal instead of parking until the timeout.
                        tracing::info!("stdin EOF, MCP server shutting down");
                        cancel.cancel();
                        break;
                    }
                    Err(e) => {
                        // A read error is also a terminal exit — cancel so the
                        // background tasks run their drain paths.
                        tracing::warn!(error = %e, "stdin read error, shutting down");
                        cancel.cancel();
                        break;
                    }
                }
            }
        }
    }

    // Every exit from the loop above has fired `cancel` (the cancellation
    // branch by definition, the EOF/read-error branches explicitly). So
    // notif_task will break out of its loop and flush_all() any buffered
    // events, and the sweeper will drain pending no_rly handles into the
    // journal. Give them a short window.
    drop(server);
    let _ = tokio::time::timeout(Duration::from_millis(500), notif_task).await;
    let _ = tokio::time::timeout(Duration::from_millis(500), sweep_task).await;

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
        "initialize" => Ok(initialize_response(
            server.mode,
            params.get("protocolVersion").and_then(Value::as_str),
        )),
        "notifications/initialized" => Ok(json!({})),

        // ── Tool discovery ────────────────────────────────────────────────────
        "tools/list" => {
            let config = crate::config::load_config(&server.state_dir);
            Ok(tools_list(
                server.mode,
                config.delivery.evidence_markers_enabled,
            ))
        }

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
    Codex(CodexEventQueue),
}

impl NotificationSink {
    fn new(
        mode: TransportMode,
        stdout: Arc<Mutex<tokio::io::Stdout>>,
        codex_queue: Option<CodexEventQueue>,
    ) -> Result<Self, &'static str> {
        match mode {
            // Claude Code must always retain the original MCP stdout path,
            // even if a stray Codex sender is present in the server fixture.
            TransportMode::ClaudeCode => Ok(Self::ClaudeCode(stdout)),
            TransportMode::Codex => codex_queue
                .map(Self::Codex)
                .ok_or("Codex mode requires a durable event queue"),
        }
    }

    async fn deliver(&self, value: &Value) -> Result<(), String> {
        match self {
            Self::ClaudeCode(stdout) => {
                write_line(stdout, value).await;
                Ok(())
            }
            Self::Codex(queue) => queue
                .enqueue(value.clone())
                .await
                .map(|_| ())
                .map_err(|error| error.to_string()),
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
/// Outcome of attempting to deliver a batch of flushed events.
struct DeliverOutcome {
    /// First error encountered, if any.
    error: Option<String>,
    /// Evidence events that could not be delivered due to a mid-FIFO sink
    /// error. The caller should re-buffer these to prevent silent drops.
    undelivered: Vec<NotificationEvent>,
}

impl DeliverOutcome {
    fn ok() -> Self {
        Self {
            error: None,
            undelivered: Vec::new(),
        }
    }

    #[cfg(test)]
    fn into_result(self) -> Result<(), String> {
        match self.error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

async fn deliver_flushed(
    sink: &NotificationSink,
    events: Vec<NotificationEvent>,
    tz: Option<chrono_tz::Tz>,
    evidence_markers_enabled: bool,
) -> DeliverOutcome {
    if events.is_empty() {
        return DeliverOutcome::ok();
    }

    let event_count = events.len();

    // The legacy coalesced format is intentionally compact and cannot carry
    // author-bound structured evidence per event. If any delayed event bears
    // evidence, preserve the entire FIFO as individual notifications at the
    // normal flush deadline rather than silently erasing provenance.
    if evidence_markers_enabled && events.iter().any(NotificationEvent::has_offered_evidence) {
        tracing::debug!(
            event_count,
            "delivering evidence-bearing flush as individual FIFO notifications"
        );
        let mut events = events;
        let mut index = 0;
        while index < events.len() {
            // Clone to build the notification — the original stays in the
            // Vec so it can be returned on failure.
            let notification = events[index].clone().into_notification_with_evidence(true);
            match sink.deliver(&notification).await {
                Ok(()) => index += 1,
                Err(error) => {
                    // Preserve the failed event AND the unattempted tail.
                    let undelivered = events.split_off(index);
                    tracing::warn!(
                        remaining = undelivered.len(),
                        "mid-FIFO sink error; preserving {} undelivered evidence events for requeue",
                        undelivered.len()
                    );
                    return DeliverOutcome {
                        error: Some(error),
                        undelivered,
                    };
                }
            }
        }
        return DeliverOutcome::ok();
    }

    match coalesce(events, tz) {
        Some(CoalesceResult::Single(event)) => {
            let notification = event.into_notification_with_evidence(evidence_markers_enabled);
            if let Err(error) = sink.deliver(&notification).await {
                return DeliverOutcome {
                    error: Some(error),
                    undelivered: Vec::new(),
                };
            }
        }
        Some(CoalesceResult::Coalesced(notification)) => {
            tracing::debug!(
                event_count,
                "coalesced {event_count} events into single delivery"
            );
            if let Err(error) = sink.deliver(&notification).await {
                return DeliverOutcome {
                    error: Some(error),
                    undelivered: Vec::new(),
                };
            }
        }
        None => {
            // Empty — nothing to deliver.
        }
    }
    DeliverOutcome::ok()
}

struct MessageRateLimitKey {
    participant: ParticipantId,
    channel: ChannelRef,
    user_id: u64,
    chat_id: u64,
}

fn message_rate_limit_key(event: &NotificationEvent) -> Option<MessageRateLimitKey> {
    let NotificationEvent::Message(MessageEvent {
        user_id, chat_id, ..
    }) = event
    else {
        return None;
    };
    let user_id = user_id.get();
    let chat_id = chat_id.get();
    Some(MessageRateLimitKey {
        participant: ParticipantId::new(user_id.to_string()),
        channel: ChannelRef::new(chat_id.to_string()),
        user_id,
        chat_id,
    })
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

    /// Exposes `IntoNotification::into_notification` for unit testing notification format.
    pub fn make_notification(event: NotificationEvent) -> Value {
        use crate::mcp::notifications::IntoNotification;
        event.into_notification()
    }

    /// Exposes `tools_list` for unit testing tool discovery.
    pub fn get_tools_list() -> Value {
        crate::mcp::protocol::tools_list(TransportMode::ClaudeCode, false)
    }

    pub fn get_tools_list_with_evidence() -> Value {
        crate::mcp::protocol::tools_list(TransportMode::ClaudeCode, true)
    }

    pub fn get_codex_tools_list() -> Value {
        crate::mcp::protocol::tools_list(TransportMode::Codex, false)
    }

    /// Exposes `initialize_response` for unit testing the handshake.
    pub fn get_initialize_response() -> Value {
        crate::mcp::protocol::initialize_response(TransportMode::ClaudeCode, None)
    }

    /// Exposes the Codex initialize response for protocol tests.
    pub fn get_codex_initialize_response() -> Value {
        crate::mcp::protocol::initialize_response(TransportMode::Codex, None)
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
        rate_limiter::{OverflowPolicy, RateLimitConfig, ScopeConfig},
        timestamp::Timestamp,
    };
    use serenity::model::id::{ChannelId, MessageId, UserId};
    use std::collections::HashMap;

    #[tokio::test]
    async fn messaging_context_uses_process_installed_pipeline() {
        let pipeline = crate::pre_send::configured_pipeline(true, Vec::new())
            .expect("configured pipeline")
            .expect("enabled pipeline");
        crate::pre_send::install_pipeline(Some(pipeline));
        let dir = tempfile::TempDir::new().expect("temp dir");
        let state_dir =
            camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf-8 path");
        let (notification_tx, _notification_rx) = mpsc::channel(1);
        let server = DioneServer::new(
            crate::state::new_state(),
            Arc::new(Mutex::new(crate::queue::AccessQueue::load(&state_dir))),
            Arc::new(serenity::http::Http::new("fake")),
            state_dir.clone(),
            notification_tx,
            TraceLevelController::noop(),
            TransportMode::ClaudeCode,
            Arc::new(crate::no_rly::consent::ConsentGate::new(&state_dir)),
            Arc::new(crate::ingress_ledger::IngressLedger::new()),
        );

        let context = server.messaging_ctx(Arc::new(LoadedConfig::from_raw(Config::default())));

        assert!(context.has_pre_send_pipeline());
        crate::pre_send::install_pipeline(None);
    }

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
        let sink = NotificationSink::new(
            TransportMode::ClaudeCode,
            Arc::new(Mutex::new(tokio::io::stdout())),
            None,
        )
        .unwrap();
        assert!(matches!(sink, NotificationSink::ClaudeCode(_)));
    }

    #[test]
    fn codex_mode_requires_durable_event_queue() {
        let result = NotificationSink::new(
            TransportMode::Codex,
            Arc::new(Mutex::new(tokio::io::stdout())),
            None,
        );
        assert!(result.is_err());
    }

    #[test]
    fn codex_mode_selects_durable_event_queue() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        let queue = crate::codex::CodexEventQueue::load(&path).unwrap();
        let sink = NotificationSink::new(
            TransportMode::Codex,
            Arc::new(Mutex::new(tokio::io::stdout())),
            Some(queue),
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
            targeting: crate::discord::events::MessageTargeting::Ambient,
            timestamp: Timestamp::parse("2026-01-01T00:00:00Z").unwrap(),
            attachments: vec![],
            is_voice_message: false,
            thread_parent_id: None,
            reply_to_message_id: None,
            reply_to_user_id: None,
            reply_to_user: None,
            reply_to_content_preview: None,
            bells: None,
            bells_status: None,
        })
    }

    fn evidence_message_event(index: u64) -> NotificationEvent {
        NotificationEvent::Message(MessageEvent {
            chat_id: ChannelId::new(42),
            message_id: MessageId::new(100 + index),
            user: format!("user-{index}"),
            user_id: UserId::new(200 + index),
            content: format!("claim-{index} [🔍=v1:AAAAAAAAAAw] [🔍=v1:AAAAAAAAACI]"),
            targeting: crate::discord::events::MessageTargeting::Ambient,
            timestamp: Timestamp::parse("2026-01-01T00:00:00Z").unwrap(),
            attachments: vec![],
            is_voice_message: false,
            thread_parent_id: None,
            reply_to_message_id: None,
            reply_to_user_id: None,
            reply_to_user: None,
            reply_to_content_preview: None,
            bells: None,
            bells_status: None,
        })
    }

    #[tokio::test(start_paused = true)]
    async fn evidence_burst_overflow_reaches_real_sink_losslessly_in_fifo_order() {
        let dir = tempfile::TempDir::new().unwrap();
        let state_dir = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        let queue = CodexEventQueue::load(&state_dir).unwrap();
        let consumer = queue
            .register_consumer(
                "evidence overflow test".to_owned(),
                Duration::from_secs(60),
                true,
                true,
            )
            .await
            .unwrap()
            .consumer_id;
        let sink = NotificationSink::Codex(queue.clone());
        let mut buffer = DeliveryBuffer::new();

        for index in 0..6 {
            match buffer.buffer_event(evidence_message_event(index), 500) {
                BufferResult::Immediate(event) => {
                    sink.deliver(&event.into_notification()).await.unwrap();
                }
                BufferResult::Buffered => {
                    assert!(index >= 4, "only budget overflow may be delayed");
                }
                BufferResult::FlushThenImmediate { .. } => {
                    panic!("one uninterrupted burst has no buffered predecessor recovery")
                }
            }
        }

        tokio::time::advance(Duration::from_millis(500)).await;
        let overflow = buffer.flush_ready(tokio::time::Instant::now());
        assert_eq!(overflow.len(), 2);
        let outcome = deliver_flushed(&sink, overflow, None, true).await;
        assert!(outcome.undelivered.is_empty());
        outcome.into_result().unwrap();

        for index in 0..6 {
            let leased = queue
                .next_event(&consumer, Duration::ZERO, Duration::from_secs(60))
                .await
                .unwrap()
                .expect("all six events must reach the final sink");
            assert_eq!(
                leased.event["params"]["content"],
                format!("claim-{index} [🔍=v1:AAAAAAAAAAw] [🔍=v1:AAAAAAAAACI]")
            );
            let evidence = leased.event["params"]["meta"]["evidence"]
                .as_array()
                .expect("structured evidence must survive final delivery");
            assert_eq!(evidence.len(), 2);
            assert_eq!(evidence[0]["locator"], "v1:AAAAAAAAAAw");
            assert_eq!(evidence[1]["locator"], "v1:AAAAAAAAACI");
            assert_eq!(evidence[0]["author_id"], (200 + index).to_string());
            assert_eq!(evidence[1]["author_id"], (200 + index).to_string());
            queue
                .acknowledge(&consumer, &leased.delivery_token)
                .await
                .unwrap();
        }
        assert_eq!(queue.status().await.queued, 0);
    }

    #[tokio::test]
    async fn disabled_evidence_delivery_preserves_text_without_projection() {
        let dir = tempfile::TempDir::new().unwrap();
        let state_dir = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        let queue = CodexEventQueue::load(&state_dir).unwrap();
        let consumer = queue
            .register_consumer(
                "disabled evidence test".to_owned(),
                Duration::from_secs(60),
                true,
                true,
            )
            .await
            .unwrap()
            .consumer_id;
        let sink = NotificationSink::Codex(queue.clone());
        let event = evidence_message_event(0);
        let expected_content = match &event {
            NotificationEvent::Message(message) => message.content.clone(),
            _ => unreachable!(),
        };

        deliver_flushed(&sink, vec![event], None, false)
            .await
            .into_result()
            .unwrap();
        let leased = queue
            .next_event(&consumer, Duration::ZERO, Duration::from_secs(60))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(leased.event["params"]["content"], expected_content);
        assert!(leased.event["params"]["meta"].get("evidence").is_none());
    }

    #[test]
    fn extract_delay_message_uses_channel_config() {
        let config = config_with_channel_delay(42, 500);
        assert_eq!(extract_delay_ms(&message_event(42), &config), 500);
    }

    #[test]
    fn direct_and_represented_same_human_share_the_actual_rate_limit_bucket() {
        let mut limiter = RateLimiter::new(RateLimitConfig {
            enabled: true,
            default: ScopeConfig {
                max_tokens: 1,
                window: Duration::from_secs(3600),
                cooldown: Duration::from_secs(3600),
                overflow: OverflowPolicy::Drop { notify: true },
            },
            classes: Vec::new(),
            individuals: HashMap::new(),
            channels: HashMap::new(),
        });
        let direct = message_event(42);
        let mut represented = message_event(42);
        let NotificationEvent::Message(represented_message) = &mut represented else {
            unreachable!("message_event always builds a message")
        };
        represented_message.message_id = MessageId::new(2);
        represented_message.user = "visible PK member".into();

        let direct_key = message_rate_limit_key(&direct).expect("direct message key");
        let represented_key =
            message_rate_limit_key(&represented).expect("represented message key");
        let now = Instant::now();

        assert!(matches!(
            limiter.check_message(&direct_key.participant, &direct_key.channel, &[], now),
            RateLimitDecision::Allowed { remaining: 0, .. }
        ));
        assert!(matches!(
            limiter.check_message(
                &represented_key.participant,
                &represented_key.channel,
                &[],
                now
            ),
            RateLimitDecision::Denied { .. }
        ));
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
            self_react: false,
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
            BufferResult::FlushThenImmediate { .. } => {
                panic!("delay=0 cannot require a preceding flush")
            }
            BufferResult::Buffered => panic!("expected Immediate for delay=0"),
        };
        assert_eq!(notification["jsonrpc"], "2.0");
        assert_eq!(notification["method"], "notifications/claude/channel");
        assert_eq!(notification["params"]["meta"]["chat_id"], "42");
    }
}
