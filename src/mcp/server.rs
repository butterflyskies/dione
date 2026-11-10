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

use crate::mcp::dispatch::call_tool;
use crate::mcp::harness_wire::PRIVATE_CONTROL_TOOLS;
use crate::mcp::harness_wire::RequestOrigin;
use crate::mcp::tools::diagnostics::DiagnosticsCtx;
use crate::mcp::tools::no_rly::NoRlyCtx;
use camino::Utf8PathBuf;
use dione_access_requests::AccessCtx;
use dione_access_requests::AccessQueue;
use dione_contracts::ModuleInstanceId;
use dione_discord_events::NotificationEvent;
use dione_discord_operations::messaging::MessagingCtx;
use dione_discord_presence::PresenceCapability;
use dione_discord_typing::TypingCtx;
use dione_harness::AttentionPolicy;
use dione_harness::HarnessConnection;
use dione_harness::ProtocolOutput;
use dione_harness_runtime::StdioExitPolicy;
use dione_mcp_protocol::initialize_response;
use dione_mcp_protocol::tools_list;
use dione_mcp_tools::RegisteredTool;
use dione_mcp_tools::RegistrationError;
use dione_mcp_tools::ToolNameError;
use dione_mcp_tools::ToolRegistry;
use dione_messages::delivery::EvidenceVisibility;
use dione_messages::delivery::PresentationContext;
use dione_outbound_consent::consent::ConsentGate;
use dione_tracing_channel::TraceLevelController;
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
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
    pub queue: Arc<Mutex<AccessQueue>>,
    pub notification_tx: mpsc::Sender<Value>,
    pub presence: PresenceCapability,
    pub trace_controller: TraceLevelController,
    harness: HarnessConnection,
    pub attention: Arc<dione_recipient_attention::AttentionRuntime>,
    pub(super) module_tools: ToolRegistry,
    discord_tools: Vec<RegisteredTool>,
    discord: Arc<dione_discord_operations::reply_tool::ReplyServices>,
}

pub(crate) fn reserved_tool_names(harness: &HarnessConnection) -> BTreeSet<String> {
    let definitions = tools_list(
        harness.protocol().tools(),
        true,
        dione_evidence::EvidenceMarkerVersion::V3,
    );
    let mut reserved: BTreeSet<String> = dione_codex::TOOL_NAMES
        .iter()
        .map(|name| (*name).to_owned())
        .collect();
    if let Some(tools) = definitions["tools"].as_array() {
        reserved.extend(
            tools
                .iter()
                .filter_map(|tool| tool["name"].as_str())
                .map(str::to_owned),
        );
    }
    reserved.extend(PRIVATE_CONTROL_TOOLS.iter().map(|name| (*name).to_owned()));
    reserved
}

impl DioneServer {
    /// Borrows the Discord HTTP service shared by registered and legacy tools.
    pub fn http(&self) -> &Arc<serenity::http::Http> {
        self.discord.http()
    }
    /// Borrows the shared Discord state.
    pub fn state(&self) -> &dione_discord_state::State {
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
    pub fn ingress_ledger(&self) -> &Arc<dione_discord_runtime::IngressLedger> {
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
        state: dione_discord_state::State,
        queue: Arc<Mutex<AccessQueue>>,
        http: Arc<serenity::http::Http>,
        state_dir: Utf8PathBuf,
        notification_tx: mpsc::Sender<Value>,
        trace_controller: TraceLevelController,
        harness: HarnessConnection,
        no_rly: Arc<ConsentGate>,
        ingress_ledger: Arc<dione_discord_runtime::IngressLedger>,
        authority_module: ModuleInstanceId,
        event_tx: Option<mpsc::Sender<NotificationEvent>>,
    ) -> Result<Self, ServerConstructionError> {
        let attention = Arc::new(
            dione_recipient_attention::AttentionRuntime::new(
                dione_recipient_attention::AttentionServices::new(
                    http.clone(),
                    state.clone(),
                    state_dir.clone(),
                    ingress_ledger.clone(),
                ),
                harness.attention_policy() == AttentionPolicy::SafeTurn,
                authority_module,
            )
            .await,
        );
        let discord = Arc::new(dione_discord_operations::reply_tool::ReplyServices::new(
            http,
            state,
            state_dir,
            no_rly,
            ingress_ledger,
            event_tx,
        ));
        let mut discord_tools =
            dione_discord_operations::introspection_tools::registrations(discord.clone())
                .map_err(ServerConstructionError::definition)?;
        discord_tools.extend(
            dione_discord_operations::query_tools::registrations(
                discord.clone(),
                attention.authority_module().clone(),
            )
            .map_err(ServerConstructionError::definition)?,
        );
        discord_tools.extend(
            dione_discord_operations::management_tools::registrations(discord.clone())
                .map_err(ServerConstructionError::definition)?,
        );
        discord_tools.push(
            dione_discord_operations::reply_tool::registration(discord.clone())
                .map_err(ServerConstructionError::definition)?,
        );
        let module_tools = ToolRegistry::new(&reserved_tool_names(&harness), discord_tools.clone())
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
        self.module_tools = ToolRegistry::new(&reserved_tool_names(&self.harness), tools)?;
        Ok(self)
    }
}

impl DioneServer {
    pub(crate) fn harness(&self) -> &HarnessConnection {
        &self.harness
    }

    pub(crate) fn messaging_ctx(&self, config: Arc<dione_config::LoadedConfig>) -> MessagingCtx {
        self.discord.messaging_ctx(config)
    }

    pub(crate) fn no_rly_ctx(&self, config: Arc<dione_config::LoadedConfig>) -> NoRlyCtx {
        NoRlyCtx {
            gate: self.no_rly().clone(),
            config,
        }
    }

    pub(crate) fn access_ctx(&self, config: Arc<dione_config::LoadedConfig>) -> AccessCtx {
        AccessCtx::new(self.queue.clone(), config, self.state_dir().clone())
    }

    pub(crate) fn typing_ctx(&self, config: Arc<dione_config::LoadedConfig>) -> TypingCtx {
        TypingCtx::new(self.http().clone(), self.state().clone(), config)
    }

    pub(crate) fn diagnostics_ctx(&self) -> DiagnosticsCtx<'_> {
        DiagnosticsCtx {
            trace_controller: &self.trace_controller,
        }
    }
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

    let notif_task = tokio::spawn(dione_recipient_attention::run_notifications(
        server.attention.clone(),
        server.harness.clone(),
        server.no_rly().clone(),
        server.event_tx().clone(),
        event_rx,
        cancel.clone(),
    ));

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
            env!("CARGO_PKG_VERSION"),
            server.harness.protocol().capabilities(),
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
            let config = dione_config::load_config(server.state_dir());
            let mut response = tools_list(
                server.harness.protocol().tools(),
                config.delivery.evidence_markers_enabled,
                config.delivery.evidence_marker_version,
            );
            server.module_tools.append_definitions(
                &mut response,
                dione_mcp_tools::ToolDiscoveryContext {
                    evidence_markers_enabled: config.delivery.evidence_markers_enabled,
                    evidence_marker_version: config.delivery.evidence_marker_version,
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

            let config = dione_config::load_config(server.state_dir());
            if let Err(e) = dione_permissions::send_permission_request(
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

/// Test helpers that expose internal functions through the crate's public API.
pub mod test_helpers {
    use super::*;

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

    /// Converts a test event into the current typed delivery.
    pub fn make_notification(
        event: NotificationEvent,
    ) -> Result<dione_messages::delivery::Delivery, Box<dyn std::error::Error + Send + Sync>> {
        let module = dione_contracts::ModuleInstanceId::new(
            dione_contracts::ProviderId::new("discord")?,
            dione_contracts::ModuleSlotId::new("fixture")?,
        );
        let delivery = dione_discord_events::single(&module, event, presentation(None, true))?;
        Ok(delivery)
    }

    /// Converts a nonempty test batch into the current typed delivery.
    pub fn make_batch(
        events: Vec<NotificationEvent>,
        tz: Option<chrono_tz::Tz>,
    ) -> Result<Option<dione_messages::delivery::Delivery>, Box<dyn std::error::Error + Send + Sync>>
    {
        let mut events = events.into_iter();
        let Some(first) = events.next() else {
            return Ok(None);
        };
        let module = dione_contracts::ModuleInstanceId::new(
            dione_contracts::ProviderId::new("discord")?,
            dione_contracts::ModuleSlotId::new("fixture")?,
        );
        let delivery =
            dione_discord_events::batch(&module, first, events.collect(), presentation(tz, true))?;
        Ok(Some(delivery))
    }

    fn complete_tools_list(harness: &HarnessConnection, evidence: bool) -> Value {
        let mut response = tools_list(
            harness.protocol().tools(),
            evidence,
            dione_evidence::EvidenceMarkerVersion::V3,
        );
        response["tools"].as_array_mut().expect("tools array").push(
            dione_discord_tool_contracts::reply::definition(
                evidence,
                dione_evidence::EvidenceMarkerVersion::V3,
            ),
        );
        response["tools"]
            .as_array_mut()
            .expect("tools array")
            .extend(dione_discord_tool_contracts::metadata::definitions());
        response["tools"]
            .as_array_mut()
            .unwrap()
            .extend(dione_discord_tool_contracts::query::definitions());
        response["tools"]
            .as_array_mut()
            .expect("tools array")
            .extend(dione_discord_tool_contracts::management::definitions());
        response
    }

    /// Exposes `tools_list` for unit testing tool discovery.
    pub fn get_tools_list() -> Value {
        complete_tools_list(
            &dione_claude::connect(
                dione_harness::ProtocolOutput::new(tokio::io::stdout()),
                dione_codex::TOOL_NAMES,
            ),
            false,
        )
    }

    pub fn get_tools_list_with_evidence() -> Value {
        complete_tools_list(
            &dione_claude::connect(
                dione_harness::ProtocolOutput::new(tokio::io::stdout()),
                dione_codex::TOOL_NAMES,
            ),
            true,
        )
    }

    pub fn get_codex_tools_list(harness: &HarnessConnection) -> Value {
        complete_tools_list(harness, false)
    }

    /// Exposes `initialize_response` for unit testing the handshake.
    pub fn get_initialize_response() -> Value {
        initialize_response(
            env!("CARGO_PKG_VERSION"),
            dione_claude::connect(
                dione_harness::ProtocolOutput::new(tokio::io::stdout()),
                dione_codex::TOOL_NAMES,
            )
            .protocol()
            .capabilities(),
            None,
        )
    }

    /// Exposes the Codex initialize response for protocol tests.
    pub fn get_codex_initialize_response(harness: &HarnessConnection) -> Value {
        initialize_response(
            env!("CARGO_PKG_VERSION"),
            harness.protocol().capabilities(),
            None,
        )
    }

    /// Exposes `handle_request` for unit testing request dispatch.
    pub async fn dispatch_request(server: &DioneServer, req: Value) -> Option<Value> {
        handle_request(server, req).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dione_config::{Config, LoadedConfig};
    use dione_provider_delivery::ReplyHandle;

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

        impl dione_teams::runtime::TeamsReplyAuthority for HeldReply {
            fn reply<'a>(
                &'a self,
                handle: &'a ReplyHandle,
                text: &'a str,
            ) -> std::pin::Pin<
                Box<
                    dyn std::future::Future<
                            Output = Result<
                                dione_teams::edge::LiveReplyReceipt,
                                dione_teams::runtime::TeamsRuntimeError,
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
                    Ok(dione_teams::edge::LiveReplyReceipt {
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
            dione_discord_state::new_state(),
            Arc::new(Mutex::new(AccessQueue::load(&state_dir))),
            Arc::new(serenity::http::Http::new("fake")),
            state_dir.clone(),
            notification_tx,
            TraceLevelController::noop(),
            harness,
            Arc::new(dione_outbound_consent::consent::ConsentGate::new(
                &state_dir,
            )),
            Arc::new(dione_discord_runtime::IngressLedger::new()),
            dione_contracts::ModuleInstanceId::new(
                dione_contracts::ProviderId::new("discord").unwrap(),
                dione_contracts::ModuleSlotId::new("primary").unwrap(),
            ),
            None,
        )
        .await
        .expect("Discord reply registration")
        .with_module_tools(dione_teams::mcp_tools(Some(authority)).expect("valid Teams tool"))
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
        let pipeline = dione_pre_send::configured_pipeline(true, Vec::new())
            .expect("configured pipeline")
            .expect("enabled pipeline");
        dione_pre_send::install_pipeline(Some(pipeline));
        let dir = tempfile::TempDir::new().expect("temp dir");
        let state_dir =
            camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf-8 path");
        let (notification_tx, _notification_rx) = mpsc::channel(1);
        let server = DioneServer::new(
            dione_discord_state::new_state(),
            Arc::new(Mutex::new(AccessQueue::load(&state_dir))),
            Arc::new(serenity::http::Http::new("fake")),
            state_dir.clone(),
            notification_tx,
            TraceLevelController::noop(),
            dione_claude::connect(
                dione_harness::ProtocolOutput::new(tokio::io::stdout()),
                dione_codex::TOOL_NAMES,
            ),
            Arc::new(dione_outbound_consent::consent::ConsentGate::new(
                &state_dir,
            )),
            Arc::new(dione_discord_runtime::IngressLedger::new()),
            dione_contracts::ModuleInstanceId::new(
                dione_contracts::ProviderId::new("discord").unwrap(),
                dione_contracts::ModuleSlotId::new("primary").unwrap(),
            ),
            None,
        )
        .await
        .expect("Discord reply registration");

        let context = server.messaging_ctx(Arc::new(
            LoadedConfig::try_from_raw(Config::default()).expect("valid test configuration"),
        ));

        assert!(context.has_pre_send_pipeline());
        dione_pre_send::install_pipeline(None);
    }
}

#[cfg(test)]
fn test_connection_for_queue(queue: dione_codex::CodexEventQueue) -> HarnessConnection {
    let (binding, _receiver) = tokio::sync::watch::channel(None);
    dione_codex::connect(queue, binding)
}
#[cfg(test)]
pub(super) fn test_codex_connection() -> (tempfile::TempDir, HarnessConnection) {
    let dir = tempfile::TempDir::new().unwrap();
    let path = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let harness = test_connection_for_queue(
        dione_codex::CodexEventQueue::load(
            &path,
            dione_codex::LegacyDiscordScope::new(dione_contracts::ModuleInstanceId::new(
                dione_contracts::ProviderId::new("discord").unwrap(),
                dione_contracts::ModuleSlotId::new("primary").unwrap(),
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
#[path = "registration_tests.rs"]
mod registration_tests;
