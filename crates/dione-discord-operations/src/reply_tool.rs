//! Discord-owned reply tool registration, parsing, and publication.
use super::messaging::MessagingCtx;
use super::messaging::ReplyToolOptions;
use super::messaging::reply_with_evidence_and_hook_overrides;
use camino::Utf8PathBuf;
use dione_config::load_config;
use dione_discord_events::NotificationEvent;
use dione_discord_messaging_contracts::wire as messaging_wire;
use dione_discord_runtime::IngressLedger;
use dione_discord_state::State;
use dione_discord_tool_contracts::reply::{DESCRIPTION, ReplyRequest, definition};
use dione_mcp_tools::McpTool;
use dione_mcp_tools::RegisteredTool;
use dione_mcp_tools::ToolCallError;
use dione_mcp_tools::ToolCallErrorKind;
use dione_mcp_tools::ToolDefinition;
use dione_mcp_tools::ToolDiscoveryContext;
use dione_mcp_tools::ToolName;
use dione_mcp_tools::ToolNameError;
use dione_outbound_consent::consent::ConsentGate;
use futures_util::future::BoxFuture;
use serde_json::Value;
use serenity::http::Http;
use std::sync::Arc;
use tokio::sync::mpsc;

/// Concrete shared Discord resources used by the actual registered operations.
pub struct ReplyServices {
    http: Arc<Http>,
    state: State,
    state_dir: Utf8PathBuf,
    no_rly: Arc<ConsentGate>,
    ingress_ledger: Arc<IngressLedger>,
    event_tx: Option<mpsc::Sender<NotificationEvent>>,
}

impl ReplyServices {
    /// Builds a read context from the current validated configuration and shared resources.
    pub fn query_ctx(
        &self,
        config: Arc<dione_config::LoadedConfig>,
    ) -> dione_discord_queries::QueryCtx {
        let construct_id = config.pre_send_construct_id.clone();
        dione_discord_queries::QueryCtx::new(
            self.http.clone(),
            self.state.clone(),
            config,
            self.state_dir.clone(),
            construct_id,
        )
    }
    /// Assembles the existing concrete shared Discord resources without minting authority.
    pub fn new(
        http: Arc<Http>,
        state: State,
        state_dir: Utf8PathBuf,
        no_rly: Arc<ConsentGate>,
        ingress_ledger: Arc<IngressLedger>,
        event_tx: Option<mpsc::Sender<NotificationEvent>>,
    ) -> Self {
        Self {
            http,
            state,
            state_dir,
            no_rly,
            ingress_ledger,
            event_tx,
        }
    }
    /// Captures current configuration and the single installed pipeline for one operation.
    pub fn messaging_ctx(&self, config: Arc<dione_config::LoadedConfig>) -> MessagingCtx {
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
    /// Borrows the shared authenticated Discord HTTP service.
    pub fn http(&self) -> &Arc<Http> {
        &self.http
    }
    /// Borrows the same application state used by gateway and operations.
    pub fn state(&self) -> &State {
        &self.state
    }
    /// Borrows the configured state directory.
    pub fn state_dir(&self) -> &Utf8PathBuf {
        &self.state_dir
    }
    /// Borrows the existing consent gate.
    pub fn no_rly(&self) -> &Arc<ConsentGate> {
        &self.no_rly
    }
    /// Borrows the enforcing ingress facade shared with the gateway.
    pub fn ingress_ledger(&self) -> &Arc<IngressLedger> {
        &self.ingress_ledger
    }
    /// Borrows the optional existing notification sender.
    pub fn event_tx(&self) -> &Option<mpsc::Sender<NotificationEvent>> {
        &self.event_tx
    }
}

pub fn registration(services: Arc<ReplyServices>) -> Result<RegisteredTool, ToolNameError> {
    Ok(RegisteredTool::new(
        ToolDefinition::new(
            ToolName::new("reply")?,
            DESCRIPTION,
            definition(true, dione_evidence::EvidenceMarkerVersion::V3)["inputSchema"].clone(),
        ),
        services,
    )
    .with_resident_access())
}

impl McpTool for ReplyServices {
    fn present_definition(
        &self,
        _definition: &ToolDefinition,
        context: ToolDiscoveryContext,
    ) -> Value {
        definition(
            context.evidence_markers_enabled,
            context.evidence_marker_version,
        )
    }
    fn call<'a>(&'a self, arguments: &'a Value) -> BoxFuture<'a, Result<Value, ToolCallError>> {
        Box::pin(async move {
            let config = load_config(&self.state_dir);
            let request = ReplyRequest::parse(
                arguments,
                config.delivery.evidence_markers_enabled,
                config.delivery.evidence_marker_version,
            )
            .map_err(|message| ToolCallError {
                kind: ToolCallErrorKind::OwnerInput(Box::new(message)),
            })?;
            let ctx = self.messaging_ctx(config);
            Ok(messaging_wire::reply(
                reply_with_evidence_and_hook_overrides(
                    &ctx,
                    request.channel,
                    request.content,
                    request.reply_to,
                    ReplyToolOptions {
                        suppress_ping: request.ping,
                        suppress_embeds: request.previews,
                        no_rly_hooks: &request.hooks,
                        sentex_handles: &request.handles,
                    },
                )
                .await,
            ))
        })
    }
}
