use crate::{
    config::{ChunkMode, DmPolicy, LoadedConfig},
    contradictionary::{Action, BlockOutcome, DiaryRecord, append_diary_record},
    coordination::{ClaimOutcome, Coordinator},
    discord::{chunk, chunk_preserving_fences_with_context, events::NotificationEvent},
    evidence::{
        SentexHandles, SentexRole, SentexTransport, append_markers, has_terminal_sentex_syntax,
        locator_metadata, parse_sentex_locators, project_sentexes,
    },
    gate::OutboundGate,
    ingress_ledger::{IngressLedger, VerifyResult},
    no_rly::{
        consent::{
            BounceTicket, ConsentGate, DeliverError, DeliverReply, RejectedHandle, Rephrased,
            ReplyRequest,
        },
        judge::{AlwaysClear, OutboundJudge, Verdict},
        queue::HoldHandle,
    },
    pre_send::{
        ChannelType as HookChannelType, ConstructId, HookContext, HookDecision, HookName,
        OutboundDestination, PreSendPipeline,
    },
    state::State,
};
use camino::Utf8PathBuf;
use serde_json::{Value, json};
use serenity::{
    builder::{CreateAllowedMentions, CreateAttachment, CreateMessage, EditMessage},
    http::MessagePagination,
    model::{
        Timestamp,
        channel::{Channel, Message},
        id::{ChannelId, MessageId, UserId},
    },
};
use std::{sync::Arc, time::Instant};
use tokio::sync::mpsc;

/// Self-react emoji for contradictionary celebrate hits (✨ — sparkles).
const CONTRADICTIONARY_CELEBRATE_REACT: &str = "\u{2728}";
static NO_SENTEX_HANDLES: SentexHandles = SentexHandles::empty();

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

fn canonical_requires_ingress() -> Value {
    json!({
        "error": "target requires an admitted ingress receipt for this audience",
        "reason": "canonical_requires_ingress",
    })
}

/// Canonical existence is sufficient for conversational targets, not for
/// management operations that can change someone else's older message.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum TargetPolicy {
    CanonicalReplyOrReact(bool),
    IngressOrOwnSend(bool),
}

/// Verify a message target before performing a Discord mutation. The ingress
/// ledger is session-scoped: an unknown target may be an older real message.
/// Only reply and react may verify a direct-author target when neither mention
/// nor identity filtering applies. Management requires ingress or own-send.
/// Missing canonical targets raise the canary; unledgered management attempts
/// are separately alerted and blocked. Inconclusive lookups block without accusation.
/// `Expired` and `Unavailable` still allow the operation.
///
/// `own_send` exempts one specific `Unknown` case: a target this seat itself
/// authored. Our own sends are (correctly) absent from the ingress ledger,
/// whose domain is *received* messages, so acting on one — reacting to, replying
/// to, or deleting our own message — otherwise trips the canary on our own hand
/// (dione#334). The exemption is centralized here so react/reply/pin/delete all
/// inherit it. It keys on the caller's *authenticated* own-send signal
/// (`SharedState::is_own_send`), never a target's claimed-author field, so a
/// spoofed inbound claiming our identity is still quarantined. It applies **only**
/// to `Unknown`; `ChannelMismatch`, `Expired`, and `Unavailable` are unaffected.
///
/// `operation` names the egress path for tracing and alerts (e.g. "react",
/// "pin_message").
pub(crate) async fn verify_message_target(
    ledger: &IngressLedger,
    http: &Arc<serenity::http::Http>,
    config: &LoadedConfig,
    message_id: MessageId,
    channel_id: ChannelId,
    operation: &str,
    policy: TargetPolicy,
) -> Result<(), Value> {
    verify_message_target_with_mute_store(
        TargetVerificationContext {
            ledger,
            http,
            config,
            mute_store_override: None,
        },
        message_id,
        channel_id,
        operation,
        policy,
    )
    .await
}

struct TargetVerificationContext<'a> {
    ledger: &'a IngressLedger,
    http: &'a Arc<serenity::http::Http>,
    config: &'a LoadedConfig,
    mute_store_override: Option<&'a crate::mute_store::MuteStore>,
}

async fn verify_message_target_with_mute_store(
    context: TargetVerificationContext<'_>,
    message_id: MessageId,
    channel_id: ChannelId,
    operation: &str,
    policy: TargetPolicy,
) -> Result<(), Value> {
    let TargetVerificationContext {
        ledger,
        http,
        config,
        mute_store_override,
    } = context;
    let own_send = match policy {
        TargetPolicy::CanonicalReplyOrReact(own_send)
        | TargetPolicy::IngressOrOwnSend(own_send) => own_send,
    };
    let verdict = ledger.verify(message_id, channel_id);
    if matches!(&verdict, VerifyResult::Unknown) && !own_send {
        if matches!(policy, TargetPolicy::IngressOrOwnSend(_)) {
            tracing::warn!(
                message_id = message_id.get(),
                channel_id = channel_id.get(),
                operation,
                "egress: unledgered management target blocked"
            );
            if let Some(alert_ch) = config.phantom_canary_channel {
                phantom_canary_alert(
                    http,
                    alert_ch,
                    &format!(
                        "⚠️ PHANTOM CANARY: {operation} target message {} in channel {} absent from ingress ledger; management blocked",
                        message_id.get(),
                        channel_id.get()
                    ),
                );
            }
            return Err(json!({
                "error": "message target requires an ingress receipt or own send",
                "reason": "ingress_required",
            }));
        }
        let unrestricted = config
            .channel_policy(channel_id.get())
            .is_some_and(|policy| !policy.require_mention && !policy.has_identity_filter());
        match http.get_message(channel_id, message_id).await {
            Ok(message) if message.id == message_id && message.channel_id == channel_id => {
                if !unrestricted
                    || config.is_ignored(message.author.id.get())
                    || message.webhook_id.is_some()
                    || (message.author.bot && !config.is_allowed(message.author.id.get()))
                {
                    return Err(canonical_requires_ingress());
                }
                // REST messages need not carry guild_id; resolve it rather
                // than treating an unknown guild as unmuted.
                let guild_id = match message.guild_id {
                    Some(id) => id,
                    None => match http.get_channel(channel_id).await {
                        Ok(Channel::Guild(channel)) => channel.guild_id,
                        Ok(_) => return Err(canonical_requires_ingress()),
                        Err(error) => {
                            return Err(json!({
                                "error": format!("could not verify target guild: {error}"),
                                "reason": "canonical_lookup_failed",
                            }));
                        }
                    },
                };
                let parent_id = message
                    .message_reference
                    .as_ref()
                    .and_then(|reference| reference.message_id)
                    .or_else(|| {
                        message
                            .referenced_message
                            .as_deref()
                            .map(|parent| parent.id)
                    });
                let drops = crate::drop_ledger::global();
                let guild_muted = match mute_store_override {
                    Some(store) => store.is_guild_muted(guild_id.get()),
                    None => crate::mute_store::global()
                        .is_some_and(|store| store.is_guild_muted(guild_id.get())),
                };
                if guild_muted
                    || drops.contains(channel_id, message_id)
                    || drops.reply_inherits_drop(channel_id, parent_id)
                {
                    return Err(canonical_requires_ingress());
                }
                tracing::info!(
                    message_id = message_id.get(),
                    channel_id = channel_id.get(),
                    operation,
                    "egress: canonical target exists but is absent from ingress ledger"
                );
                return Ok(());
            }
            Ok(_) => {
                return Err(json!({
                    "error": "canonical message did not match the requested target",
                    "reason": "canonical_mismatch",
                }));
            }
            Err(serenity::Error::Http(serenity::http::HttpError::UnsuccessfulRequest(
                response,
            ))) if response.status_code.as_u16() == 404 && response.error.code == 10008 => {}
            Err(error) => {
                return Err(json!({
                    "error": format!("could not verify message target: {error}"),
                    "reason": "canonical_lookup_failed",
                }));
            }
        }
    }
    verify_message_target_with_alert(
        verdict,
        config,
        message_id,
        channel_id,
        operation,
        own_send,
        |alert_channel, content| phantom_canary_alert(http, alert_channel, &content),
    )
}

// The Unknown arm is reached only after Discord confirmed code 10008, except
// for authenticated own-sends. Do not pass an unchecked ledger miss here.
fn verify_message_target_with_alert(
    verdict: VerifyResult,
    config: &LoadedConfig,
    message_id: MessageId,
    channel_id: ChannelId,
    operation: &str,
    own_send: bool,
    mut alert: impl FnMut(ChannelId, String),
) -> Result<(), Value> {
    match verdict {
        crate::ingress_ledger::VerifyResult::Admitted { .. } => Ok(()),
        crate::ingress_ledger::VerifyResult::Unknown if own_send => {
            // The acting seat authored this target. It is correctly absent from
            // the received-message ingress ledger, so its `Unknown` is expected,
            // not suspicious. Exempt from the phantom canary — proven our own
            // hand by an authenticated own-send record, not a claimed author.
            tracing::debug!(
                message_id = message_id.get(),
                channel_id = channel_id.get(),
                operation,
                "egress: own-authored target absent from ingress ledger (expected); phantom canary exempt"
            );
            Ok(())
        }
        crate::ingress_ledger::VerifyResult::Unknown => {
            tracing::warn!(
                message_id = message_id.get(),
                channel_id = channel_id.get(),
                operation,
                "egress: Discord confirmed Unknown Message after ingress miss"
            );
            if let Some(alert_ch) = config.phantom_canary_channel {
                alert(
                    alert_ch,
                    format!(
                        "⚠️ PHANTOM CANARY: {operation} target message {msg} in channel {ch} returned Discord Unknown Message (10008)",
                        msg = message_id.get(),
                        ch = channel_id.get(),
                    ),
                );
            }
            Err(json!({
                "error": format!(
                    "message {} was not found by Discord in channel {}; {operation} blocked",
                    message_id.get(), channel_id.get()
                ),
                "reason": "target_not_found",
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
            Err(json!({
                "error": format!(
                    "message {message_id} was admitted in channel {admitted_channel}, not claimed channel {claimed_channel}; {operation} blocked",
                ),
                "reason": "channel_mismatch",
                "message_id": message_id.to_string(),
                "admitted_channel_id": admitted_channel.to_string(),
                "claimed_channel_id": claimed_channel.to_string(),
                "operation": operation,
                "blocked": true,
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
    sentex_handles: &'a SentexHandles,
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
            sentex_handles: &NO_SENTEX_HANDLES,
        }
    }

    fn dm_recipient(user_id: UserId, text: &'a str, pre_send: PreSendOptions<'a>) -> Self {
        Self {
            destination: OutboundDestination::DmRecipient(user_id),
            text,
            reply_to: None,
            pre_send,
            sentex_handles: &NO_SENTEX_HANDLES,
        }
    }

    fn with_sentex_handles(mut self, sentex_handles: &'a SentexHandles) -> Self {
        self.sentex_handles = sentex_handles;
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
    reject_raw_sentex_locators(draft.text)?;
    let disabled_handles = SentexHandles::empty();
    let sentex_handles = if ctx.config.delivery.evidence_markers_enabled {
        draft.sentex_handles
    } else {
        &disabled_handles
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
            text: append_markers(draft.text, sentex_handles),
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
    if !sentex_handles.is_empty() {
        let claim_locators = locator_metadata(sentex_handles.claims(), SentexRole::Claim);
        let citation_locators = locator_metadata(sentex_handles.citations(), SentexRole::Citation);
        if !claim_locators.is_empty() {
            hook_context = hook_context.with_metadata("claim_locators", claim_locators);
        }
        if !citation_locators.is_empty() {
            hook_context = hook_context.with_metadata("citation_locators", citation_locators);
        }
        hook_context = hook_context.with_metadata(
            "sentex_transport",
            SentexTransport::TerminalVisibleRoleSuffixV2AfterHooks.as_str(),
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

    let final_text = outcome.final_text().unwrap_or(draft.text);
    reject_raw_sentex_locators(final_text)?;
    Ok(PreparedOutbound {
        destination,
        text: append_markers(final_text, sentex_handles),
        reply_to,
        surface: draft.pre_send.surface,
    })
}

fn reject_raw_sentex_locators(content: &str) -> Result<(), Value> {
    if !has_terminal_sentex_syntax(content) {
        return Ok(());
    }
    Err(json!({
        "error": "raw terminal sentex locators are not accepted in content; use structured handles on reply or send_dm"
    }))
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
            sentex_handles: &NO_SENTEX_HANDLES,
        },
    )
    .await
}

pub(crate) struct ReplyToolOptions<'a> {
    pub(crate) suppress_ping: bool,
    pub(crate) no_rly_hooks: &'a [HookName],
    pub(crate) sentex_handles: &'a SentexHandles,
}

pub(crate) async fn reply_with_evidence_and_hook_overrides(
    ctx: &MessagingCtx,
    channel_id: ChannelId,
    content: &str,
    reply_to_message_id: Option<MessageId>,
    options: ReplyToolOptions<'_>,
) -> Value {
    if let Err(error) = check_outbound(ctx, channel_id).await {
        return error;
    }

    if let Some(ref_id) = reply_to_message_id {
        let own_send = ctx.state.read().await.is_own_send(ref_id.get());
        if let Err(error) = verify_message_target(
            &ctx.ingress_ledger,
            &ctx.http,
            &ctx.config,
            ref_id,
            channel_id,
            "reply_to",
            TargetPolicy::CanonicalReplyOrReact(own_send),
        )
        .await
        {
            return error;
        }
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
        .with_sentex_handles(options.sentex_handles),
    )
    .await
    {
        Ok(prepared) => prepared,
        Err(error) => return error,
    };
    if ctx.config.delivery.evidence_markers_enabled
        && !options.sentex_handles.is_empty()
        && parse_sentex_locators(&prepared.text).is_empty()
    {
        return json!({
            "error": "sentex locators cannot be attached inside quoted or fenced content"
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

/// The coordinator (and its config-block name) for a channel, if one is
/// configured and the channel is opted in via `coordinate`. A thread without
/// its own policy uses its parent's, as the inbound and outbound gates do.
async fn channel_coordinator(
    ctx: &MessagingCtx,
    channel_id: ChannelId,
) -> Option<(&str, &Coordinator)> {
    let policy = match ctx.config.channel_policy(channel_id.get()) {
        Some(policy) => policy,
        None => {
            let parent = ctx
                .state
                .read()
                .await
                .thread_parents
                .get(&channel_id.get())
                .copied()
                .flatten()?;
            ctx.config.channel_policy(parent)?
        }
    };
    let name = policy.coordinate.as_deref()?;
    ctx.config
        .coordinators
        .get(name)
        .map(|coordinator| (name, coordinator))
}

/// Claim the right to answer `message_id` before sending. `Err` is the tool
/// error returned to the construct; `Ok` lets the send proceed, carrying
/// `true` when this seat holds a claim it must `done` or `release`. That
/// includes a fail-open send whose claim reached the server unanswered: the
/// server may have made this seat the owner, and a `done` it does not
/// recognise is only refused.
async fn coordinate_reply(
    ctx: &MessagingCtx,
    channel_id: ChannelId,
    message_id: MessageId,
) -> Result<bool, Value> {
    let Some((name, coordinator)) = channel_coordinator(ctx, channel_id).await else {
        return Ok(false);
    };
    let mid = message_id.get().to_string();
    match coordinator.claim(&channel_id.get().to_string(), &mid).await {
        ClaimOutcome::Proceed { .. } => Ok(true),
        ClaimOutcome::Unavailable { reason, claim_sent } if coordinator.config().fail_open => {
            tracing::warn!(
                channel = %channel_id,
                coordinator = %name,
                %reason,
                claim_sent,
                "claim-once unavailable; failing open and replying"
            );
            Ok(claim_sent)
        }
        ClaimOutcome::Unavailable { reason, claim_sent } => {
            // The refused reply will not go out; do not leave a claim the
            // server may have recorded to run out its lease.
            if claim_sent {
                release_reply_claim(ctx, channel_id, message_id).await;
            }
            Err(json!({
                "error": format!("claim-once unavailable and fail-closed: {reason}")
            }))
        }
        ClaimOutcome::Wait { ahead, .. } => {
            let who = ahead
                .first()
                .map(String::as_str)
                .unwrap_or("another construct");
            Err(json!({
                "error": format!("claim-once: {who} is already answering this message")
            }))
        }
    }
}

/// Report a successful reply back to the coordinator so waiters are released.
/// Fire-and-forget: the report runs in a background task, so a slow or
/// blackholed server never delays the reply; failures only log inside the
/// client.
async fn report_reply_done(
    ctx: &MessagingCtx,
    channel_id: ChannelId,
    message_id: MessageId,
    reply_id: MessageId,
) {
    let Some((_name, coordinator)) = channel_coordinator(ctx, channel_id).await else {
        return;
    };
    let coordinator = coordinator.clone();
    tokio::spawn(async move {
        coordinator
            .done(&message_id.get().to_string(), &reply_id.get().to_string())
            .await;
    });
}

/// Step aside on a claimed `message_id` whose reply did not go out, so the
/// next seat in line is promoted now rather than at lease expiry.
/// Fire-and-forget, like [`report_reply_done`].
async fn release_reply_claim(ctx: &MessagingCtx, channel_id: ChannelId, message_id: MessageId) {
    let Some((_name, coordinator)) = channel_coordinator(ctx, channel_id).await else {
        return;
    };
    let coordinator = coordinator.clone();
    tokio::spawn(async move {
        coordinator.release(&message_id.get().to_string()).await;
    });
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
        fence_context: Default::default(),
    };

    if let Some(ref judge) = ctx.config.contradictionary
        && let Verdict::Bounce(reason) = judge.judge(&content)
    {
        if ctx.config.delivery.evidence_markers_enabled
            && !parse_sentex_locators(&content).is_empty()
        {
            return json!({
                "error": "sentex-bearing messages cannot enter the no_rly hold lifecycle; revise and send a fresh sentex-bearing reply"
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

    // Claim the right to answer only now, after the hooks, the evidence
    // checks and the judge, so a reply that never goes out never holds the
    // claim. `Wait` becomes the tool error naming who is ahead.
    let claimed = match reply_to_message_id {
        Some(ref_id) => match coordinate_reply(ctx, channel_id, ref_id).await {
            Ok(claimed) => claimed.then_some(ref_id),
            Err(error) => return error,
        },
        None => None,
    };

    match deliver_reply(ctx, &request).await {
        Ok(sent_ids) => {
            if let Some(ref_id) = claimed {
                match sent_ids.first() {
                    Some(reply_id) => {
                        report_reply_done(ctx, channel_id, ref_id, MessageId::new(*reply_id)).await;
                    }
                    None => release_reply_claim(ctx, channel_id, ref_id).await,
                }
            }
            let mut response = json!({ "ok": true, "message_ids": sent_ids });
            let locators = parse_sentex_locators(&content);
            if ctx.config.delivery.evidence_markers_enabled && !locators.is_empty() {
                let claim_locators = locators
                    .iter()
                    .filter(|locator| locator.role() == SentexRole::Claim)
                    .map(crate::evidence::SentexLocator::as_str)
                    .collect::<Vec<_>>();
                let citation_locators = locators
                    .iter()
                    .filter(|locator| locator.role() == SentexRole::Citation)
                    .map(crate::evidence::SentexLocator::as_str)
                    .collect::<Vec<_>>();
                if !claim_locators.is_empty() {
                    response["claim_locators"] = json!(claim_locators);
                }
                if !citation_locators.is_empty() {
                    response["citation_locators"] = json!(citation_locators);
                }
            }
            response
        }
        Err(e) => {
            // A chunk that already posted is a visible reply: report `done`
            // with the first posted id, or the other seat is promoted into a
            // second answer. Release only when nothing went out.
            if let Some(ref_id) = claimed {
                match e.sent_ids.first() {
                    Some(reply_id) => {
                        report_reply_done(ctx, channel_id, ref_id, MessageId::new(*reply_id)).await;
                    }
                    None => release_reply_claim(ctx, channel_id, ref_id).await,
                }
            }
            json!({ "error": e.message })
        }
    }
}

fn validate_evidence_chunking(config: &LoadedConfig, content: &str) -> Result<(), Value> {
    if !config.delivery.evidence_markers_enabled {
        return Ok(());
    }
    if parse_sentex_locators(content).is_empty() {
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
            "error": "sentex-bearing messages must fit in one Discord message"
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

    let chunks = chunk_preserving_fences_with_context(
        content,
        effective_limit,
        effective_mode,
        request.fence_context(),
    )
    .map_err(|error| DeliverError::total(error.to_string()))?;
    let mut sent_ids: Vec<u64> = Vec::new();
    let mut first_msg_id: Option<MessageId> = None;
    let mut bot_author: Option<(UserId, BotDisplayName)> = None;

    for (i, chunk) in chunks.iter().enumerate() {
        let mut builder = CreateMessage::new().content(&chunk.rendered);

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
                    request.set_fence_context(chunks[i].incoming.clone());
                    Some(content[chunks[i].source.start..].to_string())
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
    if let Err(error) = reject_raw_sentex_locators(content) {
        return error;
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

    let own_send = ctx.state.read().await.is_own_send(message_id.get());
    if let Err(e) = verify_message_target(
        &ctx.ingress_ledger,
        &ctx.http,
        &ctx.config,
        message_id,
        channel_id,
        "react",
        TargetPolicy::CanonicalReplyOrReact(own_send),
    )
    .await
    {
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
    // Reply linkage, mirroring the push-notification meta: prefer the
    // `message_reference` id (present even when the referenced message was
    // deleted), fall back to the resolved `referenced_message`. Keys are
    // omitted for non-replies so existing output stays byte-identical.
    let reply_to_message_id = m
        .message_reference
        .as_ref()
        .and_then(|r| r.message_id)
        .or_else(|| m.referenced_message.as_deref().map(|r| r.id));
    if let Some(reply_id) = reply_to_message_id {
        message["reply_to_message_id"] = json!(reply_id.get().to_string());
    }
    if let Some(referenced) = m.referenced_message.as_deref() {
        message["reply_to_user_id"] = json!(referenced.author.id.get().to_string());
    }
    if config.delivery.evidence_markers_enabled {
        project_sentexes(&mut message, &m.content, m.author.id);
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
    send_dm_with_evidence_and_hook_overrides(
        ctx,
        user_id,
        content,
        no_rly_hooks,
        &NO_SENTEX_HANDLES,
    )
    .await
}

pub(crate) async fn send_dm_with_evidence_and_hook_overrides(
    ctx: &MessagingCtx,
    user_id: UserId,
    content: &str,
    no_rly_hooks: &[HookName],
    sentex_handles: &SentexHandles,
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
        .with_sentex_handles(sentex_handles),
    )
    .await
    {
        Ok(prepared) => prepared,
        Err(error) => return error,
    };

    if ctx.config.delivery.evidence_markers_enabled
        && !sentex_handles.is_empty()
        && parse_sentex_locators(&prepared.text).is_empty()
    {
        return json!({
            "error": "sentex locators cannot be attached inside quoted or fenced content"
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
        Ok(m) => {
            let mut projected = get_message_json(&ctx.config, &m);
            if ctx.config.vaelii.is_enabled() {
                match crate::vaelii::write_get_message_receipt(
                    &ctx.config.vaelii,
                    message_id.get(),
                    ctx.construct_id.as_str(),
                )
                .await
                {
                    Ok(Some(receipt)) => {
                        projected["vaelii_receipt"] = json!({
                            "ok": true,
                            "invocation": receipt.invocation,
                            "receipt": receipt.receipt,
                        });
                    }
                    Ok(None) => {}
                    Err(error) => {
                        tracing::warn!(%error, message_id = message_id.get(), "Vaelii receipt write failed");
                        projected["vaelii_receipt"] = json!({
                            "ok": false,
                            "error": error.to_string(),
                        });
                    }
                }
            }
            projected
        }
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
        mute_store::{GuildMute, MuteState, MuteStore},
        no_rly::judge::{ReasonEntry, RejectReason},
        pre_send::{
            Assessment, AuditSink, AuditTrail, ConstructFeedback, FeedbackSink, HookContext,
            HookDecision, HookOutput, PipelineMode, PreSendHook, PreSendPipeline, SinkError,
            SinkFailurePolicy,
        },
        state::new_state,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::{
        io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

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

    struct RawSentexRewriteHook(&'static str);

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

    impl PreSendHook for RawSentexRewriteHook {
        fn name(&self) -> HookName {
            HookName::parse("raw-sentex-rewrite").unwrap()
        }

        fn execute(&self, _context: &HookContext) -> HookOutput {
            HookOutput::new(
                HookDecision::Rewrite {
                    text: format!("rewritten {}", self.0),
                },
                ConstructFeedback::default(),
                AuditTrail::default(),
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

    fn blocking_test_config_with_evidence_markers(enabled: bool) -> LoadedConfig {
        let mut raw = Config::default();
        raw.delivery.evidence_markers_enabled = enabled;
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

    fn blocking_test_config() -> LoadedConfig {
        blocking_test_config_with_evidence_markers(true)
    }

    fn configured_ingress_test_config() -> LoadedConfig {
        let mut raw = Config::default();
        raw.channels.push(ChannelConfig {
            id: "42".into(),
            require_mention: false,
            ..Default::default()
        });
        raw.phantom_canary.alert_channel_id = "99".into();
        LoadedConfig::from_raw(raw)
    }

    #[test]
    fn ingress_rejections_route_alerts_by_verdict() {
        let config = configured_ingress_test_config();
        let ledger = IngressLedger::new();
        ledger.note_admitted(
            MessageId::new(7),
            ChannelId::new(41),
            UserId::new(100),
            "known message",
        );
        let mut alerts = Vec::new();

        let mismatch = verify_message_target_with_alert(
            ledger.verify(MessageId::new(7), ChannelId::new(42)),
            &config,
            MessageId::new(7),
            ChannelId::new(42),
            "react",
            false,
            |channel, content| alerts.push((channel, content)),
        )
        .expect_err("a channel mismatch must be blocked");

        assert_eq!(mismatch["reason"], "channel_mismatch");
        assert_eq!(mismatch["message_id"], "7");
        assert_eq!(mismatch["admitted_channel_id"], "41");
        assert_eq!(mismatch["claimed_channel_id"], "42");
        assert_eq!(mismatch["operation"], "react");
        assert_eq!(mismatch["blocked"], true);
        assert!(mismatch["error"].as_str().is_some_and(|error| {
            error.contains("admitted in channel 41")
                && error.contains("claimed channel 42")
                && error.contains("react blocked")
        }));
        assert!(
            alerts.is_empty(),
            "a known mismatch must not page as phantom"
        );

        let unknown = verify_message_target_with_alert(
            ledger.verify(MessageId::new(8), ChannelId::new(42)),
            &config,
            MessageId::new(8),
            ChannelId::new(42),
            "react",
            false,
            |channel, content| alerts.push((channel, content)),
        )
        .expect_err("an unknown target must be blocked");

        assert_eq!(unknown["reason"], "target_not_found");
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].0, ChannelId::new(99));
    }

    /// dione#334: the acting seat's own-authored targets are exempt from the
    /// phantom canary, and nothing else is. Each assertion is a mutation guard —
    /// the comment names the change that turns it red.
    #[test]
    fn own_send_exemption_covers_unknown_and_never_a_spoof() {
        let config = configured_ingress_test_config();
        let ledger = IngressLedger::new();
        let mut alerts = Vec::new();

        // (1) known-mine: an own-authored target is (correctly) absent from the
        // received-message ledger, so its `Unknown` is expected — exempt, no
        // alert, no block. RED if the `Unknown if own_send` arm is deleted.
        verify_message_target_with_alert(
            ledger.verify(MessageId::new(500), ChannelId::new(42)),
            &config,
            MessageId::new(500),
            ChannelId::new(42),
            "react",
            true, // authenticated own-send
            |channel, content| alerts.push((channel, content)),
        )
        .expect("an own-authored target must be exempt from the phantom canary");
        assert!(
            alerts.is_empty(),
            "own-send exemption must not page the phantom canary"
        );

        // (2) spoof guard (Ari's guard — security-critical): the SAME target id
        // with own_send=false — an inbound merely *claiming* our identity, which
        // never passed a send path — must still alert and block. This is also
        // the fail-closed case: no authenticated own-send evidence => not exempt.
        // RED if the arm is weakened to `Unknown =>` (drops the `if own_send`).
        let spoof = verify_message_target_with_alert(
            ledger.verify(MessageId::new(500), ChannelId::new(42)),
            &config,
            MessageId::new(500),
            ChannelId::new(42),
            "react",
            false, // no authenticated own-send record
            |channel, content| alerts.push((channel, content)),
        )
        .expect_err("a target with no authenticated own-send record must be blocked");
        assert_eq!(spoof["reason"], "target_not_found");
        assert_eq!(alerts.len(), 1, "the spoof case must page exactly once");
        assert_eq!(alerts[0].0, ChannelId::new(99));

        // (3) scope: own_send must NOT rescue a channel mismatch — the exemption
        // is `Unknown`-only. Admitted in channel 41, claimed in 42, own_send=true
        // still blocks as a mismatch with no page. RED if the exemption is
        // broadened past the `Unknown` arm.
        ledger.note_admitted(
            MessageId::new(7),
            ChannelId::new(41),
            UserId::new(100),
            "known message",
        );
        let mismatch = verify_message_target_with_alert(
            ledger.verify(MessageId::new(7), ChannelId::new(42)),
            &config,
            MessageId::new(7),
            ChannelId::new(42),
            "react",
            true,
            |channel, content| alerts.push((channel, content)),
        )
        .expect_err("own_send must not exempt a channel mismatch");
        assert_eq!(mismatch["reason"], "channel_mismatch");
        assert_eq!(
            alerts.len(),
            1,
            "a channel mismatch must not page even when own_send is set"
        );
    }

    #[tokio::test]
    async fn reply_to_canonical_message_outside_ingress_window() {
        let (http, requests, server) = fake_discord_http().await;
        let ctx = MessagingCtx::new(
            http,
            new_state(),
            Arc::new(configured_ingress_test_config()),
            "/tmp".into(),
            Arc::new(ConsentGate::new(camino::Utf8Path::new("/tmp"))),
            Arc::new(IngressLedger::new()),
        );

        let result = reply(
            &ctx,
            ChannelId::new(42),
            "answer to earlier message",
            Some(MessageId::new(9001)),
            false,
        )
        .await;

        assert_eq!(
            result["ok"], true,
            "old canonical target should be replyable: {result}"
        );
        let paths: Vec<_> = requests
            .lock()
            .expect("request capture lock")
            .iter()
            .map(|(path, _)| path.clone())
            .collect();
        assert!(
            paths
                .iter()
                .any(|path| path.ends_with("/channels/42/messages/9001"))
        );
        assert!(
            paths
                .iter()
                .any(|path| path.ends_with("/channels/42/messages"))
        );
        server.abort();
    }

    #[tokio::test]
    async fn react_to_canonical_message_outside_ingress_window() {
        let (http, requests, server) = fake_discord_http().await;
        let ctx = messaging_ctx_with_http(configured_ingress_test_config(), http);
        let result = react(&ctx, ChannelId::new(42), MessageId::new(9001), "✅").await;
        assert_eq!(
            result["ok"], true,
            "canonical target should be reactable: {result}"
        );
        let seen = requests.lock().expect("request capture lock");
        assert!(
            seen.iter()
                .any(|(path, _)| path.ends_with("/channels/42/messages/9001"))
        );
        assert!(
            seen.iter()
                .any(|(path, _)| path.contains("/messages/9001/reactions/")),
            "reaction mutation must reach Discord: {seen:?}"
        );
        server.abort();
    }

    #[tokio::test]
    async fn mention_gated_channel_rejects_unmentioned_canonical_target() {
        let (http, requests, server) = fake_discord_http().await;
        let mut raw = Config::default();
        raw.channels.push(ChannelConfig {
            id: "42".into(),
            ..Default::default()
        });
        let ctx = messaging_ctx_with_http(LoadedConfig::from_raw(raw), http);
        let result = reply(
            &ctx,
            ChannelId::new(42),
            "must not reply",
            Some(MessageId::new(9001)),
            false,
        )
        .await;
        assert_eq!(result["reason"], "canonical_requires_ingress");
        let seen = requests.lock().expect("request capture lock");
        assert_eq!(seen.len(), 1, "no reply may be sent: {seen:?}");
        assert!(seen[0].0.ends_with("/channels/42/messages/9001"));
        server.abort();
    }

    #[tokio::test]
    async fn unledgered_management_attempt_alerts_without_claiming_discord_404() {
        let (http, requests, server) = fake_discord_http().await;
        let config = configured_ingress_test_config();
        let result = verify_message_target(
            &IngressLedger::new(),
            &http,
            &config,
            MessageId::new(9001),
            ChannelId::new(42),
            "delete_message",
            TargetPolicy::IngressOrOwnSend(false),
        )
        .await
        .expect_err("unledgered management must block");
        assert_eq!(result["reason"], "ingress_required");
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if !requests.lock().expect("request capture lock").is_empty() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("canary alert delivered");
        let seen = requests.lock().expect("request capture lock");
        assert_eq!(seen.len(), 1, "no target fetch or mutation: {seen:?}");
        assert!(seen[0].0.ends_with("/channels/99/messages"));
        assert!(seen[0].1.contains("absent from ingress ledger"));
        assert!(!seen[0].1.contains("Unknown Message"));
        server.abort();
    }

    #[tokio::test]
    async fn restricted_channel_does_not_admit_old_message_by_existence_alone() {
        let (http, requests, server) = fake_discord_http().await;
        let mut raw = Config::default();
        raw.channels.push(ChannelConfig {
            id: "42".into(),
            allow_from: vec!["100".into()],
            require_mention: false,
            ..Default::default()
        });
        raw.phantom_canary.alert_channel_id = "99".into();
        let ctx = messaging_ctx_with_http(LoadedConfig::from_raw(raw), http);

        let result = reply(
            &ctx,
            ChannelId::new(42),
            "do not cross audience gate",
            Some(MessageId::new(9001)),
            false,
        )
        .await;

        assert_eq!(result["reason"], "canonical_requires_ingress");
        {
            let seen = requests.lock().expect("request capture lock");
            assert_eq!(seen.len(), 1, "real target must not reach a mutation");
            assert!(seen[0].0.ends_with("/channels/42/messages/9001"));
        }

        let missing = react(&ctx, ChannelId::new(42), MessageId::new(8), "✅").await;
        assert_eq!(missing["reason"], "target_not_found");
        server.abort();
    }

    #[tokio::test]
    async fn ignored_author_is_not_admitted_by_canonical_lookup() {
        let (http, requests, server) = fake_discord_http().await;
        let mut raw = Config::default();
        raw.channels.push(ChannelConfig {
            id: "42".into(),
            require_mention: false,
            ..Default::default()
        });
        raw.access.ignore_from.push("210987654321098765".into());
        let ctx = messaging_ctx_with_http(LoadedConfig::from_raw(raw), http);
        let result = reply(
            &ctx,
            ChannelId::new(42),
            "do not reply to ignored author",
            Some(MessageId::new(9001)),
            false,
        )
        .await;

        assert_eq!(result["reason"], "canonical_requires_ingress");
        let seen = requests.lock().expect("request capture lock");
        assert_eq!(seen.len(), 1, "ignored author must not reach a mutation");
        assert!(seen[0].0.ends_with("/channels/42/messages/9001"));
        server.abort();
    }

    #[tokio::test]
    async fn canonical_fallback_rejects_filtered_bot_and_dropped_reply_chain() {
        let (http, requests, server) = fake_discord_http().await;
        let ctx = messaging_ctx_with_http(configured_ingress_test_config(), http);
        let channel = ChannelId::new(42);
        crate::drop_ledger::global().record(channel, MessageId::new(9006));
        for id in [9004, 9006, 9007] {
            let result = reply(
                &ctx,
                channel,
                "must not send",
                Some(MessageId::new(id)),
                false,
            )
            .await;
            assert_eq!(
                result["reason"], "canonical_requires_ingress",
                "target {id}"
            );
        }
        let seen = requests.lock().expect("request capture lock");
        assert_eq!(
            seen.len(),
            3,
            "filtered targets cannot reach mutation or alert: {seen:?}"
        );
        assert!(seen.iter().all(|(path, _)| path.contains("/messages/")));
        server.abort();
    }

    #[tokio::test]
    async fn canonical_fallback_checks_active_mute_and_resolves_missing_rest_guild() {
        let (http, requests, server) = fake_discord_http().await;
        let now = chrono::Utc::now();
        let mut state = MuteState::default();
        state.mutes.insert(
            500,
            GuildMute {
                guild_id: 500,
                muted_until: now + chrono::Duration::minutes(5),
                muted_by: "test".into(),
                reason: None,
                muted_at: now,
                cutoff_event_id: String::new(),
            },
        );
        let muted = MuteStore::from_state(state, camino::Utf8Path::new("/tmp"));
        let config = configured_ingress_test_config();
        let ledger = IngressLedger::new();
        for id in [9001, 9008] {
            let result = verify_message_target_with_mute_store(
                TargetVerificationContext {
                    ledger: &ledger,
                    http: &http,
                    config: &config,
                    mute_store_override: Some(&muted),
                },
                MessageId::new(id),
                ChannelId::new(42),
                "reply_to",
                TargetPolicy::CanonicalReplyOrReact(false),
            )
            .await
            .expect_err("muted guild must not admit a canonical target");
            assert_eq!(
                result["reason"], "canonical_requires_ingress",
                "target {id}"
            );
        }
        let seen = requests.lock().expect("request capture lock");
        assert_eq!(
            seen.len(),
            3,
            "only two target GETs and one channel GET: {seen:?}"
        );
        assert!(seen.iter().any(|(path, _)| path.ends_with("/channels/42")));
        assert!(seen.iter().all(|(path, _)| !path.ends_with("/messages")));
        server.abort();
    }

    // Isolate the process-global OnceLock from parallel unit tests.
    #[tokio::test]
    async fn reply_and_react_respect_global_mute_on_old_canonical_targets() {
        const CHILD: &str = "DIONE_GLOBAL_MUTE_REGRESSION_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let status = std::process::Command::new(std::env::current_exe().expect("test binary"))
                .arg("--exact")
                .arg("mcp::tools::messaging::tests::reply_and_react_respect_global_mute_on_old_canonical_targets")
                .env(CHILD, "1")
                .status()
                .expect("run isolated mute regression");
            assert!(
                status.success(),
                "isolated production-path mute regression failed"
            );
            return;
        }

        let now = chrono::Utc::now();
        let mut state = MuteState::default();
        state.mutes.insert(
            500,
            GuildMute {
                guild_id: 500,
                muted_until: now + chrono::Duration::minutes(5),
                muted_by: "test".into(),
                reason: None,
                muted_at: now,
                cutoff_event_id: String::new(),
            },
        );
        crate::mute_store::init_global(MuteStore::from_state(state, camino::Utf8Path::new("/tmp")));
        assert!(
            crate::mute_store::global()
                .expect("global store installed")
                .is_guild_muted(500)
        );

        let (http, requests, server) = fake_discord_http().await;
        let ctx = messaging_ctx_with_http(configured_ingress_test_config(), http);
        let replied = reply(
            &ctx,
            ChannelId::new(42),
            "must not reply",
            Some(MessageId::new(9001)),
            false,
        )
        .await;
        assert_eq!(replied["reason"], "canonical_requires_ingress");
        let reacted = react(&ctx, ChannelId::new(42), MessageId::new(9008), "✅").await;
        assert_eq!(reacted["reason"], "canonical_requires_ingress");
        let seen = requests.lock().expect("request capture lock");
        assert_eq!(
            seen.len(),
            3,
            "only target and channel GETs allowed: {seen:?}"
        );
        assert!(
            seen.iter()
                .any(|(path, _)| path.ends_with("/channels/42/messages/9001"))
        );
        assert!(
            seen.iter()
                .any(|(path, _)| path.ends_with("/channels/42/messages/9008"))
        );
        assert!(seen.iter().any(|(path, _)| path.ends_with("/channels/42")));
        assert!(
            seen.iter()
                .all(|(path, _)| !path.contains("/reactions/") && !path.ends_with("/messages"))
        );
        server.abort();
    }
    #[tokio::test]
    async fn inaccessible_old_target_does_not_raise_phantom_canary() {
        let (http, requests, server) = fake_discord_http().await;
        let ctx = messaging_ctx_with_http(configured_ingress_test_config(), http);

        let result = reply(
            &ctx,
            ChannelId::new(42),
            "do not send",
            Some(MessageId::new(9003)),
            false,
        )
        .await;

        assert_eq!(result["reason"], "canonical_lookup_failed");
        let seen = requests.lock().expect("request capture lock");
        assert_eq!(seen.len(), 1, "lookup failures must not mutate or alert");
        assert!(seen[0].0.ends_with("/channels/42/messages/9003"));
        server.abort();
    }

    #[tokio::test]
    async fn ingress_rejections_do_not_reach_discord_mutation() {
        let (http, requests, server) = fake_discord_http().await;
        let mut raw = Config::default();
        raw.channels.push(ChannelConfig {
            id: "42".into(),
            ..Default::default()
        });
        let ledger = Arc::new(IngressLedger::new());
        ledger.note_admitted(
            MessageId::new(7),
            ChannelId::new(41),
            UserId::new(100),
            "known message",
        );
        let ctx = MessagingCtx::new(
            http,
            new_state(),
            Arc::new(LoadedConfig::from_raw(raw)),
            "/tmp".into(),
            Arc::new(ConsentGate::new(camino::Utf8Path::new("/tmp"))),
            ledger,
        );

        let mismatch = react(&ctx, ChannelId::new(42), MessageId::new(7), "✅").await;
        let unknown = react(&ctx, ChannelId::new(42), MessageId::new(8), "✅").await;
        let mismatched_reply = reply(
            &ctx,
            ChannelId::new(42),
            "reply body",
            Some(MessageId::new(7)),
            false,
        )
        .await;
        let unknown_reply = reply(
            &ctx,
            ChannelId::new(42),
            "reply body",
            Some(MessageId::new(8)),
            false,
        )
        .await;
        let disallowed_reply = reply(
            &ctx,
            ChannelId::new(43),
            "reply body",
            Some(MessageId::new(7)),
            false,
        )
        .await;

        assert_eq!(mismatch["reason"], "channel_mismatch");
        assert_eq!(mismatched_reply["reason"], "channel_mismatch");
        assert_eq!(unknown["reason"], "target_not_found");
        assert_eq!(unknown_reply["reason"], "target_not_found");
        assert!(
            disallowed_reply.get("admitted_channel_id").is_none(),
            "an unauthorized destination must not expose ledger provenance"
        );
        let seen = requests.lock().expect("request capture lock");
        assert_eq!(seen.len(), 2, "only unknown targets need canonical lookup");
        assert!(
            seen.iter()
                .all(|(path, _)| path.ends_with("/channels/42/messages/8")),
            "rejections must not reach a Discord mutation: {seen:?}"
        );
        server.abort();
    }

    /// dione#334 end-to-end through the real `react` call site: a recorded
    /// own-send is exempt and reaches Discord; a non-own unknown target is
    /// blocked before the boundary. This pins the caller wiring the private
    /// `verify_message_target_with_alert` unit test cannot — it fails if `react`
    /// drops/inverts `own_send`, or if `is_own_send` stops consulting state.
    #[tokio::test]
    async fn react_exempts_recorded_own_send_and_blocks_non_own() {
        let (http, requests, server) = fake_discord_http().await;
        let mut raw = Config::default();
        raw.channels.push(ChannelConfig {
            id: "42".into(),
            ..Default::default()
        });
        raw.phantom_canary.alert_channel_id = "99".into();
        // Empty ledger: every target verifies as Unknown, so only the own-send
        // signal can distinguish exempt from blocked.
        let ledger = Arc::new(IngressLedger::new());
        let ctx = MessagingCtx::new(
            http,
            new_state(),
            Arc::new(LoadedConfig::from_raw(raw)),
            "/tmp".into(),
            Arc::new(ConsentGate::new(camino::Utf8Path::new("/tmp"))),
            ledger,
        );

        // Non-own target: blocked by the canary before any Discord mutation.
        let blocked = react(&ctx, ChannelId::new(42), MessageId::new(8), "✅").await;
        assert_eq!(blocked["reason"], "target_not_found");

        // Own send recorded via note_sent: exempt, reaction reaches Discord.
        ctx.state.write().await.note_sent(500);
        let exempt = react(&ctx, ChannelId::new(42), MessageId::new(500), "✅").await;
        assert_ne!(exempt["reason"], "target_not_found");

        let seen = requests.lock().expect("request capture lock");
        assert!(
            seen.iter()
                .any(|(path, _)| path.contains("/messages/500/reactions")),
            "the exempt own-send reaction must reach the Discord boundary; saw {seen:?}"
        );
        assert!(
            !seen
                .iter()
                .any(|(path, _)| path.contains("/messages/8/reactions")),
            "the blocked non-own reaction must not reach the Discord boundary; saw {seen:?}"
        );
        server.abort();
    }

    fn messaging_ctx(config: LoadedConfig) -> MessagingCtx {
        messaging_ctx_with_http(config, Arc::new(serenity::http::Http::new("fake")))
    }

    fn messaging_ctx_with_http(
        config: LoadedConfig,
        http: Arc<serenity::http::Http>,
    ) -> MessagingCtx {
        MessagingCtx::new(
            http,
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
            // Each posted message gets its own id (9001, 9002, ...), so a
            // test can tell one chunk's id from another's.
            let mut next_post_id = 9001u64;
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
                } else if request_line.starts_with("GET ")
                    // 7001 is a source message whose id no posted reply
                    // shares (posted ids start at 9001).
                    && [7001, 9001, 9004, 9006, 9007, 9008]
                        .iter()
                        .any(|id| path.ends_with(&format!("/channels/42/messages/{id}")))
                {
                    let id: u64 = path.rsplit('/').next().unwrap().parse().unwrap();
                    let mut message = wire_message(
                        id,
                        "explicit read",
                        "2026-08-15T09:00:00.000000+00:00",
                        json!([]),
                    );
                    message["channel_id"] = json!("42");
                    if id != 9008 {
                        message["guild_id"] = json!("500");
                    }
                    if id == 9004 {
                        message["author"]["bot"] = json!(true);
                    }
                    if id == 9007 {
                        message["message_reference"] =
                            json!({"message_id":"9006", "channel_id":"42", "guild_id":"500"});
                    }
                    ("200 OK", message.to_string())
                } else if request_line.starts_with("GET ") && path.ends_with("/channels/42") {
                    (
                        "200 OK",
                        json!({"id":"42", "type":0, "guild_id":"500",
                        "position":0, "permission_overwrites":[], "name":"fixture",
                        "nsfw":false, "parent_id":null, "topic":null,
                        "last_message_id":null})
                        .to_string(),
                    )
                } else if request_line.starts_with("GET ")
                    && path.ends_with("/channels/42/messages/8")
                {
                    (
                        "404 Not Found",
                        json!({ "message": "Unknown Message", "code": 10008 }).to_string(),
                    )
                } else if request_line.starts_with("GET ")
                    && path.ends_with("/channels/42/messages/9003")
                {
                    (
                        "403 Forbidden",
                        json!({ "message": "Missing Access", "code": 50001 }).to_string(),
                    )
                } else if request_line.starts_with("GET ") && path.contains("/messages/") {
                    (
                        "200 OK",
                        wire_message(
                            9001,
                            "explicit read",
                            "2026-08-15T09:00:00.000000+00:00",
                            json!([]),
                        )
                        .to_string(),
                    )
                } else if request_line.starts_with("GET ") && path.contains("/messages?") {
                    (
                        "200 OK",
                        json!([wire_message(
                            9002,
                            "incidental fetch",
                            "2026-08-15T09:00:01.000000+00:00",
                            json!([]),
                        )])
                        .to_string(),
                    )
                } else if request_line.starts_with("PUT ") && path.contains("/reactions/") {
                    ("204 No Content", String::new())
                } else if request_line.starts_with("POST ")
                    && path.ends_with("/messages")
                    && body.contains("force-delivery-failure")
                {
                    (
                        "403 Forbidden",
                        json!({ "message": "Missing Permissions", "code": 50013 }).to_string(),
                    )
                } else if request_line.starts_with("POST ") && path.ends_with("/messages") {
                    let content = serde_json::from_str::<Value>(body)
                        .ok()
                        .and_then(|body| body["content"].as_str().map(str::to_owned))
                        .unwrap_or_default();
                    let id = next_post_id;
                    next_post_id += 1;
                    (
                        "200 OK",
                        wire_message(id, &content, "2026-08-15T09:00:00.000000+00:00", json!([]))
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

    async fn fake_vaelii_http(
        status: &'static str,
    ) -> (
        String,
        Arc<std::sync::Mutex<Option<String>>>,
        tokio::task::JoinHandle<()>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind fake Vaelii API");
        let address = listener.local_addr().expect("fake Vaelii API address");
        let captured = Arc::new(std::sync::Mutex::new(None));
        let request_slot = Arc::clone(&captured);
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept Vaelii request");
            let mut request = Vec::new();
            let mut buffer = [0_u8; 4096];
            loop {
                let read = stream.read(&mut buffer).await.expect("read Vaelii request");
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
                let Some(header_end) = request.windows(4).position(|window| window == b"\r\n\r\n")
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
            *request_slot.lock().expect("request capture lock") =
                Some(String::from_utf8(request).expect("HTTP request is UTF-8"));
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/edn\r\nContent-Length: 10\r\nConnection: close\r\n\r\n{{:ok true}}"
            );
            stream
                .write_all(response.as_bytes())
                .await
                .expect("write Vaelii response");
        });
        (format!("http://{address}"), captured, server)
    }

    #[tokio::test]
    async fn explicit_get_message_writes_receipt_only_when_vaelii_url_is_configured() {
        let (discord_http, _discord_requests, discord_server) = fake_discord_http().await;
        let mut raw = Config::default();
        raw.channels.push(ChannelConfig {
            id: "42".into(),
            ..Default::default()
        });
        let ctx = messaging_ctx_with_http(LoadedConfig::from_raw(raw), discord_http);

        let projected = get_message(&ctx, ChannelId::new(42), MessageId::new(9001)).await;
        assert_eq!(projected["content"], "explicit read");
        assert!(projected.get("vaelii_receipt").is_none());
        discord_server.abort();

        let (discord_http, _discord_requests, discord_server) = fake_discord_http().await;
        let (server_url, vaelii_request, vaelii_server) = fake_vaelii_http("200 OK").await;
        let mut raw = Config::default();
        raw.channels.push(ChannelConfig {
            id: "42".into(),
            ..Default::default()
        });
        raw.vaelii.server_url = Some(server_url);
        raw.vaelii.actor_term = Some("Syne".to_owned());
        let ctx = messaging_ctx_with_http(LoadedConfig::from_raw(raw), discord_http);

        let projected = get_message(&ctx, ChannelId::new(42), MessageId::new(9001)).await;
        assert_eq!(projected["content"], "explicit read");
        assert_eq!(projected["vaelii_receipt"]["ok"], true);
        assert_eq!(projected["vaelii_receipt"]["invocation"], "Check9001");
        vaelii_server.await.unwrap();
        let request = vaelii_request
            .lock()
            .expect("Vaelii request capture lock")
            .clone()
            .expect("Vaelii request was sent");
        assert!(request.starts_with("POST /op HTTP/1.1\r\n"));
        discord_server.abort();
    }

    #[tokio::test]
    async fn configured_receipt_failure_preserves_the_retrieved_message() {
        let (discord_http, _discord_requests, discord_server) = fake_discord_http().await;
        let (server_url, _vaelii_request, vaelii_server) =
            fake_vaelii_http("500 Internal Server Error").await;
        let mut raw = Config::default();
        raw.channels.push(ChannelConfig {
            id: "42".into(),
            ..Default::default()
        });
        raw.vaelii.server_url = Some(server_url);
        raw.vaelii.actor_term = Some("Syne".to_owned());
        let ctx = messaging_ctx_with_http(LoadedConfig::from_raw(raw), discord_http);

        let projected = get_message(&ctx, ChannelId::new(42), MessageId::new(9001)).await;
        assert_eq!(projected["content"], "explicit read");
        assert_eq!(projected["vaelii_receipt"]["ok"], false);
        assert_eq!(
            projected["vaelii_receipt"]["error"],
            "Vaelii receipt write returned HTTP 500 Internal Server Error"
        );
        vaelii_server.await.unwrap();
        discord_server.abort();
    }

    #[tokio::test]
    async fn incidental_fetch_does_not_write_a_vaelii_receipt() {
        let (discord_http, _discord_requests, discord_server) = fake_discord_http().await;
        let vaelii_listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind receipt tripwire");
        let vaelii_address = vaelii_listener
            .local_addr()
            .expect("receipt tripwire address");
        let mut raw = Config::default();
        raw.channels.push(ChannelConfig {
            id: "42".into(),
            ..Default::default()
        });
        raw.vaelii.server_url = Some(format!("http://{vaelii_address}"));
        raw.vaelii.actor_term = Some("Syne".to_owned());
        let ctx = messaging_ctx_with_http(LoadedConfig::from_raw(raw), discord_http);

        let projected = fetch_messages(&ctx, ChannelId::new(42), None, None, 10).await;
        assert_eq!(projected["messages"][0]["content"], "incidental fetch");
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(50),
                vaelii_listener.accept()
            )
            .await
            .is_err(),
            "fetch_messages must not connect to Vaelii"
        );
        discord_server.abort();
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
    async fn ordered_sentex_metadata_and_transport_reach_hooks_and_final_audit_context() {
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
        let handles = crate::evidence::parse_tool_sentex_handles(&json!({
            "claim_handles": ["34"],
            "citation_handles": ["12"]
        }))
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
            .with_sentex_handles(&handles),
        )
        .await
        .unwrap();

        let hook_context = hook_context.lock().unwrap().clone().unwrap();
        assert_eq!(hook_context.text(), "original");
        assert_eq!(
            hook_context.metadata("claim_locators"),
            Some("v2:claim:AAAAAAAAACI")
        );
        assert_eq!(
            hook_context.metadata("citation_locators"),
            Some("v2:citation:AAAAAAAAAAw")
        );
        assert_eq!(
            hook_context.metadata("sentex_transport"),
            Some("terminal-visible-role-suffix-v2-after-hooks")
        );

        let audit_context = audit_context.lock().unwrap().clone().unwrap();
        assert_eq!(audit_context.text(), "rewritten");
        assert_eq!(
            audit_context.metadata("claim_locators"),
            Some("v2:claim:AAAAAAAAACI")
        );
        assert_eq!(
            audit_context.metadata("citation_locators"),
            Some("v2:citation:AAAAAAAAAAw")
        );
        assert_eq!(
            audit_context.metadata("sentex_transport"),
            Some(SentexTransport::TerminalVisibleRoleSuffixV2AfterHooks.as_str())
        );
        assert_eq!(
            prepared.text,
            "rewritten [🔍=v2:claim:AAAAAAAAACI] [🔍=v2:citation:AAAAAAAAAAw]"
        );
    }

    #[tokio::test]
    async fn raw_terminal_sentex_locators_cannot_bypass_structured_handle_input() {
        for enabled in [false, true] {
            let mut raw = Config::default();
            raw.delivery.evidence_markers_enabled = enabled;
            raw.channels.push(ChannelConfig {
                id: "42".into(),
                ..Default::default()
            });
            let ctx = messaging_ctx(LoadedConfig::from_raw(raw));

            for marker in [
                "[🔍=v1:AAAAAAAAAAw]",
                "[🔍=v2:claim:AAAAAAAAAAw]",
                "[🔍=v2:citation:AAAAAAAAAAw]",
                "[🔍=v2:claim:garbage]",
                concat!(
                    "[🔍=v2:claim:AAAAAAAAAAE] ",
                    "[🔍=v2:claim:AAAAAAAAAAI] ",
                    "[🔍=v2:claim:AAAAAAAAAAM] ",
                    "[🔍=v2:claim:AAAAAAAAAAQ] ",
                    "[🔍=v2:claim:AAAAAAAAAAU]"
                ),
            ] {
                let content = format!("raw {marker}");
                let error = prepare_outbound(
                    &ctx,
                    OutboundDraft::channel(
                        ChannelId::new(42),
                        &content,
                        None,
                        PreSendOptions {
                            surface: OutboundSurface::Reply,
                            bypasses: &[],
                        },
                    ),
                )
                .await
                .unwrap_err();
                assert_eq!(
                    error["error"],
                    "raw terminal sentex locators are not accepted in content; use structured handles on reply or send_dm"
                );
            }
        }
    }

    #[tokio::test]
    async fn raw_terminal_sentex_locators_are_rejected_before_hooks_run() {
        let calls = Arc::new(AtomicUsize::new(0));
        let pipeline = PreSendPipeline::new(vec![Box::new(CountingDecisionHook {
            decision: HookDecision::Rewrite {
                text: "raw locator removed".to_owned(),
            },
            calls: calls.clone(),
        })])
        .expect("pipeline")
        .with_mode(PipelineMode::Enforce);
        let ctx = messaging_ctx(test_config()).with_pre_send_pipeline(Arc::new(pipeline));

        let error = prepare_outbound(
            &ctx,
            OutboundDraft::channel(
                ChannelId::new(42),
                "raw [🔍=v2:claim:AAAAAAAAAAw]",
                None,
                PreSendOptions {
                    surface: OutboundSurface::Reply,
                    bypasses: &[],
                },
            ),
        )
        .await
        .unwrap_err();

        assert_eq!(
            error["error"],
            "raw terminal sentex locators are not accepted in content; use structured handles on reply or send_dm"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn mid_message_sentex_prose_before_an_unrelated_bracket_is_allowed() {
        let ctx = messaging_ctx(blocking_test_config());
        let content = "discussion [🔍=v2:claim:garbage] (see appendix [A])";

        let prepared = prepare_outbound(
            &ctx,
            OutboundDraft::channel(
                ChannelId::new(42),
                content,
                None,
                PreSendOptions {
                    surface: OutboundSurface::Reply,
                    bypasses: &[],
                },
            ),
        )
        .await
        .expect("mid-message sentex prose is not a terminal locator");

        assert_eq!(prepared.text, content);
    }

    #[tokio::test]
    async fn pre_send_rewrite_cannot_inject_an_unaudited_sentex_locator() {
        for marker in [
            "[🔍=v2:claim:AAAAAAAAAAw]",
            "[🔍=v2:claim:garbage]",
            concat!(
                "[🔍=v2:claim:AAAAAAAAAAE] ",
                "[🔍=v2:claim:AAAAAAAAAAI] ",
                "[🔍=v2:claim:AAAAAAAAAAM] ",
                "[🔍=v2:claim:AAAAAAAAAAQ] ",
                "[🔍=v2:claim:AAAAAAAAAAU]"
            ),
        ] {
            let mut raw = Config::default();
            raw.delivery.evidence_markers_enabled = true;
            raw.channels.push(ChannelConfig {
                id: "42".into(),
                ..Default::default()
            });
            let pipeline = PreSendPipeline::new(vec![Box::new(RawSentexRewriteHook(marker))])
                .expect("pipeline")
                .with_mode(PipelineMode::Enforce);
            let ctx = messaging_ctx(LoadedConfig::from_raw(raw))
                .with_pre_send_pipeline(Arc::new(pipeline));

            let error = prepare_outbound(
                &ctx,
                OutboundDraft::channel(
                    ChannelId::new(42),
                    "plain",
                    None,
                    PreSendOptions {
                        surface: OutboundSurface::Reply,
                        bypasses: &[],
                    },
                ),
            )
            .await
            .unwrap_err();

            assert_eq!(
                error["error"],
                "raw terminal sentex locators are not accepted in content; use structured handles on reply or send_dm"
            );
        }
    }

    #[tokio::test]
    async fn sentex_bearing_multi_chunk_reply_fails_before_discord_delivery() {
        let mut raw = Config::default();
        raw.delivery.evidence_markers_enabled = true;
        raw.channels.push(ChannelConfig {
            id: "42".into(),
            ..Default::default()
        });
        raw.delivery.text_chunk_limit = 8;
        let ctx = messaging_ctx(LoadedConfig::from_raw(raw));
        let handles =
            crate::evidence::parse_tool_sentex_handles(&json!({ "claim_handles": ["12"] }))
                .expect("valid sentex handle");

        let response = reply_with_evidence_and_hook_overrides(
            &ctx,
            ChannelId::new(42),
            "long enough to split",
            None,
            ReplyToolOptions {
                suppress_ping: false,
                no_rly_hooks: &[],
                sentex_handles: &handles,
            },
        )
        .await;

        assert_eq!(
            response["error"],
            "sentex-bearing messages must fit in one Discord message"
        );
    }

    #[tokio::test]
    async fn disabled_outbound_sentexes_are_a_text_no_op() {
        let mut raw = Config::default();
        raw.channels.push(ChannelConfig {
            id: "42".into(),
            ..Default::default()
        });
        let ctx = messaging_ctx(LoadedConfig::from_raw(raw));
        let handles =
            crate::evidence::parse_tool_sentex_handles(&json!({ "claim_handles": ["12"] }))
                .unwrap();

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
            .with_sentex_handles(&handles),
        )
        .await
        .unwrap();

        assert_eq!(prepared.text, "byte exact");
    }

    #[tokio::test]
    async fn sentex_bearing_reply_cannot_enter_held_lifecycle() {
        let ctx = messaging_ctx(blocking_test_config());
        let handles = crate::evidence::parse_tool_sentex_handles(&json!({
            "citation_handles": ["12"]
        }))
        .expect("valid sentex handle");

        let response = reply_with_evidence_and_hook_overrides(
            &ctx,
            ChannelId::new(42),
            "straightforward",
            None,
            ReplyToolOptions {
                suppress_ping: false,
                no_rly_hooks: &[],
                sentex_handles: &handles,
            },
        )
        .await;

        assert_eq!(
            response["error"],
            "sentex-bearing messages cannot enter the no_rly hold lifecycle; revise and send a fresh sentex-bearing reply"
        );
        assert_eq!(ctx.no_rly.pending().await, 0);
    }

    #[tokio::test]
    async fn sentex_bearing_rephrase_is_refused_without_consuming_handle() {
        for enabled in [false, true] {
            let ctx = messaging_ctx(blocking_test_config_with_evidence_markers(enabled));
            let bounce = reply(&ctx, ChannelId::new(42), "straightforward", None, false).await;
            let handle = bounce["held"]["handle"]
                .as_str()
                .expect("ordinary blocked reply should return a handle");
            assert_eq!(ctx.no_rly.pending().await, 1);

            for marker in [
                "[🔍=v2:claim:AAAAAAAAAAw]",
                "[🔍=v2:claim:garbage]",
                concat!(
                    "[🔍=v2:claim:AAAAAAAAAAE] ",
                    "[🔍=v2:claim:AAAAAAAAAAI] ",
                    "[🔍=v2:claim:AAAAAAAAAAM] ",
                    "[🔍=v2:claim:AAAAAAAAAAQ] ",
                    "[🔍=v2:claim:AAAAAAAAAAU]"
                ),
            ] {
                let response = rephrase_held(&ctx, handle, &format!("grounded {marker}")).await;

                assert_eq!(
                    response["error"],
                    "raw terminal sentex locators are not accepted in content; use structured handles on reply or send_dm"
                );
                assert_eq!(ctx.no_rly.pending().await, 1);
            }
        }
    }

    #[tokio::test]
    async fn first_contact_dm_branch_preserves_role_separated_sentex_receipt() {
        let (http, requests, server) = fake_discord_http().await;
        let ctx = MessagingCtx::new(
            http,
            new_state(),
            Arc::new(test_config()),
            "/tmp".into(),
            Arc::new(ConsentGate::new(camino::Utf8Path::new("/tmp"))),
            Arc::new(crate::ingress_ledger::IngressLedger::new()),
        );
        let handles = crate::evidence::parse_tool_sentex_handles(&json!({
            "claim_handles": ["34"],
            "citation_handles": ["12"]
        }))
        .expect("valid sentex handles");

        let result = send_dm_with_evidence_and_hook_overrides(
            &ctx,
            UserId::new(77),
            "grounded",
            &[],
            &handles,
        )
        .await;
        server.abort();

        assert_eq!(
            result,
            json!({
                "ok": true,
                "channel_id": "4242",
                "message_ids": [9001],
                "claim_locators": ["v2:claim:AAAAAAAAACI"],
                "citation_locators": ["v2:citation:AAAAAAAAAAw"],
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
            "grounded [🔍=v2:claim:AAAAAAAAACI] [🔍=v2:citation:AAAAAAAAAAw]"
        );
    }

    #[test]
    fn get_message_and_fetch_share_role_separated_sentex_projection() {
        let content = "grounded [🔍=v2:claim:AAAAAAAAAAw] [🔍=v1:AAAAAAAAACI]";
        let messages = from_wire(json!([wire_message(
            3001,
            content,
            "2026-06-09T12:00:00.000000+00:00",
            json!([])
        )]));

        let fetched = message_json(&test_config(), &messages[0]);
        let single = get_message_json(&test_config(), &messages[0]);
        let expected_claims = json!([{
            "locator": "v2:claim:AAAAAAAAAAw",
            "author_id": "210987654321098765",
        }]);
        let expected_citations = json!([{
            "locator": "v1:AAAAAAAAACI",
            "author_id": "210987654321098765",
        }]);
        assert_eq!(single["content"], fetched["content"]);
        assert_eq!(single["claim_locators"], expected_claims);
        assert_eq!(single["citation_locators"], expected_citations);
        assert_eq!(fetched["claim_locators"], expected_claims);
        assert_eq!(fetched["citation_locators"], expected_citations);
        assert_eq!(single["claim_locators"], fetched["claim_locators"]);
        assert_eq!(single["citation_locators"], fetched["citation_locators"]);
        assert_eq!(single["author_id"], fetched["author_id"]);
        assert!(single.get("evidence").is_none());
        assert!(fetched.get("evidence").is_none());
    }

    #[test]
    fn sentex_projection_is_absent_when_evidence_markers_are_disabled() {
        let content = "grounded [🔍=v2:claim:AAAAAAAAAAw]";
        let messages = from_wire(json!([wire_message(
            3001,
            content,
            "2026-06-09T12:00:00.000000+00:00",
            json!([])
        )]));
        let disabled = LoadedConfig::from_raw(Config::default());

        let projected = message_json(&disabled, &messages[0]);
        assert_eq!(projected["content"], content);
        assert!(projected.get("claim_locators").is_none());
        assert!(projected.get("citation_locators").is_none());
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
        let (http, requests, server) = fake_discord_http().await;
        let mut raw = Config::default();
        raw.channels.push(ChannelConfig {
            id: "42".to_owned(),
            require_mention: false,
            ..Default::default()
        });
        let (tx, mut rx) = mpsc::channel(4);
        let mut ctx = messaging_ctx_with_http(LoadedConfig::from_raw(raw), http);
        ctx.event_tx = Some(tx);

        let result = react(&ctx, ChannelId::new(42), MessageId::new(9001), "👍").await;
        assert_eq!(result["ok"], true);
        assert!(
            requests
                .lock()
                .expect("request capture lock")
                .iter()
                .any(|(path, _)| path.contains("/messages/9001/reactions/"))
        );
        assert!(
            rx.try_recv().is_err(),
            "ordinary reacts must not synthesize notification events"
        );
        server.abort();
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
        let (http, requests, server) = fake_discord_http().await;
        let (mut ctx, _) = messaging_ctx_with_halt_pipeline(LoadedConfig::from_raw(raw));
        ctx.http = http;
        ctx.ingress_ledger = Arc::clone(&ledger);

        // Admitted reply (msg 7 in ch 42) → reaches halt hook.
        let admitted = reply_with_hook_overrides(
            &ctx,
            ChannelId::new(42),
            "text",
            Some(MessageId::new(7)),
            false,
            &[],
        )
        .await;
        assert_eq!(admitted["error"], "blocked by test hook");

        // Unknown reply (msg 9003 not in ledger; REST 403) → blocked before halt hook.
        let unknown = reply_with_hook_overrides(
            &ctx,
            ChannelId::new(42),
            "text",
            Some(MessageId::new(9003)),
            false,
            &[],
        )
        .await;
        assert_eq!(
            unknown["reason"], "canonical_lookup_failed",
            "unverified reply_to must be blocked before reaching hooks: {unknown}"
        );

        // Mismatch reply (msg 8 admitted in ch 41, claimed ch 42) → blocked before halt hook.
        let mismatch = reply_with_hook_overrides(
            &ctx,
            ChannelId::new(42),
            "text",
            Some(MessageId::new(8)),
            false,
            &[],
        )
        .await;
        assert_eq!(
            mismatch["reason"], "channel_mismatch",
            "channel mismatch reply_to must be blocked before reaching hooks: {mismatch}"
        );

        assert_eq!(
            ledger.take_observed_verifications(),
            vec![
                crate::ingress_ledger::VerifyResult::Admitted {
                    channel: crate::ingress_ledger::ChannelRef::new(42),
                },
                crate::ingress_ledger::VerifyResult::Unknown,
                crate::ingress_ledger::VerifyResult::ChannelMismatch {
                    admitted_channel: crate::ingress_ledger::ChannelRef::new(41),
                    claimed_channel: crate::ingress_ledger::ChannelRef::new(42),
                },
            ]
        );
        assert!(
            requests
                .lock()
                .expect("request capture lock")
                .iter()
                .any(|(path, _)| path.ends_with("/channels/42/messages/9003"))
        );
        server.abort();
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
                "username": "example-user",
                "global_name": "Example User",
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

        assert_eq!(msg["author"], "example-user");
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

    #[test]
    fn message_json_carries_reply_linkage_only_for_replies() {
        let mut reply = wire_message(
            3002,
            "replying",
            "2026-06-09T12:01:00.000000+00:00",
            json!([]),
        );
        reply["type"] = json!(19);
        reply["message_reference"] = json!({
            "message_id": "3001",
            "channel_id": "1080000000000000001"
        });
        let mut parent = wire_message(
            3001,
            "original",
            "2026-06-09T12:00:00.000000+00:00",
            json!([]),
        );
        parent["author"]["id"] = json!("333333333333333333");
        reply["referenced_message"] = parent;
        let plain = wire_message(3003, "plain", "2026-06-09T12:02:00.000000+00:00", json!([]));
        let messages = from_wire(json!([reply, plain]));

        let reply_json = message_json(&test_config(), &messages[0]);
        assert_eq!(reply_json["reply_to_message_id"], "3001");
        assert_eq!(reply_json["reply_to_user_id"], "333333333333333333");

        let plain_json = message_json(&test_config(), &messages[1]);
        assert!(
            plain_json.get("reply_to_message_id").is_none()
                && plain_json.get("reply_to_user_id").is_none(),
            "non-replies must not carry reply keys"
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

    /// What the fake claim-once server saw, by message id.
    #[derive(Default)]
    struct ClaimLog {
        /// Message ids claimed (every claim, whatever the outcome).
        claimed: Vec<String>,
        /// Message ids reported `done`.
        done: Vec<String>,
        /// The reply message id each `done` carried, parallel to `done`.
        done_replies: Vec<String>,
        /// Message ids released by their owner.
        released: Vec<String>,
    }

    /// Minimal in-process claim-once server speaking the real server's
    /// shapes: first claim on a key proceeds, later ones from other bots
    /// wait behind the owner; the owner may `done` or `release`.
    async fn fake_claim_server() -> (String, Arc<std::sync::Mutex<ClaimLog>>) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind fake claim-once");
        let addr = listener
            .local_addr()
            .expect("fake claim-once addr")
            .to_string();
        let owners = Arc::new(std::sync::Mutex::new(std::collections::HashMap::<
            String,
            String,
        >::new()));
        let log = Arc::new(std::sync::Mutex::new(ClaimLog::default()));
        let (owners2, log2) = (Arc::clone(&owners), Arc::clone(&log));
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let (owners, log) = (Arc::clone(&owners2), Arc::clone(&log2));
                tokio::spawn(async move {
                    let (read_half, mut write_half) = stream.into_split();
                    let mut lines = tokio::io::BufReader::new(read_half).lines();
                    let mut bot = String::new();
                    while let Ok(Some(line)) = lines.next_line().await {
                        let req: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
                        let mid = req["message_id"].as_str().unwrap_or("").to_owned();
                        let resp = match req.get("msg").and_then(Value::as_str) {
                            Some("hello") => {
                                bot = req["bot_id"].as_str().unwrap_or("").to_owned();
                                json!({ "ok": true })
                            }
                            Some("claim") => {
                                log.lock().unwrap().claimed.push(mid.clone());
                                let mut owners = owners.lock().unwrap();
                                let owner = owners.entry(mid).or_insert_with(|| bot.clone());
                                if *owner == bot {
                                    json!({ "status": "proceed", "lease_ms": 10000 })
                                } else {
                                    json!({ "status": "wait", "ahead": [owner], "lease_ms": 10000 })
                                }
                            }
                            Some(kind @ ("done" | "release")) => {
                                let mut owners = owners.lock().unwrap();
                                if owners.get(&mid) != Some(&bot) {
                                    json!({ "error": "not owner" })
                                } else {
                                    let mut log = log.lock().unwrap();
                                    if kind == "done" {
                                        log.done.push(mid);
                                        log.done_replies.push(
                                            req["reply_message_id"]
                                                .as_str()
                                                .unwrap_or("")
                                                .to_owned(),
                                        );
                                    } else {
                                        owners.remove(&mid);
                                        log.released.push(mid);
                                    }
                                    json!({ "ok": true })
                                }
                            }
                            _ => continue,
                        };
                        if write_half
                            .write_all(format!("{resp}\n").as_bytes())
                            .await
                            .is_err()
                        {
                            return;
                        }
                    }
                });
            }
        });
        (addr, log)
    }

    /// Poll `check` against the fake server's log for up to two seconds.
    async fn eventually(
        log: &std::sync::Mutex<ClaimLog>,
        check: impl Fn(&ClaimLog) -> bool,
    ) -> bool {
        for _ in 0..40 {
            if check(&log.lock().unwrap()) {
                return true;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        false
    }

    /// A `LoadedConfig` with channel 42 opted into a `claim-once` block at
    /// `addr`, identifying as `bot_id`.
    fn coordinated_config(addr: &str, bot_id: u64) -> LoadedConfig {
        let mut raw = Config::default();
        raw.pre_send.author_id = Some(UserId::new(bot_id));
        raw.coordination.insert(
            "claim-once".to_owned(),
            crate::coordination::CoordinationConfig {
                addr: addr.to_owned(),
                lease_ms: 10_000,
                connect_timeout_ms: 2_000,
                fail_open: true,
            },
        );
        raw.channels.push(ChannelConfig {
            id: "42".to_owned(),
            require_mention: false,
            coordinate: Some("claim-once".to_owned()),
            ..Default::default()
        });
        raw.phantom_canary.alert_channel_id = "99".into();
        LoadedConfig::from_raw(raw)
    }

    /// A thread under an opted-in channel resolves to its parent's policy (as
    /// the inbound and outbound gates do), so the second seat waits there too.
    #[tokio::test]
    async fn thread_under_coordinated_channel_is_coordinated() {
        let (addr, _done) = fake_claim_server().await;
        let http = Arc::new(serenity::http::Http::new("fake"));
        let ctx_a = messaging_ctx_with_http(coordinated_config(&addr, 111), Arc::clone(&http));
        let ctx_b = messaging_ctx_with_http(coordinated_config(&addr, 222), Arc::clone(&http));
        for ctx in [&ctx_a, &ctx_b] {
            ctx.state.write().await.record_thread_parent(4242, Some(42));
        }

        assert!(
            coordinate_reply(&ctx_a, ChannelId::new(4242), MessageId::new(7))
                .await
                .is_ok(),
            "first claim in the thread must proceed"
        );
        let err = coordinate_reply(&ctx_b, ChannelId::new(4242), MessageId::new(7))
            .await
            .expect_err("second seat must wait in a thread of a coordinated channel");
        assert!(
            err["error"]
                .as_str()
                .is_some_and(|e| e.contains("already answering")),
            "{err}"
        );
    }

    /// A reply refused by a pre-send hook never goes out, so it must not hold
    /// the claim (the other seat would sit out the whole lease).
    #[tokio::test]
    async fn hook_refused_reply_does_not_hold_the_claim() {
        let (addr, log) = fake_claim_server().await;
        let (http, _requests, server) = fake_discord_http().await;
        let pipeline = PreSendPipeline::new(vec![Box::new(SurfaceHaltHook {
            surfaces: Arc::new(std::sync::Mutex::new(Vec::new())),
        })])
        .expect("valid pipeline")
        .with_mode(PipelineMode::Enforce);
        let ctx_a = messaging_ctx_with_http(coordinated_config(&addr, 111), Arc::clone(&http))
            .with_pre_send_pipeline(Arc::new(pipeline));
        let ctx_b = messaging_ctx_with_http(coordinated_config(&addr, 222), http);

        let refused = reply(
            &ctx_a,
            ChannelId::new(42),
            "no",
            Some(MessageId::new(9001)),
            false,
        )
        .await;
        assert!(
            refused.get("error").is_some(),
            "hook must refuse: {refused}"
        );
        assert!(
            !log.lock().unwrap().claimed.iter().any(|mid| mid == "9001"),
            "a hook-refused reply must not claim"
        );
        assert!(
            coordinate_reply(&ctx_b, ChannelId::new(42), MessageId::new(9001))
                .await
                .is_ok(),
            "a refused reply must leave the message free for the other seat"
        );
        server.abort();
    }

    /// A claimed reply whose delivery fails releases the claim so the next seat
    /// is promoted at once.
    #[tokio::test]
    async fn failed_delivery_releases_the_claim() {
        let (addr, log) = fake_claim_server().await;
        let (http, _requests, server) = fake_discord_http().await;
        let ctx_a = messaging_ctx_with_http(coordinated_config(&addr, 111), Arc::clone(&http));
        let ctx_b = messaging_ctx_with_http(coordinated_config(&addr, 222), http);

        let failed = reply(
            &ctx_a,
            ChannelId::new(42),
            "force-delivery-failure",
            Some(MessageId::new(9001)),
            false,
        )
        .await;
        assert!(
            failed.get("error").is_some(),
            "delivery must fail: {failed}"
        );
        assert!(
            eventually(&log, |log| log.released.iter().any(|mid| mid == "9001")).await,
            "a failed delivery must release its claim"
        );
        assert!(
            coordinate_reply(&ctx_b, ChannelId::new(42), MessageId::new(9001))
                .await
                .is_ok(),
            "after the release the other seat may answer"
        );
        server.abort();
    }

    /// A multi-chunk reply whose first chunk posted before a later chunk failed
    /// is already visible, so it reports `done` with the first posted id.
    /// Releasing would promote the other seat into a double reply.
    ///
    /// The source message (7001) and the posted chunks (9001, 9002) have
    /// distinct ids, so `done` carrying the source id or a later chunk's id
    /// fails here.
    #[tokio::test]
    async fn partly_delivered_reply_reports_done_not_release() {
        let (addr, log) = fake_claim_server().await;
        let (http, requests, server) = fake_discord_http().await;
        let mut raw = coordinated_config(&addr, 111).raw;
        raw.delivery.text_chunk_limit = 24;
        let ctx_a = messaging_ctx_with_http(LoadedConfig::from_raw(raw), Arc::clone(&http));
        let ctx_b = messaging_ctx_with_http(coordinated_config(&addr, 222), http);

        let partial = reply(
            &ctx_a,
            ChannelId::new(42),
            "first chunk goes out\n\nsecond chunk goes too\n\nforce-delivery-failure",
            Some(MessageId::new(7001)),
            false,
        )
        .await;
        assert!(
            partial.get("error").is_some(),
            "the third chunk must fail: {partial}"
        );
        assert_eq!(
            requests
                .lock()
                .unwrap()
                .iter()
                .filter(|(path, _)| path.ends_with("/channels/42/messages"))
                .count(),
            3,
            "two chunks post (9001, 9002) before the third fails"
        );
        assert!(
            eventually(&log, |log| log.done.iter().any(|mid| mid == "7001")).await,
            "a partly-delivered reply must report done"
        );
        {
            let log = log.lock().unwrap();
            assert_eq!(
                log.done_replies,
                vec!["9001".to_owned()],
                "done must carry the first posted chunk's id, not the source's or a later chunk's"
            );
            assert!(
                !log.released.iter().any(|mid| mid == "7001"),
                "a partly-delivered reply must not release its claim"
            );
        }
        assert!(
            coordinate_reply(&ctx_b, ChannelId::new(42), MessageId::new(7001))
                .await
                .is_err(),
            "the other seat must not be promoted into a second reply"
        );
        server.abort();
    }

    /// Reporting `done` is fire-and-forget, so a server that accepts the
    /// connection but never acknowledges must not hold the reply for the ack
    /// timeout.
    #[tokio::test]
    async fn done_report_does_not_block_on_a_stalled_server() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let (read_half, _write_half) = stream.into_split();
                    let mut lines = tokio::io::BufReader::new(read_half).lines();
                    while let Ok(Some(_)) = lines.next_line().await {}
                });
            }
        });
        let ctx = messaging_ctx(coordinated_config(&addr, 111));
        let started = std::time::Instant::now();
        report_reply_done(
            &ctx,
            ChannelId::new(42),
            MessageId::new(7),
            MessageId::new(99),
        )
        .await;
        release_reply_claim(&ctx, ChannelId::new(42), MessageId::new(8)).await;
        assert!(
            started.elapsed() < std::time::Duration::from_millis(500),
            "done/release must not wait on the server: took {:?}",
            started.elapsed()
        );
    }

    /// A claim-once server that records a `claim` but never answers it,
    /// while answering `hello`, `done` and `release` at once: a claim ack
    /// slower than the client's timeout.
    async fn fake_claim_server_silent_on_claim() -> (String, Arc<std::sync::Mutex<ClaimLog>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let log = Arc::new(std::sync::Mutex::new(ClaimLog::default()));
        let log2 = Arc::clone(&log);
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let log = Arc::clone(&log2);
                tokio::spawn(async move {
                    let (read_half, mut write_half) = stream.into_split();
                    let mut lines = tokio::io::BufReader::new(read_half).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        let req: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
                        let mid = req["message_id"].as_str().unwrap_or("").to_owned();
                        match req.get("msg").and_then(Value::as_str) {
                            Some("hello") => {}
                            Some("claim") => {
                                log.lock().unwrap().claimed.push(mid);
                                continue;
                            }
                            Some("done") => {
                                let mut log = log.lock().unwrap();
                                log.done.push(mid);
                                log.done_replies.push(
                                    req["reply_message_id"].as_str().unwrap_or("").to_owned(),
                                );
                            }
                            Some("release") => log.lock().unwrap().released.push(mid),
                            _ => continue,
                        }
                        if write_half.write_all(b"{\"ok\":true}\n").await.is_err() {
                            return;
                        }
                    }
                });
            }
        });
        (addr, log)
    }

    /// A claim the server received but acknowledged too late fails open, and
    /// the server still holds this seat as owner. The reply must report `done`
    /// anyway, or the other seat is promoted at lease expiry into a second
    /// answer.
    #[tokio::test]
    async fn fail_open_after_unanswered_claim_still_reports_done() {
        let (addr, log) = fake_claim_server_silent_on_claim().await;
        let (http, _requests, server) = fake_discord_http().await;
        let ctx = messaging_ctx_with_http(coordinated_config(&addr, 111), http);
        let sent = reply(
            &ctx,
            ChannelId::new(42),
            "answer",
            Some(MessageId::new(7001)),
            false,
        )
        .await;
        assert_eq!(sent["ok"], true, "the reply must fail open: {sent}");
        assert!(
            eventually(&log, |log| log.done.iter().any(|mid| mid == "7001")).await,
            "a fail-open reply whose claim was sent must still report done"
        );
        let log = log.lock().unwrap();
        assert_eq!(log.claimed, vec!["7001".to_owned()]);
        assert_eq!(log.done_replies, vec!["9001".to_owned()]);
        assert!(log.released.is_empty());
        server.abort();
    }

    /// The same unanswered claim when the reply does not go out: fail-open
    /// with a failed delivery, and fail-closed, both release the claim the
    /// server may hold rather than leave it to the lease.
    #[tokio::test]
    async fn unanswered_claim_is_released_when_no_reply_goes_out() {
        let (addr, log) = fake_claim_server_silent_on_claim().await;
        let (http, _requests, server) = fake_discord_http().await;
        let ctx = messaging_ctx_with_http(coordinated_config(&addr, 111), Arc::clone(&http));
        let failed = reply(
            &ctx,
            ChannelId::new(42),
            "force-delivery-failure",
            Some(MessageId::new(7001)),
            false,
        )
        .await;
        assert!(
            failed.get("error").is_some(),
            "delivery must fail: {failed}"
        );
        assert!(
            eventually(&log, |log| log.released.iter().any(|mid| mid == "7001")).await,
            "a failed fail-open delivery must release the unanswered claim"
        );

        let mut raw = coordinated_config(&addr, 111).raw.clone();
        raw.coordination.get_mut("claim-once").unwrap().fail_open = false;
        let closed = messaging_ctx_with_http(LoadedConfig::from_raw(raw), http);
        coordinate_reply(&closed, ChannelId::new(42), MessageId::new(7002))
            .await
            .expect_err("fail_open = false must refuse");
        assert!(
            eventually(&log, |log| log.released.iter().any(|mid| mid == "7002")).await,
            "a fail-closed refusal must release the unanswered claim"
        );
        assert!(log.lock().unwrap().done.is_empty());
        server.abort();
    }

    /// With no `pre_send.author_id`, coordination must not silently switch off;
    /// it identifies as the gateway's own bot user id, learned at Ready.
    #[tokio::test]
    async fn coordination_without_author_id_uses_gateway_bot_id() {
        let (addr, log) = fake_claim_server().await;
        let mut raw = coordinated_config(&addr, 1).raw.clone();
        raw.pre_send.author_id = None;
        let ctx = messaging_ctx(LoadedConfig::from_raw(raw));
        crate::coordination::set_gateway_bot_id(424_242);
        assert_eq!(
            coordinate_reply(&ctx, ChannelId::new(42), MessageId::new(445)).await,
            Ok(true),
            "an opted-in channel must claim even without pre_send.author_id"
        );
        assert!(log.lock().unwrap().claimed.iter().any(|mid| mid == "445"));
    }

    /// A coordinated config whose claim-once server is a closed port.
    async fn unreachable_coordinator_config(fail_open: bool) -> LoadedConfig {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        drop(listener);
        let mut raw = coordinated_config(&addr, 111).raw.clone();
        let block = raw.coordination.get_mut("claim-once").unwrap();
        block.connect_timeout_ms = 500;
        block.fail_open = fail_open;
        LoadedConfig::from_raw(raw)
    }

    /// An unreachable coordinator lets the reply through when `fail_open`, and
    /// refuses it when not.
    #[tokio::test]
    async fn unreachable_coordinator_fails_open_or_closed_as_configured() {
        let open = messaging_ctx(unreachable_coordinator_config(true).await);
        assert_eq!(
            coordinate_reply(&open, ChannelId::new(42), MessageId::new(7)).await,
            Ok(false),
            "fail_open = true must let the reply through, holding no claim"
        );
        let closed = messaging_ctx(unreachable_coordinator_config(false).await);
        let err = coordinate_reply(&closed, ChannelId::new(42), MessageId::new(7))
            .await
            .expect_err("fail_open = false must refuse");
        assert!(
            err["error"]
                .as_str()
                .is_some_and(|e| e.contains("fail-closed")),
            "{err}"
        );
    }

    /// A real coordinated `reply` that reaches Discord reports `done` with the
    /// sent message's id.
    ///
    /// The source message (7001) and the posted reply (9001) have distinct ids,
    /// so `done` carrying the source id fails here.
    #[tokio::test]
    async fn coordinated_reply_reports_done_after_send() {
        let (addr, log) = fake_claim_server().await;
        let (http, requests, server) = fake_discord_http().await;
        let ctx = messaging_ctx_with_http(coordinated_config(&addr, 111), http);
        let sent = reply(
            &ctx,
            ChannelId::new(42),
            "answer",
            Some(MessageId::new(7001)),
            false,
        )
        .await;
        assert_eq!(sent["ok"], true, "{sent}");
        assert!(
            requests
                .lock()
                .unwrap()
                .iter()
                .any(|(path, _)| path.ends_with("/channels/42/messages")),
            "the reply must reach Discord"
        );
        assert!(
            eventually(&log, |log| log.done.iter().any(|mid| mid == "7001")).await,
            "a sent reply must report done"
        );
        assert_eq!(
            log.lock().unwrap().done_replies,
            vec!["9001".to_owned()],
            "done must carry the posted reply's id, not the source message's"
        );
        server.abort();
    }

    /// A channel that does not opt in never touches the coordinator.
    #[tokio::test]
    async fn channel_without_coordinate_makes_no_claim() {
        let (addr, log) = fake_claim_server().await;
        let mut raw = coordinated_config(&addr, 111).raw.clone();
        raw.channels.push(ChannelConfig {
            id: "43".to_owned(),
            require_mention: false,
            ..Default::default()
        });
        let ctx = messaging_ctx(LoadedConfig::from_raw(raw));
        assert_eq!(
            coordinate_reply(&ctx, ChannelId::new(43), MessageId::new(7)).await,
            Ok(false)
        );
        assert!(
            log.lock().unwrap().claimed.is_empty(),
            "no claim for an opted-out channel"
        );
    }

    /// Two constructs sharing one coordination block: the first claim proceeds,
    /// the second is told who is ahead, and the winner's `done` reaches the
    /// server.
    #[tokio::test]
    async fn coordination_orders_two_constructs_one_proceed_one_wait() {
        let (addr, done) = fake_claim_server().await;
        let http = Arc::new(serenity::http::Http::new("fake"));

        let config = |bot_id: u64| {
            let mut raw = Config::default();
            raw.pre_send.author_id = Some(UserId::new(bot_id));
            raw.coordination.insert(
                "claim-once".to_owned(),
                crate::coordination::CoordinationConfig {
                    addr: addr.clone(),
                    lease_ms: 10_000,
                    connect_timeout_ms: 2_000,
                    fail_open: true,
                },
            );
            raw.channels.push(ChannelConfig {
                id: "42".to_owned(),
                coordinate: Some("claim-once".to_owned()),
                ..Default::default()
            });
            LoadedConfig::from_raw(raw)
        };

        let ctx_a = messaging_ctx_with_http(config(111), Arc::clone(&http));
        let ctx_b = messaging_ctx_with_http(config(222), Arc::clone(&http));

        assert!(
            coordinate_reply(&ctx_a, ChannelId::new(42), MessageId::new(7))
                .await
                .is_ok(),
            "first claim must proceed"
        );
        let err = coordinate_reply(&ctx_b, ChannelId::new(42), MessageId::new(7))
            .await
            .expect_err("second claim must wait");
        assert!(
            err["error"]
                .as_str()
                .is_some_and(|e| e.contains("already answering")),
            "wait must name who is ahead: {err}"
        );

        report_reply_done(
            &ctx_a,
            ChannelId::new(42),
            MessageId::new(7),
            MessageId::new(99),
        )
        .await;
        assert!(
            eventually(&done, |log| log.done.iter().any(|mid| mid == "7")).await,
            "done must reach the claim server"
        );
    }
}
