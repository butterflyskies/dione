//! Discord-owned reply tool registration, parsing, and publication.
use super::{
    events::NotificationEvent,
    messaging::{
        LinkPreviews, MessagingCtx, ReplyPing, ReplyToolOptions,
        reply_with_evidence_and_hook_overrides,
    },
    messaging_wire,
    tool_arguments::{
        ArgumentError, ArgumentErrorKind, parse_enabled_sentex_handles, parse_hook_overrides,
        parse_id, parse_optional_bool, parse_optional_id, sentex_handles_schema,
    },
};
use crate::{
    config::load_config,
    evidence::SentexHandles,
    ingress_ledger::IngressLedger,
    mcp::tool_registry::{
        McpTool, RegisteredTool, ToolCallError, ToolCallErrorKind, ToolDefinition,
        ToolDiscoveryContext, ToolName, ToolNameError,
    },
    no_rly::consent::ConsentGate,
    pre_send::HookName,
    state::State,
};
use camino::Utf8PathBuf;
use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use serenity::{
    http::Http,
    model::id::{ChannelId, MessageId},
};
use std::sync::Arc;
use tokio::sync::mpsc;

pub(crate) struct ReplyServices {
    http: Arc<Http>,
    state: State,
    state_dir: Utf8PathBuf,
    no_rly: Arc<ConsentGate>,
    ingress_ledger: Arc<IngressLedger>,
    event_tx: Option<mpsc::Sender<NotificationEvent>>,
}

impl ReplyServices {
    pub(crate) fn new(
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
    pub(crate) fn http(&self) -> &Arc<Http> {
        &self.http
    }
    pub(crate) fn state(&self) -> &State {
        &self.state
    }
    pub(crate) fn state_dir(&self) -> &Utf8PathBuf {
        &self.state_dir
    }
    pub(crate) fn no_rly(&self) -> &Arc<ConsentGate> {
        &self.no_rly
    }
    pub(crate) fn ingress_ledger(&self) -> &Arc<IngressLedger> {
        &self.ingress_ledger
    }
    pub(crate) fn event_tx(&self) -> &Option<mpsc::Sender<NotificationEvent>> {
        &self.event_tx
    }
}

const DESCRIPTION: &str = "Send a reply to a Discord channel or DM. A contradictionary block-tier match does not send: the message is held under a single-use handle and the error names the matched pattern(s) plus the handle. Act on it with no_rly (send verbatim), rephrase (replacement, re-checked), or ignore it to let it expire. An auto-tier match is rewritten to its replacement and sent; the result lists each rewrite under auto_rewrites (`auto: <match> → <replace>`).";

pub(crate) fn definition(evidence_markers_enabled: bool) -> Value {
    let mut definition = crate::mcp::protocol::tool(
        "reply",
        DESCRIPTION,
        json!({
            "type": "object",
            "required": ["channel_id", "content"],
            "properties": {
                "suppress_embeds": { "type": "boolean", "description": "Suppress Discord link previews (default: false)" },
                "channel_id": { "type": "string", "description": "Discord channel ID" },
                "content": { "type": "string", "description": "Message content" },
                "reply_to_message_id": { "type": "string", "description": "Optional message ID to reply to" },
                "suppress_ping": { "type": "boolean", "description": "When true, the reply will not ping the user being replied to (default: false)" },
                "no_rly_hooks": { "type": "array", "items": { "type": "string" }, "description": "Names individual pre-send hooks to bypass for this send. Every bypass is audited. This does not bypass the contradictionary; use the no_rly(handle) tool for that." },
                "claim_handles": sentex_handles_schema("Claims asserted by this message"),
                "citation_handles": sentex_handles_schema("Claims cited as support or context")
            }
        }),
    );
    if !evidence_markers_enabled
        && let Some(properties) = definition["inputSchema"]["properties"].as_object_mut()
    {
        properties.remove("claim_handles");
        properties.remove("citation_handles");
    }
    definition
}

pub(crate) fn registration(services: Arc<ReplyServices>) -> Result<RegisteredTool, ToolNameError> {
    Ok(RegisteredTool::new(
        ToolDefinition::new(
            ToolName::new("reply")?,
            DESCRIPTION,
            definition(true)["inputSchema"].clone(),
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
        self::definition(context.evidence_markers_enabled)
    }
    fn call<'a>(&'a self, arguments: &'a Value) -> BoxFuture<'a, Result<Value, ToolCallError>> {
        Box::pin(async move {
            let config = load_config(&self.state_dir);
            let request = ReplyRequest::parse(arguments, config.delivery.evidence_markers_enabled)
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

struct ReplyRequest<'a> {
    channel: ChannelId,
    content: &'a str,
    reply_to: Option<MessageId>,
    ping: ReplyPing,
    previews: LinkPreviews,
    hooks: Vec<HookName>,
    handles: SentexHandles,
}
impl<'a> ReplyRequest<'a> {
    fn parse(arguments: &'a Value, evidence_enabled: bool) -> Result<Self, ArgumentError> {
        let channel = parse_id(arguments, "channel_id")?.channel();
        let content = arguments
            .get("content")
            .and_then(Value::as_str)
            .ok_or(ArgumentErrorKind::Missing("content"))?;
        let reply_to = parse_optional_id(arguments, "reply_to_message_id")?
            .map(crate::discord::ids::Snowflake::message);
        let ping = ReplyPing::from_suppressed(
            arguments
                .get("suppress_ping")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        );
        let hooks = parse_hook_overrides(arguments)?;
        let handles = parse_enabled_sentex_handles(arguments, evidence_enabled)?;
        let previews = LinkPreviews::from_suppressed(
            parse_optional_bool(arguments, "suppress_embeds")?.unwrap_or(false),
        );
        Ok(Self {
            channel,
            content,
            reply_to,
            ping,
            previews,
            hooks,
            handles,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{evidence::SentexError, pre_send::PipelineError};
    use std::error::Error;

    #[test]
    fn reply_evidence_errors_preserve_typed_parser_cause() {
        let invalid =
            json!({"channel_id":"42", "content":"text", "claim_handles":["18446744073709551616"]});
        let error = ReplyRequest::parse(&invalid, true).err().unwrap();
        assert_eq!(
            error.to_string(),
            "claim_handles must contain canonical positive decimal u64 strings"
        );
        let parser_error = error
            .source()
            .unwrap()
            .source()
            .unwrap()
            .downcast_ref::<SentexError>()
            .expect("original evidence parser cause");
        let mut cause = parser_error.source();
        let mut found_number = false;
        while let Some(source) = cause {
            found_number |= source.is::<std::num::ParseIntError>();
            cause = source.source();
        }
        assert!(
            found_number,
            "out-of-range numeric cause must survive the reply boundary"
        );
        let valid = json!({"channel_id":"42", "content":"text", "claim_handles":["18446744073709551615"], "citation_handles":["1"]});
        let request = ReplyRequest::parse(&valid, true).unwrap();
        assert_eq!(
            crate::evidence::append_markers(request.content, &request.handles),
            "text [🔍=v2:claim:__________8] [🔍=v2:citation:AAAAAAAAAAE]"
        );
    }

    #[test]
    fn reply_argument_errors_retain_causes_and_historical_optional_behavior() {
        let bad = json!({"channel_id":"bad", "content":"text"});
        let error = ReplyRequest::parse(&bad, true).err().unwrap();
        assert_eq!(error.to_string(), "invalid channel_id: not a valid u64");
        assert!(
            error
                .source()
                .unwrap()
                .source()
                .unwrap()
                .is::<std::num::ParseIntError>()
        );
        let bad_hook = json!({"channel_id":"42", "content":"text", "no_rly_hooks":["bad hook"]});
        let hook_error = ReplyRequest::parse(&bad_hook, true).err().unwrap();
        assert_eq!(
            hook_error.to_string(),
            "invalid pre-send hook name `bad hook`"
        );
        let pipeline_error = hook_error
            .source()
            .unwrap()
            .source()
            .expect("invalid hook must retain its PipelineError source")
            .downcast_ref::<PipelineError>()
            .expect("invalid hook source must be PipelineError");
        assert_eq!(
            pipeline_error.to_string(),
            "invalid pre-send hook name `bad hook`"
        );
        let good = json!({"channel_id":"42", "content":"text", "reply_to_message_id":{}, "suppress_ping":"legacy ignored", "suppress_embeds":true, "no_rly_hooks":["valid-hook"]});
        let parsed = ReplyRequest::parse(&good, true).unwrap();
        assert_eq!(parsed.channel, ChannelId::new(42));
        assert_eq!(parsed.content, "text");
        assert_eq!(parsed.reply_to, None);
        assert_eq!(parsed.hooks.len(), 1);
        assert_eq!(parsed.hooks[0].as_str(), "valid-hook");
        let bad_bool = json!({"channel_id":42,"content":"text","suppress_embeds":"true"});
        assert_eq!(
            ReplyRequest::parse(&bad_bool, true)
                .err()
                .unwrap()
                .to_string(),
            "suppress_embeds must be a boolean"
        );
    }
}
