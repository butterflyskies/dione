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

#[cfg(test)]
use crate::core::{ModuleSlotId, ProviderId};
pub use crate::tracing_channel::TraceLevelController;
use crate::{
    attention::{
        delivery::AttentionGuardFailure,
        types::{Admission, DeliveryState},
    },
    bell_rings::{BellEvaluator, render_bells},
    config::BellMode,
    core::{
        ModuleInstanceId,
        delivery::{Delivery, DeliveryKind, EvidenceVisibility, PresentationContext},
    },
    delivery_buffer::BufferResult,
    discord::{
        access::AccessCtx,
        events::{MessageEvent, NotificationEvent},
        messaging::MessagingCtx,
        presence::PresenceCapability,
        scheduling::DeliveryBuffer,
        typing::TypingCtx,
    },
    harness::{AttentionPolicy, HarnessConnection, HarnessError, ProtocolOutput},
    mcp::{
        dispatch::call_tool,
        harness_wire::{PRIVATE_CONTROL_TOOLS, RequestOrigin},
        protocol::{initialize_response, tools_list},
        tool_registry::{RegisteredTool, RegistrationError, ToolNameError, ToolRegistry},
        tools::{diagnostics::DiagnosticsCtx, no_rly::NoRlyCtx},
    },
    no_rly::consent::ConsentGate,
    rate_limiter::{ChannelRef, ParticipantId, RateLimitDecision, RateLimiter},
};
use camino::Utf8PathBuf;
use serde_json::{Value, json};
use std::{
    os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt},
    sync::Arc,
    time::{Duration, Instant},
};
use thiserror::Error;
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, BufReader},
    net::{UnixListener, UnixStream},
    sync::{Mutex, Semaphore, mpsc},
};
use tokio_util::{sync::CancellationToken, task::TaskTracker};

/// Failure to construct the server's immutable owner registrations.
#[derive(Debug, Error)]
#[error("{kind}")]
pub struct ServerConstructionError {
    /// The definition or registration failure with its original cause.
    #[source]
    pub kind: ServerConstructionErrorKind,
}

/// The stage that prevented the server from accepting requests.
#[derive(Debug, Error)]
pub enum ServerConstructionErrorKind {
    /// An owner supplied an invalid tool name.
    #[error("invalid module tool definition: {0}")]
    Definition(#[source] ToolNameError),
    /// Two callable tools would share a name.
    #[error("invalid module tool registration: {0}")]
    Registration(#[source] RegistrationError),
}

impl ServerConstructionError {
    fn definition(source: ToolNameError) -> Self {
        Self {
            kind: ServerConstructionErrorKind::Definition(source),
        }
    }

    fn registration(source: RegistrationError) -> Self {
        Self {
            kind: ServerConstructionErrorKind::Registration(source),
        }
    }
}

/// Runtime context for the MCP server.
#[derive(Clone)]
pub struct DioneServer {
    pub queue: Arc<Mutex<crate::queue::AccessQueue>>,
    pub notification_tx: mpsc::Sender<Value>,
    pub presence: PresenceCapability,
    pub trace_controller: TraceLevelController,
    harness: HarnessConnection,
    pub attention: Arc<crate::attention::runtime::AttentionRuntime>,
    pub(super) module_tools: ToolRegistry,
    discord_tools: Vec<RegisteredTool>,
    discord: Arc<crate::discord::reply_tool::ReplyServices>,
}

impl DioneServer {
    /// Borrows the Discord HTTP service shared by registered and legacy tools.
    pub fn http(&self) -> &Arc<serenity::http::Http> {
        self.discord.http()
    }
    /// Borrows the shared Discord state.
    pub fn state(&self) -> &crate::state::State {
        self.discord.state()
    }
    /// Borrows the state directory shared by every tool.
    pub fn state_dir(&self) -> &Utf8PathBuf {
        self.discord.state_dir()
    }
    /// Borrows the consent authority used by reply and release tools.
    pub fn no_rly(&self) -> &Arc<ConsentGate> {
        self.discord.no_rly()
    }
    /// Borrows the ingress authority shared by Discord tools.
    pub fn ingress_ledger(&self) -> &Arc<crate::ingress_ledger::IngressLedger> {
        self.discord.ingress_ledger()
    }
    /// Borrows the configured gateway event sender.
    pub fn event_tx(&self) -> &Option<mpsc::Sender<NotificationEvent>> {
        self.discord.event_tx()
    }
    /// Builds the core server and awaits attention state initialization.
    /// The harness connection supplies delivery, protocol capabilities, and
    /// the attention policy. Construction fixes shared Discord services; builders attach presence and module tools.
    #[expect(
        clippy::too_many_arguments,
        reason = "composition supplies the shared services and event sender together so registered and legacy tools use one authority"
    )]
    pub async fn new(
        state: crate::state::State,
        queue: Arc<Mutex<crate::queue::AccessQueue>>,
        http: Arc<serenity::http::Http>,
        state_dir: Utf8PathBuf,
        notification_tx: mpsc::Sender<Value>,
        trace_controller: TraceLevelController,
        harness: HarnessConnection,
        no_rly: Arc<ConsentGate>,
        ingress_ledger: Arc<crate::ingress_ledger::IngressLedger>,
        authority_module: ModuleInstanceId,
        event_tx: Option<mpsc::Sender<NotificationEvent>>,
    ) -> Result<Self, ServerConstructionError> {
        let attention = Arc::new(
            crate::attention::runtime::AttentionRuntime::new(
                crate::attention::source::SourceResolver {
                    http: http.clone(),
                    state: state.clone(),
                    state_dir: state_dir.clone(),
                    ledger: ingress_ledger.clone(),
                },
                harness.attention_policy() == AttentionPolicy::SafeTurn,
                authority_module,
            )
            .await,
        );
        let discord = Arc::new(crate::discord::reply_tool::ReplyServices::new(
            http,
            state,
            state_dir,
            no_rly,
            ingress_ledger,
            event_tx,
        ));
        let mut discord_tools = crate::discord::introspection_tools::registrations(discord.clone())
            .map_err(ServerConstructionError::definition)?;
        discord_tools.extend(
            crate::discord::query_tools::registrations(
                discord.clone(),
                attention.authority_module().clone(),
            )
            .map_err(ServerConstructionError::definition)?,
        );
        discord_tools.extend(
            crate::discord::management_tools::registrations(discord.clone())
                .map_err(ServerConstructionError::definition)?,
        );
        discord_tools.push(
            crate::discord::reply_tool::registration(discord.clone())
                .map_err(ServerConstructionError::definition)?,
        );
        let module_tools = ToolRegistry::new(&harness, discord_tools.clone())
            .map_err(ServerConstructionError::registration)?;
        Ok(Self {
            queue,
            notification_tx,
            presence: PresenceCapability::Unavailable,
            trace_controller,
            harness,
            attention,
            module_tools,
            discord_tools,
            discord,
        })
    }

    /// Selects the direct, queued, or unavailable presence connection.
    pub fn with_presence(mut self, presence: PresenceCapability) -> Self {
        self.presence = presence;
        self
    }

    /// Replaces optional module tools while retaining the Discord registrations.
    /// The resulting registrations remain immutable while serving requests.
    pub fn with_module_tools(
        mut self,
        mut tools: Vec<RegisteredTool>,
    ) -> Result<Self, RegistrationError> {
        tools.extend(self.discord_tools.iter().cloned());
        self.module_tools = ToolRegistry::new(&self.harness, tools)?;
        Ok(self)
    }
}

impl DioneServer {
    pub(crate) fn harness(&self) -> &HarnessConnection {
        &self.harness
    }

    pub(crate) fn messaging_ctx(&self, config: Arc<crate::config::LoadedConfig>) -> MessagingCtx {
        self.discord.messaging_ctx(config)
    }

    pub(crate) fn no_rly_ctx(&self, config: Arc<crate::config::LoadedConfig>) -> NoRlyCtx {
        NoRlyCtx {
            gate: self.no_rly().clone(),
            config,
        }
    }

    pub(crate) fn access_ctx(&self, config: Arc<crate::config::LoadedConfig>) -> AccessCtx {
        AccessCtx::new(self.queue.clone(), config, self.state_dir().clone())
    }

    pub(crate) fn typing_ctx(&self, config: Arc<crate::config::LoadedConfig>) -> TypingCtx {
        TypingCtx::new(self.http().clone(), self.state().clone(), config)
    }

    pub(crate) fn diagnostics_ctx(&self) -> DiagnosticsCtx<'_> {
        DiagnosticsCtx {
            trace_controller: &self.trace_controller,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StdioExitPolicy {
    ShutdownProcess,
    DetachUntilCancellation,
}

/// Runs the MCP server: reads JSON-RPC from stdin, writes to stdout.
///
/// Also spawns a task that converts [`NotificationEvent`]s from Discord into
/// MCP notifications and writes them to stdout.
pub async fn run(
    server: Arc<DioneServer>,
    event_rx: mpsc::Receiver<NotificationEvent>,
    cancel: CancellationToken,
    stdio_exit_policy: StdioExitPolicy,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    run_with_input(
        server,
        event_rx,
        cancel,
        stdio_exit_policy,
        BufReader::new(tokio::io::stdin()),
    )
    .await
}

async fn run_with_input(
    server: Arc<DioneServer>,
    event_rx: mpsc::Receiver<NotificationEvent>,
    cancel: CancellationToken,
    stdio_exit_policy: StdioExitPolicy,
    stdin: impl tokio::io::AsyncBufRead + Unpin,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let stdout = server.harness.output().clone();

    // Both rate limiter and delivery buffer configs are live-reloadable
    // via the ArcSwap config cache. Rate limiter config is refreshed per
    // event; existing bucket state is preserved across config changes.
    let config = crate::config::load_config(server.state_dir());
    let mut rate_limiter = RateLimiter::new(config.rate_limit_runtime().clone());
    let mut delivery_buffer = DeliveryBuffer::new(server.attention.authority_module().clone());
    let bell_evaluator = Arc::new(BellEvaluator::new());

    // Lazy-init: providers connect on first message, no pre-warming.

    // Resolve timezone once at startup so `deliver_flushed` doesn't need to
    // load config just for the tz. Updated opportunistically when we already
    // load config per-event for the rate limiter.
    let initial_tz = config.tz;
    let initial_evidence_markers_enabled = config.delivery.evidence_markers_enabled;

    let state_dir_notif = server.state_dir().clone();

    // Expiry sweeper for the no_rly hold queue. Expiry is enforced lazily at
    // claim time too — this task just makes sure abandoned handles get their
    // journal record promptly instead of at the next claim. On shutdown it
    // drains everything still pending as expired, so the in-memory queue
    // never swallows a bounce without an audit trail.
    let no_rly_sweeper = server.no_rly().clone();
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
    let notification_sink = server.harness.clone();
    let cancel_notif = cancel.clone();
    let attention = server.attention.clone();
    let notice_no_rly = server.no_rly().clone();
    let notice_event_tx = server.event_tx().clone();
    let notif_task = tokio::spawn(async move {
        let mut rx = event_rx;
        let mut events_since_prune: u64 = 0;
        let mut tz = initial_tz;
        let mut evidence_markers_enabled = initial_evidence_markers_enabled;
        const PRUNE_INTERVAL: u64 = 100;
        let mut admissions = crate::attention::admission::AdmissionController::default();
        let mut evaluations = tokio::task::JoinSet::new();
        let mut attention_config = crate::config::subscribe_generation();
        let mut maintenance = tokio::task::JoinSet::new();
        let mut notices = tokio::task::JoinSet::new();
        let mut attention_tick = tokio::time::interval(Duration::from_secs(1));
        attention_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut next_maintenance = tokio::time::Instant::now();

        loop {
            let flush_deadline = delivery_buffer.next_flush_deadline();

            tokio::select! {
                biased;

                // Cancellation takes priority — break to drain path.
                _ = cancel_notif.cancelled() => {
                    tracing::debug!("notif_task: cancellation received, draining buffer");
                    break;
                }

                _ = attention_config.changed() => {
                    let ready = attention_transition(&mut admissions, &attention).await;
                    if let Err(error) = forward_attention_results(
                        ready,
                        &mut admissions,
                        &attention,
                        &mut delivery_buffer,
                        &bell_evaluator,
                        &notification_sink,
                    )
                    .await
                    {
                        tracing::error!(%error, "attention transition delivery failed"); cancel_notif.cancel(); break;
                    }
                }
                completed = evaluations.join_next(), if !evaluations.is_empty() => {
                    match completed {
                        Some(Ok(record)) => {
                            let cfg = crate::config::load_config(&state_dir_notif);
                            let ready = admissions.complete(record, &cfg);
                            if let Err(error) = forward_attention_results(
                                ready,
                                &mut admissions,
                                &attention,
                                &mut delivery_buffer,
                                &bell_evaluator,
                                &notification_sink,
                            )
                            .await
                            {
                                tracing::error!(%error, "attention admission delivery failed"); cancel_notif.cancel(); break;
                            }
                        }
                        Some(Err(_)) => {
                            attention.unknown("attention worker failed");
                            let ready = attention_transition(&mut admissions, &attention).await;
                            if let Err(error) = forward_attention_results(
                                ready,
                                &mut admissions,
                                &attention,
                                &mut delivery_buffer,
                                &bell_evaluator,
                                &notification_sink,
                            )
                            .await
                            {
                                tracing::error!(%error, "attention worker fallback failed"); cancel_notif.cancel(); break;
                            }
                        }
                        None => {}
                    }
                }

                _ = attention_tick.tick() => {
                    let ready = admissions.retry_ready();
                    if let Err(error) = forward_attention_results(
                        ready,
                        &mut admissions,
                        &attention,
                        &mut delivery_buffer,
                        &bell_evaluator,
                        &notification_sink,
                    )
                    .await
                    {
                        tracing::error!(%error, "attention retry delivery failed"); cancel_notif.cancel(); break;
                    }
                    let now = tokio::time::Instant::now();
                    if maintenance.is_empty() && now >= next_maintenance {
                        let runtime = attention.clone();
                        let incarnation = admissions.incarnation().to_owned();
                        let cfg = crate::config::load_config(&state_dir_notif);
                        next_maintenance = now + Duration::from_millis(cfg.raw.attention.revalidate_ms);
                        maintenance.spawn(async move {
                            runtime.clone().maintain().await;
                            runtime.recover_pending(incarnation).await
                        });
                    }
                    if notices.is_empty() {
                        notices.spawn(attention.clone().publish_notice(
                            notice_no_rly.clone(), notice_event_tx.clone(),
                        ));
                    }
                }
                completed = maintenance.join_next(), if !maintenance.is_empty() => {
                    match completed {
                        Some(Ok(ready)) => {
                            if let Err(error) = forward_attention_results(
                                ready,
                                &mut admissions,
                                &attention,
                                &mut delivery_buffer,
                                &bell_evaluator,
                                &notification_sink,
                            )
                            .await
                            {
                                tracing::error!(%error, "attention recovery delivery failed"); cancel_notif.cancel(); break;
                            }
                        }
                        Some(Err(_)) => attention.unknown("attention recovery worker failed"),
                        None => {}
                    }
                }
                _ = notices.join_next(), if !notices.is_empty() => {}

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
                        delivery_buffer.module(),
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
                    let Some(event) = event else { break };

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
                                    retry_after_ms = %retry_after.as_millis(),
                                    "rate limiter: message denied, dropping"
                                );
                                continue;
                            }
                        }
                    }

                    use crate::attention::admission::Submission;
                    if let NotificationEvent::MessageEdit { chat_id, message_id, .. }
                        | NotificationEvent::MessageDelete { chat_id, message_id, .. } = &event {
                        let source = crate::attention::types::SourceKey { channel_id: *chat_id, message_id: *message_id };
                        let ready = admissions.invalidate(source);
                        if let Err(error) = attention.invalidate(source).await { attention.unknown(error.to_string()); }
                        let scoped_source = crate::core::MessageRef::new(
                            crate::core::ConversationRef::new(attention.authority_module().clone(), crate::core::ConversationId::numeric(source.channel_id.into())),
                            crate::core::MessageId::numeric(source.message_id.into()),
                        );
                        if let Err(error) = notification_sink.invalidate_attention(scoped_source).await {
                            attention.unknown(error.to_string());
                        }
                        if let Err(error) = forward_attention_results(
                            ready,
                            &mut admissions,
                            &attention,
                            &mut delivery_buffer,
                            &bell_evaluator,
                            &notification_sink,
                        )
                        .await
                        {
                            tracing::error!(%error, "attention lifecycle delivery failed"); cancel_notif.cancel(); break;
                        }
                    }
                    let submission = begin_attention(&mut admissions, attention.as_ref(), event, &cfg);
                    let ordinary = match submission {
                        Submission::Ordinary(event) => Some(event),
                        Submission::Unknown { event, .. } => Some(event),
                        Submission::Duplicate => None,
                        Submission::Judge { work, ordinary } => {
                            evaluations.spawn(attention.clone().evaluate(*work));
                            ordinary
                        }
                        Submission::Saturated { event } => Some(event),
                    };
                    if let Some(event) = ordinary
                        && let Err(error) = forward_ordinary(event, &cfg, &mut delivery_buffer, &bell_evaluator, &notification_sink).await {
                        tracing::error!(%error, "inbound delivery failed; shutting down"); cancel_notif.cancel(); break;
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
        drop(admissions);
        evaluations.abort_all();
        maintenance.abort_all();
        notices.abort_all();
        while maintenance.join_next().await.is_some() {}
        while notices.join_next().await.is_some() {}
        while evaluations.join_next().await.is_some() {}

        // Channel closed — flush any remaining buffered events.
        let remaining = delivery_buffer.flush_all();
        let outcome = deliver_flushed(
            &notification_sink,
            delivery_buffer.module(),
            remaining,
            tz,
            evidence_markers_enabled,
        )
        .await;
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

            _ = stdout.failed() => {
                tracing::error!("MCP output failed; shutting down");
                cancel.cancel();
                break;
            }

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
                                if let Some(resp_value) = resp
                                    && let Err(error) = stdout.write(&resp_value).await {
                                    tracing::error!(%error, "MCP response output failed");
                                    cancel.cancel();
                                    break;
                                }
                            }
                            Err(e) => {
                                tracing::warn!(error = %e, "failed to parse JSON-RPC line");
                                let err_resp = json!({
                                    "jsonrpc": "2.0",
                                    "id": null,
                                    "error": { "code": -32700, "message": "parse error" }
                                });
                                if let Err(error) = stdout.write(&err_resp).await {
                                    tracing::error!(%error, "MCP error response output failed");
                                    cancel.cancel();
                                    break;
                                }
                            }
                        }
                    }
                    Ok(None) => {
                        // EOF — the MCP client closed the pipe. This is the
                        // normal way the client dies, and it must trigger the
                        // same graceful shutdown as Ctrl-C: fire the cancel so
                        // the sweeper drains pending no_rly handles into the
                        // journal instead of parking until the timeout.
                        if stdio_exit_policy == StdioExitPolicy::ShutdownProcess {
                            tracing::info!("stdin EOF, MCP server shutting down");
                            cancel.cancel();
                        } else {
                            tracing::info!("stdin EOF, stdio transport detached");
                        }
                        break;
                    }
                    Err(e) => {
                        // A read error is also a terminal exit — cancel so the
                        // background tasks run their drain paths.
                        if stdio_exit_policy == StdioExitPolicy::ShutdownProcess {
                            tracing::warn!(error = %e, "stdin read error, shutting down");
                            cancel.cancel();
                        } else {
                            tracing::warn!(error = %e, "stdin read error, stdio transport detached");
                        }
                        break;
                    }
                }
            }
        }
    }

    if let Err(error) = server.harness.protocol().stdio_disconnected().await {
        tracing::error!(%error, "failed to revoke stdio readiness");
        cancel.cancel();
    }

    // The resident socket may keep the process alive after stdio detaches.
    // Drain notification and sweeper tasks only after process cancellation.
    cancel.cancelled().await;
    drop(server);
    let _ = tokio::time::timeout(Duration::from_millis(500), notif_task).await;
    let _ = tokio::time::timeout(Duration::from_millis(500), sweep_task).await;

    Ok(())
}

const RESIDENT_QUEUE_TOOLS: &[&str] = &[
    "register_event_consumer",
    "handoff_event_consumer",
    "next_event",
    "ack_event",
    "event_queue_status",
];
const RESIDENT_SOCIAL_TOOLS: &[&str] = &[
    "react",
    "edit_message",
    "send_file",
    "render_latex",
    "render_latex_to_channel",
];
const RESIDENT_MAX_FRAME_BYTES: usize = 1024 * 1024;
const RESIDENT_MAX_CONNECTIONS: usize = 4;
const RESIDENT_READ_TIMEOUT: Duration = Duration::from_secs(15);

fn resident_request_allowed(registry: &ToolRegistry, request: &Value) -> bool {
    match request.get("method").and_then(Value::as_str).unwrap_or("") {
        "initialize" | "notifications/initialized" => true,
        "tools/call" => request
            .pointer("/params/name")
            .and_then(Value::as_str)
            .is_some_and(|name| {
                PRIVATE_CONTROL_TOOLS.contains(&name)
                    || RESIDENT_QUEUE_TOOLS.contains(&name)
                    || RESIDENT_SOCIAL_TOOLS.contains(&name)
                    || registry.resident_allowed(name)
            }),
        _ => false,
    }
}

async fn read_resident_frame<R>(
    reader: &mut R,
    input: &mut Vec<u8>,
) -> Result<Option<Vec<u8>>, Box<dyn std::error::Error + Send + Sync>>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut chunk = [0_u8; 8192];
    loop {
        if let Some(newline) = input.iter().position(|byte| *byte == b'\n') {
            if newline > RESIDENT_MAX_FRAME_BYTES {
                return Err("resident control frame exceeds byte limit".into());
            }
            let mut frame: Vec<u8> = input.drain(..=newline).collect();
            frame.pop();
            return Ok(Some(frame));
        }
        if input.len() > RESIDENT_MAX_FRAME_BYTES {
            return Err("resident control frame exceeds byte limit".into());
        }
        let count = tokio::time::timeout(RESIDENT_READ_TIMEOUT, reader.read(&mut chunk))
            .await
            .map_err(|_| "resident control read timed out")??;
        if count == 0 {
            if input.is_empty() {
                return Ok(None);
            }
            return Err("resident control frame ended without newline".into());
        }
        input.extend_from_slice(&chunk[..count]);
    }
}

async fn serve_resident_connection(
    server: Arc<DioneServer>,
    stream: UnixStream,
    cancel: CancellationToken,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (mut reader, writer) = stream.into_split();
    let writer = ProtocolOutput::new(writer);
    let mut input = Vec::new();
    loop {
        // Only idle reads are cancellable. Once a request is accepted its
        // dispatch and durable settlement remain owned through shutdown.
        let frame = tokio::select! {
            biased;
            _ = cancel.cancelled() => break,
            frame = read_resident_frame(&mut reader, &mut input) => frame?,
        };
        let Some(frame) = frame else { break };
        let text = std::str::from_utf8(&frame)?;
        let trimmed = text.trim();
        if trimmed.is_empty() {
            continue;
        }
        let request = match serde_json::from_str::<Value>(trimmed) {
            Ok(request) => request,
            Err(_) => {
                writer
                    .write(&json!({
                        "jsonrpc": "2.0",
                        "id": null,
                        "error": { "code": -32700, "message": "parse error" }
                    }))
                    .await?;
                continue;
            }
        };
        if !resident_request_allowed(&server.module_tools, &request) {
            let id = request.get("id").cloned().unwrap_or(Value::Null);
            writer.write(
                    &json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": { "code": -32601, "message": "method is unavailable on the resident control socket" }
                    }),
                )
                .await?;
            continue;
        }
        if let Some(response) =
            handle_request_from(&server, request, RequestOrigin::ResidentControl).await
        {
            writer.write(&response).await?;
        }
    }
    Ok(())
}

/// Serves the resident control API on an operator-owned Unix socket.
///
/// Permits the fixed queue and social tool allowlists and explicitly
/// resident-enabled module registrations. Rejects other methods and tools.
pub async fn run_resident_control(
    server: Arc<DioneServer>,
    socket_path: Utf8PathBuf,
    cancel: CancellationToken,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if !server.harness.protocol().resident_control() {
        return Err("resident control is unavailable for this harness".into());
    }
    let parent = socket_path
        .parent()
        .ok_or("resident control socket needs a parent directory")?;
    match tokio::fs::symlink_metadata(parent).await {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            tokio::fs::create_dir_all(parent).await?;
            tokio::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700)).await?;
        }
        Err(error) => return Err(error.into()),
    }
    let parent_metadata = tokio::fs::symlink_metadata(parent).await?;
    if !parent_metadata.is_dir()
        || parent_metadata.file_type().is_symlink()
        || parent_metadata.uid() != unsafe { libc::geteuid() }
        || parent_metadata.permissions().mode() & 0o777 != 0o700
    {
        return Err("resident control parent must be an owned mode-0700 directory".into());
    }
    match tokio::fs::symlink_metadata(&socket_path).await {
        Ok(metadata)
            if metadata.file_type().is_socket() && metadata.uid() == unsafe { libc::geteuid() } =>
        {
            tokio::fs::remove_file(&socket_path).await?;
        }
        Ok(_) => return Err("refusing to replace a non-owned or non-socket control path".into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let listener = UnixListener::bind(socket_path.as_std_path())?;
    tokio::fs::set_permissions(
        socket_path.as_std_path(),
        std::fs::Permissions::from_mode(0o600),
    )
    .await?;
    let connections = Arc::new(Semaphore::new(RESIDENT_MAX_CONNECTIONS));
    let accepted_tasks = TaskTracker::new();
    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => break,
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                if stream.peer_cred()?.uid() != unsafe { libc::geteuid() } {
                    tracing::warn!("rejected resident control connection from another user");
                    continue;
                }
                let Ok(permit) = connections.clone().try_acquire_owned() else {
                    drop(stream);
                    continue;
                };
                let connection_server = server.clone();
                let connection_cancel = cancel.clone();
                accepted_tasks.spawn(async move {
                    let _permit = permit;
                    if let Err(error) = serve_resident_connection(connection_server, stream, connection_cancel).await {
                        tracing::warn!(error = %error, "resident control connection failed");
                    }
                });
            }
        }
    }
    drop(listener);
    accepted_tasks.close();
    accepted_tasks.wait().await;
    tokio::fs::remove_file(socket_path.as_std_path()).await?;
    Ok(())
}

async fn handle_request(server: &DioneServer, req: Value) -> Option<Value> {
    handle_request_from(server, req, RequestOrigin::Stdio).await
}

async fn handle_request_from(
    server: &DioneServer,
    req: Value,
    origin: RequestOrigin,
) -> Option<Value> {
    let id = req.get("id").cloned();
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    let params = req.get("params").cloned().unwrap_or(json!({}));

    let result = dispatch(server, method, params, origin).await;

    let id = id?;
    Some(match result {
        Ok(v) => json!({ "jsonrpc": "2.0", "id": id, "result": v }),
        Err(msg) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": -32000, "message": msg }
        }),
    })
}

async fn dispatch(
    server: &DioneServer,
    method: &str,
    params: Value,
    origin: RequestOrigin,
) -> Result<Value, String> {
    match method {
        "initialize" => Ok(initialize_response(
            &server.harness,
            params.get("protocolVersion").and_then(Value::as_str),
        )),
        "notifications/initialized" => {
            if origin == RequestOrigin::Stdio {
                server
                    .harness
                    .protocol()
                    .stdio_initialized()
                    .await
                    .map_err(|error| error.to_string())?;
            }
            Ok(json!({}))
        }
        "tools/list" => {
            let config = crate::config::load_config(server.state_dir());
            let mut response =
                tools_list(&server.harness, config.delivery.evidence_markers_enabled);
            server.module_tools.append_definitions(
                &mut response,
                crate::mcp::tool_registry::ToolDiscoveryContext {
                    evidence_markers_enabled: config.delivery.evidence_markers_enabled,
                },
            );
            Ok(response)
        }
        "tools/call" => {
            let tool_name = params
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| "missing tool name".to_string())?;
            let arguments = params.get("arguments").cloned().unwrap_or(json!({}));
            if PRIVATE_CONTROL_TOOLS.contains(&tool_name) {
                if origin != RequestOrigin::ResidentControl {
                    return Err("tool is unavailable on stdio".to_owned());
                }
                let result = server
                    .harness
                    .protocol()
                    .private_control(tool_name, &arguments)
                    .await
                    .ok_or_else(|| "private control is unavailable for this harness".to_owned())?
                    .map_err(|error| error.to_string())?;
                return Ok(json!({"content": [{"type": "text", "text": result.to_string()}]}));
            }
            call_tool(server, tool_name, arguments).await
        }
        "notifications/claude/channel/permission_request"
            if server.harness.protocol().permission_relay() =>
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

            let config = crate::config::load_config(server.state_dir());
            if let Err(e) = crate::permissions::send_permission_request(
                server.http(),
                &config,
                server.state(),
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

/// Outcome of attempting to deliver a batch of flushed events.
struct DeliverOutcome {
    /// First error encountered, if any.
    error: Option<HarnessError>,
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
    fn into_result(self) -> Result<(), HarnessError> {
        match self.error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

fn presentation(tz: Option<chrono_tz::Tz>, evidence: bool) -> PresentationContext {
    PresentationContext::new(
        tz,
        if evidence {
            EvidenceVisibility::Enabled
        } else {
            EvidenceVisibility::Disabled
        },
    )
}

async fn deliver_flushed(
    sink: &HarnessConnection,
    module: &ModuleInstanceId,
    events: Vec<NotificationEvent>,
    tz: Option<chrono_tz::Tz>,
    evidence_markers_enabled: bool,
) -> DeliverOutcome {
    let mut iterator = events.iter().cloned();
    let Some(first) = iterator.next() else {
        return DeliverOutcome::ok();
    };
    let context = presentation(tz, evidence_markers_enabled);
    let delivery = match crate::discord::delivery::batch(module, first, iterator.collect(), context)
    {
        Ok(delivery) => delivery,
        Err(error) => {
            return DeliverOutcome {
                error: Some(HarnessError::source(error)),
                undelivered: events,
            };
        }
    };
    if evidence_markers_enabled
        && events.iter().any(NotificationEvent::has_offered_evidence)
        && let DeliveryKind::Ordinary { first, rest } = delivery.kind()
    {
        let mut events = events;
        for (index, event) in std::iter::once(first).chain(rest).enumerate() {
            let single = Delivery::single(event.clone(), context);
            if let Err(error) = sink.deliver(&single).await {
                let retry_from = if error.completion_is_unknown() {
                    index + 1
                } else {
                    index
                };
                return DeliverOutcome {
                    error: Some(error),
                    undelivered: events.split_off(retry_from),
                };
            }
        }
        return DeliverOutcome::ok();
    }
    match sink.deliver(&delivery).await {
        Ok(()) => DeliverOutcome::ok(),
        Err(error) => DeliverOutcome {
            error: Some(error),
            undelivered: Vec::new(),
        },
    }
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

/// Extracts the configured duration at the Discord configuration boundary.
///
/// Returns the configured delay for channel events (Message, MessageEdit,
/// MessageDelete, Reaction). Control events have no delay.
fn extract_delay(event: &NotificationEvent, config: &crate::config::LoadedConfig) -> Duration {
    let channel_id = match event {
        NotificationEvent::Message(msg) => Some(msg.chat_id.get()),
        NotificationEvent::MessageEdit { chat_id, .. }
        | NotificationEvent::MessageDelete { chat_id, .. }
        | NotificationEvent::Reaction { chat_id, .. } => Some(chat_id.get()),
        NotificationEvent::PermissionResponse { .. }
        | NotificationEvent::Trace { .. }
        | NotificationEvent::ConfigError { .. }
        | NotificationEvent::Coordination(_) => None,
    };
    match channel_id {
        Some(id) => Duration::from_millis(config.delivery_delay_ms(id)),
        None => Duration::ZERO,
    }
}

/// Test helpers that expose internal functions through the crate's public API.
pub mod test_helpers {
    use super::*;

    /// Projects a test event through the current typed delivery and legacy codec.
    pub fn make_notification(
        event: NotificationEvent,
    ) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
        let module = crate::core::ModuleInstanceId::new(
            crate::core::ProviderId::new("discord")?,
            crate::core::ModuleSlotId::new("fixture")?,
        );
        let delivery = crate::discord::delivery::single(&module, event, presentation(None, true))?;
        Ok(crate::codex::legacy_delivery::encode(
            &delivery,
            &crate::codex::LegacyDiscordScope::new(module),
        )?)
    }

    /// Projects a nonempty batch through the current typed delivery and legacy codec.
    pub fn make_batch(
        events: Vec<NotificationEvent>,
        tz: Option<chrono_tz::Tz>,
    ) -> Result<Option<Value>, Box<dyn std::error::Error + Send + Sync>> {
        let mut events = events.into_iter();
        let Some(first) = events.next() else {
            return Ok(None);
        };
        let module = crate::core::ModuleInstanceId::new(
            crate::core::ProviderId::new("discord")?,
            crate::core::ModuleSlotId::new("fixture")?,
        );
        let delivery = crate::discord::delivery::batch(
            &module,
            first,
            events.collect(),
            presentation(tz, true),
        )?;
        Ok(Some(crate::codex::legacy_delivery::encode(
            &delivery,
            &crate::codex::LegacyDiscordScope::new(module),
        )?))
    }

    fn complete_tools_list(harness: &HarnessConnection, evidence: bool) -> Value {
        let mut response = crate::mcp::protocol::tools_list(harness, evidence);
        response["tools"]
            .as_array_mut()
            .expect("tools array")
            .push(crate::discord::reply_tool::definition(evidence));
        response["tools"]
            .as_array_mut()
            .expect("tools array")
            .extend(crate::discord::introspection_tools::definitions());
        response["tools"]
            .as_array_mut()
            .unwrap()
            .extend(crate::discord::query_tools::definitions());
        response["tools"]
            .as_array_mut()
            .expect("tools array")
            .extend(crate::discord::management_tools::definitions());
        response
    }

    /// Exposes `tools_list` for unit testing tool discovery.
    pub fn get_tools_list() -> Value {
        complete_tools_list(&HarnessConnection::claude(), false)
    }

    pub fn get_tools_list_with_evidence() -> Value {
        complete_tools_list(&HarnessConnection::claude(), true)
    }

    pub fn get_codex_tools_list(harness: &HarnessConnection) -> Value {
        complete_tools_list(harness, false)
    }

    /// Exposes `initialize_response` for unit testing the handshake.
    pub fn get_initialize_response() -> Value {
        crate::mcp::protocol::initialize_response(&HarnessConnection::claude(), None)
    }

    /// Exposes the Codex initialize response for protocol tests.
    pub fn get_codex_initialize_response(harness: &HarnessConnection) -> Value {
        crate::mcp::protocol::initialize_response(harness, None)
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
        delivery_buffer::BufferResult,
        discord::scheduling::DeliveryBuffer,
        harness::test_support::FixtureNotification,
        rate_limiter::{OverflowPolicy, RateLimitConfig, ScopeConfig},
        teams::ReplyHandle,
        timestamp::Timestamp,
    };
    use serenity::model::id::{ChannelId, MessageId, UserId};
    use std::collections::HashMap;

    #[test]
    fn resident_control_allows_only_bounded_tools() {
        for name in RESIDENT_QUEUE_TOOLS
            .iter()
            .chain(RESIDENT_SOCIAL_TOOLS)
            .chain(PRIVATE_CONTROL_TOOLS)
        {
            let allowed = json!({
                "method": "tools/call",
                "params": { "name": name, "arguments": {} }
            });
            assert!(
                resident_request_allowed(&ToolRegistry::default(), &allowed),
                "{name} must be allowed"
            );
        }
        let denied = json!({
            "method": "tools/call",
            "params": { "name": "reload_config", "arguments": {} }
        });
        assert!(!resident_request_allowed(&ToolRegistry::default(), &denied));
        assert!(!resident_request_allowed(
            &ToolRegistry::default(),
            &json!({ "method": "tools/list" })
        ));
    }

    #[test]
    fn resident_social_tools_are_the_exact_closed_bridge_surface() {
        assert_eq!(
            RESIDENT_SOCIAL_TOOLS,
            &[
                "react",
                "edit_message",
                "send_file",
                "render_latex",
                "render_latex_to_channel",
            ]
        );
    }

    #[tokio::test]
    async fn resident_control_frames_are_bounded_and_newline_delimited() {
        let mut exact = vec![b'x'; RESIDENT_MAX_FRAME_BYTES];
        exact.push(b'\n');
        exact.extend_from_slice(b"second\n");
        let mut reader = std::io::Cursor::new(exact);
        let mut buffered = Vec::new();
        let first = read_resident_frame(&mut reader, &mut buffered)
            .await
            .expect("maximum-size frame")
            .expect("first frame");
        assert_eq!(first.len(), RESIDENT_MAX_FRAME_BYTES);
        let second = read_resident_frame(&mut reader, &mut buffered)
            .await
            .expect("buffered second frame")
            .expect("second frame");
        assert_eq!(second, b"second");

        let mut oversized = vec![b'x'; RESIDENT_MAX_FRAME_BYTES + 1];
        oversized.push(b'\n');
        let error = read_resident_frame(&mut std::io::Cursor::new(oversized), &mut Vec::new())
            .await
            .expect_err("oversized frame must fail");
        assert!(error.to_string().contains("exceeds byte limit"));

        let error = read_resident_frame(
            &mut std::io::Cursor::new(b"partial".to_vec()),
            &mut Vec::new(),
        )
        .await
        .expect_err("unterminated frame must fail");
        assert!(error.to_string().contains("without newline"));
    }

    #[tokio::test]
    async fn resident_shutdown_waits_for_accepted_reply_after_client_disconnect() {
        use std::{
            os::unix::fs::PermissionsExt,
            sync::atomic::{AtomicBool, Ordering},
        };
        use tokio::{io::AsyncWriteExt, sync::oneshot};

        struct HeldReply {
            started: std::sync::Mutex<Option<oneshot::Sender<()>>>,
            release: std::sync::Mutex<Option<oneshot::Receiver<()>>>,
            settled: Arc<AtomicBool>,
        }

        impl crate::teams::runtime::TeamsReplyAuthority for HeldReply {
            fn reply<'a>(
                &'a self,
                handle: &'a ReplyHandle,
                text: &'a str,
            ) -> std::pin::Pin<
                Box<
                    dyn std::future::Future<
                            Output = Result<
                                crate::teams::edge::LiveReplyReceipt,
                                crate::teams::runtime::TeamsRuntimeError,
                            >,
                        > + Send
                        + 'a,
                >,
            > {
                Box::pin(async move {
                    assert_eq!(
                        handle.as_str(),
                        "teams-00000000-0000-4000-8000-000000000001"
                    );
                    assert_eq!(text, "resident reply");
                    self.started
                        .lock()
                        .expect("started lock")
                        .take()
                        .expect("single accepted request")
                        .send(())
                        .expect("test waits for acceptance");
                    let release = self
                        .release
                        .lock()
                        .expect("release lock")
                        .take()
                        .expect("single release");
                    release.await.expect("test releases settlement");
                    self.settled.store(true, Ordering::SeqCst);
                    Ok(crate::teams::edge::LiveReplyReceipt {
                        incoming_activity_id: "inbound".parse().unwrap(),
                        outgoing_activity_id: "outbound".parse().unwrap(),
                        conversation_id: "conversation".parse().unwrap(),
                    })
                })
            }
        }

        let dir = tempfile::TempDir::new().expect("private socket directory");
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))
            .expect("owned mode-0700 socket directory");
        let state_dir = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf())
            .expect("UTF-8 fixture path");
        let socket_path = state_dir.join("s");
        let (started_tx, started_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        let settled = Arc::new(AtomicBool::new(false));
        let authority = Arc::new(HeldReply {
            started: std::sync::Mutex::new(Some(started_tx)),
            release: std::sync::Mutex::new(Some(release_rx)),
            settled: settled.clone(),
        });
        let (notification_tx, _notification_rx) = mpsc::channel(1);
        let (_queue_dir, harness) = test_codex_connection();
        let server = DioneServer::new(
            crate::state::new_state(),
            Arc::new(Mutex::new(crate::queue::AccessQueue::load(&state_dir))),
            Arc::new(serenity::http::Http::new("fake")),
            state_dir.clone(),
            notification_tx,
            TraceLevelController::noop(),
            harness,
            Arc::new(crate::no_rly::consent::ConsentGate::new(&state_dir)),
            Arc::new(crate::ingress_ledger::IngressLedger::new()),
            crate::core::ModuleInstanceId::new(
                crate::core::ProviderId::new("discord").unwrap(),
                crate::core::ModuleSlotId::new("primary").unwrap(),
            ),
            None,
        )
        .await
        .expect("Discord reply registration")
        .with_module_tools(crate::teams::mcp_tools(Some(authority)).expect("valid Teams tool"))
        .expect("disjoint tool registrations");
        let cancel = CancellationToken::new();
        let mut listener = tokio::spawn(run_resident_control(
            Arc::new(server),
            socket_path.clone(),
            cancel.clone(),
        ));
        let mut client = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Ok(stream) = UnixStream::connect(socket_path.as_std_path()).await {
                    break stream;
                }
                if listener.is_finished() {
                    let result = (&mut listener).await;
                    panic!("resident listener exited before bind: {result:?}");
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("resident socket binds");
        let request = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "teams_reply",
                "arguments": { "reply_handle": "teams-00000000-0000-4000-8000-000000000001", "content": "resident reply" }
            }
        });
        client
            .write_all(format!("{request}\n").as_bytes())
            .await
            .expect("write accepted tool request");
        tokio::time::timeout(Duration::from_secs(2), started_rx)
            .await
            .expect("request reaches authority")
            .expect("acceptance signal survives");

        drop(client);
        cancel.cancel();
        assert!(
            tokio::time::timeout(Duration::from_millis(100), &mut listener)
                .await
                .is_err(),
            "shutdown must wait for accepted request settlement"
        );
        assert!(!settled.load(Ordering::SeqCst));
        release_tx.send(()).expect("release accepted settlement");
        tokio::time::timeout(Duration::from_secs(2), listener)
            .await
            .expect("resident server drains")
            .expect("resident task joins")
            .expect("resident server exits cleanly");
        assert!(settled.load(Ordering::SeqCst));
        assert!(!socket_path.exists());
    }

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
            HarnessConnection::claude(),
            Arc::new(crate::no_rly::consent::ConsentGate::new(&state_dir)),
            Arc::new(crate::ingress_ledger::IngressLedger::new()),
            crate::core::ModuleInstanceId::new(
                crate::core::ProviderId::new("discord").unwrap(),
                crate::core::ModuleSlotId::new("primary").unwrap(),
            ),
            None,
        )
        .await
        .expect("Discord reply registration");

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
            author_kind: crate::attention::types::SourceAuthorKind::DirectHuman,
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

    pub(super) fn evidence_message_event(index: u64) -> NotificationEvent {
        NotificationEvent::Message(MessageEvent {
            chat_id: ChannelId::new(42),
            message_id: MessageId::new(100 + index),
            user: format!("user-{index}"),
            user_id: UserId::new(200 + index),
            author_kind: crate::attention::types::SourceAuthorKind::DirectHuman,
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

    #[tokio::test]
    async fn unknown_admission_does_not_requeue_ambiguous_event_but_retains_tail() {
        let directory = tempfile::TempDir::new().unwrap();
        let root = camino::Utf8PathBuf::from_path_buf(directory.path().to_owned()).unwrap();
        let module = ModuleInstanceId::new(
            ProviderId::new("discord").unwrap(),
            ModuleSlotId::new("primary").unwrap(),
        );
        let queue = crate::codex::CodexEventQueue::open(
            &root,
            crate::codex::LegacyDiscordScope::new(module.clone()),
        )
        .await
        .unwrap();
        let sink = test_connection_for_queue(queue.clone());
        let (reached, release) = queue.pause_owned_completion_for_test(true);
        let events = vec![evidence_message_event(0), evidence_message_event(1)];
        let producer =
            tokio::spawn(async move { deliver_flushed(&sink, &module, events, None, true).await });
        reached.await.unwrap();
        release.send(()).unwrap();
        let outcome = producer.await.unwrap();
        assert!(outcome.error.as_ref().unwrap().completion_is_unknown());
        assert_eq!(outcome.undelivered.len(), 1);
        let NotificationEvent::Message(tail) = &outcome.undelivered[0] else {
            panic!("literal message tail")
        };
        assert_eq!(tail.message_id, MessageId::new(101));
        assert_eq!(queue.status().await.queued, 1);
        let sink = test_connection_for_queue(queue.clone());
        let recovered = deliver_flushed(
            &sink,
            queue.legacy_scope().module(),
            outcome.undelivered,
            None,
            true,
        )
        .await;
        assert!(recovered.error.is_none());
        assert_eq!(queue.status().await.queued, 2);
    }

    #[tokio::test(start_paused = true)]
    async fn evidence_burst_overflow_reaches_real_sink_losslessly_in_fifo_order() {
        let dir = tempfile::TempDir::new().unwrap();
        let state_dir = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        let queue = crate::codex::CodexEventQueue::load(
            &state_dir,
            crate::codex::LegacyDiscordScope::new(crate::core::ModuleInstanceId::new(
                crate::core::ProviderId::new("discord").unwrap(),
                crate::core::ModuleSlotId::new("primary").unwrap(),
            )),
        )
        .unwrap();
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
        let sink = test_connection_for_queue(queue.clone());
        let mut buffer = DeliveryBuffer::new(ModuleInstanceId::new(
            ProviderId::new("discord").unwrap(),
            ModuleSlotId::new("primary").unwrap(),
        ));

        for index in 0..6 {
            match buffer.buffer_event(evidence_message_event(index), Duration::from_millis(500)) {
                BufferResult::Immediate(event) => {
                    sink.deliver(
                        &crate::discord::delivery::single(
                            buffer.module(),
                            *event,
                            presentation(None, true),
                        )
                        .unwrap(),
                    )
                    .await
                    .unwrap();
                }
                BufferResult::Buffered => {
                    assert!(index >= 4, "only budget overflow may be delayed");
                }
                BufferResult::Rejected { error, .. } => {
                    panic!("valid fixture delay rejected: {error}")
                }
                BufferResult::FlushThenImmediate { .. } => {
                    panic!("one uninterrupted burst has no buffered predecessor recovery")
                }
            }
        }

        tokio::time::advance(Duration::from_millis(500)).await;
        let overflow = buffer.flush_ready(tokio::time::Instant::now());
        assert_eq!(overflow.len(), 2);
        let outcome = deliver_flushed(&sink, buffer.module(), overflow, None, true).await;
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
            let citations = leased.event["params"]["meta"]["citation_locators"]
                .as_array()
                .expect("legacy evidence must survive final delivery as citations");
            assert!(
                leased.event["params"]["meta"]
                    .get("claim_locators")
                    .is_none()
            );
            assert_eq!(citations.len(), 2);
            assert_eq!(citations[0]["locator"], "v1:AAAAAAAAAAw");
            assert_eq!(citations[1]["locator"], "v1:AAAAAAAAACI");
            assert_eq!(citations[0]["author_id"], (200 + index).to_string());
            assert_eq!(citations[1]["author_id"], (200 + index).to_string());
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
        let queue = crate::codex::CodexEventQueue::load(
            &state_dir,
            crate::codex::LegacyDiscordScope::new(crate::core::ModuleInstanceId::new(
                crate::core::ProviderId::new("discord").unwrap(),
                crate::core::ModuleSlotId::new("primary").unwrap(),
            )),
        )
        .unwrap();
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
        let sink = test_connection_for_queue(queue.clone());
        let event = evidence_message_event(0);
        let expected_content = match &event {
            NotificationEvent::Message(message) => message.content.clone(),
            _ => unreachable!(),
        };

        deliver_flushed(
            &sink,
            queue.legacy_scope().module(),
            vec![event],
            None,
            false,
        )
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
        assert!(
            leased.event["params"]["meta"]
                .get("claim_locators")
                .is_none()
        );
        assert!(
            leased.event["params"]["meta"]
                .get("citation_locators")
                .is_none()
        );
    }

    #[test]
    fn extract_delay_message_uses_channel_config() {
        let config = config_with_channel_delay(42, 500);
        assert_eq!(
            extract_delay(&message_event(42), &config),
            Duration::from_millis(500)
        );
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
        assert_eq!(extract_delay(&event, &config), Duration::from_millis(200));
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
        assert_eq!(extract_delay(&event, &config), Duration::from_millis(300));
    }

    #[test]
    fn extract_delay_message_delete_uses_channel_config() {
        let config = config_with_channel_delay(55, 100);
        let event = NotificationEvent::MessageDelete {
            chat_id: ChannelId::new(55),
            message_id: MessageId::new(1),
            thread_parent_id: None,
        };
        assert_eq!(extract_delay(&event, &config), Duration::from_millis(100));
    }

    #[test]
    fn extract_delay_non_channel_event_is_zero() {
        let config = config_with_channel_delay(1, 999);
        let event = NotificationEvent::ConfigError {
            error: "oops".into(),
        };
        assert_eq!(extract_delay(&event, &config), Duration::from_millis(0));
    }

    #[test]
    fn extract_delay_unconfigured_channel_falls_back_to_global() {
        let config = config_with_global_delay(750);
        assert_eq!(
            extract_delay(&message_event(9999), &config),
            Duration::from_millis(750)
        );
    }

    #[test]
    fn immediate_buffer_result_dereferences_to_notification() {
        let mut buf = DeliveryBuffer::new(ModuleInstanceId::new(
            ProviderId::new("discord").unwrap(),
            ModuleSlotId::new("primary").unwrap(),
        ));
        let result = buf.buffer_event(message_event(42), Duration::from_millis(0));
        let notification = match result {
            BufferResult::Immediate(event) => (*event).fixture_notification(),
            BufferResult::FlushThenImmediate { .. } => {
                panic!("delay=0 cannot require a preceding flush")
            }
            BufferResult::Buffered => panic!("expected Immediate for delay=0"),
            BufferResult::Rejected { error, .. } => panic!("zero delay rejected: {error}"),
        };
        assert_eq!(notification["jsonrpc"], "2.0");
        assert_eq!(notification["method"], "notifications/claude/channel");
        assert_eq!(notification["params"]["meta"]["chat_id"], "42");
    }
}

async fn prepare_bells(
    mut event: NotificationEvent,
    cfg: &crate::config::LoadedConfig,
    evaluator: &Arc<BellEvaluator>,
) -> NotificationEvent {
    match cfg.bell_rings.mode {
        BellMode::Shadow => {
            let evaluator = evaluator.clone();
            let shadow_event = event.clone();
            let shadow_config = cfg.bell_rings.clone();
            tokio::spawn(async move {
                let _ = evaluator.evaluate(shadow_event, &shadow_config).await;
            });
        }
        BellMode::Live => {
            let (returned, bells, status) = evaluator.evaluate(event, &cfg.bell_rings).await;
            event = returned;
            if let NotificationEvent::Message(message) = &mut event {
                if let Some(status) = status {
                    message.bells_status = Some(status);
                }
                if !bells.is_empty() {
                    message.bells = Some(render_bells(&bells));
                }
            }
        }
    }
    event
}

/// The same forwarding unit serves off, log, direct, and failure fallback paths.
async fn forward_ordinary(
    event: NotificationEvent,
    cfg: &crate::config::LoadedConfig,
    buffer: &mut DeliveryBuffer,
    evaluator: &Arc<BellEvaluator>,
    sink: &HarnessConnection,
) -> Result<(), HarnessError> {
    let event = prepare_bells(event, cfg, evaluator).await;
    let evidence = cfg.delivery.evidence_markers_enabled;
    let delay = extract_delay(&event, cfg);
    match buffer.buffer_event_with_evidence(event, delay, evidence) {
        BufferResult::Immediate(event) => {
            let delivery = match crate::discord::delivery::single(
                buffer.module(),
                (*event).clone(),
                presentation(cfg.tz, evidence),
            ) {
                Ok(delivery) => delivery,
                Err(error) => {
                    buffer.requeue(vec![*event]);
                    return Err(HarnessError::source(error));
                }
            };
            sink.deliver(&delivery).await
        }
        BufferResult::FlushThenImmediate { preceding, event } => {
            let outcome = deliver_flushed(sink, buffer.module(), preceding, cfg.tz, evidence).await;
            buffer.requeue(outcome.undelivered);
            if let Some(error) = outcome.error {
                buffer.requeue(vec![*event]);
                return Err(error);
            }
            let delivery = match crate::discord::delivery::single(
                buffer.module(),
                (*event).clone(),
                presentation(cfg.tz, evidence),
            ) {
                Ok(delivery) => delivery,
                Err(error) => {
                    buffer.requeue(vec![*event]);
                    return Err(HarnessError::source(error));
                }
            };
            sink.deliver(&delivery).await
        }
        BufferResult::Buffered => Ok(()),
        BufferResult::Rejected { event, error } => {
            buffer.requeue(vec![*event]);
            Err(HarnessError::source(error))
        }
    }
}

async fn forward_attention_results(
    results: Vec<crate::attention::admission::AdmissionResult>,
    admissions: &mut crate::attention::admission::AdmissionController,
    attention: &Arc<crate::attention::runtime::AttentionRuntime>,
    buffer: &mut DeliveryBuffer,
    evaluator: &Arc<BellEvaluator>,
    sink: &HarnessConnection,
) -> Result<(), HarnessError> {
    let mut results = results.into_iter();
    while let Some(mut result) = results.next() {
        let cfg = crate::config::load_config(&attention.resolver.state_dir);
        if result.record.recipient != cfg.raw.attention.recipient {
            continue;
        }
        if result.record.delivery == DeliveryState::Invalidated {
            continue;
        }
        result.event.reply_to_content_preview = None;
        let Some(source) = result.record.sources.first() else {
            continue;
        };
        match attention.resolver.ledger.delivery_check(
            source.key.message_id,
            source.key.channel_id,
            &source.content_hash,
            &cfg,
            result.event.targeting,
        ) {
            Ok(()) => {}
            Err(crate::ingress_ledger::SourceDeliveryFailure::Unavailable) => {
                attention.unknown("pending source authority is unavailable");
                let mut deferred = Vec::with_capacity(results.len() + 1);
                deferred.push(result);
                deferred.extend(results);
                admissions.defer(deferred);
                return Ok(());
            }
            Err(crate::ingress_ledger::SourceDeliveryFailure::Invalidated) => {
                if let Err(error) = attention
                    .mark_delivery(result.record.id, DeliveryState::Invalidated)
                    .await
                {
                    attention.unknown(error.to_string());
                }
                continue;
            }
        }
        match result.record.actual {
            Admission::RetrievalOnly => {}
            Admission::Prompt | Admission::NextTurn => {
                if !attention.enforcement_supported {
                    attention.unknown("consumer has no safe-turn capability");
                    continue;
                }
                // Retain the original source-bound event until the nonblocking final
                // guard accepts it; temporary store contention returns it to the FIFO.
                let mut message = result.event.clone();
                message.reply_to_content_preview = None;
                let event =
                    prepare_bells(NotificationEvent::Message(message), &cfg, evaluator).await;
                // Keep the managed envelope separate while draining older same-room
                // ordinary work. Direct sink delivery would overtake its buffer.
                let event = match buffer.buffer_event_with_evidence(
                    event,
                    Duration::from_millis(0),
                    cfg.delivery.evidence_markers_enabled,
                ) {
                    BufferResult::Immediate(event) => *event,
                    BufferResult::FlushThenImmediate { preceding, event } => {
                        let outcome = deliver_flushed(
                            sink,
                            buffer.module(),
                            preceding,
                            cfg.tz,
                            cfg.delivery.evidence_markers_enabled,
                        )
                        .await;
                        buffer.requeue(outcome.undelivered);
                        if let Some(error) = outcome.error {
                            return Err(error);
                        }
                        *event
                    }
                    BufferResult::Buffered | BufferResult::Rejected { .. } => {
                        unreachable!("zero-delay events are immediate")
                    }
                };
                let NotificationEvent::Message(managed_message) = event else {
                    return Err(HarnessError::source(std::io::Error::other(
                        "managed attention delivery lost its source message",
                    )));
                };
                let directive = if result.record.actual == Admission::Prompt {
                    crate::attention::delivery::AttentionDirective::Prompt
                } else {
                    crate::attention::delivery::AttentionDirective::NextTurn
                };
                let notification = Delivery::managed(
                    crate::discord::delivery::message(
                        attention.authority_module(),
                        managed_message,
                    ),
                    directive,
                    result.record.id.clone(),
                    presentation(cfg.tz, cfg.delivery.evidence_markers_enabled),
                );
                let Some(descriptor) = notification.attention() else {
                    return Err(HarnessError::source(std::io::Error::other(
                        "managed attention descriptor is unavailable",
                    )));
                };
                // A managed event must remain a source-bound envelope; coalescing
                // it with an ordinary event would erase its lifecycle fence.
                match crate::attention::delivery::AttentionDeliveryGuard::check(
                    attention.as_ref(),
                    &descriptor,
                ) {
                    Ok(()) => sink.deliver(&notification).await?,
                    Err(AttentionGuardFailure::Unavailable) => {
                        attention.unknown("pending source authority is unavailable");
                        let mut deferred = Vec::with_capacity(results.len() + 1);
                        deferred.push(result);
                        deferred.extend(results);
                        admissions.defer(deferred);
                        return Ok(());
                    }
                    Err(AttentionGuardFailure::Invalidated) => {
                        if let Err(error) = attention
                            .mark_delivery(result.record.id, DeliveryState::Invalidated)
                            .await
                        {
                            attention.unknown(error.to_string());
                        }
                    }
                }
            }
            Admission::Ordinary | Admission::Unknown => {
                forward_ordinary(
                    NotificationEvent::Message(result.event),
                    &cfg,
                    buffer,
                    evaluator,
                    sink,
                )
                .await?;
                // Observed means the ordinary queue owns delivery, not that a model
                // or Discord recipient has acknowledged seeing the event.
                if let Err(error) = attention
                    .mark_delivery(result.record.id, DeliveryState::Observed)
                    .await
                {
                    attention.unknown(error.to_string());
                }
            }
        }
    }
    Ok(())
}

async fn attention_transition(
    admissions: &mut crate::attention::admission::AdmissionController,
    attention: &Arc<crate::attention::runtime::AttentionRuntime>,
) -> Vec<crate::attention::admission::AdmissionResult> {
    let _publication = crate::config::attention_effect_guard().await;
    admissions.cancel_work();
    let incarnation = admissions.incarnation().to_owned();
    let recipient = crate::config::load_config(&attention.resolver.state_dir)
        .raw
        .attention
        .recipient
        .clone();
    let committed = attention
        .with_store(move |store| {
            Ok(store
                .list_records(&recipient)
                .into_iter()
                .filter(|record| {
                    record.incarnation == incarnation
                        && record.judgment.is_some()
                        && matches!(
                            record.delivery,
                            DeliveryState::Admitted
                                | DeliveryState::Deferred
                                | DeliveryState::Observed
                        )
                })
                .cloned()
                .collect())
        })
        .await;
    let committed = match committed {
        Ok(records) => records,
        Err(_) => {
            attention.unknown("attention transition metadata unavailable");
            Vec::new()
        }
    };
    admissions.configuration_changed(committed)
}

fn begin_attention(
    admissions: &mut crate::attention::admission::AdmissionController,
    attention: &crate::attention::runtime::AttentionRuntime,
    event: NotificationEvent,
    config: &crate::config::LoadedConfig,
) -> crate::attention::admission::Submission {
    use crate::attention::admission::Submission;
    let duplicate = match &event {
        NotificationEvent::Message(message) => {
            admissions.owns_source(&crate::attention::types::SourceKey {
                channel_id: message.chat_id,
                message_id: message.message_id,
            })
        }
        _ => false,
    };
    let submission = if duplicate {
        Submission::Duplicate
    } else if attention.probe_due() {
        admissions.submit(
            event,
            config,
            crate::attention::source::now_ms(),
            attention.enforcement_supported,
        )
    } else {
        Submission::Ordinary(event)
    };
    match &submission {
        Submission::Unknown { reason, .. } => attention.unknown(*reason),
        Submission::Saturated { .. } => attention.unknown("attention evaluation capacity reached"),
        _ => {}
    }
    submission
}

#[cfg(test)]
#[path = "attention_tests.rs"]
mod attention_tests;

#[cfg(test)]
fn test_connection_for_queue(queue: crate::codex::CodexEventQueue) -> HarnessConnection {
    let (binding, _receiver) = tokio::sync::watch::channel(None);
    HarnessConnection::codex(queue, binding)
}
#[cfg(test)]
pub(super) fn test_codex_connection() -> (tempfile::TempDir, HarnessConnection) {
    let dir = tempfile::TempDir::new().unwrap();
    let path = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let harness = test_connection_for_queue(
        crate::codex::CodexEventQueue::load(
            &path,
            crate::codex::LegacyDiscordScope::new(crate::core::ModuleInstanceId::new(
                crate::core::ProviderId::new("discord").unwrap(),
                crate::core::ModuleSlotId::new("primary").unwrap(),
            )),
        )
        .unwrap(),
    );
    (dir, harness)
}

#[cfg(test)]
#[path = "harness_tests.rs"]
mod harness_tests;

#[cfg(test)]
#[path = "scheduling_tests.rs"]
mod scheduling_tests;

#[cfg(test)]
#[path = "registration_tests.rs"]
mod registration_tests;
