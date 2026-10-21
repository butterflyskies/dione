//! Discord messaging operations with access, verification, and consent checks.

/// Discord message reads and attachment downloads.
pub mod queries;
mod rook_publication;
pub(crate) mod search;
mod types;
use crate::{
    config::{ChunkMode, DmPolicy, LoadedConfig},
    contradictionary::{
        Action, AutoRewrite, AutoRewritten, BlockOutcome, DiaryRecord, append_diary_record,
    },
    coordination::{ClaimOutcome, Coordinator},
    discord::{chunk, chunk_preserving_fences_with_context, events::NotificationEvent},
    evidence::{
        SentexHandles, SentexRole, SentexTransport, append_markers, has_terminal_sentex_syntax,
        locator_metadata, parse_sentex_locators,
    },
    gate::OutboundGate,
    ingress_ledger::{IngressLedger, VerifyResult},
    no_rly::{
        consent::{
            ConsentGate, DeliverError, DeliverReply, DeliveryCause, PostedMessages, RejectedHandle,
            Released, Rephrased, Replacement, ReplyRequest,
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
pub use rook_publication::{DiscordPublicationInitError, DiscordPublicationProvider};
use serde::Deserialize;
use serde_json::json;
use serenity::{
    builder::{CreateAllowedMentions, CreateAttachment, CreateMessage, EditMessage},
    http::{LightMethod, Request, Route},
    model::{
        channel::{Channel, MessageFlags},
        id::{ChannelId, MessageId, UserId},
    },
};
use std::{sync::Arc, time::Instant};
use tokio::sync::mpsc;
pub use types::{
    DeliveryReceipt, DmDeliveryRoute, DmOutcome, LinkPreviews, MessagingError, MessagingErrorKind,
    PartialReceipt, RedirectRestrictedSurface, ReplyOutcome, ReplyPing, Report, TargetError,
    TargetOperation,
};

/// Preserve wire bits that the installed SDK's typed message flags would truncate.
#[derive(Deserialize)]
struct ExistingMessageFlags {
    flags: Option<u64>,
}

/// Self-react emoji for contradictionary celebrate hits (✨ — sparkles).
const CONTRADICTIONARY_CELEBRATE_REACT: &str = "\u{2728}";
static NO_SENTEX_HANDLES: SentexHandles = SentexHandles::empty();
#[cfg(test)]
const MESSAGE_FLAG_NOT_IN_SDK: u64 = 1 << 14;

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
    operation: TargetOperation,
    policy: TargetPolicy,
) -> Result<(), TargetError> {
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
    operation: TargetOperation,
    policy: TargetPolicy,
) -> Result<(), TargetError> {
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
                operation = %operation,
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
            return Err(TargetError::IngressRequired);
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
                    return Err(TargetError::CanonicalRequiresIngress);
                }
                // REST messages need not carry guild_id; resolve it rather
                // than treating an unknown guild as unmuted.
                let guild_id = match message.guild_id {
                    Some(id) => id,
                    None => match http.get_channel(channel_id).await {
                        Ok(Channel::Guild(channel)) => channel.guild_id,
                        Ok(_) => return Err(TargetError::CanonicalRequiresIngress),
                        Err(error) => {
                            return Err(TargetError::GuildLookup(error.into()));
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
                    return Err(TargetError::CanonicalRequiresIngress);
                }
                tracing::info!(
                    message_id = message_id.get(),
                    channel_id = channel_id.get(),
                    operation = %operation,
                    "egress: canonical target exists but is absent from ingress ledger"
                );
                return Ok(());
            }
            Ok(_) => {
                return Err(TargetError::CanonicalMismatch);
            }
            Err(serenity::Error::Http(serenity::http::HttpError::UnsuccessfulRequest(
                response,
            ))) if response.status_code.as_u16() == 404 && response.error.code == 10008 => {}
            Err(error) => {
                return Err(TargetError::MessageLookup(error.into()));
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
    operation: TargetOperation,
    own_send: bool,
    mut alert: impl FnMut(ChannelId, String),
) -> Result<(), TargetError> {
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
                operation = %operation,
                "egress: own-authored target absent from ingress ledger (expected); phantom canary exempt"
            );
            Ok(())
        }
        crate::ingress_ledger::VerifyResult::Unknown => {
            tracing::warn!(
                message_id = message_id.get(),
                channel_id = channel_id.get(),
                operation = %operation,
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
            Err(TargetError::NotFound {
                message: message_id,
                channel: channel_id,
                operation,
            })
        }
        crate::ingress_ledger::VerifyResult::ChannelMismatch {
            admitted_channel,
            claimed_channel,
        } => {
            tracing::warn!(
                message_id = message_id.get(),
                admitted_channel = admitted_channel.get(),
                claimed_channel = claimed_channel.get(),
                operation = %operation,
                "egress: message_id channel mismatch"
            );
            Err(TargetError::ChannelMismatch {
                message: message_id,
                admitted: admitted_channel,
                claimed: claimed_channel,
                operation,
            })
        }
        crate::ingress_ledger::VerifyResult::Expired => {
            tracing::info!(
                message_id = message_id.get(),
                channel_id = channel_id.get(),
                operation = %operation,
                "egress: message_id expired from ingress ledger"
            );
            Ok(())
        }
        crate::ingress_ledger::VerifyResult::Unavailable => {
            tracing::warn!(
                message_id = message_id.get(),
                operation = %operation,
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
    pub(crate) fn with_pre_send_pipeline(mut self, pipeline: Arc<PreSendPipeline>) -> Self {
        self.pre_send_pipeline = Some(pipeline);
        self
    }

    #[cfg(test)]
    pub(crate) fn has_pre_send_pipeline(&self) -> bool {
        self.pre_send_pipeline.is_some()
    }
}

// ── Gate helper ───────────────────────────────────────────────────────────────

/// Check current outbound policy before delivery.
async fn ensure_outbound(ctx: &MessagingCtx, channel_id: ChannelId) -> Result<(), DeliveryCause> {
    let state = ctx.state.read().await;
    if OutboundGate::check_channel_with_threads(
        &ctx.config,
        channel_id.get(),
        &state.dm_channel_ids,
        &state.thread_parents,
    ) {
        Ok(())
    } else {
        Err(DeliveryCause::OutboundDenied(channel_id))
    }
}

/// Check current outbound policy and retain a typed denial.
pub(crate) async fn check_outbound(
    ctx: &MessagingCtx,
    channel_id: ChannelId,
) -> Result<(), MessagingError> {
    ensure_outbound(ctx, channel_id)
        .await
        .map_err(|_| MessagingErrorKind::OutboundDenied(channel_id).into())
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
) -> Option<MessagingError> {
    (caption.is_none() && !no_rly_hooks.is_empty())
        .then(|| MessagingErrorKind::CaptionlessHookOverrides.into())
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
    redirect_error: Option<RedirectRestrictedSurface>,
}

impl SurfacePolicy {
    const fn for_surface(surface: OutboundSurface) -> Self {
        let redirect_error = match surface {
            OutboundSurface::Reply
            | OutboundSurface::SendFileCaption
            | OutboundSurface::RenderLatexCaption
            | OutboundSurface::SendDm => None,
            OutboundSurface::EditMessage => Some(RedirectRestrictedSurface::Edit),
            OutboundSurface::VoiceBeforeTts => Some(RedirectRestrictedSurface::Voice),
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
    /// The contradictionary's `auto` rewrites applied to the draft, if any.
    auto: Option<AutoApplied>,
}

/// What the `auto` tier changed in a draft: the text as written and each
/// rewrite, for the sender-facing lines and the diary.
#[derive(Debug)]
struct AutoApplied {
    written: String,
    rewrites: Vec<AutoRewrite>,
}

impl PreparedOutbound {
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
) -> Result<PreparedOutbound, MessagingError> {
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
                .map_err(MessagingErrorKind::Pipeline)?;
            Some((pipeline, no_rly))
        }
        None if !draft.pre_send.bypasses.is_empty() => {
            return Err(MessagingErrorKind::HooksNotRegistered.into());
        }
        None => None,
    };
    if let OutboundDestination::Channel(channel_id) = draft.destination {
        check_outbound(ctx, channel_id).await?;
    }
    // The `auto` rewrite runs first, so the hooks judge, and the evidence
    // markers are appended to, the text that will be sent.
    let rewritten = match draft.pre_send.surface {
        OutboundSurface::Reply | OutboundSurface::SendDm => apply_auto_rewrites(ctx, draft.text),
        _ => None,
    };
    let (rewritten_text, auto) = match rewritten {
        Some(rewritten) => (
            Some(rewritten.content),
            Some(AutoApplied {
                written: draft.text.to_owned(),
                rewrites: rewritten.rewrites,
            }),
        ),
        None => (None, None),
    };
    let text = rewritten_text.as_deref().unwrap_or(draft.text);
    reject_raw_sentex_locators(text)?;
    let Some((pipeline, no_rly)) = prepared_pipeline else {
        return Ok(PreparedOutbound {
            destination: draft.destination,
            text: append_markers(text, sentex_handles),
            reply_to: draft.reply_to,
            surface: draft.pre_send.surface,
            auto,
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
        text,
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
            SentexTransport::TerminalVisibleRoleSuffixV3AfterHooks.as_str(),
        );
    }
    let outcome =
        match tokio::task::spawn_blocking(move || pipeline.run(&hook_context, &no_rly)).await {
            Ok(Ok(outcome)) => outcome,
            Ok(Err(error)) => return Err(MessagingErrorKind::Pipeline(error).into()),
            Err(error) => {
                return Err(MessagingErrorKind::PipelineTask(error).into());
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
            let feedback: Vec<String> = outcome
                .to_construct()
                .as_slice()
                .iter()
                .map(|assessment| assessment.detail().to_owned())
                .collect();
            return Err(MessagingErrorKind::HookHalted {
                reason: reason.clone(),
                feedback,
            }
            .into());
        }
        HookDecision::Redirect { channel_id: target } => {
            if let Some(error) = SurfacePolicy::for_surface(draft.pre_send.surface).redirect_error {
                return Err(MessagingErrorKind::RedirectUnsupported(error).into());
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

    let final_text = outcome.final_text().unwrap_or(text);
    reject_raw_sentex_locators(final_text)?;
    Ok(PreparedOutbound {
        destination,
        text: append_markers(final_text, sentex_handles),
        reply_to,
        surface: draft.pre_send.surface,
        auto,
    })
}

fn reject_raw_sentex_locators(content: &str) -> Result<(), MessagingError> {
    if !has_terminal_sentex_syntax(content) {
        return Ok(());
    }
    Err(MessagingErrorKind::RawSentexLocators.into())
}

/// Applies the same pre-send seam to text before a voice backend performs TTS.
pub async fn prepare_voice_text(
    ctx: &MessagingCtx,
    channel_id: ChannelId,
    content: &str,
    no_rly_hooks: &[HookName],
) -> Result<String, MessagingError> {
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

/// Send a channel reply with explicit notification policy.
pub async fn reply(
    ctx: &MessagingCtx,
    channel_id: ChannelId,
    content: &str,
    reply_to_message_id: Option<MessageId>,
    suppress_ping: ReplyPing,
) -> Result<Report<ReplyOutcome>, MessagingError> {
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

/// Send a channel reply with named hook overrides.
pub async fn reply_with_hook_overrides(
    ctx: &MessagingCtx,
    channel_id: ChannelId,
    content: &str,
    reply_to_message_id: Option<MessageId>,
    suppress_ping: ReplyPing,
    no_rly_hooks: &[HookName],
) -> Result<Report<ReplyOutcome>, MessagingError> {
    reply_with_evidence_and_hook_overrides(
        ctx,
        channel_id,
        content,
        reply_to_message_id,
        ReplyToolOptions {
            suppress_ping,
            suppress_embeds: LinkPreviews::Allow,
            no_rly_hooks,
            sentex_handles: &NO_SENTEX_HANDLES,
        },
    )
    .await
}

pub(crate) struct ReplyToolOptions<'a> {
    pub(crate) suppress_ping: ReplyPing,
    pub(crate) suppress_embeds: LinkPreviews,
    pub(crate) no_rly_hooks: &'a [HookName],
    pub(crate) sentex_handles: &'a SentexHandles,
}

pub(crate) async fn reply_with_evidence_and_hook_overrides(
    ctx: &MessagingCtx,
    channel_id: ChannelId,
    content: &str,
    reply_to_message_id: Option<MessageId>,
    options: ReplyToolOptions<'_>,
) -> Result<Report<ReplyOutcome>, MessagingError> {
    check_outbound(ctx, channel_id).await?;

    if let Some(ref_id) = reply_to_message_id {
        let own_send = ctx.state.read().await.is_own_send(ref_id.get());
        if let Err(error) = verify_message_target(
            &ctx.ingress_ledger,
            &ctx.http,
            &ctx.config,
            ref_id,
            channel_id,
            TargetOperation::ReplyTo,
            TargetPolicy::CanonicalReplyOrReact(own_send),
        )
        .await
        {
            return Err(error.into());
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
        Err(error) => return Err(error),
    };
    if ctx.config.delivery.evidence_markers_enabled
        && !options.sentex_handles.is_empty()
        && parse_sentex_locators(&prepared.text).is_empty()
    {
        return Err(MessagingErrorKind::SentexInsideQuoteOrFence.into());
    }
    let channel_id = prepared.channel_id().unwrap_or(channel_id);
    deliver_prepared_reply(
        ctx,
        ChannelPrepared {
            channel_id,
            prepared,
        },
        ReplyTransportOptions {
            suppress_ping: options.suppress_ping,
            suppress_embeds: options.suppress_embeds,
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

fn claim_winner_display(
    ahead: &[String],
    names: &std::collections::BTreeMap<u64, String>,
) -> String {
    let Some(first) = ahead.first() else {
        return "another construct".to_owned();
    };
    let user = first
        .parse::<u64>()
        .ok()
        .filter(|id| *id != 0 && id.to_string() == *first);
    match user {
        Some(id) => crate::core::identity_display::format_identity(
            first,
            names.get(&id).map(String::as_str),
        ),
        None => first.clone(),
    }
}

/// Claim the right to answer `message_id` before sending. `Err` is the tool
/// error returned to the construct: when another seat holds the claim, it
/// names who is ahead. `Ok` lets the send proceed, carrying `true` when this
/// seat holds a claim it must `done` or `release`. That
/// includes a fail-open send whose claim reached the server unanswered: the
/// server may have made this seat the owner, and a `done` it does not
/// recognise is only refused.
async fn coordinate_reply(
    ctx: &MessagingCtx,
    channel_id: ChannelId,
    message_id: MessageId,
) -> Result<bool, MessagingError> {
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
            Err(MessagingErrorKind::CoordinationUnavailable(reason).into())
        }
        ClaimOutcome::Wait { ahead, .. } => {
            let winner_display = {
                let state = ctx.state.read().await;
                claim_winner_display(&ahead, &state.user_names)
            };
            Err(MessagingErrorKind::CoordinationBusy {
                ahead,
                winner_display,
            }
            .into())
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

struct ChannelPrepared {
    channel_id: ChannelId,
    prepared: PreparedOutbound,
}

struct ReplyTransportOptions {
    suppress_ping: ReplyPing,
    suppress_embeds: LinkPreviews,
}

/// Attach rewrite metadata at the delivery stage, including holds and failures.
async fn deliver_prepared_reply(
    ctx: &MessagingCtx,
    mut prepared: ChannelPrepared,
    options: ReplyTransportOptions,
) -> Result<Report<ReplyOutcome>, MessagingError> {
    let auto = prepared.prepared.auto.take();
    let result = deliver_prepared_text(ctx, prepared, auto.as_ref(), options).await;
    let auto_rewrites = auto.map(|auto| auto.rewrites).unwrap_or_default();
    match result {
        Ok(outcome) => Ok(Report {
            outcome,
            auto_rewrites,
        }),
        Err(mut error) => {
            error.auto_rewrites = auto_rewrites;
            Err(error)
        }
    }
}

async fn deliver_prepared_text(
    ctx: &MessagingCtx,
    resolved: ChannelPrepared,
    auto: Option<&AutoApplied>,
    options: ReplyTransportOptions,
) -> Result<ReplyOutcome, MessagingError> {
    let ChannelPrepared {
        channel_id,
        prepared,
    } = resolved;
    debug_assert!(matches!(
        prepared.surface,
        OutboundSurface::Reply | OutboundSurface::SendDm
    ));
    let reply_to_message_id = prepared.reply_to;
    let content = prepared.text;
    validate_evidence_chunking(&ctx.config, &content)?;

    // A rewritten send owes its `auto` records alongside the usual send-side
    // ones, all written once every chunk lands.
    let pending_diary_records = match auto {
        Some(auto) => auto_send_records(ctx, &auto.rewrites, &auto.written, &content),
        None => Vec::new(),
    };
    let request = ReplyRequest {
        channel_id,
        content: content.to_string(),
        reply_to_message_id,
        suppress_ping: options.suppress_ping.suppressed(),
        suppress_embeds: options.suppress_embeds.suppressed(),
        pending_diary_records,
        fence_context: Default::default(),
    };

    if let Some(ref judge) = ctx.config.contradictionary
        && let Verdict::Bounce(reason) = judge.judge(&content)
    {
        if ctx.config.delivery.evidence_markers_enabled
            && !parse_sentex_locators(&content).is_empty()
        {
            return Err(MessagingErrorKind::SentexCannotBeHeld.into());
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
        return Ok(ReplyOutcome::Held(ticket));
    }

    // Claim the right to answer only now, after the hooks, the evidence
    // checks and the judge, so a reply that never goes out never holds the
    // claim.
    let claimed = match reply_to_message_id {
        Some(ref_id) => match coordinate_reply(ctx, channel_id, ref_id).await {
            Ok(claimed) => claimed.then_some(ref_id),
            Err(error) => return Err(error),
        },
        None => None,
    };

    match deliver_reply(ctx, &request).await {
        Ok(sent_ids) => {
            if let Some(ref_id) = claimed {
                match sent_ids.first() {
                    Some(reply_id) => report_reply_done(ctx, channel_id, ref_id, *reply_id).await,
                    None => release_reply_claim(ctx, channel_id, ref_id).await,
                }
            }
            let locators = if ctx.config.delivery.evidence_markers_enabled {
                parse_sentex_locators(&content)
            } else {
                Vec::new()
            };
            Ok(ReplyOutcome::Delivered(DeliveryReceipt {
                message_ids: sent_ids,
                locators,
            }))
        }
        Err(DeliverError::Total(failure)) => {
            if let Some(ref_id) = claimed {
                release_reply_claim(ctx, channel_id, ref_id).await;
            }
            Err(MessagingErrorKind::Delivery(failure.cause).into())
        }
        Err(DeliverError::Partial(failure)) => {
            if let Some(ref_id) = claimed {
                report_reply_done(ctx, channel_id, ref_id, failure.sent.first()).await;
            }
            let auto_records = failure
                .diary
                .into_iter()
                .filter(|record| record.action == Action::Auto)
                .collect::<Vec<_>>();
            append_diary_records(ctx, &auto_records);
            Ok(ReplyOutcome::PartiallyDelivered(PartialReceipt {
                sent: failure.sent,
                cause: failure.cause,
            }))
        }
    }
}

fn validate_evidence_chunking(config: &LoadedConfig, content: &str) -> Result<(), MessagingError> {
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
        return Err(MessagingErrorKind::SentexRequiresSingleMessage.into());
    }
    Ok(())
}

/// Apply the contradictionary's `auto` rewrites to outbound `content`.
///
/// The rewrite is kept only when it is clean
/// ([`Contradictionary::apply_auto`]). Otherwise the original text goes on to
/// the judge unrewritten, and the judge holds it: its `auto` hit still gates
/// like `block`.
///
/// [`Contradictionary::apply_auto`]: crate::contradictionary::Contradictionary::apply_auto
fn apply_auto_rewrites(ctx: &MessagingCtx, content: &str) -> Option<AutoRewritten> {
    ctx.config
        .contradictionary
        .as_ref()
        .and_then(|contradictionary| contradictionary.apply_auto(content))
}

/// The diary records a rewritten send owes once it lands: one
/// `action: "auto"` record per rewrite, then the send-side records for the
/// sent text (see [`send_side_records`]). Carried as the request's
/// `pending_diary_records`, which take the place of the send-side scan.
fn auto_send_records(
    ctx: &MessagingCtx,
    rewrites: &[AutoRewrite],
    written: &str,
    sent: &str,
) -> Vec<DiaryRecord> {
    let mut records: Vec<DiaryRecord> = rewrites
        .iter()
        .map(|rewrite| DiaryRecord::auto_now(rewrite, written, sent))
        .collect();
    if let Some(ref contradictionary) = ctx.config.contradictionary {
        records.extend(send_side_records(
            contradictionary,
            &contradictionary.check(sent),
            sent,
        ));
    }
    records
}

/// The records a send of `content` owes the diary once every chunk lands:
/// its `log`/`celebrate` hits, and the override when a held block is
/// released.
fn send_side_records(
    contradictionary: &crate::contradictionary::Contradictionary,
    hits: &[crate::contradictionary::Hit],
    content: &str,
) -> Vec<DiaryRecord> {
    match contradictionary.evaluate_block(hits, content, true) {
        BlockOutcome::Clear => Vec::new(),
        BlockOutcome::Overridden(records) | BlockOutcome::Recorded(records) => records,
        BlockOutcome::Rejected { .. } => {
            // Should not happen with `no_rly` set — the judge already ruled.
            Vec::new()
        }
    }
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
) -> Result<Vec<MessageId>, DeliverError> {
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
        send_side_records(contradictionary, &contradictionary_hits, content)
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
    .map_err(|error| DeliverError::total(DeliveryCause::Chunking(error)))?;
    let mut sent_ids: Vec<MessageId> = Vec::new();
    let mut first_msg_id: Option<MessageId> = None;
    let mut bot_author: Option<(UserId, BotDisplayName)> = None;

    for (i, chunk) in chunks.iter().enumerate() {
        let mut builder = CreateMessage::new().content(&chunk.rendered);
        if request.suppress_embeds {
            builder = builder.flags(MessageFlags::SUPPRESS_EMBEDS);
        }

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
                sent_ids.push(msg.id);
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
                let cause = DeliveryCause::ChunkSend {
                    index: i,
                    source: e.into(),
                };
                let mut ids = sent_ids.into_iter();
                return Err(match ids.next() {
                    Some(first) => {
                        request.set_fence_context(chunks[i].incoming.clone());
                        DeliverError::partial(
                            cause,
                            PostedMessages::new(first, ids.collect()),
                            content[chunks[i].source.start..].to_owned(),
                            pending_diary_records,
                        )
                    }
                    None => DeliverError::total_with_diary(
                        cause,
                        if request.pending_diary_records.is_empty() {
                            Vec::new()
                        } else {
                            pending_diary_records
                        },
                    ),
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
                first_id,
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
    async fn deliver(&self, request: &ReplyRequest) -> Result<Vec<MessageId>, DeliverError> {
        deliver_reply(self, request).await
    }
}

// ── no_rly (release) and rephrase ────────────────────────────────────────────

/// The `no_rly` tool: release a held message, sending the byte-identical
/// queued text. The handle dies on success; a failed send leaves it live
/// until expiry.
pub async fn release_held(
    ctx: &MessagingCtx,
    handle: &HoldHandle,
) -> Result<Released, RejectedHandle> {
    let released = ctx.no_rly.release(ctx, handle, Instant::now()).await?;
    tracing::info!(%handle, latency_ms = released.latency_ms, "no_rly released held message");
    Ok(released)
}

/// The `rephrase` tool: replace a held message's text. The replacement is
/// re-judged — a clean verdict sends it (and journals the original, reason,
/// and replacement as a triple); a re-bounce mints a new handle chained to
/// the dead one.
pub async fn rephrase_held(
    ctx: &MessagingCtx,
    handle: &HoldHandle,
    content: &str,
) -> Result<Report<Rephrased>, MessagingError> {
    // Reject an empty/whitespace replacement up front with a clear message —
    // it would otherwise pass the judge and 400 at Discord with a confusing
    // "cannot send an empty message" error, stranding the handle.
    if content.trim().is_empty() {
        return Err(MessagingErrorKind::EmptyRephrase.into());
    }
    reject_raw_sentex_locators(content)?;
    let auto = apply_auto_rewrites(ctx, content);
    let (replacement, auto_rewrites) = match auto {
        Some(ref rewritten) => (
            Replacement {
                written: content,
                send: &rewritten.content,
                diary_records: auto_send_records(
                    ctx,
                    &rewritten.rewrites,
                    content,
                    &rewritten.content,
                ),
            },
            rewritten.rewrites.as_slice(),
        ),
        None => (Replacement::from(content), &[][..]),
    };
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
        .rephrase(ctx, judge, handle, replacement, ttl, now)
        .await;

    match result {
        Ok(outcome) => {
            match &outcome {
                Rephrased::Sent { .. } => {
                    tracing::info!(%handle, "rephrase sent replacement for held message")
                }
                Rephrased::ReBounced(ticket) => {
                    tracing::info!(old_handle = %handle, new_handle = %ticket.handle, reason = %ticket.reason, "rephrase bounced again; chained new handle");
                    append_diary_records(
                        ctx,
                        &[DiaryRecord::held_now(&ticket.reason.to_string(), content)],
                    );
                }
            }
            Ok(Report {
                outcome,
                auto_rewrites: auto_rewrites.to_vec(),
            })
        }
        Err(error) => Err(MessagingError {
            kind: MessagingErrorKind::Handle(error),
            auto_rewrites: auto_rewrites.to_vec(),
        }),
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

/// React to a verified, permitted message target.
pub async fn react(
    ctx: &MessagingCtx,
    channel_id: ChannelId,
    message_id: MessageId,
    emoji: &str,
) -> Result<(), MessagingError> {
    check_outbound(ctx, channel_id).await?;

    let own_send = ctx.state.read().await.is_own_send(message_id.get());
    if let Err(e) = verify_message_target(
        &ctx.ingress_ledger,
        &ctx.http,
        &ctx.config,
        message_id,
        channel_id,
        TargetOperation::React,
        TargetPolicy::CanonicalReplyOrReact(own_send),
    )
    .await
    {
        return Err(e.into());
    }

    let reaction = match parse_reaction_type(emoji) {
        Ok(r) => r,
        Err(e) => return Err(e),
    };
    match ctx
        .http
        .create_reaction(channel_id, message_id, &reaction)
        .await
    {
        Ok(()) => Ok(()),
        Err(e) => Err(MessagingErrorKind::Discord(e.into()).into()),
    }
}

// ── edit_message ──────────────────────────────────────────────────────────────

/// Edit message content after outbound preparation.
pub async fn edit_message(
    ctx: &MessagingCtx,
    channel_id: ChannelId,
    message_id: MessageId,
    new_content: &str,
) -> Result<MessageId, MessagingError> {
    edit_message_with_hook_overrides(ctx, channel_id, message_id, new_content, &[]).await
}

/// Edit message content with named hook overrides.
pub async fn edit_message_with_hook_overrides(
    ctx: &MessagingCtx,
    channel_id: ChannelId,
    message_id: MessageId,
    new_content: &str,
    no_rly_hooks: &[HookName],
) -> Result<MessageId, MessagingError> {
    edit_message_with_embed_control(ctx, channel_id, message_id, new_content, no_rly_hooks, None)
        .await
}

pub(crate) async fn edit_message_with_embed_control(
    ctx: &MessagingCtx,
    channel_id: ChannelId,
    message_id: MessageId,
    new_content: &str,
    no_rly_hooks: &[HookName],
    suppress_embeds: Option<bool>,
) -> Result<MessageId, MessagingError> {
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
        Err(error) => return Err(error),
    };
    let mut builder = EditMessage::new().content(prepared.text);
    if let Some(suppress) = suppress_embeds {
        let message = match ctx
            .http
            .fire::<ExistingMessageFlags>(Request::new(
                Route::ChannelMessage {
                    channel_id,
                    message_id,
                },
                LightMethod::Get,
            ))
            .await
        {
            Ok(message) => message,
            Err(error) => return Err(MessagingErrorKind::Discord(error.into()).into()),
        };
        let mut flags = MessageFlags::from_bits_retain(message.flags.unwrap_or_default());
        flags.set(MessageFlags::SUPPRESS_EMBEDS, suppress);
        builder = builder.flags(flags);
    }
    match ctx
        .http
        .edit_message(channel_id, message_id, &builder, vec![])
        .await
    {
        Ok(msg) => Ok(msg.id),
        Err(e) => Err(MessagingErrorKind::Discord(e.into()).into()),
    }
}

// ── send_attachment (shared helper) ──────────────────────────────────────────

pub(crate) async fn send_attachment_with_hook_overrides(
    ctx: &MessagingCtx,
    channel_id: ChannelId,
    attachment: CreateAttachment,
    caption: Option<&str>,
    no_rly_hooks: &[HookName],
    surface: OutboundSurface,
) -> Result<MessageId, MessagingError> {
    if let Some(error) = reject_captionless_hook_overrides(caption, no_rly_hooks) {
        return Err(error);
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
            Err(error) => return Err(error),
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
            Ok(msg.id)
        }
        Err(e) => Err(MessagingErrorKind::SendAttachment(e.into()).into()),
    }
}

// ── send_file ────────────────────────────────────────────────────────────────

/// Upload a permitted file with an optional caption.
pub async fn send_file(
    ctx: &MessagingCtx,
    channel_id: ChannelId,
    file_path: &str,
    caption: Option<&str>,
) -> Result<MessageId, MessagingError> {
    send_file_with_hook_overrides(ctx, channel_id, file_path, caption, &[]).await
}

/// Upload a permitted file with caption hook overrides.
pub async fn send_file_with_hook_overrides(
    ctx: &MessagingCtx,
    channel_id: ChannelId,
    file_path: &str,
    caption: Option<&str>,
    no_rly_hooks: &[HookName],
) -> Result<MessageId, MessagingError> {
    if let Some(error) = reject_captionless_hook_overrides(caption, no_rly_hooks) {
        return Err(error);
    }
    let path = std::path::Path::new(file_path);
    if !path.is_absolute() {
        return Err(MessagingErrorKind::RelativeFilePath.into());
    }

    check_outbound(ctx, channel_id).await?;

    let utf8_path = camino::Utf8Path::new(file_path);
    if !OutboundGate::check_file_send(utf8_path, &ctx.state_dir) {
        return Err(MessagingErrorKind::FileNotPermitted.into());
    }

    let attachment = match CreateAttachment::path(path).await {
        Ok(a) => a,
        Err(e) => return Err(MessagingErrorKind::ReadFile(e.into()).into()),
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
) -> Result<serenity::model::channel::PrivateChannel, Box<serenity::Error>> {
    let dm_body = json!({ "recipient_id": user_id.get().to_string() });
    http.create_private_channel(&dm_body)
        .await
        .map_err(Box::new)
}

// ── send_dm ──────────────────────────────────────────────────────────────────

/// Open (or reuse) a DM channel and send through the shared [`reply`] path.
///
/// DMs are judged like every other outbound message — this is deliberate and
/// carries over from v1, where the contradictionary block check also ran on
/// the shared reply path. The judge polices the construct's own speech, not
/// the audience, so a block-tier tic bounces in a DM exactly as it would in
/// a channel: held under a handle, releasable, rephrasable.
pub async fn send_dm(
    ctx: &MessagingCtx,
    user_id: UserId,
    content: &str,
) -> Result<Report<DmOutcome>, MessagingError> {
    send_dm_with_hook_overrides(ctx, user_id, content, &[]).await
}

/// Send a DM request with named hook overrides.
pub async fn send_dm_with_hook_overrides(
    ctx: &MessagingCtx,
    user_id: UserId,
    content: &str,
    no_rly_hooks: &[HookName],
) -> Result<Report<DmOutcome>, MessagingError> {
    send_dm_with_evidence_and_hook_overrides(
        ctx,
        user_id,
        content,
        no_rly_hooks,
        &NO_SENTEX_HANDLES,
        LinkPreviews::Allow,
    )
    .await
}

pub(crate) async fn send_dm_with_evidence_and_hook_overrides(
    ctx: &MessagingCtx,
    user_id: UserId,
    content: &str,
    no_rly_hooks: &[HookName],
    sentex_handles: &SentexHandles,
    suppress_embeds: LinkPreviews,
) -> Result<Report<DmOutcome>, MessagingError> {
    if ctx.config.access.dm_policy == DmPolicy::Disabled {
        return Err(MessagingErrorKind::DmDisabled.into());
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
        Err(error) => return Err(error),
    };

    if ctx.config.delivery.evidence_markers_enabled
        && !sentex_handles.is_empty()
        && parse_sentex_locators(&prepared.text).is_empty()
    {
        return Err(MessagingErrorKind::SentexInsideQuoteOrFence.into());
    }
    validate_evidence_chunking(&ctx.config, &prepared.text)?;

    if let Some(channel_id) = prepared.channel_id() {
        return deliver_prepared_reply(
            ctx,
            ChannelPrepared {
                channel_id,
                prepared,
            },
            ReplyTransportOptions {
                suppress_ping: ReplyPing::Allow,
                suppress_embeds,
            },
        )
        .await
        .map(|report| report.dm(DmDeliveryRoute::Redirected));
    }

    let channel = match create_dm_channel(&ctx.http, user_id).await {
        Ok(c) => c,
        Err(e) => return Err(MessagingErrorKind::CreateDm(e).into()),
    };

    let channel_id = channel.id;

    {
        let mut state = ctx.state.write().await;
        state.record_dm_channel(user_id.get(), channel_id.get());
    }

    let result = deliver_prepared_reply(
        ctx,
        ChannelPrepared {
            channel_id,
            prepared,
        },
        ReplyTransportOptions {
            suppress_ping: ReplyPing::Allow,
            suppress_embeds,
        },
    )
    .await;

    result.map(|report| report.dm(DmDeliveryRoute::Direct(channel_id)))
}

// ── Helpers ──────────────────────────────────────────────────────────────────

/// Parses an emoji string into the appropriate serenity ReactionType.
/// Handles both Unicode emoji ("👍") and custom Discord emoji ("<:name:id>" or "<a:name:id>").
///
/// Returns an error for custom emoji with a zero ID: snowflakes are nonzero,
/// and serenity's `EmojiId::new` (NonZeroU64-backed) panics on 0.
fn parse_reaction_type(
    emoji: &str,
) -> Result<serenity::model::channel::ReactionType, MessagingError> {
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
                    return Err(MessagingErrorKind::ZeroEmojiId(trimmed.to_owned()).into());
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::{ChannelConfig, Config},
        contradictionary::{Action, Entry, MatchMode},
        mute_store::{GuildMute, MuteState, MuteStore},
        pre_send::{
            Assessment, AuditSink, AuditTrail, ConstructFeedback, FeedbackSink, HookContext,
            HookDecision, HookOutput, PipelineMode, PreSendHook, PreSendPipeline, SinkError,
            SinkFailurePolicy,
        },
        state::new_state,
    };
    use serde_json::Value;
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

    pub(super) fn test_config() -> LoadedConfig {
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
            replace: None,
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
            TargetOperation::React,
            false,
            |channel, content| alerts.push((channel, content)),
        )
        .map_err(crate::discord::messaging_wire::target_error)
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
            TargetOperation::React,
            false,
            |channel, content| alerts.push((channel, content)),
        )
        .map_err(crate::discord::messaging_wire::target_error)
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
            TargetOperation::React,
            true, // authenticated own-send
            |channel, content| alerts.push((channel, content)),
        )
        .map_err(crate::discord::messaging_wire::target_error)
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
            TargetOperation::React,
            false, // no authenticated own-send record
            |channel, content| alerts.push((channel, content)),
        )
        .map_err(crate::discord::messaging_wire::target_error)
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
            TargetOperation::React,
            true,
            |channel, content| alerts.push((channel, content)),
        )
        .map_err(crate::discord::messaging_wire::target_error)
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

        let result = crate::discord::messaging_wire::reply(
            reply(
                &ctx,
                ChannelId::new(42),
                "answer to earlier message",
                Some(MessageId::new(9001)),
                ReplyPing::Allow,
            )
            .await,
        );

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
        let result = crate::discord::messaging_wire::unit(
            react(&ctx, ChannelId::new(42), MessageId::new(9001), "✅").await,
        );
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
        let result = crate::discord::messaging_wire::reply(
            reply(
                &ctx,
                ChannelId::new(42),
                "must not reply",
                Some(MessageId::new(9001)),
                ReplyPing::Allow,
            )
            .await,
        );
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
            TargetOperation::DeleteMessage,
            TargetPolicy::IngressOrOwnSend(false),
        )
        .await
        .map_err(crate::discord::messaging_wire::target_error)
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

        let result = crate::discord::messaging_wire::reply(
            reply(
                &ctx,
                ChannelId::new(42),
                "do not cross audience gate",
                Some(MessageId::new(9001)),
                ReplyPing::Allow,
            )
            .await,
        );

        assert_eq!(result["reason"], "canonical_requires_ingress");
        {
            let seen = requests.lock().expect("request capture lock");
            assert_eq!(seen.len(), 1, "real target must not reach a mutation");
            assert!(seen[0].0.ends_with("/channels/42/messages/9001"));
        }

        let missing = crate::discord::messaging_wire::unit(
            react(&ctx, ChannelId::new(42), MessageId::new(8), "✅").await,
        );
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
        let result = crate::discord::messaging_wire::reply(
            reply(
                &ctx,
                ChannelId::new(42),
                "do not reply to ignored author",
                Some(MessageId::new(9001)),
                ReplyPing::Allow,
            )
            .await,
        );

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
            let result = crate::discord::messaging_wire::reply(
                reply(
                    &ctx,
                    channel,
                    "must not send",
                    Some(MessageId::new(id)),
                    ReplyPing::Allow,
                )
                .await,
            );
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
                TargetOperation::ReplyTo,
                TargetPolicy::CanonicalReplyOrReact(false),
            )
            .await
            .map_err(crate::discord::messaging_wire::target_error)
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
            let output = std::process::Command::new(std::env::current_exe().expect("test binary"))
                .arg("--exact")
                .arg("discord::messaging::tests::reply_and_react_respect_global_mute_on_old_canonical_targets")
                .env(CHILD, "1")
                .output()
                .expect("run isolated mute regression");
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                output.status.success(),
                "isolated production-path mute regression failed\nstdout: {stdout}\nstderr: {stderr}"
            );
            assert!(
                stdout.contains("test result: ok. 1 passed; 0 failed;"),
                "isolated mute regression must run exactly one test\nstdout: {stdout}\nstderr: {stderr}"
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
        let replied = crate::discord::messaging_wire::reply(
            reply(
                &ctx,
                ChannelId::new(42),
                "must not reply",
                Some(MessageId::new(9001)),
                ReplyPing::Allow,
            )
            .await,
        );
        assert_eq!(replied["reason"], "canonical_requires_ingress");
        let reacted = crate::discord::messaging_wire::unit(
            react(&ctx, ChannelId::new(42), MessageId::new(9008), "✅").await,
        );
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

        let result = crate::discord::messaging_wire::reply(
            reply(
                &ctx,
                ChannelId::new(42),
                "do not send",
                Some(MessageId::new(9003)),
                ReplyPing::Allow,
            )
            .await,
        );

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

        let mismatch = crate::discord::messaging_wire::unit(
            react(&ctx, ChannelId::new(42), MessageId::new(7), "✅").await,
        );
        let unknown = crate::discord::messaging_wire::unit(
            react(&ctx, ChannelId::new(42), MessageId::new(8), "✅").await,
        );
        let mismatched_reply = crate::discord::messaging_wire::reply(
            reply(
                &ctx,
                ChannelId::new(42),
                "reply body",
                Some(MessageId::new(7)),
                ReplyPing::Allow,
            )
            .await,
        );
        let unknown_reply = crate::discord::messaging_wire::reply(
            reply(
                &ctx,
                ChannelId::new(42),
                "reply body",
                Some(MessageId::new(8)),
                ReplyPing::Allow,
            )
            .await,
        );
        let disallowed_reply = crate::discord::messaging_wire::reply(
            reply(
                &ctx,
                ChannelId::new(43),
                "reply body",
                Some(MessageId::new(7)),
                ReplyPing::Allow,
            )
            .await,
        );

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
        let blocked = crate::discord::messaging_wire::unit(
            react(&ctx, ChannelId::new(42), MessageId::new(8), "✅").await,
        );
        assert_eq!(blocked["reason"], "target_not_found");

        // Own send recorded via note_sent: exempt, reaction reaches Discord.
        ctx.state.write().await.note_sent(500);
        let exempt = crate::discord::messaging_wire::unit(
            react(&ctx, ChannelId::new(42), MessageId::new(500), "✅").await,
        );
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

    pub(super) fn messaging_ctx(config: LoadedConfig) -> MessagingCtx {
        messaging_ctx_with_http(config, Arc::new(serenity::http::Http::new("fake")))
    }

    pub(super) fn messaging_ctx_with_http(
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

    pub(super) async fn fake_discord_http() -> (
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

                let (status, response_body) = if path.ends_with("/users/@me/channels")
                    && serde_json::from_str::<Value>(body).unwrap()["recipient_id"] == "78"
                {
                    (
                        "403 Forbidden",
                        json!({"code": 50007, "message": "Cannot send messages to this user"})
                            .to_string(),
                    )
                } else if path.ends_with("/users/@me/channels") {
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
                    ("200 OK", {
                        let mut message = wire_message(
                            9001,
                            "explicit read",
                            "2026-08-15T09:00:00.000000+00:00",
                            json!([]),
                        );
                        if path.ends_with("/messages/9015") {
                            message["flags"] = json!(
                                (MessageFlags::SUPPRESS_NOTIFICATIONS
                                    | MessageFlags::SUPPRESS_EMBEDS)
                                    .bits()
                                    | MESSAGE_FLAG_NOT_IN_SDK
                            );
                        }
                        if path.ends_with("/messages/9016") {
                            message["flags"] = json!("invalid");
                        }
                        message.to_string()
                    })
                } else if request_line.starts_with("GET ") && path.contains("/messages?") {
                    let mut message = wire_message(
                        9002,
                        "incidental fetch",
                        "2026-08-15T09:00:01.000000+00:00",
                        json!([]),
                    );
                    message["channel_id"] = json!("42");
                    ("200 OK", json!([message]).to_string())
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
                } else if request_line.starts_with("PATCH ") && path.contains("/messages/") {
                    let body: Value = serde_json::from_str(body).expect("edit body");
                    (
                        "200 OK",
                        wire_message(
                            9001,
                            body["content"].as_str().unwrap_or_default(),
                            "2026-08-15T09:00:00.000000+00:00",
                            json!([]),
                        )
                        .to_string(),
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
        .map_err(crate::discord::messaging_wire::error)
        .unwrap();

        let hook_context = hook_context.lock().unwrap().clone().unwrap();
        assert_eq!(hook_context.text(), "original");
        assert_eq!(hook_context.metadata("claim_locators"), Some("v3:claim:Ig"));
        assert_eq!(
            hook_context.metadata("citation_locators"),
            Some("v3:citation:DA")
        );
        assert_eq!(
            hook_context.metadata("sentex_transport"),
            Some("terminal-visible-role-suffix-v3-after-hooks")
        );

        let audit_context = audit_context.lock().unwrap().clone().unwrap();
        assert_eq!(audit_context.text(), "rewritten");
        assert_eq!(
            audit_context.metadata("claim_locators"),
            Some("v3:claim:Ig")
        );
        assert_eq!(
            audit_context.metadata("citation_locators"),
            Some("v3:citation:DA")
        );
        assert_eq!(
            audit_context.metadata("sentex_transport"),
            Some(SentexTransport::TerminalVisibleRoleSuffixV3AfterHooks.as_str())
        );
        assert_eq!(
            prepared.text,
            "rewritten [🔍=v3:claim:Ig] [🔍=v3:citation:DA]"
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
                "[🔍=v3:claim:CRY]",
                "[🔍=v3:citation:AAAAAAAAAAw]",
                "[🔍=v3:claim:garbage]",
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
                .map_err(crate::discord::messaging_wire::error)
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
        .map_err(crate::discord::messaging_wire::error)
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
        .map_err(crate::discord::messaging_wire::error)
        .expect("mid-message sentex prose is not a terminal locator");

        assert_eq!(prepared.text, content);
    }

    #[tokio::test]
    async fn pre_send_rewrite_cannot_inject_an_unaudited_sentex_locator() {
        for marker in [
            "[🔍=v2:claim:AAAAAAAAAAw]",
            "[🔍=v2:claim:garbage]",
            "[🔍=v3:claim:CRY]",
            "[🔍=v3:citation:garbage]",
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
            .map_err(crate::discord::messaging_wire::error)
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

        let response = crate::discord::messaging_wire::reply(
            reply_with_evidence_and_hook_overrides(
                &ctx,
                ChannelId::new(42),
                "long enough to split",
                None,
                ReplyToolOptions {
                    suppress_ping: ReplyPing::Allow,
                    suppress_embeds: LinkPreviews::Allow,
                    no_rly_hooks: &[],
                    sentex_handles: &handles,
                },
            )
            .await,
        );

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
        .map_err(crate::discord::messaging_wire::error)
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

        let response = crate::discord::messaging_wire::reply(
            reply_with_evidence_and_hook_overrides(
                &ctx,
                ChannelId::new(42),
                "straightforward",
                None,
                ReplyToolOptions {
                    suppress_ping: ReplyPing::Allow,
                    suppress_embeds: LinkPreviews::Allow,
                    no_rly_hooks: &[],
                    sentex_handles: &handles,
                },
            )
            .await,
        );

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
            let bounce = crate::discord::messaging_wire::reply(
                reply(
                    &ctx,
                    ChannelId::new(42),
                    "straightforward",
                    None,
                    ReplyPing::Allow,
                )
                .await,
            );
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
                let response = crate::discord::messaging_wire::rephrase(
                    &HoldHandle::new(handle),
                    rephrase_held(
                        &ctx,
                        &HoldHandle::new(handle),
                        &format!("grounded {marker}"),
                    )
                    .await,
                );

                assert_eq!(
                    response["error"],
                    "raw terminal sentex locators are not accepted in content; use structured handles on reply or send_dm"
                );
                assert_eq!(ctx.no_rly.pending().await, 1);
            }
        }
    }

    /// `blocking_test_config` plus an `auto` entry: utilize → use.
    fn auto_test_config() -> Config {
        let mut raw = Config::default();
        raw.channels.push(ChannelConfig {
            id: "42".into(),
            ..Default::default()
        });
        raw.contradictionary.enabled = true;
        raw.contradictionary.entries.push(Entry {
            pattern: "straightforward".into(),
            action: Action::Block,
            match_mode: MatchMode::Word,
            replace: None,
            reason: Some("nothing is ever straightforward".into()),
        });
        raw.contradictionary.entries.push(Entry {
            pattern: "utilize".into(),
            action: Action::Auto,
            match_mode: MatchMode::Word,
            replace: Some("use".into()),
            reason: None,
        });
        raw
    }

    /// A messaging context on the fake Discord API, with its own state dir
    /// (so the diary lands somewhere the test can read).
    async fn auto_test_ctx() -> (
        MessagingCtx,
        Arc<std::sync::Mutex<Vec<(String, String)>>>,
        tokio::task::JoinHandle<()>,
        tempfile::TempDir,
    ) {
        auto_test_ctx_with(auto_test_config()).await
    }

    async fn auto_test_ctx_with(
        raw: Config,
    ) -> (
        MessagingCtx,
        Arc<std::sync::Mutex<Vec<(String, String)>>>,
        tokio::task::JoinHandle<()>,
        tempfile::TempDir,
    ) {
        let (http, requests, server) = fake_discord_http().await;
        let dir = tempfile::TempDir::new().expect("tempdir");
        let state_dir = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf-8 path");
        let ctx = MessagingCtx::new(
            http,
            new_state(),
            Arc::new(LoadedConfig::from_raw(raw)),
            state_dir.clone(),
            Arc::new(ConsentGate::new(&state_dir)),
            Arc::new(crate::ingress_ledger::IngressLedger::new()),
        );
        (ctx, requests, server, dir)
    }

    #[tokio::test]
    async fn suppress_embeds_is_carried_through_chunks_and_held_rephrases() {
        let mut raw = auto_test_config();
        raw.delivery.text_chunk_limit = 40;
        let (ctx, requests, server, _dir) = auto_test_ctx_with(raw).await;
        for suppress_embeds in [false, true] {
            let content = "https://example.com/ ".repeat(5);
            let response = crate::discord::messaging_wire::reply(
                reply_with_evidence_and_hook_overrides(
                    &ctx,
                    ChannelId::new(42),
                    &content,
                    None,
                    ReplyToolOptions {
                        suppress_ping: ReplyPing::Allow,
                        suppress_embeds: LinkPreviews::from_suppressed(suppress_embeds),
                        no_rly_hooks: &[],
                        sentex_handles: &NO_SENTEX_HANDLES,
                    },
                )
                .await,
            );
            assert_eq!(response["ok"], true, "{response}");
            let mut captured = requests.lock().expect("capture");
            let posted: Vec<Value> = captured
                .iter()
                .filter(|(path, _)| path.ends_with("/messages"))
                .map(|(_, body)| serde_json::from_str(body).unwrap())
                .collect();
            assert!(posted.len() > 1);
            for body in posted {
                assert_eq!(body.get("flags"), suppress_embeds.then_some(&json!(4)));
            }
            captured.clear();
        }
        for rephrase in [false, true] {
            let bounce = crate::discord::messaging_wire::reply(
                reply_with_evidence_and_hook_overrides(
                    &ctx,
                    ChannelId::new(42),
                    "straightforward https://example.com/",
                    None,
                    ReplyToolOptions {
                        suppress_ping: ReplyPing::Allow,
                        suppress_embeds: LinkPreviews::Suppress,
                        no_rly_hooks: &[],
                        sentex_handles: &NO_SENTEX_HANDLES,
                    },
                )
                .await,
            );
            let handle = bounce["held"]["handle"].as_str().expect("held");
            assert!(posted_contents(&requests).is_empty());
            let sent = if rephrase {
                crate::discord::messaging_wire::rephrase(
                    &HoldHandle::new(handle),
                    rephrase_held(&ctx, &HoldHandle::new(handle), "https://example.com/").await,
                )
            } else {
                crate::discord::messaging_wire::release(
                    &HoldHandle::new(handle),
                    release_held(&ctx, &HoldHandle::new(handle)).await,
                )
            };
            assert_eq!(sent["ok"], true, "{sent}");
            let mut captured = requests.lock().expect("capture");
            for (_, body) in captured
                .iter()
                .filter(|(path, _)| path.ends_with("/messages"))
            {
                let body: Value = serde_json::from_str(body).unwrap();
                assert_eq!(body["flags"], 4);
            }
            captured.clear();
        }
        server.abort();
    }

    #[tokio::test]
    async fn suppress_embeds_dm_and_edit_wire_semantics() {
        let (ctx, requests, server, _dir) = auto_test_ctx().await;
        for suppress in [false, true] {
            let result = crate::discord::messaging_wire::dm(
                send_dm_with_evidence_and_hook_overrides(
                    &ctx,
                    UserId::new(77),
                    "https://example.com/",
                    &[],
                    &NO_SENTEX_HANDLES,
                    LinkPreviews::from_suppressed(suppress),
                )
                .await,
            );
            assert_eq!(result["ok"], true, "{result}");
            let mut captured = requests.lock().expect("capture");
            let (_, body) = captured
                .iter()
                .find(|(path, _)| path.ends_with("/messages"))
                .expect("DM sent");
            let body: Value = serde_json::from_str(body).unwrap();
            assert_eq!(body.get("flags"), suppress.then_some(&json!(4)));
            captured.clear();
        }
        for suppress in [None, Some(true), Some(false)] {
            let result = crate::discord::messaging_wire::message(
                edit_message_with_embed_control(
                    &ctx,
                    ChannelId::new(42),
                    MessageId::new(9015),
                    "https://example.com/",
                    &[],
                    suppress,
                )
                .await,
            );
            assert_eq!(result["ok"], true, "{result}");
            let mut captured = requests.lock().expect("capture");
            assert_eq!(
                captured
                    .iter()
                    .filter(|(path, _)| path.ends_with("/messages/9015"))
                    .count(),
                if suppress.is_some() { 2 } else { 1 },
                "omission must not fetch the existing message"
            );
            let (_, body) = captured
                .iter()
                .rev()
                .find(|(path, _)| path.ends_with("/messages/9015"))
                .expect("edited");
            let body: Value = serde_json::from_str(body).unwrap();
            assert_eq!(
                body.get("flags"),
                suppress
                    .map(|v| json!(
                        MessageFlags::SUPPRESS_NOTIFICATIONS.bits()
                            | MESSAGE_FLAG_NOT_IN_SDK
                            | if v { 4 } else { 0 }
                    ))
                    .as_ref()
            );
            captured.clear();
        }
        server.abort();
    }

    #[tokio::test]
    async fn suppress_embeds_edit_fetch_errors_never_patch() {
        let (ctx, requests, server, _dir) = auto_test_ctx().await;
        for message_id in [9003, 9016] {
            let result = crate::discord::messaging_wire::message(
                edit_message_with_embed_control(
                    &ctx,
                    ChannelId::new(42),
                    MessageId::new(message_id),
                    "new text",
                    &[],
                    Some(true),
                )
                .await,
            );
            assert!(result["error"].is_string(), "{result}");
            let mut captured = requests.lock().expect("capture");
            assert_eq!(captured.len(), 1, "failed flag fetch must not edit");
            assert!(captured[0].1.is_empty(), "only a GET, no PATCH payload");
            captured.clear();
        }
        server.abort();
    }

    /// The `content` of every message POSTed to the fake Discord API.
    fn posted_contents(requests: &std::sync::Mutex<Vec<(String, String)>>) -> Vec<String> {
        requests
            .lock()
            .expect("request capture lock")
            .iter()
            .filter(|(path, _)| path.ends_with("/messages"))
            .filter_map(|(_, body)| {
                serde_json::from_str::<Value>(body).ok()?["content"]
                    .as_str()
                    .map(str::to_owned)
            })
            .collect()
    }

    /// The pre-send hooks judge the text that will be sent, so the rewrite
    /// runs before them.
    #[tokio::test]
    async fn auto_rewrite_runs_before_the_pre_send_hooks() {
        let (ctx, requests, server, _dir) = auto_test_ctx().await;
        let captured = Arc::new(std::sync::Mutex::new(None));
        let ctx = ctx.with_pre_send_pipeline(Arc::new(
            PreSendPipeline::new(vec![Box::new(ContextCaptureHook(Arc::clone(&captured)))])
                .unwrap()
                .with_mode(PipelineMode::Enforce),
        ));

        let response = crate::discord::messaging_wire::reply(
            reply(
                &ctx,
                ChannelId::new(42),
                "we utilize it",
                None,
                ReplyPing::Allow,
            )
            .await,
        );

        assert_eq!(response["error"], "captured", "{response}");
        assert!(response.get("auto_rewrites").is_none(), "{response}");
        let context = captured.lock().unwrap().clone().expect("the hook ran");
        assert_eq!(context.text(), "we use it");
        assert!(posted_contents(&requests).is_empty());
        server.abort();
    }

    /// The rewrite runs before the evidence markers are appended, so it
    /// never reaches inside one, and a sentex-bearing reply still sends.
    #[tokio::test]
    async fn auto_rewrite_leaves_evidence_markers_intact() {
        let mut raw = auto_test_config();
        raw.delivery.evidence_markers_enabled = true;
        let (ctx, requests, server, _dir) = auto_test_ctx_with(raw).await;
        let handles = crate::evidence::parse_tool_sentex_handles(&json!({
            "claim_handles": ["34"]
        }))
        .unwrap();

        let response = crate::discord::messaging_wire::reply(
            reply_with_evidence_and_hook_overrides(
                &ctx,
                ChannelId::new(42),
                "we utilize it",
                None,
                ReplyToolOptions {
                    suppress_ping: ReplyPing::Allow,
                    suppress_embeds: LinkPreviews::Allow,
                    no_rly_hooks: &[],
                    sentex_handles: &handles,
                },
            )
            .await,
        );

        assert_eq!(response["ok"], true, "{response}");
        assert_eq!(
            posted_contents(&requests),
            vec![append_markers("we use it", &handles)]
        );
        assert_eq!(
            response["auto_rewrites"],
            json!(["auto: utilize \u{2192} use"])
        );
        assert!(response["claim_locators"].is_array(), "{response}");
        server.abort();
    }

    /// An `auto` pattern that names a marker's own token (`citation`) never
    /// rewrites the marker: the marker's hit is not rewritable, and a
    /// sentex-bearing message cannot be held, so the send is refused.
    #[tokio::test]
    async fn auto_never_rewrites_inside_an_evidence_marker() {
        let mut raw = auto_test_config();
        raw.delivery.evidence_markers_enabled = true;
        raw.contradictionary.entries.push(Entry {
            pattern: "citation".into(),
            action: Action::Auto,
            match_mode: MatchMode::Word,
            replace: Some("source".into()),
            reason: None,
        });
        let (ctx, requests, server, _dir) = auto_test_ctx_with(raw).await;
        let handles = crate::evidence::parse_tool_sentex_handles(&json!({
            "citation_handles": ["12"]
        }))
        .unwrap();

        let response = crate::discord::messaging_wire::reply(
            reply_with_evidence_and_hook_overrides(
                &ctx,
                ChannelId::new(42),
                "see this",
                None,
                ReplyToolOptions {
                    suppress_ping: ReplyPing::Allow,
                    suppress_embeds: LinkPreviews::Allow,
                    no_rly_hooks: &[],
                    sentex_handles: &handles,
                },
            )
            .await,
        );

        assert!(response["error"].is_string(), "{response}");
        assert!(posted_contents(&requests).is_empty());
        server.abort();
    }

    /// The `action: "auto"` records in the diary under `dir`.
    fn auto_diary_records(dir: &tempfile::TempDir) -> Vec<Value> {
        diary_records(dir)
            .into_iter()
            .filter(|record| record["action"] == "auto")
            .collect()
    }

    fn diary_records(dir: &tempfile::TempDir) -> Vec<Value> {
        std::fs::read_to_string(dir.path().join(crate::contradictionary::DIARY_FILE_NAME))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect()
    }

    #[tokio::test]
    async fn reply_sends_auto_rewrite_reports_and_records_it() {
        let (ctx, requests, server, dir) = auto_test_ctx().await;

        let response = {
            let result = reply(
                &ctx,
                ChannelId::new(42),
                "we utilize it and utilize it again",
                None,
                ReplyPing::Allow,
            )
            .await;
            assert!(matches!(
                &result.as_ref().unwrap().outcome,
                ReplyOutcome::Delivered(_)
            ));
            crate::discord::messaging_wire::reply(result)
        };

        assert_eq!(response["ok"], true, "a clean auto hit sends: {response}");
        assert_eq!(
            posted_contents(&requests),
            vec!["we use it and use it again"]
        );
        assert_eq!(
            response["auto_rewrites"],
            json!(vec!["auto: utilize \u{2192} use"; 2])
        );
        assert_eq!(ctx.no_rly.pending().await, 0);
        let records = auto_diary_records(&dir);
        assert_eq!(records.len(), 2, "one record per rewrite: {records:?}");
        for record in records {
            assert_eq!(record["pattern"], "utilize");
            assert_eq!(record["matched"], "utilize");
            assert_eq!(record["replacement"], "use");
            assert_eq!(record["message"], "we utilize it and utilize it again");
            assert_eq!(record["sent"], "we use it and use it again");
            assert_eq!(record["override"], false);
        }
        server.abort();
    }

    #[tokio::test]
    async fn send_dm_sends_auto_rewrite_and_reports_it() {
        let (ctx, requests, server, dir) = auto_test_ctx().await;

        let response = crate::discord::messaging_wire::dm(
            send_dm(&ctx, UserId::new(77), "we utilize the cache").await,
        );

        assert_eq!(response["ok"], true, "{response}");
        assert_eq!(posted_contents(&requests), vec!["we use the cache"]);
        assert_eq!(
            response["auto_rewrites"],
            json!(["auto: utilize \u{2192} use"])
        );
        assert_eq!(auto_diary_records(&dir).len(), 1);
        server.abort();
    }

    #[tokio::test]
    async fn dm_preflight_and_channel_failure_omit_rewrite_metadata() {
        let mut raw = auto_test_config();
        raw.delivery.evidence_markers_enabled = true;
        raw.delivery.text_chunk_limit = 24;
        let (ctx, requests, server, _dir) = auto_test_ctx_with(raw).await;
        let handles =
            crate::evidence::parse_tool_sentex_handles(&json!({"claim_handles": ["12"]})).unwrap();
        let preflight = crate::discord::messaging_wire::dm(
            send_dm_with_evidence_and_hook_overrides(
                &ctx,
                UserId::new(77),
                "we utilize it",
                &[],
                &handles,
                LinkPreviews::Allow,
            )
            .await,
        );
        assert_eq!(
            preflight,
            json!({"error": "sentex-bearing messages must fit in one Discord message"})
        );
        assert!(requests.lock().unwrap().is_empty());

        let failure = send_dm(&ctx, UserId::new(78), "we utilize it")
            .await
            .unwrap_err();
        assert!(matches!(&failure.kind, MessagingErrorKind::CreateDm(_)));
        let response = crate::discord::messaging_wire::error(failure);
        assert!(
            response["error"]
                .as_str()
                .unwrap()
                .starts_with("failed to create DM channel:")
        );
        assert!(response.get("auto_rewrites").is_none(), "{response}");
        assert_eq!(requests.lock().unwrap().len(), 1);
        assert!(posted_contents(&requests).is_empty());
        server.abort();
    }

    #[tokio::test]
    async fn rephrase_validation_precedes_rewrite_metadata_and_handle_lookup() {
        let (ctx, requests, server, _dir) = auto_test_ctx().await;
        let handle = HoldHandle::new("nr-missing-1");
        for (content, expected) in [
            ("   ", "rephrase content must not be empty"),
            (
                "we utilize it [🔍=v2:claim:AAAAAAAAAAw]",
                "raw terminal sentex locators are not accepted in content; use structured handles on reply or send_dm",
            ),
        ] {
            let response = crate::discord::messaging_wire::rephrase(
                &handle,
                rephrase_held(&ctx, &handle, content).await,
            );
            assert_eq!(response, json!({"error": expected}));
        }
        let failure = rephrase_held(&ctx, &handle, "we utilize it")
            .await
            .unwrap_err();
        assert!(matches!(
            &failure.kind,
            MessagingErrorKind::Handle(RejectedHandle::Unknown(_))
        ));
        let response = crate::discord::messaging_wire::error(failure);
        assert_eq!(
            response["auto_rewrites"],
            json!(["auto: utilize \u{2192} use"])
        );
        assert!(requests.lock().unwrap().is_empty());
        server.abort();
    }

    /// An `auto` hit that cannot be rewritten (in a marked span, or beside a
    /// `block` hit) holds the message as written: nothing is rewritten,
    /// sent, or recorded but the hold, and releasing it sends the original.
    #[tokio::test]
    async fn held_auto_hit_holds_and_releases_the_original() {
        for content in ["run `utilize` now", "a straightforward way to utilize it"] {
            let (ctx, requests, server, dir) = auto_test_ctx().await;

            let response = {
                let result = reply(&ctx, ChannelId::new(42), content, None, ReplyPing::Allow).await;
                assert!(matches!(
                    &result.as_ref().unwrap().outcome,
                    ReplyOutcome::Held(_)
                ));
                crate::discord::messaging_wire::reply(result)
            };

            let handle = response["held"]["handle"].as_str().expect("held");
            assert!(response.get("auto_rewrites").is_none(), "{response}");
            assert!(posted_contents(&requests).is_empty());
            let records = diary_records(&dir);
            assert_eq!(records.len(), 1, "only the hold: {records:?}");
            assert_eq!(records[0]["action"], "block");
            assert_eq!(records[0]["message"], content);

            let released = crate::discord::messaging_wire::release(
                &HoldHandle::new(handle),
                release_held(&ctx, &HoldHandle::new(handle)).await,
            );
            assert_eq!(released["ok"], true, "{released}");
            assert_eq!(posted_contents(&requests), vec![content]);
            assert!(auto_diary_records(&dir).is_empty());
            server.abort();
        }
    }

    #[tokio::test]
    async fn rephrase_sends_auto_rewrite_reports_and_records_it() {
        let (ctx, requests, server, dir) = auto_test_ctx().await;
        let bounce = crate::discord::messaging_wire::reply(
            reply(
                &ctx,
                ChannelId::new(42),
                "straightforward",
                None,
                ReplyPing::Allow,
            )
            .await,
        );
        let handle = bounce["held"]["handle"].as_str().expect("held");

        let response = crate::discord::messaging_wire::rephrase(
            &HoldHandle::new(handle),
            rephrase_held(&ctx, &HoldHandle::new(handle), "we utilize the cache").await,
        );

        assert_eq!(response["ok"], true, "{response}");
        assert_eq!(posted_contents(&requests), vec!["we use the cache"]);
        assert_eq!(
            response["auto_rewrites"],
            json!(["auto: utilize \u{2192} use"])
        );
        let records = auto_diary_records(&dir);
        assert_eq!(records.len(), 1, "{records:?}");
        assert_eq!(records[0]["message"], "we utilize the cache");
        assert_eq!(records[0]["sent"], "we use the cache");
        server.abort();
    }

    /// A rewritten reply whose later chunk fails has already shown the
    /// rewrite in the room: the error says so, and the diary records it.
    #[tokio::test]
    async fn partly_delivered_auto_rewrite_is_reported_and_recorded() {
        let mut raw = auto_test_config();
        raw.delivery.text_chunk_limit = 24;
        for (pattern, action) in [("it", Action::Log), ("now", Action::Celebrate)] {
            raw.contradictionary.entries.push(Entry {
                pattern: pattern.into(),
                action,
                match_mode: MatchMode::Word,
                replace: None,
                reason: None,
            });
        }
        let (ctx, requests, server, dir) = auto_test_ctx_with(raw).await;

        let response = {
            let result = reply(
                &ctx,
                ChannelId::new(42),
                "we utilize it now\n\nforce-delivery-failure",
                None,
                ReplyPing::Allow,
            )
            .await;
            assert!(matches!(
                &result.as_ref().unwrap().outcome,
                ReplyOutcome::PartiallyDelivered(_)
            ));
            crate::discord::messaging_wire::reply(result)
        };

        assert!(response["error"].is_string(), "{response}");
        assert!(posted_contents(&requests)[0].starts_with("we use it now"));
        assert_eq!(
            response["auto_rewrites"],
            json!(["auto: utilize \u{2192} use"])
        );
        let records = diary_records(&dir);
        assert!(records.iter().all(|record| record["action"] == "auto"));
        assert_eq!(records.len(), 1, "{records:?}");
        assert_eq!(records[0]["matched"], "utilize");

        let completed = reply(
            &ctx,
            ChannelId::new(42),
            "we utilize it now\n\nlast chunk succeeds",
            None,
            ReplyPing::Allow,
        )
        .await
        .unwrap();
        assert!(matches!(completed.outcome, ReplyOutcome::Delivered(_)));
        let records = diary_records(&dir);
        let mut actions = records
            .iter()
            .map(|record| record["action"].as_str().unwrap())
            .collect::<Vec<_>>();
        actions.sort_unstable();
        assert_eq!(actions, ["auto", "auto", "celebrate", "log"]);
        server.abort();
    }

    /// A rewritten reply that fails outright still reports the rewrite it
    /// attempted, and records nothing: nothing reached the room.
    #[tokio::test]
    async fn failed_auto_reply_reports_the_rewrite() {
        let (ctx, requests, server, dir) = auto_test_ctx().await;

        let response = {
            let result = reply(
                &ctx,
                ChannelId::new(42),
                "we utilize force-delivery-failure",
                None,
                ReplyPing::Allow,
            )
            .await;
            assert!(matches!(
                &result.as_ref().unwrap_err().kind,
                MessagingErrorKind::Delivery(DeliveryCause::ChunkSend { index: 0, .. })
            ));
            crate::discord::messaging_wire::reply(result)
        };

        assert!(response["error"].is_string(), "{response}");
        assert_eq!(
            posted_contents(&requests),
            vec!["we use force-delivery-failure"]
        );
        assert_eq!(
            response["auto_rewrites"],
            json!(["auto: utilize \u{2192} use"])
        );
        assert!(auto_diary_records(&dir).is_empty());
        server.abort();
    }

    /// A rewritten replacement that fails to deliver stays held as the
    /// construct wrote it: a later `no_rly` sends that text verbatim.
    #[tokio::test]
    async fn failed_rephrase_holds_the_replacement_as_written() {
        let (ctx, requests, server, dir) = auto_test_ctx().await;
        let bounce = crate::discord::messaging_wire::reply(
            reply(
                &ctx,
                ChannelId::new(42),
                "straightforward",
                None,
                ReplyPing::Allow,
            )
            .await,
        );
        let handle = bounce["held"]["handle"].as_str().expect("held");

        let response = crate::discord::messaging_wire::rephrase(
            &HoldHandle::new(handle),
            rephrase_held(
                &ctx,
                &HoldHandle::new(handle),
                "we utilize force-delivery-failure",
            )
            .await,
        );
        assert_eq!(response["handle_still_live"], handle, "{response}");

        let released = crate::discord::messaging_wire::release(
            &HoldHandle::new(handle),
            release_held(&ctx, &HoldHandle::new(handle)).await,
        );
        assert!(released["error"].is_string(), "{released}");
        assert_eq!(
            posted_contents(&requests),
            vec![
                "we use force-delivery-failure",
                "we utilize force-delivery-failure"
            ]
        );
        assert!(auto_diary_records(&dir).is_empty());
        assert_eq!(
            response["auto_rewrites"],
            json!(["auto: utilize \u{2192} use"])
        );
        server.abort();
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

        let result = crate::discord::messaging_wire::dm(
            send_dm_with_evidence_and_hook_overrides(
                &ctx,
                UserId::new(77),
                "grounded",
                &[],
                &handles,
                LinkPreviews::Allow,
            )
            .await,
        );
        server.abort();

        assert_eq!(
            result,
            json!({
                "ok": true,
                "channel_id": "4242",
                "message_ids": [9001],
                "claim_locators": ["v3:claim:Ig"],
                "citation_locators": ["v3:citation:DA"],
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
            "grounded [🔍=v3:claim:Ig] [🔍=v3:citation:DA]"
        );
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

        let result = crate::discord::messaging_wire::unit(
            react(&ctx, ChannelId::new(42), MessageId::new(9001), "👍").await,
        );
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
        .map_err(crate::discord::messaging_wire::error)
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

        let result = crate::discord::messaging_wire::reply(
            reply(&ctx, ChannelId::new(42), "text", None, ReplyPing::Allow).await,
        );

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
        let admitted = crate::discord::messaging_wire::reply(
            reply_with_hook_overrides(
                &ctx,
                ChannelId::new(42),
                "text",
                Some(MessageId::new(7)),
                ReplyPing::Allow,
                &[],
            )
            .await,
        );
        assert_eq!(admitted["error"], "blocked by test hook");

        // Unknown reply (msg 9003 not in ledger; REST 403) → blocked before halt hook.
        let unknown = crate::discord::messaging_wire::reply(
            reply_with_hook_overrides(
                &ctx,
                ChannelId::new(42),
                "text",
                Some(MessageId::new(9003)),
                ReplyPing::Allow,
                &[],
            )
            .await,
        );
        assert_eq!(
            unknown["reason"], "canonical_lookup_failed",
            "unverified reply_to must be blocked before reaching hooks: {unknown}"
        );

        // Mismatch reply (msg 8 admitted in ch 41, claimed ch 42) → blocked before halt hook.
        let mismatch = crate::discord::messaging_wire::reply(
            reply_with_hook_overrides(
                &ctx,
                ChannelId::new(42),
                "text",
                Some(MessageId::new(8)),
                ReplyPing::Allow,
                &[],
            )
            .await,
        );
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

        let result = crate::discord::messaging_wire::message(
            edit_message(&ctx, ChannelId::new(42), MessageId::new(7), "text").await,
        );

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

        let result = crate::discord::messaging_wire::message(
            send_attachment_with_hook_overrides(
                &ctx,
                ChannelId::new(42),
                attachment,
                Some("caption"),
                &[],
                OutboundSurface::SendFileCaption,
            )
            .await,
        );

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

        let result =
            crate::discord::messaging_wire::dm(send_dm(&ctx, UserId::new(77), "text").await);

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

        let result =
            crate::discord::messaging_wire::dm(send_dm(&ctx, UserId::new(77), "text").await);

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
        raw.delivery.evidence_markers_enabled = true;
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
        let (http, requests, server) = fake_discord_http().await;
        let ctx = messaging_ctx_with_http(LoadedConfig::from_raw(raw), http)
            .with_pre_send_pipeline(Arc::new(pipeline));
        let handles = crate::evidence::parse_tool_sentex_handles(
            &json!({"claim_handles":["34"],"citation_handles":["12"]}),
        )
        .unwrap();
        let result = send_dm_with_evidence_and_hook_overrides(
            &ctx,
            UserId::new(77),
            "text",
            &[],
            &handles,
            LinkPreviews::Allow,
        )
        .await;
        assert!(matches!(
            &result.as_ref().unwrap().outcome,
            DmOutcome::Delivered {
                route: DmDeliveryRoute::Redirected,
                ..
            }
        ));
        assert_eq!(
            crate::discord::messaging_wire::dm(result),
            json!({"ok":true,"message_ids":[9001],"claim_locators":["v3:claim:Ig"],"citation_locators":["v3:citation:DA"]})
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(
            !requests
                .lock()
                .unwrap()
                .iter()
                .any(|(path, _)| path.contains("/users/@me/channels"))
        );
        server.abort();
    }

    #[tokio::test]
    async fn captionless_send_file_rejects_hook_overrides_before_file_access() {
        let ctx = messaging_ctx(test_config());
        let hook = HookName::parse("surface-halt").unwrap();

        let result = crate::discord::messaging_wire::message(
            send_file_with_hook_overrides(
                &ctx,
                ChannelId::new(42),
                "/does/not/exist",
                None,
                &[hook],
            )
            .await,
        );

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

        let result = prepare_voice_text(&ctx, ChannelId::new(42), "speech", &[])
            .await
            .map_err(crate::discord::messaging_wire::error);

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
            .map_err(crate::discord::messaging_wire::error)
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

        let result = crate::discord::messaging_wire::reply(
            reply(&ctx, ChannelId::new(42), "text", None, ReplyPing::Allow).await,
        );

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
    pub(super) fn wire_message(
        id: u64,
        content: &str,
        timestamp: &str,
        attachments: Value,
    ) -> Value {
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
            parse_reaction_type("👍").unwrap(),
            ReactionType::Unicode("👍".to_string())
        );
    }

    /// What the fake claim-once server saw, by message id.
    #[derive(Default)]
    struct ClaimLog {
        /// Message ids claimed (every claim, whatever the outcome).
        claimed: Vec<String>,
        done: Vec<String>,
        /// The reply message id each `done` carried, parallel to `done`.
        done_replies: Vec<String>,
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
        assert!(err.to_string().contains("already answering"), "{err}");
    }

    #[test]
    fn claim_winner_display_preserves_raw_winner_order_and_opaque_labels() {
        let names = std::collections::BTreeMap::from([
            (111, "FixtureComet (she/her)".to_owned()),
            (222, "Later".to_owned()),
            (333, "   ".to_owned()),
        ]);
        for (ahead, expected) in [
            (vec!["111", "222"], "111=FixtureComet (she/her)"),
            (vec!["999", "111"], "999"),
            (vec!["333"], "333"),
            (vec!["operator", "111"], "operator"),
            (vec!["0111", "222"], "0111"),
            (vec![], "another construct"),
        ] {
            let ahead: Vec<String> = ahead.into_iter().map(str::to_owned).collect();
            let before = ahead.clone();
            assert_eq!(claim_winner_display(&ahead, &names), expected);
            assert_eq!(ahead, before);
        }
    }

    #[tokio::test]
    async fn claim_wait_uses_cached_winner_identity_without_discord_http() {
        let (addr, _log) = fake_claim_server().await;
        let (http, requests, server) = fake_discord_http().await;
        let winner = messaging_ctx_with_http(coordinated_config(&addr, 111), Arc::clone(&http));
        let waiter = messaging_ctx_with_http(coordinated_config(&addr, 222), http);
        assert!(
            coordinate_reply(&winner, ChannelId::new(42), MessageId::new(8001))
                .await
                .unwrap()
        );
        for (cached, expected) in [
            (
                Some("FixtureComet (she/her)"),
                "claim-once: 111=FixtureComet (she/her) is already answering this message",
            ),
            (None, "claim-once: 111 is already answering this message"),
            (
                Some(" 	"),
                "claim-once: 111 is already answering this message",
            ),
        ] {
            {
                let mut state = waiter.state.write().await;
                state.user_names.remove(&111);
                if let Some(name) = cached {
                    state.cache_username(111, name.to_owned());
                }
            }
            let error = coordinate_reply(&waiter, ChannelId::new(42), MessageId::new(8001))
                .await
                .unwrap_err();
            assert_eq!(error.to_string(), expected);
            let MessagingErrorKind::CoordinationBusy { ahead, .. } = &error.kind else {
                panic!("winner must refuse the second claimant");
            };
            assert_eq!(ahead, &["111".to_owned()]);
        }
        assert!(
            requests.lock().unwrap().is_empty(),
            "claim refusal must never fetch Discord identity"
        );
        server.abort();
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

        let refused = crate::discord::messaging_wire::reply(
            reply(
                &ctx_a,
                ChannelId::new(42),
                "no",
                Some(MessageId::new(9001)),
                ReplyPing::Allow,
            )
            .await,
        );
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

        let failed = {
            let result = reply(
                &ctx_a,
                ChannelId::new(42),
                "force-delivery-failure",
                Some(MessageId::new(9001)),
                ReplyPing::Allow,
            )
            .await;
            assert!(matches!(
                &result.as_ref().unwrap_err().kind,
                MessagingErrorKind::Delivery(DeliveryCause::ChunkSend { index: 0, .. })
            ));
            crate::discord::messaging_wire::reply(result)
        };
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

        let partial = {
            let result = reply(
                &ctx_a,
                ChannelId::new(42),
                "first chunk goes out\n\nsecond chunk goes too\n\nforce-delivery-failure",
                Some(MessageId::new(7001)),
                ReplyPing::Allow,
            )
            .await;
            assert!(matches!(
                &result.as_ref().unwrap().outcome,
                ReplyOutcome::PartiallyDelivered(_)
            ));
            crate::discord::messaging_wire::reply(result)
        };
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
        let sent = crate::discord::messaging_wire::reply(
            reply(
                &ctx,
                ChannelId::new(42),
                "answer",
                Some(MessageId::new(7001)),
                ReplyPing::Allow,
            )
            .await,
        );
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
        let failed = crate::discord::messaging_wire::reply(
            reply(
                &ctx,
                ChannelId::new(42),
                "force-delivery-failure",
                Some(MessageId::new(7001)),
                ReplyPing::Allow,
            )
            .await,
        );
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
        assert!(
            coordinate_reply(&ctx, ChannelId::new(42), MessageId::new(445))
                .await
                .unwrap(),
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
        assert!(
            !coordinate_reply(&open, ChannelId::new(42), MessageId::new(7))
                .await
                .unwrap(),
            "fail_open = true must let the reply through, holding no claim"
        );
        let closed = messaging_ctx(unreachable_coordinator_config(false).await);
        let err = coordinate_reply(&closed, ChannelId::new(42), MessageId::new(7))
            .await
            .expect_err("fail_open = false must refuse");
        assert!(err.to_string().contains("fail-closed"), "{err}");
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
        let sent = {
            let result = reply(
                &ctx,
                ChannelId::new(42),
                "answer",
                Some(MessageId::new(7001)),
                ReplyPing::Allow,
            )
            .await;
            assert!(matches!(
                &result.as_ref().unwrap().outcome,
                ReplyOutcome::Delivered(_)
            ));
            crate::discord::messaging_wire::reply(result)
        };
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
        assert!(
            !coordinate_reply(&ctx, ChannelId::new(43), MessageId::new(7))
                .await
                .unwrap()
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
            err.to_string().contains("already answering"),
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
    #[tokio::test]
    async fn empty_reply_reaches_the_transport_without_new_validation() {
        let (ctx, requests, server, _dir) = auto_test_ctx().await;
        let result = reply(&ctx, ChannelId::new(42), "", None, ReplyPing::Allow).await;
        let ReplyOutcome::Delivered(delivery) = &result.as_ref().unwrap().outcome else {
            panic!("empty reply must complete")
        };
        assert_eq!(delivery.message_ids(), [MessageId::new(9001)]);
        assert_eq!(
            crate::discord::messaging_wire::reply(result),
            json!({"ok": true, "message_ids": [9001]})
        );
        assert_eq!(posted_contents(&requests), [""]);
        server.abort();
    }
    #[tokio::test]
    async fn registered_reply_reloads_discovery_and_policy_and_preserves_options() {
        let dir = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(dir.path().to_owned()).unwrap();
        std::fs::write(
            path.join("config.toml"),
            "[delivery]\nevidence_markers_enabled = false\n",
        )
        .unwrap();
        assert!(crate::config::reload_config(&path).await.1.is_none());
        let (http, requests, peer) = fake_discord_http().await;
        let (notifications, _rx) = tokio::sync::mpsc::channel(4);
        let server = crate::mcp::server::DioneServer::new(
            new_state(),
            Arc::new(tokio::sync::Mutex::new(crate::queue::AccessQueue::load(
                &path,
            ))),
            http,
            path.clone(),
            notifications,
            crate::tracing_channel::TraceLevelController::noop(),
            crate::harness::HarnessConnection::claude(),
            Arc::new(ConsentGate::new(&path)),
            Arc::new(IngressLedger::new()),
            crate::core::ModuleInstanceId::new(
                crate::core::ProviderId::new("discord").unwrap(),
                crate::core::ModuleSlotId::new("primary").unwrap(),
            ),
            None,
        )
        .await
        .unwrap()
        .with_module_tools(Vec::new())
        .unwrap();
        let list = json!({"jsonrpc":"2.0","id":1,"method":"tools/list"});
        let initial = crate::mcp::server::test_helpers::dispatch_request(&server, list.clone())
            .await
            .unwrap();
        let reply = initial["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool["name"] == "reply")
            .unwrap();
        assert!(
            reply["inputSchema"]["properties"]
                .get("claim_handles")
                .is_none()
        );
        assert_eq!(
            initial["result"]["tools"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|tool| tool["name"] == "reply")
                .count(),
            1
        );
        let call = |arguments| json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"reply","arguments":arguments}});
        let disabled = crate::mcp::server::test_helpers::dispatch_request(
            &server,
            call(json!({"channel_id":"42","content":"hello","claim_handles":[]})),
        )
        .await
        .unwrap();
        assert_eq!(
            disabled["error"]["message"],
            "claim_handles and citation_handles require evidence_markers_enabled"
        );
        let denied = crate::mcp::server::test_helpers::dispatch_request(
            &server,
            call(json!({"channel_id":"42","content":"hello"})),
        )
        .await
        .unwrap();
        let denied: Value =
            serde_json::from_str(denied["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert!(
            denied["error"]
                .as_str()
                .unwrap()
                .contains("not a permitted outbound target")
        );
        assert!(requests.lock().unwrap().is_empty());

        std::fs::write(path.join("config.toml"), "[[channels]]\nid = \"42\"\nrequire_mention = false\n[delivery]\nevidence_markers_enabled = true\n").unwrap();
        assert!(crate::config::reload_config(&path).await.1.is_none());
        let enabled = crate::mcp::server::test_helpers::dispatch_request(&server, list)
            .await
            .unwrap();
        let reply = enabled["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool["name"] == "reply")
            .unwrap();
        assert_eq!(
            reply["inputSchema"]["properties"]["claim_handles"]["type"],
            "array"
        );
        assert_eq!(
            reply["inputSchema"]["properties"]["citation_handles"]["type"],
            "array"
        );
        let accepted = crate::mcp::server::test_helpers::dispatch_request(&server, call(json!({"channel_id":42,"content":"hello","reply_to_message_id":"7001","suppress_ping":true,"suppress_embeds":true,"no_rly_hooks":[],"claim_handles":["1"]}))).await.unwrap();
        assert!(accepted["result"].get("isError").is_none(), "{accepted}");
        let accepted: Value =
            serde_json::from_str(accepted["result"]["content"][0]["text"].as_str().unwrap())
                .unwrap();
        assert_eq!(accepted["message_ids"], json!([9001]));
        assert_eq!(accepted["ok"], true);
        assert_eq!(accepted["claim_locators"].as_array().unwrap().len(), 1);
        let sent: Vec<Value> = requests
            .lock()
            .unwrap()
            .iter()
            .filter(|(route, body)| route.ends_with("/channels/42/messages") && !body.is_empty())
            .map(|(_, body)| serde_json::from_str(body).unwrap())
            .collect();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0]["message_reference"]["message_id"], "7001");
        assert_eq!(sent[0]["allowed_mentions"]["replied_user"], false);
        assert_eq!(sent[0]["flags"], 4);
        assert!(
            sent[0]["content"]
                .as_str()
                .unwrap()
                .starts_with("hello [🔍=v3:claim:")
        );

        std::fs::write(path.join("config.toml"), "[[channels]]\nid = \"42\"\nrequire_mention = false\n[contradictionary]\nenabled = true\n[[contradictionary.entries]]\npattern = \"blocked\"\naction = \"block\"\nmatch_mode = \"word\"\n").unwrap();
        assert!(crate::config::reload_config(&path).await.1.is_none());
        let before_hold = requests.lock().unwrap().len();
        let held = crate::mcp::server::test_helpers::dispatch_request(
            &server,
            call(json!({"channel_id":"42","content":"blocked"})),
        )
        .await
        .unwrap();
        assert_eq!(held["result"]["isError"], true);
        let held: Value =
            serde_json::from_str(held["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        let handle = held["held"]["handle"]
            .as_str()
            .expect("reply issued a held capability");
        assert_eq!(requests.lock().unwrap().len(), before_hold);
        let release = json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"no_rly","arguments":{"handle":handle}}});
        let released = crate::mcp::server::test_helpers::dispatch_request(&server, release.clone())
            .await
            .unwrap();
        assert!(released["result"].get("isError").is_none(), "{released}");
        let released: Value =
            serde_json::from_str(released["result"]["content"][0]["text"].as_str().unwrap())
                .unwrap();
        assert_eq!(released["ok"], true);
        assert_eq!(released["released"], handle);
        assert_eq!(released["message_ids"], json!([9002]));
        let release_count = requests.lock().unwrap().len();
        let duplicate = crate::mcp::server::test_helpers::dispatch_request(&server, release)
            .await
            .unwrap();
        assert_eq!(duplicate["result"]["isError"], true);
        assert_eq!(requests.lock().unwrap().len(), release_count);

        let before_denial = requests.lock().unwrap().len();
        std::fs::write(
            path.join("config.toml"),
            "[delivery]\nevidence_markers_enabled = false\n",
        )
        .unwrap();
        assert!(crate::config::reload_config(&path).await.1.is_none());
        let denied = crate::mcp::server::test_helpers::dispatch_request(
            &server,
            call(json!({"channel_id":"42","content":"after revocation"})),
        )
        .await
        .unwrap();
        assert_eq!(denied["result"]["isError"], true);
        assert_eq!(requests.lock().unwrap().len(), before_denial);
        peer.abort();
    }
}
