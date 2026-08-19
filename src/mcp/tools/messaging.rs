use std::{sync::Arc, time::Instant};

use camino::Utf8PathBuf;
use serde_json::{Value, json};
use serenity::builder::{CreateAllowedMentions, CreateAttachment, CreateMessage, EditMessage};
use serenity::http::MessagePagination;
use serenity::model::Timestamp;
use serenity::model::channel::Message;
use serenity::model::id::{ChannelId, MessageId, UserId};

use tokio::sync::mpsc;

use crate::config::{ChunkMode, DmPolicy, LoadedConfig};
use crate::contradictionary::{Action, BlockOutcome, DiaryRecord, append_diary_record};
use crate::discord::chunk;
use crate::discord::events::NotificationEvent;
use crate::evidence::{
    EvidenceKeys, EvidenceTransport, append_markers, locator_metadata, parse_evidence_locators,
    project_evidence,
};
use crate::gate::OutboundGate;
use crate::ingress_ledger::IngressLedger;
use crate::no_rly::consent::{
    BounceTicket, ConsentGate, DeliverError, DeliverReply, RejectedHandle, Rephrased, ReplyRequest,
};
use crate::no_rly::judge::{AlwaysClear, OutboundJudge, Verdict};
use crate::no_rly::queue::HoldHandle;
use crate::pre_send::{
    ChannelType as HookChannelType, ConstructId, HookContext, HookDecision, HookName,
    OutboundDestination, PreSendPipeline,
};
use crate::state::State;

/// Self-react emoji for contradictionary celebrate hits (✨ — sparkles).
const CONTRADICTIONARY_CELEBRATE_REACT: &str = "\u{2728}";
static NO_EVIDENCE: EvidenceKeys = EvidenceKeys::empty();

/// Fire-and-forget phantom canary alert to the configured alert channel.
pub(crate) fn phantom_canary_alert(
    http: &Arc<serenity::http::Http>,
    channel: ChannelId,
    message: &str,
) {
    let http = Arc::clone(http);
    let content = message.to_owned();
    tokio::spawn(async move {
        let msg = CreateMessage::new().content(content);
        if let Err(e) = channel.send_message(&http, msg).await {
            tracing::warn!(error = %e, "failed to send phantom canary alert");
        }
    });
}

/// Verify a message target against the ingress ledger before performing a
/// Discord mutation. Returns `Err(json)` with a canary alert on `Unknown`
/// or `ChannelMismatch`, blocking the operation. `Expired` and `Unavailable`
/// log but allow the operation to proceed (legitimate edge cases).
///
/// `operation` names the egress path for tracing and alerts (e.g. "react",
/// "pin_message").
pub(crate) fn verify_message_target(
    ledger: &IngressLedger,
    http: &Arc<serenity::http::Http>,
    config: &LoadedConfig,
    message_id: MessageId,
    channel_id: ChannelId,
    operation: &str,
) -> Result<(), Value> {
    match ledger.verify(message_id, channel_id) {
        crate::ingress_ledger::VerifyResult::Admitted { .. } => Ok(()),
        crate::ingress_ledger::VerifyResult::Unknown => {
            tracing::warn!(
                message_id = message_id.get(),
                channel_id = channel_id.get(),
                operation,
                "egress: message_id not in ingress ledger"
            );
            if let Some(alert_ch) = config.phantom_canary_channel {
                phantom_canary_alert(
                    http,
                    alert_ch,
                    &format!(
                        "⚠️ PHANTOM CANARY: {operation} target message {msg} in channel {ch} not in ingress ledger (Unknown)",
                        msg = message_id.get(),
                        ch = channel_id.get(),
                    ),
                );
            }
            Err(json!({
                "error": format!(
                    "message {} not in ingress ledger — possible phantom. {operation} blocked.",
                    message_id.get()
                )
            }))
        }
        crate::ingress_ledger::VerifyResult::ChannelMismatch {
            admitted_channel,
            claimed_channel,
        } => {
            tracing::warn!(
                message_id = message_id.get(),
                admitted_channel = admitted_channel.get(),
                claimed_channel = claimed_channel.get(),
                operation,
                "egress: message_id channel mismatch"
            );
            if let Some(alert_ch) = config.phantom_canary_channel {
                phantom_canary_alert(
                    http,
                    alert_ch,
                    &format!(
                        "⚠️ PHANTOM CANARY: {operation} target message {msg} channel mismatch — admitted from {admitted}, claimed {claimed} (ChannelMismatch)",
                        msg = message_id.get(),
                        admitted = admitted_channel,
                        claimed = claimed_channel,
                    ),
                );
            }
            Err(json!({
                "error": format!(
                    "message {} channel mismatch — possible phantom. {operation} blocked.",
                    message_id.get()
                )
            }))
        }
        crate::ingress_ledger::VerifyResult::Expired => {
            tracing::info!(
                message_id = message_id.get(),
                channel_id = channel_id.get(),
                operation,
                "egress: message_id expired from ingress ledger"
            );
            Ok(())
        }
        crate::ingress_ledger::VerifyResult::Unavailable => {
            tracing::warn!(
                message_id = message_id.get(),
                operation,
                "egress: ingress ledger unavailable; cannot verify message target"
            );
            Ok(())
        }
    }
}

/// Bounded display name for bot author attribution (Discord caps usernames at 32 chars).
#[derive(Debug, Clone)]
struct BotDisplayName(String);

impl BotDisplayName {
    const MAX_BYTES: usize = 32;

    fn from_discord(name: &str) -> Self {
        let truncated = if name.len() > Self::MAX_BYTES {
            &name[..name.floor_char_boundary(Self::MAX_BYTES)]
        } else {
            name
        };
        Self(truncated.to_owned())
    }

    fn as_str(&self) -> &str {
        &self.0
    }
}

/// Context available to all messaging tools.
pub struct MessagingCtx {
    pub http: Arc<serenity::http::Http>,
    pub state: State,
    pub config: Arc<LoadedConfig>,
    pub state_dir: Utf8PathBuf,
    pre_send_pipeline: Option<Arc<PreSendPipeline>>,
    author_id: Option<UserId>,
    construct_id: ConstructId,
    pub no_rly: Arc<ConsentGate>,
    pub event_tx: Option<mpsc::Sender<NotificationEvent>>,
    pub ingress_ledger: Arc<IngressLedger>,
}

impl MessagingCtx {
    pub fn new(
        http: Arc<serenity::http::Http>,
        state: State,
        config: Arc<LoadedConfig>,
        state_dir: Utf8PathBuf,
        no_rly: Arc<ConsentGate>,
        ingress_ledger: Arc<IngressLedger>,
    ) -> Self {
        let pre_send_pipeline = config
            .pre_send
            .enabled
            .then(crate::pre_send::installed_pipeline)
            .flatten();
        Self {
            author_id: config.pre_send_author_id,
            construct_id: config.pre_send_construct_id.clone(),
            http,
            state,
            config,
            state_dir,
            pre_send_pipeline,
            no_rly,
            event_tx: None,
            ingress_ledger,
        }
    }

    #[cfg(test)]
    fn with_pre_send_pipeline(mut self, pipeline: Arc<PreSendPipeline>) -> Self {
        self.pre_send_pipeline = Some(pipeline);
        self
    }

    #[cfg(test)]
    pub(crate) fn has_pre_send_pipeline(&self) -> bool {
        self.pre_send_pipeline.is_some()
    }
}

// ── Gate helper ───────────────────────────────────────────────────────────────

/// Returns `Ok(())` if the channel is permitted, or an error message if not.
async fn ensure_outbound(ctx: &MessagingCtx, channel_id: ChannelId) -> Result<(), String> {
    let state = ctx.state.read().await;
    if OutboundGate::check_channel_with_threads(
        &ctx.config,
        channel_id.get(),
        &state.dm_channel_ids,
        &state.thread_parents,
    ) {
        Ok(())
    } else {
        Err(format!(
            "channel {channel_id} is not a permitted outbound target"
        ))
    }
}

/// Returns `Ok(())` if the channel is permitted, or `Err(json_error)` if not.
pub(crate) async fn check_outbound(ctx: &MessagingCtx, channel_id: ChannelId) -> Result<(), Value> {
    ensure_outbound(ctx, channel_id)
        .await
        .map_err(|e| json!({ "error": e }))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OutboundSurface {
    Reply,
    EditMessage,
    SendFileCaption,
    RenderLatexCaption,
    SendDm,
    VoiceBeforeTts,
}

pub(crate) fn reject_captionless_hook_overrides(
    caption: Option<&str>,
    no_rly_hooks: &[HookName],
) -> Option<Value> {
    (caption.is_none() && !no_rly_hooks.is_empty()).then(|| {
        json!({
            "error": "no_rly_hooks cannot be used when no caption is sent"
        })
    })
}

impl OutboundSurface {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Reply => "reply",
            Self::EditMessage => "edit-message",
            Self::SendFileCaption => "send-file-caption",
            Self::RenderLatexCaption => "render-latex-caption",
            Self::SendDm => "send-dm",
            Self::VoiceBeforeTts => "voice-before-tts",
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct SurfacePolicy {
    redirect_error: Option<&'static str>,
}

impl SurfacePolicy {
    const fn for_surface(surface: OutboundSurface) -> Self {
        let redirect_error = match surface {
            OutboundSurface::Reply
            | OutboundSurface::SendFileCaption
            | OutboundSurface::RenderLatexCaption
            | OutboundSurface::SendDm => None,
            OutboundSurface::EditMessage => {
                Some("pre-send redirect is not supported for message edits")
            }
            OutboundSurface::VoiceBeforeTts => {
                Some("pre-send redirect is not supported for voice output")
            }
        };
        Self { redirect_error }
    }
}

#[derive(Clone, Copy)]
struct PreSendOptions<'a> {
    surface: OutboundSurface,
    bypasses: &'a [HookName],
}

struct OutboundDraft<'a> {
    destination: OutboundDestination,
    text: &'a str,
    reply_to: Option<MessageId>,
    pre_send: PreSendOptions<'a>,
    evidence_keys: &'a EvidenceKeys,
}

impl<'a> OutboundDraft<'a> {
    fn channel(
        channel_id: ChannelId,
        text: &'a str,
        reply_to: Option<MessageId>,
        pre_send: PreSendOptions<'a>,
    ) -> Self {
        Self {
            destination: OutboundDestination::Channel(channel_id),
            text,
            reply_to,
            pre_send,
            evidence_keys: &NO_EVIDENCE,
        }
    }

    fn dm_recipient(user_id: UserId, text: &'a str, pre_send: PreSendOptions<'a>) -> Self {
        Self {
            destination: OutboundDestination::DmRecipient(user_id),
            text,
            reply_to: None,
            pre_send,
            evidence_keys: &NO_EVIDENCE,
        }
    }

    fn with_evidence(mut self, evidence_keys: &'a EvidenceKeys) -> Self {
        self.evidence_keys = evidence_keys;
        self
    }
}

#[derive(Debug)]
struct PreparedOutbound {
    destination: OutboundDestination,
    text: String,
    reply_to: Option<MessageId>,
    surface: OutboundSurface,
}

impl PreparedOutbound {
    fn resolve_dm_channel(mut self, channel_id: ChannelId) -> Self {
        debug_assert!(matches!(
            self.destination,
            OutboundDestination::DmRecipient(_)
        ));
        self.destination = OutboundDestination::Channel(channel_id);
        self
    }

    fn channel_id(&self) -> Option<ChannelId> {
        match self.destination {
            OutboundDestination::Channel(channel_id) => Some(channel_id),
            OutboundDestination::DmRecipient(_) => None,
        }
    }
}

/// Applies the configured hook pipeline before text reaches Discord.
async fn prepare_outbound(
    ctx: &MessagingCtx,
    draft: OutboundDraft<'_>,
) -> Result<PreparedOutbound, Value> {
    let disabled_evidence = EvidenceKeys::empty();
    let evidence_keys = if ctx.config.delivery.evidence_markers_enabled {
        draft.evidence_keys
    } else {
        &disabled_evidence
    };
    let prepared_pipeline = match ctx.pre_send_pipeline.clone() {
        Some(pipeline) => {
            let no_rly = pipeline
                .no_rly(draft.pre_send.bypasses)
                .map_err(|error| json!({ "error": error.to_string() }))?;
            Some((pipeline, no_rly))
        }
        None if !draft.pre_send.bypasses.is_empty() => {
            return Err(json!({
                "error": "no_rly_hooks named hooks, but no pre-send hooks are registered"
            }));
        }
        None => None,
    };
    if let OutboundDestination::Channel(channel_id) = draft.destination {
        check_outbound(ctx, channel_id).await?;
    }
    let Some((pipeline, no_rly)) = prepared_pipeline else {
        return Ok(PreparedOutbound {
            destination: draft.destination,
            text: append_markers(draft.text, evidence_keys),
            reply_to: draft.reply_to,
            surface: draft.pre_send.surface,
        });
    };

    let channel_type = match draft.destination {
        OutboundDestination::DmRecipient(_) => HookChannelType::DirectMessage,
        OutboundDestination::Channel(channel_id) => {
            let state = ctx.state.read().await;
            if state.dm_channel_ids.contains(&channel_id.get()) {
                HookChannelType::DirectMessage
            } else if state
                .thread_parents
                .get(&channel_id.get())
                .is_some_and(Option::is_some)
            {
                HookChannelType::Thread
            } else {
                HookChannelType::Public
            }
        }
    };
    let mut hook_context = HookContext::new(
        draft.text,
        draft.destination,
        channel_type,
        ctx.construct_id.clone(),
    )
    .with_author_id(ctx.author_id)
    .with_reply_to(draft.reply_to)
    .with_metadata("outbound_surface", draft.pre_send.surface.as_str());
    if !evidence_keys.is_empty() {
        hook_context = hook_context
            .with_metadata("evidence_locators", locator_metadata(evidence_keys))
            .with_metadata(
                "evidence_transport",
                EvidenceTransport::TerminalVisibleSuffixV1AfterHooks.as_str(),
            );
    }
    let outcome =
        match tokio::task::spawn_blocking(move || pipeline.run(&hook_context, &no_rly)).await {
            Ok(Ok(outcome)) => outcome,
            Ok(Err(error)) => return Err(json!({ "error": error.to_string() })),
            Err(error) => {
                return Err(json!({
                    "error": format!("pre-send pipeline task failed: {error}")
                }));
            }
        };

    for failure in outcome.sink_failures() {
        tracing::warn!(
            sink = failure.sink().as_str(),
            error = failure.detail(),
            "pre-send assessment sink degraded"
        );
    }

    let mut destination = draft.destination;
    let mut reply_to = draft.reply_to;
    match outcome.decision() {
        HookDecision::Halt { reason } => {
            let feedback: Vec<&str> = outcome
                .to_construct()
                .as_slice()
                .iter()
                .map(crate::pre_send::Assessment::detail)
                .collect();
            return Err(json!({
                "error": reason,
                "pre_send_feedback": feedback,
            }));
        }
        HookDecision::Redirect { channel_id: target } => {
            if let Some(error) = SurfacePolicy::for_surface(draft.pre_send.surface).redirect_error {
                return Err(json!({ "error": error }));
            }
            let target = *target;
            check_outbound(ctx, target).await?;
            if destination != OutboundDestination::Channel(target) {
                reply_to = None;
            }
            destination = OutboundDestination::Channel(target);
        }
        HookDecision::Continue | HookDecision::Rewrite { .. } => {}
    }

    Ok(PreparedOutbound {
        destination,
        text: append_markers(outcome.final_text().unwrap_or(draft.text), evidence_keys),
        reply_to,
        surface: draft.pre_send.surface,
    })
}

/// Applies the same pre-send seam to text before a voice backend performs TTS.
pub async fn prepare_voice_text(
    ctx: &MessagingCtx,
    channel_id: ChannelId,
    content: &str,
    no_rly_hooks: &[HookName],
) -> Result<String, Value> {
    prepare_outbound(
        ctx,
        OutboundDraft::channel(
            channel_id,
            content,
            None,
            PreSendOptions {
                surface: OutboundSurface::VoiceBeforeTts,
                bypasses: no_rly_hooks,
            },
        ),
    )
    .await
    .map(|prepared| prepared.text)
}

// ── reply ─────────────────────────────────────────────────────────────────────

pub async fn reply(
    ctx: &MessagingCtx,
    channel_id: ChannelId,
    content: &str,
    reply_to_message_id: Option<MessageId>,
    suppress_ping: bool,
) -> Value {
    reply_with_hook_overrides(
        ctx,
        channel_id,
        content,
        reply_to_message_id,
        suppress_ping,
        &[],
    )
    .await
}

pub async fn reply_with_hook_overrides(
    ctx: &MessagingCtx,
    channel_id: ChannelId,
    content: &str,
    reply_to_message_id: Option<MessageId>,
    suppress_ping: bool,
    no_rly_hooks: &[HookName],
) -> Value {
    reply_with_evidence_and_hook_overrides(
        ctx,
        channel_id,
        content,
        reply_to_message_id,
        ReplyToolOptions {
            suppress_ping,
            no_rly_hooks,
            evidence_keys: &NO_EVIDENCE,
        },
    )
    .await
}

pub(crate) struct ReplyToolOptions<'a> {
    pub(crate) suppress_ping: bool,
    pub(crate) no_rly_hooks: &'a [HookName],
    pub(crate) evidence_keys: &'a EvidenceKeys,
}

pub(crate) async fn reply_with_evidence_and_hook_overrides(
    ctx: &MessagingCtx,
    channel_id: ChannelId,
    content: &str,
    reply_to_message_id: Option<MessageId>,
    options: ReplyToolOptions<'_>,
) -> Value {
    if let Some(ref_id) = reply_to_message_id {
        // Warn-only for reply: the reply_to is optional context, not the
        // primary action. The message still sends even if verification fails.
        let _ = verify_message_target(
            &ctx.ingress_ledger,
            &ctx.http,
            &ctx.config,
            ref_id,
            channel_id,
            "reply_to",
        );
    }

    let prepared = match prepare_outbound(
        ctx,
        OutboundDraft::channel(
            channel_id,
            content,
            reply_to_message_id,
            PreSendOptions {
                surface: OutboundSurface::Reply,
                bypasses: options.no_rly_hooks,
            },
        )
        .with_evidence(options.evidence_keys),
    )
    .await
    {
        Ok(prepared) => prepared,
        Err(error) => return error,
    };
    if ctx.config.delivery.evidence_markers_enabled
        && !options.evidence_keys.is_empty()
        && parse_evidence_locators(&prepared.text).is_empty()
    {
        return json!({
            "error": "evidence markers cannot be attached inside quoted or fenced content"
        });
    }
    deliver_prepared_reply(
        ctx,
        prepared,
        ReplyTransportOptions {
            suppress_ping: options.suppress_ping,
        },
    )
    .await
}

struct ReplyTransportOptions {
    suppress_ping: bool,
}

async fn deliver_prepared_reply(
    ctx: &MessagingCtx,
    prepared: PreparedOutbound,
    options: ReplyTransportOptions,
) -> Value {
    debug_assert!(matches!(
        prepared.surface,
        OutboundSurface::Reply | OutboundSurface::SendDm
    ));
    let Some(channel_id) = prepared.channel_id() else {
        return json!({ "error": "prepared outbound destination is not a Discord channel" });
    };
    let reply_to_message_id = prepared.reply_to;
    let content = prepared.text;
    if let Err(error) = validate_evidence_chunking(&ctx.config, &content) {
        return error;
    }

    let request = ReplyRequest {
        channel_id,
        content: content.to_string(),
        reply_to_message_id,
        suppress_ping: options.suppress_ping,
        pending_diary_records: Vec::new(),
    };

    if let Some(ref judge) = ctx.config.contradictionary
        && let Verdict::Bounce(reason) = judge.judge(&content)
    {
        if ctx.config.delivery.evidence_markers_enabled
            && !parse_evidence_locators(&content).is_empty()
        {
            return json!({
                "error": "evidence-bearing messages cannot enter the no_rly hold lifecycle; revise and send a fresh evidence-bearing reply"
            });
        }
        let ticket = ctx
            .no_rly
            .bounce(
                request,
                reason,
                ctx.config.no_rly_hold_ttl(),
                ctx.config.no_rly_max_pending(),
                Instant::now(),
            )
            .await;
        tracing::info!(
            channel = %channel_id,
            handle = %ticket.handle,
            reason = %ticket.reason,
            "contradictionary held outbound message"
        );
        // Record the hold in the durable diary — the gate firing is an
        // evaluation, and every evaluation is recorded.
        let pattern = ticket.reason.to_string();
        append_diary_records(ctx, &[DiaryRecord::held_now(&pattern, &content)]);
        return bounce_json(&ticket);
    }

    match deliver_reply(ctx, &request).await {
        Ok(sent_ids) => {
            let mut response = json!({ "ok": true, "message_ids": sent_ids });
            let locators = parse_evidence_locators(&content);
            if ctx.config.delivery.evidence_markers_enabled && !locators.is_empty() {
                response["evidence_locators"] = json!(
                    locators
                        .iter()
                        .map(crate::evidence::EvidenceLocator::as_str)
                        .collect::<Vec<_>>()
                );
            }
            response
        }
        Err(e) => json!({ "error": e.message }),
    }
}

fn validate_evidence_chunking(config: &LoadedConfig, content: &str) -> Result<(), Value> {
    if !config.delivery.evidence_markers_enabled {
        return Ok(());
    }
    if parse_evidence_locators(content).is_empty() {
        return Ok(());
    }
    let limit = config.delivery.text_chunk_limit;
    let mode = config.delivery.chunk_mode;
    let effective_mode = if limit == 0 {
        ChunkMode::Paragraph
    } else {
        mode
    };
    let effective_limit = if limit == 0 { 2000 } else { limit };
    if chunk(content, effective_limit, effective_mode).len() > 1 {
        return Err(json!({
            "error": "evidence-bearing messages must fit in one Discord message"
        }));
    }
    Ok(())
}

/// The construct-facing shape of a bounce: the error names the reason, and
/// the `held` block carries the handle plus the three verbs.
fn bounce_json(ticket: &BounceTicket) -> Value {
    let mut held = json!({
        "handle": ticket.handle,
        // Canonical structured reason: the same `{ "matches": [...] }` shape
        // the journal serializes, so a client sees one reason shape everywhere
        // rather than a flat array here and a nested object in the audit log.
        "reason": ticket.reason,
        "expires_in_secs": ticket.expires_in.as_secs(),
        "next": "no_rly(handle) sends it verbatim; rephrase(handle, content) sends a replacement (re-checked); ignoring it lets it expire",
    });
    if let Some(ref parent) = ticket.parent {
        held["chained_from"] = json!(parent);
    }
    json!({
        "error": format!("\u{26a0}\u{fe0f} held by contradictionary: {}", ticket.reason),
        "held": held,
    })
}

fn append_diary_records(ctx: &MessagingCtx, records: &[DiaryRecord]) {
    for record in records {
        if let Err(e) = append_diary_record(ctx.state_dir.as_std_path(), record) {
            tracing::warn!(
                error = %e,
                action = ?record.action,
                "failed to append contradictionary record to diary"
            );
        }
    }
}

/// Send one [`ReplyRequest`] to Discord: typing indicator, chunking, reply
/// threading, and post-send celebrate self-reacts. This is the path
/// shared by judged sends and by release/rephrase — the outbound channel
/// gate is re-checked here so a held message cannot outlive a config change
/// that revoked its channel.
async fn deliver_reply(
    ctx: &MessagingCtx,
    request: &ReplyRequest,
) -> Result<Vec<u64>, DeliverError> {
    ensure_outbound(ctx, request.channel_id)
        .await
        .map_err(DeliverError::total)?;

    let ch = request.channel_id;
    let content = request.content.as_str();

    // Seam cost: a clean send scans the contradictionary twice — once as the
    // judge in `reply` (for block-tier bounces) and again here for the
    // celebrate self-reacts. The two are deliberately separate concerns
    // (the judge gates delivery; these reactions ride along after it), and a
    // bounce path never reaches here, so the second scan only runs on sends
    // that were already going out. Block hits are irrelevant here (the judge
    // already ruled); log/celebrate ride along on every path out.
    let contradictionary_hits = ctx
        .config
        .contradictionary
        .as_ref()
        .map(|c| c.check(content))
        .unwrap_or_default();

    let pending_diary_records = if !request.pending_diary_records.is_empty() {
        request.pending_diary_records.clone()
    } else if let Some(ref contradictionary) = ctx.config.contradictionary {
        // Hold send-side records until every chunk succeeds. Quiet tiers
        // describe published text, and an override is only a crossing after
        // publication. A partial delivery is intentionally not represented as
        // a successful full-message record; a retry evaluates its remainder
        // independently.
        match contradictionary.evaluate_block(&contradictionary_hits, content, true) {
            BlockOutcome::Clear => Vec::new(),
            BlockOutcome::Overridden(records) | BlockOutcome::Recorded(records) => records,
            BlockOutcome::Rejected { .. } => {
                // Should not happen in deliver_reply — the judge already cleared.
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };

    if !contradictionary_hits.is_empty() {
        let patterns: Vec<&str> = contradictionary_hits
            .iter()
            .map(|h| h.pattern.as_str())
            .collect();
        tracing::info!(
            channel = %ch,
            patterns = ?patterns,
            "contradictionary flagged outbound message"
        );
    }

    // Fire typing indicator now that we've committed to sending a reply.
    let _ = ctx.http.broadcast_typing(ch).await;

    let limit = ctx.config.delivery.text_chunk_limit;
    let mode = ctx.config.delivery.chunk_mode;
    let reply_mode = ctx.config.delivery.reply_to_mode;

    // Determine chunk mode default.
    let effective_mode = if limit == 0 {
        ChunkMode::Paragraph
    } else {
        mode
    };
    let effective_limit = if limit == 0 { 2000 } else { limit };

    let chunks = chunk(content, effective_limit, effective_mode);
    let mut sent_ids: Vec<u64> = Vec::new();
    let mut first_msg_id: Option<MessageId> = None;
    let mut bot_author: Option<(UserId, BotDisplayName)> = None;

    for (i, chunk_text) in chunks.iter().enumerate() {
        let mut builder = CreateMessage::new().content(*chunk_text);

        // Reply threading.
        let should_reply = match reply_mode {
            crate::config::ReplyToMode::Off => false,
            crate::config::ReplyToMode::First => i == 0,
            crate::config::ReplyToMode::All => true,
        };

        if should_reply {
            if i == 0 {
                if let Some(mid) = request.reply_to_message_id {
                    builder = builder.reference_message((ch, mid));
                }
            } else if let Some(prev_id) = first_msg_id {
                builder = builder.reference_message((ch, prev_id));
            }
        }

        if request.suppress_ping {
            builder = builder.allowed_mentions(CreateAllowedMentions::new().replied_user(false));
        }

        match ch.send_message(&ctx.http, builder).await {
            Ok(msg) => {
                let mid = msg.id.get();
                sent_ids.push(mid);
                if i == 0 {
                    first_msg_id = Some(msg.id);
                    bot_author = Some((
                        msg.author.id,
                        BotDisplayName::from_discord(&msg.author.name),
                    ));
                }
                // Record sent IDs in state.
                let mut state = ctx.state.write().await;
                state.note_sent(mid);
            }
            Err(e) => {
                tracing::warn!(channel_id = ch.get(), chunk = i, error = %e, "failed to send chunk");
                // Report partial progress: the chunks already posted (so a
                // retry does not double-post them) and the undelivered
                // remainder (so a retry resumes from there). When nothing has
                // landed yet the remainder is left `None` — a retry re-sends
                // the whole payload.
                let undelivered = if sent_ids.is_empty() {
                    None
                } else {
                    Some(chunks[i..].join("\n\n"))
                };
                let preserve_diary_records =
                    !sent_ids.is_empty() || !request.pending_diary_records.is_empty();
                return Err(DeliverError {
                    message: format!("failed to send chunk {i}: {e}"),
                    sent_ids,
                    undelivered,
                    diary_records: if preserve_diary_records {
                        pending_diary_records
                    } else {
                        Vec::new()
                    },
                });
            }
        }
    }

    if !pending_diary_records.is_empty() {
        if let Some(first) = pending_diary_records
            .iter()
            .find(|record| record.action == Action::Block && record.overridden)
        {
            tracing::info!(
                channel = %ch,
                pattern = %first.pattern,
                "contradictionary block override delivered"
            );
        }
        append_diary_records(ctx, &pending_diary_records);
    }

    // ── Contradictionary post-send: self-react on celebrate hits ───
    if !contradictionary_hits.is_empty()
        && let Some(&first_id) = sent_ids.first()
        && let Some((author_id, author_name)) = bot_author
    {
        let has_celebrates = contradictionary_hits
            .iter()
            .any(|h| h.action == Action::Celebrate);
        if has_celebrates {
            let patterns: Vec<&str> = contradictionary_hits
                .iter()
                .filter(|h| h.action == Action::Celebrate)
                .map(|h| h.pattern.as_str())
                .collect();
            tracing::info!(
                channel = %ch,
                patterns = ?patterns,
                "contradictionary celebrated outbound vocabulary"
            );
            self_react_and_notify(
                ctx,
                ch,
                MessageId::new(first_id),
                author_id,
                author_name.as_str(),
                CONTRADICTIONARY_CELEBRATE_REACT,
            )
            .await;
        }
    }

    Ok(sent_ids)
}

impl DeliverReply for MessagingCtx {
    async fn deliver(&self, request: &ReplyRequest) -> Result<Vec<u64>, DeliverError> {
        deliver_reply(self, request).await
    }
}

// ── no_rly (release) and rephrase ────────────────────────────────────────────

/// The `no_rly` tool: release a held message, sending the byte-identical
/// queued text. The handle dies on success; a failed send leaves it live
/// until expiry.
pub async fn release_held(ctx: &MessagingCtx, handle: &str) -> Value {
    let handle = HoldHandle::new(handle);
    match ctx.no_rly.release(ctx, &handle, Instant::now()).await {
        Ok(released) => {
            tracing::info!(
                handle = %handle,
                latency_ms = released.latency_ms,
                "no_rly released held message"
            );
            json!({
                "ok": true,
                "message_ids": released.message_ids,
                "released": handle,
                "latency_ms": released.latency_ms,
            })
        }
        Err(e) => rejected_handle_json(e),
    }
}

/// The `rephrase` tool: replace a held message's text. The replacement is
/// re-judged — a clean verdict sends it (and journals the original, reason,
/// and replacement as a triple); a re-bounce mints a new handle chained to
/// the dead one.
pub async fn rephrase_held(ctx: &MessagingCtx, handle: &str, content: &str) -> Value {
    // Reject an empty/whitespace replacement up front with a clear message —
    // it would otherwise pass the judge and 400 at Discord with a confusing
    // "cannot send an empty message" error, stranding the handle.
    if content.trim().is_empty() {
        return json!({ "error": "rephrase content must not be empty" });
    }
    if ctx.config.delivery.evidence_markers_enabled && !parse_evidence_locators(content).is_empty()
    {
        return json!({
            "error": "evidence-bearing replacements are not supported by rephrase; send a fresh evidence-bearing reply"
        });
    }
    let handle = HoldHandle::new(handle);
    let ttl = ctx.config.no_rly_hold_ttl();
    let now = Instant::now();

    // A config reload can disable the contradictionary while a handle is in
    // flight; the replacement then goes out unjudged rather than stranding.
    let judge: &dyn OutboundJudge = match ctx.config.contradictionary {
        Some(ref judge) => judge.as_ref(),
        None => &AlwaysClear,
    };
    let result = ctx
        .no_rly
        .rephrase(ctx, judge, &handle, content, ttl, now)
        .await;

    match result {
        Ok(Rephrased::Sent { message_ids }) => {
            tracing::info!(handle = %handle, "rephrase sent replacement for held message");
            json!({
                "ok": true,
                "message_ids": message_ids,
                "rephrased": handle,
            })
        }
        Ok(Rephrased::ReBounced(ticket)) => {
            tracing::info!(
                old_handle = %handle,
                new_handle = %ticket.handle,
                reason = %ticket.reason,
                "rephrase bounced again; chained new handle"
            );
            append_diary_records(
                ctx,
                &[DiaryRecord::held_now(&ticket.reason.to_string(), content)],
            );
            bounce_json(&ticket)
        }
        Err(e) => rejected_handle_json(e),
    }
}

/// Map a handle rejection to the construct-facing error shape. A still-live
/// handle (failed send) says so explicitly, so the construct knows a retry
/// is possible.
fn rejected_handle_json(error: RejectedHandle) -> Value {
    match error {
        RejectedHandle::SendFailed {
            ref handle,
            ref expires_in,
            ..
        } => json!({
            "error": error.to_string(),
            "handle_still_live": handle,
            "expires_in_secs": expires_in.as_secs(),
        }),
        RejectedHandle::Unknown(_) | RejectedHandle::Expired { .. } => {
            json!({ "error": error.to_string() })
        }
    }
}

/// Self-reacts to a just-sent message with `emoji` and emits the matching
/// `Reaction { self_react: true }` notification so the construct sees it.
///
/// The gateway `reaction_add` handler drops bot self-reactions to prevent
/// feedback loops, so intentional contradictionary self-reacts must be
/// surfaced here, at the point where they are initiated. The notification is
/// deliberately not gated on the `create_reaction` result: a transient
/// Discord failure shouldn't also drop the reinforcement signal. Failures on
/// either side are logged so a broken loop stays diagnosable.
async fn self_react_and_notify(
    ctx: &MessagingCtx,
    channel_id: ChannelId,
    message_id: MessageId,
    author_id: UserId,
    author_name: &str,
    emoji: &'static str,
) {
    let reaction = serenity::model::channel::ReactionType::Unicode(emoji.into());
    if let Err(e) = ctx
        .http
        .create_reaction(channel_id, message_id, &reaction)
        .await
    {
        tracing::warn!(
            channel_id = channel_id.get(),
            message_id = message_id.get(),
            error = %e,
            "contradictionary self-react failed"
        );
    }
    if let Some(ref tx) = ctx.event_tx {
        let event = NotificationEvent::Reaction {
            chat_id: channel_id,
            message_id,
            user: author_name.to_owned(),
            user_id: author_id,
            emoji: emoji.to_owned(),
            self_react: true,
        };
        if let Err(e) = tx.send(event).await {
            tracing::warn!(
                channel_id = channel_id.get(),
                message_id = message_id.get(),
                error = %e,
                "failed to emit contradictionary self-react notification"
            );
        }
    }
}

// ── react ─────────────────────────────────────────────────────────────────────

pub async fn react(
    ctx: &MessagingCtx,
    channel_id: ChannelId,
    message_id: MessageId,
    emoji: &str,
) -> Value {
    if let Err(e) = check_outbound(ctx, channel_id).await {
        return e;
    }

    if let Err(e) = verify_message_target(
        &ctx.ingress_ledger,
        &ctx.http,
        &ctx.config,
        message_id,
        channel_id,
        "react",
    ) {
        return e;
    }

    let reaction = match parse_reaction_type(emoji) {
        Ok(r) => r,
        Err(e) => return json!({ "error": e }),
    };
    match ctx
        .http
        .create_reaction(channel_id, message_id, &reaction)
        .await
    {
        Ok(()) => json!({ "ok": true }),
        Err(e) => json!({ "error": e.to_string() }),
    }
}

// ── edit_message ──────────────────────────────────────────────────────────────

pub async fn edit_message(
    ctx: &MessagingCtx,
    channel_id: ChannelId,
    message_id: MessageId,
    new_content: &str,
) -> Value {
    edit_message_with_hook_overrides(ctx, channel_id, message_id, new_content, &[]).await
}

pub async fn edit_message_with_hook_overrides(
    ctx: &MessagingCtx,
    channel_id: ChannelId,
    message_id: MessageId,
    new_content: &str,
    no_rly_hooks: &[HookName],
) -> Value {
    let prepared = match prepare_outbound(
        ctx,
        OutboundDraft::channel(
            channel_id,
            new_content,
            Some(message_id),
            PreSendOptions {
                surface: OutboundSurface::EditMessage,
                bypasses: no_rly_hooks,
            },
        ),
    )
    .await
    {
        Ok(prepared) => prepared,
        Err(error) => return error,
    };
    let builder = EditMessage::new().content(prepared.text);
    match ctx
        .http
        .edit_message(channel_id, message_id, &builder, vec![])
        .await
    {
        Ok(msg) => json!({ "ok": true, "message_id": msg.id.get().to_string() }),
        Err(e) => json!({ "error": e.to_string() }),
    }
}

// ── fetch_messages ────────────────────────────────────────────────────────────

fn resolve_pagination(
    before: Option<MessageId>,
    after: Option<MessageId>,
) -> Result<Option<MessagePagination>, Value> {
    match (before, after) {
        (Some(_), Some(_)) => Err(json!({ "error": "cannot specify both 'before' and 'after'" })),
        (Some(id), None) => Ok(Some(MessagePagination::Before(id))),
        (None, Some(id)) => Ok(Some(MessagePagination::After(id))),
        (None, None) => Ok(None),
    }
}

fn build_fetch_response(
    config: &LoadedConfig,
    mut messages: Vec<Message>,
    paginating: bool,
    limit: u8,
) -> Value {
    messages.sort_unstable_by_key(|m| m.id);
    let count = messages.len();
    let msgs: Vec<Value> = messages.iter().map(|m| message_json(config, m)).collect();
    let mut result = json!({ "messages": msgs });
    if paginating {
        result["count"] = json!(count);
        result["has_more"] = json!(limit > 0 && count == usize::from(limit));
    }
    result
}

pub async fn fetch_messages(
    ctx: &MessagingCtx,
    channel_id: ChannelId,
    before: Option<MessageId>,
    after: Option<MessageId>,
    limit: u8,
) -> Value {
    if let Err(e) = check_outbound(ctx, channel_id).await {
        return e;
    }

    let pagination = match resolve_pagination(before, after) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let paginating = before.is_some() || after.is_some();

    match ctx
        .http
        .get_messages(channel_id, pagination, Some(limit))
        .await
    {
        Ok(messages) => build_fetch_response(&ctx.config, messages, paginating, limit),
        Err(e) => json!({ "error": e.to_string() }),
    }
}

/// Fetches every pinned message visible in a permitted channel.
pub async fn fetch_pins(ctx: &MessagingCtx, channel_id: ChannelId) -> Value {
    if let Err(e) = check_outbound(ctx, channel_id).await {
        return e;
    }

    match ctx.http.get_pins(channel_id).await {
        Ok(messages) => build_pins_response(&ctx.config, messages),
        Err(e) => json!({ "error": e.to_string() }),
    }
}

fn build_pins_response(config: &LoadedConfig, mut messages: Vec<Message>) -> Value {
    messages.sort_unstable_by_key(|message| message.id);
    let count = messages.len();
    let messages = messages
        .iter()
        .map(|message| message_json(config, message))
        .collect::<Vec<_>>();
    json!({
        "messages": messages,
        "count": count,
    })
}

/// Serializes one message into the wire shape shared by `fetch_messages`,
/// `fetch_new_since`, and `search_messages`, so the tools cannot drift apart.
pub(crate) fn message_json(config: &LoadedConfig, m: &Message) -> Value {
    let mut message = json!({
        "id": m.id.get().to_string(),
        "author": m.author.name,
        "author_id": m.author.id.get().to_string(),
        "content": m.content,
        "timestamp": config.localize_rfc3339(&serenity_ts_to_rfc3339(&m.timestamp)),
        "attachments": m.attachments.iter().map(|a| json!({
            "name": a.filename,
            "url": a.url,
            "size": a.size,
        })).collect::<Vec<_>>(),
    });
    if config.delivery.evidence_markers_enabled {
        project_evidence(&mut message, &m.content, m.author.id);
    }
    message
}

// ── fetch_new_since ───────────────────────────────────────────────────────────

pub async fn fetch_new_since(
    ctx: &MessagingCtx,
    channel_id: ChannelId,
    after_message_id: MessageId,
    limit: u8,
) -> Value {
    if let Err(e) = check_outbound(ctx, channel_id).await {
        return e;
    }

    match ctx
        .http
        .get_messages(
            channel_id,
            Some(MessagePagination::After(after_message_id)),
            Some(limit),
        )
        .await
    {
        Ok(messages) => new_since_response(&ctx.config, messages, limit),
        Err(e) => json!({ "error": e.to_string() }),
    }
}

/// Assembles the `fetch_new_since` response: messages sorted oldest-first,
/// plus `count` and a `has_more` pagination hint.
///
/// Discord returns messages newest-first on the wire. The caller owns the
/// pagination cursor (the `id` of the last returned message), so chronological
/// order is part of the tool's contract rather than an accident of the wire
/// format.
fn new_since_response(config: &LoadedConfig, mut messages: Vec<Message>, limit: u8) -> Value {
    messages.sort_unstable_by_key(|m| m.id);
    let count = messages.len();
    let msgs: Vec<Value> = messages.iter().map(|m| message_json(config, m)).collect();
    json!({
        "messages": msgs,
        "count": count,
        // `limit > 0` guards against an empty page claiming more data:
        // dispatch clamps limit to 1..=100, but this function must not
        // produce `{count: 0, has_more: true}` even if that ever regresses.
        "has_more": limit > 0 && count == usize::from(limit),
    })
}

// ── download_attachment ───────────────────────────────────────────────────────

pub async fn download_attachment(
    ctx: &MessagingCtx,
    channel_id: ChannelId,
    message_id: MessageId,
) -> Value {
    if let Err(e) = check_outbound(ctx, channel_id).await {
        return e;
    }

    // Fetch the message to get attachment URLs.
    let msg = match ctx.http.get_message(channel_id, message_id).await {
        Ok(m) => m,
        Err(e) => return json!({ "error": format!("failed to fetch message: {e}") }),
    };

    if msg.attachments.is_empty() {
        return json!({ "error": "message has no attachments" });
    }

    // Ensure inbox directory exists.
    let inbox_dir = ctx.state_dir.join("inbox");
    if let Err(e) = tokio::fs::create_dir_all(&inbox_dir).await {
        return json!({ "error": format!("failed to create inbox: {e}") });
    }

    let mut saved_paths: Vec<String> = Vec::new();

    for (idx, attachment) in msg.attachments.iter().enumerate() {
        let safe_name = crate::gate::sanitize_filename(&attachment.filename);
        let dest = {
            let candidate = inbox_dir.join(&safe_name);
            if candidate.exists() {
                inbox_dir.join(format!("{idx}-{safe_name}"))
            } else {
                candidate
            }
        };

        // Download attachment bytes.
        match download_url(&attachment.url).await {
            Ok(bytes) => {
                if let Err(e) = tokio::fs::write(&dest, &bytes).await {
                    tracing::warn!(
                        name = %safe_name,
                        error = %e,
                        "failed to write attachment to inbox"
                    );
                } else {
                    saved_paths.push(dest.to_string());
                }
            }
            Err(e) => {
                tracing::warn!(url = %attachment.url, error = %e, "failed to download attachment");
            }
        }
    }

    json!({ "saved": saved_paths })
}

// ── send_attachment (shared helper) ──────────────────────────────────────────

pub(crate) async fn send_attachment_with_hook_overrides(
    ctx: &MessagingCtx,
    channel_id: ChannelId,
    attachment: CreateAttachment,
    caption: Option<&str>,
    no_rly_hooks: &[HookName],
    surface: OutboundSurface,
) -> Value {
    if let Some(error) = reject_captionless_hook_overrides(caption, no_rly_hooks) {
        return error;
    }
    let prepared = match caption {
        Some(caption) => match prepare_outbound(
            ctx,
            OutboundDraft::channel(
                channel_id,
                caption,
                None,
                PreSendOptions {
                    surface,
                    bypasses: no_rly_hooks,
                },
            ),
        )
        .await
        {
            Ok(prepared) => Some(prepared),
            Err(error) => return error,
        },
        None => None,
    };
    let channel_id = prepared
        .as_ref()
        .and_then(PreparedOutbound::channel_id)
        .unwrap_or(channel_id);
    let _ = ctx.http.broadcast_typing(channel_id).await;
    let mut builder = CreateMessage::new().add_file(attachment);
    if let Some(prepared) = prepared {
        builder = builder.content(prepared.text);
    }
    match channel_id.send_message(&ctx.http, builder).await {
        Ok(msg) => {
            let mid = msg.id.get();
            let mut state = ctx.state.write().await;
            state.note_sent(mid);
            json!({ "ok": true, "message_id": mid.to_string() })
        }
        Err(e) => json!({ "error": format!("failed to send: {e}") }),
    }
}

// ── send_file ────────────────────────────────────────────────────────────────

pub async fn send_file(
    ctx: &MessagingCtx,
    channel_id: ChannelId,
    file_path: &str,
    caption: Option<&str>,
) -> Value {
    send_file_with_hook_overrides(ctx, channel_id, file_path, caption, &[]).await
}

pub async fn send_file_with_hook_overrides(
    ctx: &MessagingCtx,
    channel_id: ChannelId,
    file_path: &str,
    caption: Option<&str>,
    no_rly_hooks: &[HookName],
) -> Value {
    if let Some(error) = reject_captionless_hook_overrides(caption, no_rly_hooks) {
        return error;
    }
    let path = std::path::Path::new(file_path);
    if !path.is_absolute() {
        return json!({ "error": "file_path must be absolute" });
    }

    if let Err(e) = check_outbound(ctx, channel_id).await {
        return e;
    }

    let utf8_path = camino::Utf8Path::new(file_path);
    if !OutboundGate::check_file_send(utf8_path, &ctx.state_dir) {
        return json!({ "error": "file_path is not permitted for upload" });
    }

    let attachment = match CreateAttachment::path(path).await {
        Ok(a) => a,
        Err(e) => return json!({ "error": format!("failed to read file: {e}") }),
    };

    send_attachment_with_hook_overrides(
        ctx,
        channel_id,
        attachment,
        caption,
        no_rly_hooks,
        OutboundSurface::SendFileCaption,
    )
    .await
}

// ── DM helpers ───────────────────────────────────────────────────────────────

pub(crate) async fn create_dm_channel(
    http: &serenity::http::Http,
    user_id: UserId,
) -> Result<serenity::model::channel::PrivateChannel, String> {
    let dm_body = json!({ "recipient_id": user_id.get().to_string() });
    http.create_private_channel(&dm_body)
        .await
        .map_err(|e| format!("failed to create DM channel: {e}"))
}

// ── send_dm ──────────────────────────────────────────────────────────────────

/// Open (or reuse) a DM channel and send through the shared [`reply`] path.
///
/// DMs are judged like every other outbound message — this is deliberate and
/// carries over from v1, where the contradictionary block check also ran on
/// the shared reply path. The judge polices the construct's own speech, not
/// the audience, so a block-tier tic bounces in a DM exactly as it would in
/// a channel: held under a handle, releasable, rephrasable.
pub async fn send_dm(ctx: &MessagingCtx, user_id: UserId, content: &str) -> Value {
    send_dm_with_hook_overrides(ctx, user_id, content, &[]).await
}

pub async fn send_dm_with_hook_overrides(
    ctx: &MessagingCtx,
    user_id: UserId,
    content: &str,
    no_rly_hooks: &[HookName],
) -> Value {
    send_dm_with_evidence_and_hook_overrides(ctx, user_id, content, no_rly_hooks, &NO_EVIDENCE)
        .await
}

pub(crate) async fn send_dm_with_evidence_and_hook_overrides(
    ctx: &MessagingCtx,
    user_id: UserId,
    content: &str,
    no_rly_hooks: &[HookName],
    evidence_keys: &EvidenceKeys,
) -> Value {
    if ctx.config.access.dm_policy == DmPolicy::Disabled {
        return json!({ "error": "dm_policy is set to disabled; cannot initiate DMs" });
    }

    let prepared = match prepare_outbound(
        ctx,
        OutboundDraft::dm_recipient(
            user_id,
            content,
            PreSendOptions {
                surface: OutboundSurface::SendDm,
                bypasses: no_rly_hooks,
            },
        )
        .with_evidence(evidence_keys),
    )
    .await
    {
        Ok(prepared) => prepared,
        Err(error) => return error,
    };

    if ctx.config.delivery.evidence_markers_enabled
        && !evidence_keys.is_empty()
        && parse_evidence_locators(&prepared.text).is_empty()
    {
        return json!({
            "error": "evidence markers cannot be attached inside quoted or fenced content"
        });
    }
    if let Err(error) = validate_evidence_chunking(&ctx.config, &prepared.text) {
        return error;
    }

    if prepared.channel_id().is_some() {
        return deliver_prepared_reply(
            ctx,
            prepared,
            ReplyTransportOptions {
                suppress_ping: false,
            },
        )
        .await;
    }

    let channel = match create_dm_channel(&ctx.http, user_id).await {
        Ok(c) => c,
        Err(e) => return json!({ "error": e }),
    };

    let channel_id = channel.id;

    {
        let mut state = ctx.state.write().await;
        state.record_dm_channel(user_id.get(), channel_id.get());
    }

    let result = deliver_prepared_reply(
        ctx,
        prepared.resolve_dm_channel(channel_id),
        ReplyTransportOptions {
            suppress_ping: false,
        },
    )
    .await;

    if result.get("error").is_some() {
        return result;
    }

    add_dm_channel_receipt(result, channel_id)
}

fn add_dm_channel_receipt(mut result: Value, channel_id: ChannelId) -> Value {
    if let Value::Object(fields) = &mut result {
        fields.insert(
            "channel_id".to_string(),
            Value::String(channel_id.get().to_string()),
        );
    }
    result
}

// ── Helpers ──────────────────────────────────────────────────────────────────

/// Parses an emoji string into the appropriate serenity ReactionType.
/// Handles both Unicode emoji ("👍") and custom Discord emoji ("<:name:id>" or "<a:name:id>").
///
/// Returns an error for custom emoji with a zero ID: snowflakes are nonzero,
/// and serenity's `EmojiId::new` (NonZeroU64-backed) panics on 0.
fn parse_reaction_type(emoji: &str) -> Result<serenity::model::channel::ReactionType, String> {
    use serenity::model::{channel::ReactionType, id::EmojiId};

    // Custom emoji: <:name:id> or <a:name:id>
    let trimmed = emoji.trim();
    if trimmed.starts_with('<') && trimmed.ends_with('>') {
        let inner = &trimmed[1..trimmed.len() - 1];
        let parts: Vec<&str> = inner.split(':').collect();
        if parts.len() == 3 {
            let animated = parts[0] == "a";
            let name = parts[1].to_string();
            if let Ok(id) = parts[2].parse::<u64>() {
                if id == 0 {
                    return Err(format!(
                        "invalid custom emoji {trimmed:?}: emoji ID must be nonzero"
                    ));
                }
                return Ok(ReactionType::Custom {
                    animated,
                    id: EmojiId::new(id),
                    name: Some(name),
                });
            }
        }
    }

    Ok(ReactionType::Unicode(emoji.to_string()))
}

const MAX_ATTACHMENT_BYTES: u64 = 25 * 1024 * 1024;

async fn download_url(url: &str) -> Result<Vec<u8>, Box<dyn std::error::Error + Send + Sync>> {
    let resp = reqwest::get(url).await?.error_for_status()?;

    if let Some(len) = resp.content_length()
        && len > MAX_ATTACHMENT_BYTES
    {
        return Err(
            format!("attachment too large: {len} bytes (max {MAX_ATTACHMENT_BYTES})").into(),
        );
    }

    let bytes = resp.bytes().await?;
    if bytes.len() as u64 > MAX_ATTACHMENT_BYTES {
        return Err(format!(
            "attachment too large: {} bytes (max {MAX_ATTACHMENT_BYTES})",
            bytes.len()
        )
        .into());
    }

    Ok(bytes.to_vec())
}

// ── get_message ───────────────────────────────────────────────────────────────

pub async fn get_message(
    ctx: &MessagingCtx,
    channel_id: ChannelId,
    message_id: MessageId,
) -> Value {
    if let Err(e) = check_outbound(ctx, channel_id).await {
        return e;
    }

    match ctx.http.get_message(channel_id, message_id).await {
        Ok(m) => get_message_json(&ctx.config, &m),
        Err(e) => json!({ "error": e.to_string() }),
    }
}

fn get_message_json(config: &LoadedConfig, message: &Message) -> Value {
    let mut projected = message_json(config, message);
    if let Some(attachments) = projected["attachments"].as_array_mut() {
        for (attachment, source) in attachments.iter_mut().zip(&message.attachments) {
            attachment["content_type"] = json!(source.content_type);
        }
    }
    projected
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Converts a serenity [`Timestamp`] to an RFC 3339 string.
///
/// If `to_rfc3339()` returns `None` — which indicates the timestamp is broken
/// at the Discord API level — logs a warning and falls back to the current UTC
/// time so tool responses never contain an empty timestamp string.
fn serenity_ts_to_rfc3339(ts: &Timestamp) -> String {
    match ts.to_rfc3339() {
        Some(s) => s,
        None => {
            let fallback = chrono::Utc::now().to_rfc3339();
            tracing::warn!(
                fallback = %fallback,
                "Discord timestamp failed to_rfc3339(); using current UTC time as fallback"
            );
            fallback
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::{ChannelConfig, Config},
        contradictionary::{Action, Entry, MatchMode},
        no_rly::judge::{ReasonEntry, RejectReason},
        pre_send::{
            Assessment, AuditSink, AuditTrail, ConstructFeedback, FeedbackSink, HookContext,
            HookDecision, HookOutput, PipelineMode, PreSendHook, PreSendPipeline, SinkError,
            SinkFailurePolicy,
        },
        state::new_state,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[test]
    fn outbound_surface_strings_are_canonical() {
        assert_eq!(OutboundSurface::Reply.as_str(), "reply");
        assert_eq!(OutboundSurface::EditMessage.as_str(), "edit-message");
        assert_eq!(
            OutboundSurface::SendFileCaption.as_str(),
            "send-file-caption"
        );
        assert_eq!(
            OutboundSurface::RenderLatexCaption.as_str(),
            "render-latex-caption"
        );
        assert_eq!(OutboundSurface::SendDm.as_str(), "send-dm");
        assert_eq!(OutboundSurface::VoiceBeforeTts.as_str(), "voice-before-tts");
    }

    struct SurfaceHaltHook {
        surfaces: Arc<std::sync::Mutex<Vec<String>>>,
    }

    struct ContextCaptureHook(Arc<std::sync::Mutex<Option<HookContext>>>);

    struct MetadataRewriteHook(Arc<std::sync::Mutex<Option<HookContext>>>);

    struct ContextAuditSink(Arc<std::sync::Mutex<Option<HookContext>>>);

    struct QuietFeedbackSink;

    struct CountingDecisionHook {
        decision: HookDecision,
        calls: Arc<AtomicUsize>,
    }

    impl PreSendHook for CountingDecisionHook {
        fn name(&self) -> HookName {
            HookName::parse("counting-decision").unwrap()
        }

        fn execute(&self, _context: &HookContext) -> HookOutput {
            self.calls.fetch_add(1, Ordering::SeqCst);
            HookOutput::new(
                self.decision.clone(),
                ConstructFeedback::default(),
                AuditTrail::default(),
            )
        }
    }

    impl PreSendHook for ContextCaptureHook {
        fn name(&self) -> HookName {
            HookName::parse("context-capture").unwrap()
        }

        fn execute(&self, context: &HookContext) -> HookOutput {
            *self.0.lock().expect("context lock") = Some(context.clone());
            HookOutput::new(
                HookDecision::Halt {
                    reason: "captured".to_owned(),
                },
                ConstructFeedback::default(),
                AuditTrail::default(),
            )
        }
    }

    impl PreSendHook for MetadataRewriteHook {
        fn name(&self) -> HookName {
            HookName::parse("metadata-rewrite").unwrap()
        }

        fn execute(&self, context: &HookContext) -> HookOutput {
            *self.0.lock().expect("context lock") = Some(context.clone());
            HookOutput::new(
                HookDecision::Rewrite {
                    text: "rewritten".to_string(),
                },
                ConstructFeedback::default(),
                AuditTrail::new(vec![Assessment::new("receipt", 1.0, "rewritten")]),
            )
        }
    }

    impl FeedbackSink for QuietFeedbackSink {
        fn record(
            &self,
            _feedback: &ConstructFeedback,
            _context: &HookContext,
        ) -> Result<(), SinkError> {
            Ok(())
        }
    }

    impl AuditSink for ContextAuditSink {
        fn record(&self, _trail: &AuditTrail, context: &HookContext) -> Result<(), SinkError> {
            *self.0.lock().expect("audit context lock") = Some(context.clone());
            Ok(())
        }
    }

    impl PreSendHook for SurfaceHaltHook {
        fn name(&self) -> HookName {
            HookName::parse("surface-halt").unwrap()
        }

        fn execute(&self, context: &HookContext) -> HookOutput {
            self.surfaces.lock().expect("surface lock").push(
                context
                    .metadata("outbound_surface")
                    .unwrap_or_default()
                    .to_owned(),
            );
            HookOutput::new(
                HookDecision::Halt {
                    reason: "blocked by test hook".to_owned(),
                },
                ConstructFeedback::new(vec![Assessment::new("test", 1.0, "rephrase")]),
                AuditTrail::default(),
            )
        }
    }

    // ── bounce_json wire contract ────────────────────────────────────────
    //
    // The `held` bounce error is the PR's central new wire contract: the
    // shape a client parses to extract the handle and act on a bounce. These
    // snapshots pin it so a field rename can't silently break every consumer.

    fn bounce_reason() -> RejectReason {
        RejectReason {
            matches: vec![ReasonEntry {
                pattern: "straightforward".into(),
                reason: Some("nothing is ever straightforward".into()),
            }],
        }
    }

    #[test]
    fn bounce_json_wire_shape_plain() {
        let ticket = BounceTicket {
            handle: HoldHandle::new("nr-3f92-7"),
            reason: bounce_reason(),
            expires_in: std::time::Duration::from_secs(180),
            parent: None,
        };
        insta::assert_json_snapshot!(bounce_json(&ticket));
    }

    #[test]
    fn bounce_json_wire_shape_chained() {
        let ticket = BounceTicket {
            handle: HoldHandle::new("nr-3f92-8"),
            reason: RejectReason {
                matches: vec![ReasonEntry {
                    pattern: "trivial".into(),
                    reason: Some("nothing worth building is trivial".into()),
                }],
            },
            expires_in: std::time::Duration::from_secs(180),
            parent: Some(HoldHandle::new("nr-3f92-7")),
        };
        insta::assert_json_snapshot!(bounce_json(&ticket));
    }

    /// The construct-facing `held.reason` and the journal `reason` must be the
    /// same structured shape, so a client sees one reason contract everywhere.
    #[test]
    fn bounce_reason_matches_journal_reason_shape() {
        let ticket = BounceTicket {
            handle: HoldHandle::new("nr-3f92-7"),
            reason: bounce_reason(),
            expires_in: std::time::Duration::from_secs(180),
            parent: None,
        };
        let held = bounce_json(&ticket);
        let journal = serde_json::to_value(&ticket.reason).unwrap();
        assert_eq!(
            held["held"]["reason"], journal,
            "held.reason must serialize identically to the journal's reason field"
        );
        assert_eq!(
            held["held"]["reason"]["matches"][0]["pattern"],
            "straightforward"
        );
    }

    fn test_config() -> LoadedConfig {
        let mut raw = Config::default();
        raw.delivery.evidence_markers_enabled = true;
        LoadedConfig::from_raw(raw)
    }

    fn blocking_test_config() -> LoadedConfig {
        let mut raw = Config::default();
        raw.delivery.evidence_markers_enabled = true;
        raw.channels.push(ChannelConfig {
            id: "42".into(),
            ..Default::default()
        });
        raw.contradictionary.enabled = true;
        raw.contradictionary.entries.push(Entry {
            pattern: "straightforward".into(),
            action: Action::Block,
            match_mode: MatchMode::Word,
            reason: Some("nothing is ever straightforward".into()),
        });
        LoadedConfig::from_raw(raw)
    }

    fn messaging_ctx(config: LoadedConfig) -> MessagingCtx {
        MessagingCtx::new(
            Arc::new(serenity::http::Http::new("fake")),
            new_state(),
            Arc::new(config),
            "/tmp".into(),
            Arc::new(ConsentGate::new(camino::Utf8Path::new("/tmp"))),
            Arc::new(crate::ingress_ledger::IngressLedger::new()),
        )
    }

    async fn fake_discord_http() -> (
        Arc<serenity::http::Http>,
        Arc<std::sync::Mutex<Vec<(String, String)>>>,
        tokio::task::JoinHandle<()>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind fake Discord API");
        let address = listener.local_addr().expect("fake Discord API address");
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured = Arc::clone(&requests);
        let server = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let mut request = Vec::new();
                let mut buffer = [0u8; 4096];
                loop {
                    let Ok(read) = stream.read(&mut buffer).await else {
                        return;
                    };
                    if read == 0 {
                        break;
                    }
                    request.extend_from_slice(&buffer[..read]);
                    let Some(header_end) =
                        request.windows(4).position(|window| window == b"\r\n\r\n")
                    else {
                        continue;
                    };
                    let headers = String::from_utf8_lossy(&request[..header_end]);
                    let content_length = headers
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .and_then(|value| value.trim().parse::<usize>().ok())
                        })
                        .unwrap_or_default();
                    if request.len() >= header_end + 4 + content_length {
                        break;
                    }
                }

                let request_text = String::from_utf8_lossy(&request);
                let request_line = request_text.lines().next().unwrap_or_default();
                let path = request_line.split_whitespace().nth(1).unwrap_or_default();
                let body = request_text
                    .split_once("\r\n\r\n")
                    .map_or("", |(_, body)| body);
                captured
                    .lock()
                    .expect("request capture lock")
                    .push((path.to_owned(), body.to_owned()));

                let (status, response_body) = if path.ends_with("/users/@me/channels") {
                    (
                        "200 OK",
                        json!({
                            "id": "4242",
                            "last_message_id": null,
                            "last_pin_timestamp": null,
                            "type": 1,
                            "recipients": [{
                                "id": "77",
                                "username": "recipient",
                                "global_name": null,
                                "avatar": null,
                                "discriminator": "0",
                                "public_flags": 0,
                                "bot": false
                            }]
                        })
                        .to_string(),
                    )
                } else if path.ends_with("/typing") {
                    ("204 No Content", String::new())
                } else if path.ends_with("/messages") {
                    let content = serde_json::from_str::<Value>(body)
                        .ok()
                        .and_then(|body| body["content"].as_str().map(str::to_owned))
                        .unwrap_or_default();
                    (
                        "200 OK",
                        wire_message(
                            9001,
                            &content,
                            "2026-08-15T09:00:00.000000+00:00",
                            json!([]),
                        )
                        .to_string(),
                    )
                } else {
                    ("404 Not Found", "{}".to_owned())
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response_body}",
                    response_body.len()
                );
                stream
                    .write_all(response.as_bytes())
                    .await
                    .expect("write fake Discord response");
            }
        });
        let http = serenity::http::HttpBuilder::new("fake")
            .proxy(format!("http://{address}"))
            .ratelimiter_disabled(true)
            .build();
        (Arc::new(http), requests, server)
    }

    fn messaging_ctx_with_halt_pipeline(
        config: LoadedConfig,
    ) -> (MessagingCtx, Arc<std::sync::Mutex<Vec<String>>>) {
        let surfaces = Arc::new(std::sync::Mutex::new(Vec::new()));
        let pipeline = PreSendPipeline::new(vec![Box::new(SurfaceHaltHook {
            surfaces: Arc::clone(&surfaces),
        })])
        .expect("valid pipeline")
        .with_mode(PipelineMode::Enforce);
        let ctx = messaging_ctx(config);
        let ctx = ctx.with_pre_send_pipeline(Arc::new(pipeline));
        (ctx, surfaces)
    }

    #[tokio::test]
    async fn ordered_evidence_metadata_and_transport_reach_hooks_and_final_audit_context() {
        let hook_context = Arc::new(std::sync::Mutex::new(None));
        let audit_context = Arc::new(std::sync::Mutex::new(None));
        let pipeline = PreSendPipeline::new(vec![Box::new(MetadataRewriteHook(Arc::clone(
            &hook_context,
        )))])
        .unwrap()
        .with_mode(PipelineMode::Enforce)
        .with_sinks(
            Box::new(QuietFeedbackSink),
            Box::new(ContextAuditSink(Arc::clone(&audit_context))),
            SinkFailurePolicy::FailClosed,
        );
        let mut raw = Config::default();
        raw.delivery.evidence_markers_enabled = true;
        raw.channels.push(ChannelConfig {
            id: "42".into(),
            ..Default::default()
        });
        let ctx =
            messaging_ctx(LoadedConfig::from_raw(raw)).with_pre_send_pipeline(Arc::new(pipeline));
        let keys =
            crate::evidence::parse_tool_evidence_keys(&json!({ "evidence_keys": ["34", "12"] }))
                .unwrap();

        let prepared = prepare_outbound(
            &ctx,
            OutboundDraft::channel(
                ChannelId::new(42),
                "original",
                None,
                PreSendOptions {
                    surface: OutboundSurface::Reply,
                    bypasses: &[],
                },
            )
            .with_evidence(&keys),
        )
        .await
        .unwrap();

        let hook_context = hook_context.lock().unwrap().clone().unwrap();
        assert_eq!(hook_context.text(), "original");
        assert_eq!(
            hook_context.metadata("evidence_locators"),
            Some("v1:AAAAAAAAACI,v1:AAAAAAAAAAw")
        );
        assert_eq!(
            hook_context.metadata("evidence_transport"),
            Some("terminal-visible-suffix-v1-after-hooks")
        );

        let audit_context = audit_context.lock().unwrap().clone().unwrap();
        assert_eq!(audit_context.text(), "rewritten");
        assert_eq!(
            audit_context.metadata("evidence_locators"),
            Some("v1:AAAAAAAAACI,v1:AAAAAAAAAAw")
        );
        assert_eq!(
            audit_context.metadata("evidence_transport"),
            Some(EvidenceTransport::TerminalVisibleSuffixV1AfterHooks.as_str())
        );
        assert_eq!(
            prepared.text,
            "rewritten [🔍=v1:AAAAAAAAACI] [🔍=v1:AAAAAAAAAAw]"
        );
    }

    #[tokio::test]
    async fn evidence_bearing_multi_chunk_reply_fails_before_discord_delivery() {
        let mut raw = Config::default();
        raw.delivery.evidence_markers_enabled = true;
        raw.channels.push(ChannelConfig {
            id: "42".into(),
            ..Default::default()
        });
        raw.delivery.text_chunk_limit = 8;
        let ctx = messaging_ctx(LoadedConfig::from_raw(raw));
        let keys = crate::evidence::parse_tool_evidence_keys(&json!({ "evidence_keys": ["12"] }))
            .expect("valid bridge key");

        let response = reply_with_evidence_and_hook_overrides(
            &ctx,
            ChannelId::new(42),
            "long enough to split",
            None,
            ReplyToolOptions {
                suppress_ping: false,
                no_rly_hooks: &[],
                evidence_keys: &keys,
            },
        )
        .await;

        assert_eq!(
            response["error"],
            "evidence-bearing messages must fit in one Discord message"
        );
    }

    #[tokio::test]
    async fn disabled_outbound_evidence_is_a_text_no_op() {
        let mut raw = Config::default();
        raw.channels.push(ChannelConfig {
            id: "42".into(),
            ..Default::default()
        });
        let ctx = messaging_ctx(LoadedConfig::from_raw(raw));
        let keys =
            crate::evidence::parse_tool_evidence_keys(&json!({ "evidence_keys": ["12"] })).unwrap();

        let prepared = prepare_outbound(
            &ctx,
            OutboundDraft::channel(
                ChannelId::new(42),
                "byte exact",
                None,
                PreSendOptions {
                    surface: OutboundSurface::Reply,
                    bypasses: &[],
                },
            )
            .with_evidence(&keys),
        )
        .await
        .unwrap();

        assert_eq!(prepared.text, "byte exact");
    }

    #[tokio::test]
    async fn evidence_bearing_reply_cannot_enter_held_lifecycle() {
        let ctx = messaging_ctx(blocking_test_config());
        let keys = crate::evidence::parse_tool_evidence_keys(&json!({
            "evidence_keys": ["12"]
        }))
        .expect("valid bridge key");

        let response = reply_with_evidence_and_hook_overrides(
            &ctx,
            ChannelId::new(42),
            "straightforward",
            None,
            ReplyToolOptions {
                suppress_ping: false,
                no_rly_hooks: &[],
                evidence_keys: &keys,
            },
        )
        .await;

        assert_eq!(
            response["error"],
            "evidence-bearing messages cannot enter the no_rly hold lifecycle; revise and send a fresh evidence-bearing reply"
        );
        assert_eq!(ctx.no_rly.pending().await, 0);
    }

    #[tokio::test]
    async fn evidence_bearing_rephrase_is_refused_without_consuming_handle() {
        let ctx = messaging_ctx(blocking_test_config());
        let bounce = reply(&ctx, ChannelId::new(42), "straightforward", None, false).await;
        let handle = bounce["held"]["handle"]
            .as_str()
            .expect("ordinary blocked reply should return a handle");
        assert_eq!(ctx.no_rly.pending().await, 1);

        let response = rephrase_held(&ctx, handle, "grounded [🔍=v1:AAAAAAAAAAw]").await;

        assert_eq!(
            response["error"],
            "evidence-bearing replacements are not supported by rephrase; send a fresh evidence-bearing reply"
        );
        assert_eq!(ctx.no_rly.pending().await, 1);
    }

    #[tokio::test]
    async fn first_contact_dm_branch_preserves_ordered_evidence_receipt() {
        let (http, requests, server) = fake_discord_http().await;
        let ctx = MessagingCtx::new(
            http,
            new_state(),
            Arc::new(test_config()),
            "/tmp".into(),
            Arc::new(ConsentGate::new(camino::Utf8Path::new("/tmp"))),
            Arc::new(crate::ingress_ledger::IngressLedger::new()),
        );
        let keys =
            crate::evidence::parse_tool_evidence_keys(&json!({ "evidence_keys": ["34", "12"] }))
                .expect("valid bridge keys");

        let result =
            send_dm_with_evidence_and_hook_overrides(&ctx, UserId::new(77), "grounded", &[], &keys)
                .await;
        server.abort();

        assert_eq!(
            result,
            json!({
                "ok": true,
                "channel_id": "4242",
                "message_ids": [9001],
                "evidence_locators": ["v1:AAAAAAAAACI", "v1:AAAAAAAAAAw"],
            })
        );
        let requests = requests.lock().expect("request capture lock");
        assert!(
            requests
                .iter()
                .any(|(path, _)| path.ends_with("/users/@me/channels")),
            "the first-contact branch must create the DM channel"
        );
        let sent_body = requests
            .iter()
            .find(|(path, _)| path.ends_with("/messages"))
            .map(|(_, body)| body)
            .expect("the first-contact branch must send the prepared message");
        assert_eq!(
            serde_json::from_str::<Value>(sent_body).unwrap()["content"],
            "grounded [🔍=v1:AAAAAAAAACI] [🔍=v1:AAAAAAAAAAw]"
        );
    }

    #[test]
    fn get_message_and_fetch_share_evidence_projection() {
        let content = "grounded [🔍=v1:AAAAAAAAAAw]";
        let messages = from_wire(json!([wire_message(
            3001,
            content,
            "2026-06-09T12:00:00.000000+00:00",
            json!([])
        )]));

        let fetched = message_json(&test_config(), &messages[0]);
        let single = get_message_json(&test_config(), &messages[0]);
        assert_eq!(single["content"], fetched["content"]);
        assert_eq!(single["evidence"], fetched["evidence"]);
        assert_eq!(single["author_id"], fetched["author_id"]);
    }

    #[test]
    fn message_projection_is_absent_when_evidence_markers_are_disabled() {
        let content = "grounded [🔍=v1:AAAAAAAAAAw]";
        let messages = from_wire(json!([wire_message(
            3001,
            content,
            "2026-06-09T12:00:00.000000+00:00",
            json!([])
        )]));
        let disabled = LoadedConfig::from_raw(Config::default());

        let projected = message_json(&disabled, &messages[0]);
        assert_eq!(projected["content"], content);
        assert!(projected.get("evidence").is_none());
    }

    // ── Contradictionary self-react notifications ─────────────────────────

    /// The core of the celebrate-visibility fix: a tool-initiated self-react
    /// must land on the construct event stream marked `self_react: true`.
    /// The gateway drops bot self-reactions, so this synthetic emit is the
    /// only way the construct ever sees the reinforcement signal.
    #[tokio::test]
    async fn celebrate_self_react_notification_reaches_event_stream() {
        let (tx, mut rx) = mpsc::channel(4);
        let mut ctx = messaging_ctx(test_config());
        ctx.event_tx = Some(tx);

        self_react_and_notify(
            &ctx,
            ChannelId::new(42),
            MessageId::new(7),
            UserId::new(99),
            "ariadne",
            CONTRADICTIONARY_CELEBRATE_REACT,
        )
        .await;

        let event = rx
            .try_recv()
            .expect("celebrate self-react must emit a notification");
        let NotificationEvent::Reaction {
            chat_id,
            message_id,
            user,
            user_id,
            emoji,
            self_react,
        } = event
        else {
            panic!("expected a reaction event");
        };
        assert_eq!(chat_id, ChannelId::new(42));
        assert_eq!(message_id, MessageId::new(7));
        assert_eq!(user, "ariadne");
        assert_eq!(user_id, UserId::new(99));
        assert_eq!(emoji, CONTRADICTIONARY_CELEBRATE_REACT);
        assert!(
            self_react,
            "tool-initiated self-reacts must carry self_react"
        );
    }

    /// The ordinary `react` tool must NOT synthesize notifications — only
    /// contradictionary-initiated self-reacts are surfaced, so the gateway's
    /// self-reaction filter isn't quietly bypassed for everything else.
    #[tokio::test]
    async fn ordinary_react_does_not_emit_synthetic_notifications() {
        let mut raw = Config::default();
        raw.channels.push(ChannelConfig {
            id: "42".to_owned(),
            ..Default::default()
        });
        let (tx, mut rx) = mpsc::channel(4);
        let mut ctx = messaging_ctx(LoadedConfig::from_raw(raw));
        ctx.event_tx = Some(tx);

        let _ = react(&ctx, ChannelId::new(42), MessageId::new(7), "👍").await;

        assert!(
            rx.try_recv().is_err(),
            "ordinary reacts must not synthesize notification events"
        );
    }

    #[tokio::test]
    async fn unknown_mcp_hook_override_is_rejected_explicitly() {
        let (ctx, _) = messaging_ctx_with_halt_pipeline(test_config());
        let unknown = HookName::parse("unknown-hook").unwrap();

        let error = prepare_outbound(
            &ctx,
            OutboundDraft::channel(
                ChannelId::new(42),
                "hello",
                None,
                PreSendOptions {
                    surface: OutboundSurface::Reply,
                    bypasses: &[unknown],
                },
            ),
        )
        .await
        .unwrap_err();

        assert!(
            error["error"]
                .as_str()
                .unwrap()
                .contains("unknown pre-send hook")
        );
    }

    #[tokio::test]
    async fn pre_send_hot_reload_enabled_to_disabled_applies_to_next_context() {
        let installed = crate::pre_send::observe_pipeline(Vec::new()).expect("pipeline");
        crate::pre_send::install_pipeline(Some(installed));
        let enabled = messaging_ctx(LoadedConfig::from_raw(Config::default()));
        assert!(enabled.has_pre_send_pipeline());

        let mut disabled_config = Config::default();
        disabled_config.pre_send.enabled = false;
        let disabled = messaging_ctx(LoadedConfig::from_raw(disabled_config));
        assert!(!disabled.has_pre_send_pipeline());
    }

    #[tokio::test]
    async fn pre_send_hot_reload_disabled_to_enabled_applies_to_next_context() {
        let installed = crate::pre_send::observe_pipeline(Vec::new()).expect("pipeline");
        crate::pre_send::install_pipeline(Some(installed));
        let mut disabled_config = Config::default();
        disabled_config.pre_send.enabled = false;
        let disabled = messaging_ctx(LoadedConfig::from_raw(disabled_config));
        assert!(!disabled.has_pre_send_pipeline());

        let enabled = messaging_ctx(LoadedConfig::from_raw(Config::default()));
        assert!(enabled.has_pre_send_pipeline());
    }

    #[tokio::test]
    async fn live_reply_path_runs_pre_send_pipeline() {
        let mut raw = Config::default();
        raw.channels.push(ChannelConfig {
            id: "42".to_owned(),
            ..Default::default()
        });
        let (ctx, surfaces) = messaging_ctx_with_halt_pipeline(LoadedConfig::from_raw(raw));

        let result = reply(&ctx, ChannelId::new(42), "text", None, false).await;

        assert_eq!(result["error"], "blocked by test hook");
        assert_eq!(*surfaces.lock().expect("surface lock"), vec!["reply"]);
    }

    #[tokio::test]
    async fn live_reply_path_verifies_ingress_classifications() {
        let mut raw = Config::default();
        raw.channels.push(ChannelConfig {
            id: "42".to_owned(),
            ..Default::default()
        });
        let ledger = Arc::new(crate::ingress_ledger::IngressLedger::new());
        ledger.note_admitted(
            MessageId::new(7),
            ChannelId::new(42),
            UserId::new(99),
            "admitted",
        );
        ledger.note_admitted(
            MessageId::new(8),
            ChannelId::new(41),
            UserId::new(99),
            "wrong channel",
        );
        let (mut ctx, _) = messaging_ctx_with_halt_pipeline(LoadedConfig::from_raw(raw));
        ctx.ingress_ledger = Arc::clone(&ledger);

        for message_id in [7, 9, 8] {
            let result = reply_with_hook_overrides(
                &ctx,
                ChannelId::new(42),
                "text",
                Some(MessageId::new(message_id)),
                false,
                &[],
            )
            .await;
            assert_eq!(result["error"], "blocked by test hook");
        }

        assert_eq!(
            ledger.take_observed_verifications(),
            vec![
                crate::ingress_ledger::VerifyResult::Admitted {
                    channel: auspex_core::ChannelRef::new(42),
                },
                crate::ingress_ledger::VerifyResult::Unknown,
                crate::ingress_ledger::VerifyResult::ChannelMismatch {
                    admitted_channel: auspex_core::ChannelRef::new(41),
                    claimed_channel: auspex_core::ChannelRef::new(42),
                },
            ]
        );
    }

    #[tokio::test]
    async fn live_edit_path_runs_pre_send_pipeline() {
        let mut raw = Config::default();
        raw.channels.push(ChannelConfig {
            id: "42".to_owned(),
            ..Default::default()
        });
        let (ctx, surfaces) = messaging_ctx_with_halt_pipeline(LoadedConfig::from_raw(raw));

        let result = edit_message(&ctx, ChannelId::new(42), MessageId::new(7), "text").await;

        assert_eq!(result["error"], "blocked by test hook");
        assert_eq!(
            *surfaces.lock().expect("surface lock"),
            vec!["edit-message"]
        );
    }

    #[tokio::test]
    async fn live_attachment_caption_path_runs_pre_send_pipeline() {
        let mut raw = Config::default();
        raw.channels.push(ChannelConfig {
            id: "42".to_owned(),
            ..Default::default()
        });
        let (ctx, surfaces) = messaging_ctx_with_halt_pipeline(LoadedConfig::from_raw(raw));
        let attachment = CreateAttachment::bytes(b"body".as_slice(), "test.txt");

        let result = send_attachment_with_hook_overrides(
            &ctx,
            ChannelId::new(42),
            attachment,
            Some("caption"),
            &[],
            OutboundSurface::SendFileCaption,
        )
        .await;

        assert_eq!(result["error"], "blocked by test hook");
        assert_eq!(
            *surfaces.lock().expect("surface lock"),
            vec!["send-file-caption"]
        );
    }

    #[tokio::test]
    async fn live_dm_path_runs_pre_send_pipeline_before_channel_creation() {
        let (ctx, surfaces) =
            messaging_ctx_with_halt_pipeline(LoadedConfig::from_raw(Config::default()));

        let result = send_dm(&ctx, UserId::new(77), "text").await;

        assert_eq!(result["error"], "blocked by test hook");
        assert_eq!(*surfaces.lock().expect("surface lock"), vec!["send-dm"]);
    }

    #[tokio::test]
    async fn dm_preflight_uses_typed_recipient_destination() {
        let captured = Arc::new(std::sync::Mutex::new(None));
        let pipeline =
            PreSendPipeline::new(vec![Box::new(ContextCaptureHook(Arc::clone(&captured)))])
                .unwrap()
                .with_mode(PipelineMode::Enforce);
        let ctx = messaging_ctx(test_config()).with_pre_send_pipeline(Arc::new(pipeline));

        let result = send_dm(&ctx, UserId::new(77), "text").await;

        assert_eq!(result["error"], "captured");
        let context = captured.lock().unwrap().clone().unwrap();
        assert_eq!(
            context.destination(),
            OutboundDestination::DmRecipient(UserId::new(77))
        );
        assert_eq!(context.channel_id(), None);
        assert_eq!(context.dm_recipient_id(), Some(UserId::new(77)));
        assert_eq!(context.channel_type(), HookChannelType::DirectMessage);
    }

    #[tokio::test]
    async fn dm_redirect_is_prepared_once_and_resolved_without_rerunning_hooks() {
        let mut raw = Config::default();
        raw.channels.push(ChannelConfig {
            id: "43".to_owned(),
            ..Default::default()
        });
        let calls = Arc::new(AtomicUsize::new(0));
        let pipeline = PreSendPipeline::new(vec![Box::new(CountingDecisionHook {
            decision: HookDecision::Redirect {
                channel_id: ChannelId::new(43),
            },
            calls: Arc::clone(&calls),
        })])
        .unwrap()
        .with_mode(PipelineMode::Enforce);
        let ctx =
            messaging_ctx(LoadedConfig::from_raw(raw)).with_pre_send_pipeline(Arc::new(pipeline));

        let _ = send_dm(&ctx, UserId::new(77), "text").await;

        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn captionless_send_file_rejects_hook_overrides_before_file_access() {
        let ctx = messaging_ctx(test_config());
        let hook = HookName::parse("surface-halt").unwrap();

        let result = send_file_with_hook_overrides(
            &ctx,
            ChannelId::new(42),
            "/does/not/exist",
            None,
            &[hook],
        )
        .await;

        assert_eq!(
            result["error"],
            "no_rly_hooks cannot be used when no caption is sent"
        );
    }

    #[tokio::test]
    async fn voice_before_tts_seam_runs_pre_send_pipeline() {
        let mut raw = Config::default();
        raw.channels.push(ChannelConfig {
            id: "42".to_owned(),
            ..Default::default()
        });
        let (ctx, surfaces) = messaging_ctx_with_halt_pipeline(LoadedConfig::from_raw(raw));

        let result = prepare_voice_text(&ctx, ChannelId::new(42), "speech", &[]).await;

        assert_eq!(
            result.expect_err("halt should stop TTS")["error"],
            "blocked by test hook"
        );
        assert_eq!(
            *surfaces.lock().expect("surface lock"),
            vec!["voice-before-tts"]
        );
    }

    #[tokio::test]
    async fn voice_redirect_is_rejected_instead_of_silently_discarded() {
        let mut raw = Config::default();
        for id in ["42", "43"] {
            raw.channels.push(ChannelConfig {
                id: id.to_owned(),
                ..Default::default()
            });
        }
        let calls = Arc::new(AtomicUsize::new(0));
        let pipeline = PreSendPipeline::new(vec![Box::new(CountingDecisionHook {
            decision: HookDecision::Redirect {
                channel_id: ChannelId::new(43),
            },
            calls: Arc::clone(&calls),
        })])
        .unwrap()
        .with_mode(PipelineMode::Enforce);
        let ctx =
            messaging_ctx(LoadedConfig::from_raw(raw)).with_pre_send_pipeline(Arc::new(pipeline));

        let error = prepare_voice_text(&ctx, ChannelId::new(42), "speech", &[])
            .await
            .unwrap_err();

        assert_eq!(
            error["error"],
            "pre-send redirect is not supported for voice output"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn live_context_carries_construct_author_and_explicit_unknown_guild() {
        let mut raw = Config::default();
        raw.channels.push(ChannelConfig {
            id: "42".to_owned(),
            ..Default::default()
        });
        raw.pre_send.construct_id = "syne".to_owned();
        raw.pre_send.author_id = Some(UserId::new(1522260806975099030));
        let captured = Arc::new(std::sync::Mutex::new(None));
        let pipeline =
            PreSendPipeline::new(vec![Box::new(ContextCaptureHook(Arc::clone(&captured)))])
                .expect("valid pipeline")
                .with_mode(PipelineMode::Enforce);
        let ctx =
            messaging_ctx(LoadedConfig::from_raw(raw)).with_pre_send_pipeline(Arc::new(pipeline));

        let result = reply(&ctx, ChannelId::new(42), "text", None, false).await;

        assert_eq!(result["error"], "captured");
        let captured = captured
            .lock()
            .expect("context lock")
            .clone()
            .expect("context");
        assert_eq!(captured.construct_id(), "syne");
        assert_eq!(captured.author_id(), Some(UserId::new(1522260806975099030)));
        assert_eq!(captured.guild_id(), None);
    }

    // ── check_outbound ───────────────────────────────────────────────────────

    /// Mirrors the state write that `send_dm` performs on success:
    ///   `state.record_dm_channel(user_id.get(), channel_id.get())`
    /// After that write, outbound traffic to the DM channel must be permitted.
    #[tokio::test]
    async fn dm_channel_allowed_via_record_dm_channel() {
        let user_id = 1000u64;
        let dm_channel = 2000u64;
        let ctx = messaging_ctx(test_config());
        {
            let mut state = ctx.state.write().await;
            state.record_dm_channel(user_id, dm_channel);
        }
        assert!(
            check_outbound(&ctx, ChannelId::new(dm_channel))
                .await
                .is_ok(),
            "DM channel must be allowed after record_dm_channel"
        );
    }

    #[tokio::test]
    async fn channel_not_in_config_or_state_is_denied() {
        let ctx = messaging_ctx(test_config());
        assert!(
            check_outbound(&ctx, ChannelId::new(999)).await.is_err(),
            "channel absent from config and state must be denied"
        );
    }

    #[tokio::test]
    async fn configured_channel_is_allowed() {
        let mut raw = Config::default();
        raw.channels.push(ChannelConfig {
            id: "42".into(),
            ..Default::default()
        });
        let ctx = messaging_ctx(LoadedConfig::from_raw(raw));
        assert!(
            check_outbound(&ctx, ChannelId::new(42)).await.is_ok(),
            "channel present in config must be allowed"
        );
    }

    /// One message in the shape Discord's REST API returns from
    /// `GET /channels/{channel.id}/messages` (captured shape, trimmed to the
    /// fields serenity requires).
    fn wire_message(id: u64, content: &str, timestamp: &str, attachments: Value) -> Value {
        json!({
            "id": id.to_string(),
            "type": 0,
            "channel_id": "1080000000000000001",
            "author": {
                "id": "210987654321098765",
                "username": "miranda",
                "global_name": "Miranda",
                "avatar": null,
                "discriminator": "0",
                "public_flags": 0,
                "bot": false
            },
            "content": content,
            "timestamp": timestamp,
            "edited_timestamp": null,
            "tts": false,
            "mention_everyone": false,
            "mentions": [],
            "mention_roles": [],
            "attachments": attachments,
            "embeds": [],
            "pinned": false,
            "flags": 0,
            "components": []
        })
    }

    /// Deserializes a wire payload exactly as serenity's `Http::get_messages`
    /// does.
    fn from_wire(payload: Value) -> Vec<Message> {
        serde_json::from_value(payload).expect("captured payload must deserialize as Vec<Message>")
    }

    /// Three messages newest-first, as Discord returns them on the wire.
    fn newest_first_batch() -> Vec<Message> {
        from_wire(json!([
            wire_message(3003, "third", "2026-06-09T12:02:00.000000+00:00", json!([])),
            wire_message(
                3002,
                "second",
                "2026-06-09T12:01:00.000000+00:00",
                json!([])
            ),
            wire_message(3001, "first", "2026-06-09T12:00:00.000000+00:00", json!([])),
        ]))
    }

    #[test]
    fn new_since_sorts_oldest_first() {
        let resp = new_since_response(&test_config(), newest_first_batch(), 20);
        let ids: Vec<&str> = resp["messages"]
            .as_array()
            .expect("messages array")
            .iter()
            .map(|m| m["id"].as_str().expect("string id"))
            .collect();
        assert_eq!(
            ids,
            ["3001", "3002", "3003"],
            "messages must be sorted oldest-first regardless of wire order"
        );
    }

    #[test]
    fn new_since_count_matches_returned_messages() {
        let resp = new_since_response(&test_config(), newest_first_batch(), 20);
        assert_eq!(resp["count"], 3);
        assert_eq!(
            resp["messages"].as_array().expect("messages array").len(),
            3
        );
    }

    #[test]
    fn new_since_has_more_set_at_exactly_limit() {
        let resp = new_since_response(&test_config(), newest_first_batch(), 3);
        assert_eq!(
            resp["has_more"],
            json!(true),
            "a full page (count == limit) must signal that more may follow"
        );
    }

    #[test]
    fn new_since_has_more_unset_below_limit() {
        let resp = new_since_response(&test_config(), newest_first_batch(), 4);
        assert_eq!(
            resp["has_more"],
            json!(false),
            "a partial page must signal that the caller is caught up"
        );
    }

    #[test]
    fn new_since_empty_when_caught_up() {
        let resp = new_since_response(&test_config(), Vec::new(), 20);
        assert_eq!(resp["messages"], json!([]));
        assert_eq!(resp["count"], 0);
        assert_eq!(resp["has_more"], json!(false));
    }

    #[test]
    fn new_since_zero_limit_never_signals_more() {
        // Dispatch clamps limit to 1..=100, but the response builder must
        // stay safe on its own: limit 0 with an empty page would otherwise
        // satisfy `count == limit` vacuously and claim more data exists.
        let resp = new_since_response(&test_config(), Vec::new(), 0);
        assert_eq!(resp["count"], 0);
        assert_eq!(
            resp["has_more"],
            json!(false),
            "an empty page must never claim more data is available"
        );
    }

    #[test]
    fn new_since_message_shape_matches_fetch_messages() {
        let batch = from_wire(json!([wire_message(
            4001,
            "with attachment",
            "2026-06-09T12:00:00.000000+00:00",
            json!([{
                "id": "111",
                "filename": "photo.png",
                "size": 2048,
                "url": "https://cdn.discordapp.com/attachments/1/111/photo.png",
                "proxy_url": "https://media.discordapp.net/attachments/1/111/photo.png",
                "content_type": "image/png",
                "height": null,
                "width": null
            }])
        )]));
        let resp = new_since_response(&test_config(), batch, 20);
        let msg = &resp["messages"][0];

        let mut keys: Vec<&str> = msg
            .as_object()
            .expect("message object")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "attachments",
                "author",
                "author_id",
                "content",
                "id",
                "timestamp"
            ],
            "fetch_new_since message objects must keep wire-shape parity with fetch_messages"
        );

        let mut attachment_keys: Vec<&str> = msg["attachments"][0]
            .as_object()
            .expect("attachment object")
            .keys()
            .map(String::as_str)
            .collect();
        attachment_keys.sort_unstable();
        assert_eq!(attachment_keys, ["name", "size", "url"]);

        assert_eq!(msg["author"], "miranda");
        assert_eq!(msg["author_id"], "210987654321098765");
        assert_eq!(msg["attachments"][0]["name"], "photo.png");
        assert!(
            msg["timestamp"]
                .as_str()
                .expect("string timestamp")
                .starts_with("2026-06-09T12:00:00"),
            "timestamp must round-trip from the wire payload"
        );
    }

    // ── parse_reaction_type ──────────────────────────────────────────────────

    #[test]
    fn parse_reaction_type_rejects_zero_custom_emoji_id() {
        // Regression: `<:name:0>` used to reach `EmojiId::new(0)`, which
        // panics (serenity Ids are NonZeroU64). It must be a graceful error.
        let result = parse_reaction_type("<:name:0>");
        assert!(
            result.is_err(),
            "zero emoji ID must be rejected, got: {result:?}"
        );
    }

    #[test]
    fn parse_reaction_type_rejects_zero_animated_emoji_id() {
        let result = parse_reaction_type("<a:party:0>");
        assert!(
            result.is_err(),
            "zero animated emoji ID must be rejected, got: {result:?}"
        );
    }

    #[test]
    fn parse_reaction_type_accepts_valid_custom_emoji() {
        use serenity::model::channel::ReactionType;

        match parse_reaction_type("<:blob:123456789012345678>") {
            Ok(ReactionType::Custom { animated, id, name }) => {
                assert!(!animated);
                assert_eq!(id.get(), 123456789012345678);
                assert_eq!(name.as_deref(), Some("blob"));
            }
            other => panic!("expected Custom reaction, got: {other:?}"),
        }
    }

    #[test]
    fn parse_reaction_type_passes_unicode_through() {
        use serenity::model::channel::ReactionType;

        assert_eq!(
            parse_reaction_type("👍"),
            Ok(ReactionType::Unicode("👍".to_string()))
        );
    }

    /// Live smoke test against the real Discord API. Ignored by default; run
    /// with `cargo nextest run --run-ignored=ignored-only -E 'test(live_fetch_new_since)'`
    /// after exporting the three environment variables named below.
    #[tokio::test]
    #[ignore = "live Discord smoke test; requires DISCORD_BOT_TOKEN, DIONE_TEST_CHANNEL_ID, DIONE_TEST_AFTER_MESSAGE_ID"]
    async fn live_fetch_new_since_smoke() {
        let token = std::env::var("DISCORD_BOT_TOKEN").expect("DISCORD_BOT_TOKEN must be set");
        let channel_id = ChannelId::new(
            std::env::var("DIONE_TEST_CHANNEL_ID")
                .expect("DIONE_TEST_CHANNEL_ID must be set")
                .parse::<u64>()
                .expect("DIONE_TEST_CHANNEL_ID must be a u64"),
        );
        let after_message_id = MessageId::new(
            std::env::var("DIONE_TEST_AFTER_MESSAGE_ID")
                .expect("DIONE_TEST_AFTER_MESSAGE_ID must be set")
                .parse::<u64>()
                .expect("DIONE_TEST_AFTER_MESSAGE_ID must be a u64"),
        );

        let mut raw = Config::default();
        raw.channels.push(ChannelConfig {
            id: channel_id.get().to_string(),
            require_mention: false,
            allow_from: vec![],
            ..Default::default()
        });

        let dir = tempfile::TempDir::new().expect("tempdir");
        let state_dir = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf-8 path");
        let ctx = MessagingCtx::new(
            Arc::new(serenity::http::Http::new(&token)),
            crate::state::new_state(),
            Arc::new(LoadedConfig::from_raw(raw)),
            state_dir.clone(),
            Arc::new(ConsentGate::new(&state_dir)),
            Arc::new(crate::ingress_ledger::IngressLedger::new()),
        );

        let resp = fetch_new_since(&ctx, channel_id, after_message_id, 20).await;
        assert!(resp.get("error").is_none(), "live fetch failed: {resp}");

        let messages = resp["messages"].as_array().expect("messages array");
        assert_eq!(resp["count"], messages.len() as u64);
        let ids: Vec<u64> = messages
            .iter()
            .map(|m| {
                m["id"]
                    .as_str()
                    .expect("string id")
                    .parse()
                    .expect("u64 id")
            })
            .collect();
        assert!(
            ids.windows(2).all(|w| w[0] < w[1]),
            "ids must be strictly ascending: {ids:?}"
        );
        assert!(
            ids.iter().all(|&id| id > after_message_id.get()),
            "all returned ids must be after the cursor {}: {ids:?}",
            after_message_id.get()
        );
    }

    mod proptests {
        use super::*;
        use proptest::prelude::*;

        /// Unique nonzero snowflakes in arbitrary (shuffled) order, mimicking
        /// any ordering Discord could put on the wire.
        fn ids_strategy() -> impl Strategy<Value = Vec<u64>> {
            prop::collection::hash_set(1u64..=u64::MAX, 0..=8)
                .prop_map(|set| set.into_iter().collect::<Vec<_>>())
                .prop_shuffle()
        }

        fn batch_from_ids(ids: &[u64]) -> Vec<Message> {
            from_wire(json!(
                ids.iter()
                    .map(|&id| wire_message(id, "m", "2026-06-09T12:00:00.000000+00:00", json!([])))
                    .collect::<Vec<_>>()
            ))
        }

        proptest! {
            /// The full `fetch_new_since` response contract, for any wire
            /// ordering and any limit (including the 0 edge case):
            ///
            /// 1. `count` always equals `messages.len()`
            /// 2. `has_more` is true iff `count == limit` and `limit >= 1` —
            ///    an empty page must never claim more data (the limit-0 bug)
            /// 3. messages are in chronological order (oldest first)
            /// 4. no messages are invented or dropped
            #[test]
            fn new_since_response_invariants(ids in ids_strategy(), limit in 0u8..=100) {
                let resp = new_since_response(&test_config(), batch_from_ids(&ids), limit);

                let msgs = resp["messages"].as_array().expect("messages array");
                prop_assert_eq!(
                    resp["count"].as_u64().expect("count"),
                    msgs.len() as u64,
                    "count must equal messages.len()"
                );

                let expected_more = limit >= 1 && msgs.len() == usize::from(limit);
                prop_assert_eq!(
                    resp["has_more"].as_bool().expect("has_more"),
                    expected_more,
                    "has_more must be true iff a full page (>= 1) was returned"
                );

                let out_ids: Vec<u64> = msgs
                    .iter()
                    .map(|m| m["id"].as_str().expect("string id").parse().expect("u64 id"))
                    .collect();
                prop_assert!(
                    out_ids.windows(2).all(|w| w[0] < w[1]),
                    "ids must be strictly ascending (oldest first): {:?}",
                    out_ids
                );

                let mut sorted_input = ids.clone();
                sorted_input.sort_unstable();
                prop_assert_eq!(
                    out_ids,
                    sorted_input,
                    "response must be a permutation of the input batch"
                );
            }
        }
    }

    // ── fetch_messages cursor feature tests ──────────────────────────────────

    #[test]
    fn resolve_pagination_neither_yields_none() {
        assert!(resolve_pagination(None, None).unwrap().is_none());
    }

    #[test]
    fn resolve_pagination_before_yields_before() {
        let id = MessageId::new(1234);
        let result = resolve_pagination(Some(id), None).unwrap();
        assert!(matches!(result, Some(MessagePagination::Before(m)) if m == id));
    }

    #[test]
    fn resolve_pagination_after_yields_after() {
        let id = MessageId::new(5678);
        let result = resolve_pagination(None, Some(id)).unwrap();
        assert!(matches!(result, Some(MessagePagination::After(m)) if m == id));
    }

    #[test]
    fn resolve_pagination_both_is_error() {
        let a = MessageId::new(1);
        let b = MessageId::new(2);
        let result = resolve_pagination(Some(a), Some(b));
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err["error"]
                .as_str()
                .unwrap()
                .contains("cannot specify both")
        );
    }

    #[test]
    fn build_fetch_response_sorts_oldest_first() {
        let msgs = newest_first_batch();
        let resp = build_fetch_response(&test_config(), msgs, false, 20);
        let ids: Vec<&str> = resp["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, ["3001", "3002", "3003"]);
    }

    #[test]
    fn build_pins_response_sorts_oldest_first_and_counts_messages() {
        let resp = build_pins_response(&test_config(), newest_first_batch());
        let ids = resp["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|message| message["id"].as_str().unwrap())
            .collect::<Vec<_>>();

        assert_eq!(ids, ["3001", "3002", "3003"]);
        assert_eq!(resp["count"], 3);
    }

    #[test]
    fn build_pins_response_handles_empty_channels() {
        let resp = build_pins_response(&test_config(), Vec::new());

        assert_eq!(resp["messages"], json!([]));
        assert_eq!(resp["count"], 0);
    }

    #[test]
    fn build_fetch_response_no_metadata_without_pagination() {
        let msgs = newest_first_batch();
        let resp = build_fetch_response(&test_config(), msgs, false, 20);
        assert!(resp.get("count").is_none());
        assert!(resp.get("has_more").is_none());
    }

    #[test]
    fn build_fetch_response_includes_metadata_with_pagination() {
        let msgs = newest_first_batch();
        let resp = build_fetch_response(&test_config(), msgs, true, 20);
        assert_eq!(resp["count"], 3);
        assert_eq!(resp["has_more"], false);
    }

    #[test]
    fn build_fetch_response_has_more_when_full_page() {
        let msgs = newest_first_batch();
        let resp = build_fetch_response(&test_config(), msgs, true, 3);
        assert_eq!(resp["count"], 3);
        assert_eq!(resp["has_more"], true);
    }

    #[test]
    fn build_fetch_response_no_has_more_on_short_page() {
        let msgs = from_wire(json!([wire_message(
            1001,
            "only",
            "2026-06-09T12:00:00.000000+00:00",
            json!([])
        ),]));
        let resp = build_fetch_response(&test_config(), msgs, true, 20);
        assert_eq!(resp["count"], 1);
        assert_eq!(resp["has_more"], false);
    }

    #[test]
    fn build_fetch_response_empty_page() {
        let resp = build_fetch_response(&test_config(), vec![], true, 20);
        assert_eq!(resp["count"], 0);
        assert_eq!(resp["has_more"], false);
        assert!(resp["messages"].as_array().unwrap().is_empty());
    }
}
