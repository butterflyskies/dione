use crate::{
    bell_rings::BellStatus,
    discord::{
        verified_action::{
            LifecycleContext, LifecycleProvenance, VerifiedActionGate, VerifiedGateVerdict,
        },
        verified_action_runtime::{
            BoundPrincipalResolver, VerifiedUpdateCandidate, fresh_policy_snapshot,
        },
    },
    gate::{
        GateDecision, InboundGate, MentionDetector, MentionKind, ReplyParentAction,
        ReplyParentResolution, classify_reply_parent_ignore_default,
    },
    mcp::tools::{bot_state::DiscordCommand, messaging::create_dm_channel},
    queue::AccessRequest,
    timestamp::Timestamp,
};
use serenity::{
    async_trait,
    builder::{CreateInteractionResponse, CreateInteractionResponseMessage, CreateMessage},
    gateway::{ActivityData, ShardMessenger},
    model::{event::MessageUpdateEvent, prelude::*},
    prelude::*,
};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

// ── Presence sink trait ──────────────────────────────────────────────────────

/// Abstraction over the Discord shard messenger for presence updates.
///
/// The real implementation wraps [`ShardMessenger`]; tests substitute a mock
/// that records calls without needing a live gateway connection.
pub trait PresenceSink: Send + Sync {
    /// Set the bot's presence (activity + online status) on the gateway.
    fn set_presence(&self, activity: Option<ActivityData>, status: OnlineStatus);
}

impl PresenceSink for ShardMessenger {
    fn set_presence(&self, activity: Option<ActivityData>, status: OnlineStatus) {
        ShardMessenger::set_presence(self, activity, status);
    }
}

/// The last requested presence configuration, stored for replay on reconnect.
#[derive(Clone, Debug)]
pub struct DesiredPresence {
    pub activity: Option<ActivityData>,
    pub status: OnlineStatus,
    /// When this presence was requested. Replay on reconnect re-sends the
    /// same request, so the stamp survives it: it dates the *decision*, not
    /// the latest gateway delivery.
    pub set_at: chrono::DateTime<chrono::Utc>,
}

/// A point-in-time read of the presence state: what was last requested, and
/// whether a sink from the most recent gateway `ready()` is installed.
///
/// `sink_installed` is **not** a live connection probe: the sink is replaced
/// on each `ready()` but never cleared on disconnect, so it reports "a shard
/// messenger has been installed since this process last saw `ready()`" and
/// nothing stronger.
#[derive(Clone, Debug)]
pub struct PresenceSnapshot {
    pub desired: Option<DesiredPresence>,
    pub sink_installed: bool,
}

/// The complete presence state behind one lock: the last requested presence
/// and the live sink slot. Keeping both under a single lock means every
/// observation is a coherent pair — a snapshot can never combine a desired
/// state and a sink observation from two different moments.
struct PresenceState {
    sink: Option<Arc<dyn PresenceSink>>,
    desired: Option<DesiredPresence>,
}

/// A replaceable presence sink that survives gateway reconnects.
///
/// Bundles the live sink slot with the desired-state store under one state
/// lock, so every observation is a coherent pair. Effects (calls into the
/// dynamic sink) run **outside** the state lock: a slow or reentrant sink
/// can never stall state reads or deadlock against them. Ordering of
/// effects is preserved explicitly by the dedicated `effects` mutex, which
/// every mutating path holds end-to-end — writers are serialized in arrival
/// order, and the sink observes exactly that order.
///
/// Lock order: `effects` → `state`, never the reverse. `snapshot()` takes
/// only the state lock and is never blocked by an in-flight sink call.
#[derive(Clone)]
pub struct SharedPresence {
    state: Arc<tokio::sync::RwLock<PresenceState>>,
    effects: Arc<tokio::sync::Mutex<()>>,
}

impl SharedPresence {
    /// Creates empty slots — no sink installed, no desired presence.
    pub fn new() -> Self {
        Self {
            state: Arc::new(tokio::sync::RwLock::new(PresenceState {
                sink: None,
                desired: None,
            })),
            effects: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    /// True when `other` is a handle onto this exact state — the wiring
    /// invariant `wire_shared_presence` establishes and tests pin.
    pub fn is_same_authority(&self, other: &SharedPresence) -> bool {
        Arc::ptr_eq(&self.state, &other.state)
    }

    /// Install a new sink and replay the stored desired presence.
    ///
    /// Called from `ready()` on each gateway reconnect. The whole
    /// install-and-replay runs under the effects mutex, so no other
    /// mutating path can interleave between the install and its replay; the
    /// state lock itself is released before the sink callback runs.
    pub async fn install(&self, new_sink: Arc<dyn PresenceSink>) {
        let _effects = self.effects.lock().await;
        let replay = {
            let mut state = self.state.write().await;
            let replay = state.desired.clone();
            state.sink = Some(Arc::clone(&new_sink));
            replay
        };
        if let Some(desired) = replay {
            tracing::info!("replaying desired presence on reconnect");
            new_sink.set_presence(desired.activity, desired.status);
        }
    }

    /// Store desired presence and dispatch to the current sink.
    ///
    /// The desired state is authoritative the moment this returns: a
    /// `snapshot()` immediately after always reflects it. If no sink is
    /// available (reconnecting), the state is still stored and will be
    /// replayed when the next sink installs. The effects mutex serializes
    /// concurrent callers, so the sink observes requests in the order their
    /// desired states were stored.
    pub async fn set_presence(&self, activity: Option<ActivityData>, status: OnlineStatus) {
        let _effects = self.effects.lock().await;
        let sink = {
            let mut state = self.state.write().await;
            state.desired = Some(DesiredPresence {
                activity: activity.clone(),
                status,
                set_at: chrono::Utc::now(),
            });
            state.sink.clone()
        };
        if let Some(sink) = sink {
            sink.set_presence(activity, status);
        } else {
            tracing::warn!(
                "presence command accepted but shard messenger unavailable \
                 (reconnecting); will replay on next ready"
            );
        }
    }

    /// Read the stored desired presence (test-only).
    #[cfg(test)]
    pub(crate) async fn desired_for_test(&self) -> Option<DesiredPresence> {
        self.state.read().await.desired.clone()
    }

    /// A coherent point-in-time read for `get_presence`: the last requested
    /// presence and whether a sink is installed, observed under one lock.
    /// `desired` with no sink means the request is stored and will replay on
    /// the next `ready()`; a sink with no `desired` means nothing was ever
    /// requested this process — Discord is showing the gateway default.
    pub async fn snapshot(&self) -> PresenceSnapshot {
        let state = self.state.read().await;
        PresenceSnapshot {
            desired: state.desired.clone(),
            sink_installed: state.sink.is_some(),
        }
    }
}

impl Default for SharedPresence {
    fn default() -> Self {
        Self::new()
    }
}

/// The production presence assembly: mints ONE authority and hands the
/// gateway handler and the MCP server handles onto the same state. `main`
/// must obtain both handles from this function — constructing two separate
/// `SharedPresence` values would silently split reconnect replay from MCP
/// read/write, which is exactly the divergence
/// [`SharedPresence::is_same_authority`] and the assembly tests pin.
pub fn wire_shared_presence() -> (SharedPresence, Option<SharedPresence>) {
    let authority = SharedPresence::new();
    let server_handle = Some(authority.clone());
    (authority, server_handle)
}

/// A display name with pronouns already resolved and appended (e.g. "paceheart (she/her)").
pub struct PronounDisplayName(pub String);

// ── Event types ───────────────────────────────────────────────────────────────

/// Metadata about a Discord attachment, forwarded to the MCP client.
#[derive(Debug, Clone)]
pub struct AttachmentMeta {
    pub name: String,
    pub content_type: Option<String>,
    pub size: u64,
}

/// A Discord message forwarded from the gateway to the MCP notification stream.
#[derive(Debug, Clone)]
pub struct MessageEvent {
    pub chat_id: ChannelId,
    pub message_id: MessageId,
    pub user: String,
    pub user_id: UserId,
    pub content: String,
    /// Typed targeting evidence captured at the Discord ingress boundary.
    pub targeting: MessageTargeting,
    pub timestamp: Timestamp,
    pub attachments: Vec<AttachmentMeta>,
    pub is_voice_message: bool,
    /// If the message was sent in a thread, the parent channel ID.
    pub thread_parent_id: Option<ChannelId>,
    /// If the message is a reply, the ID of the message being replied to.
    pub reply_to_message_id: Option<MessageId>,
    /// If the message is a reply, the author ID of the replied-to message.
    pub reply_to_user_id: Option<UserId>,
    /// If the message is a reply, the author name of the replied-to message.
    pub reply_to_user: Option<String>,
    /// If the message is a reply, a short preview of the replied-to content.
    pub reply_to_content_preview: Option<String>,
    /// Pre-rendered bell metadata (live mode only).
    pub bells: Option<String>,
    /// Retrieval status for bell evaluation.
    pub bells_status: Option<BellStatus>,
}

/// Whether Discord ingress had explicit evidence that a message targeted this construct.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageTargeting {
    /// An accepted direct message.
    DirectMessage,
    /// A guild message with explicit typed mention evidence.
    GuildDirected(MentionKind),
    /// An opted-in guild message delivered without directed evidence.
    Ambient,
}

impl MessageTargeting {
    /// Returns whether the message is eligible for directed-only processing.
    pub fn is_directed(self) -> bool {
        !matches!(self, Self::Ambient)
    }
}

/// Events forwarded from the Discord gateway to the MCP notification stream.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub enum NotificationEvent {
    Message(MessageEvent),
    Reaction {
        chat_id: ChannelId,
        message_id: MessageId,
        user: String,
        user_id: UserId,
        emoji: String,
        /// When true, this reaction was added by the bot itself (e.g. contradictionary celebrate).
        /// Gateway self-reactions are filtered, so this is only set for tool-initiated reacts.
        self_react: bool,
    },
    PermissionResponse {
        request_id: String,
        granted: bool,
    },
    Trace {
        level: String,
        target: String,
        message: String,
        fields: Vec<(String, String)>,
    },
    MessageEdit {
        chat_id: ChannelId,
        message_id: MessageId,
        user: String,
        user_id: UserId,
        new_content: String,
        timestamp: Timestamp,
        /// If the edit was in a thread, the parent channel ID.
        thread_parent_id: Option<ChannelId>,
        /// If the edited message is a reply, the ID of the message being replied to.
        reply_to_message_id: Option<MessageId>,
    },
    MessageDelete {
        chat_id: ChannelId,
        message_id: MessageId,
        /// If the delete was in a thread, the parent channel ID.
        thread_parent_id: Option<ChannelId>,
    },
    ConfigError {
        error: String,
    },
}

impl NotificationEvent {
    /// Whether this event carries an exact terminal author-offered evidence marker.
    pub(crate) fn has_offered_evidence(&self) -> bool {
        match self {
            Self::Message(message) => {
                !crate::evidence::parse_evidence_locators(&message.content).is_empty()
            }
            Self::MessageEdit { new_content, .. } => {
                !crate::evidence::parse_evidence_locators(new_content).is_empty()
            }
            _ => false,
        }
    }
}

// ── Handler struct ────────────────────────────────────────────────────────────

/// Serenity event handler — bridges Discord gateway events to the MCP layer.
pub struct Handler {
    pub state: crate::state::State,
    pub queue: Arc<tokio::sync::Mutex<crate::queue::AccessQueue>>,
    pub tx: tokio::sync::mpsc::Sender<NotificationEvent>,
    pub state_dir: camino::Utf8PathBuf,
    pub bot_user_id: AtomicU64,
    /// Receiver for gateway-level commands from MCP tools (e.g. presence updates).
    /// Taken once during `ready()` to spawn the command processing task.
    pub discord_cmd_rx: tokio::sync::Mutex<Option<tokio::sync::mpsc::Receiver<DiscordCommand>>>,
    /// Presence sink + desired-state store — updated on each `ready()` with the
    /// current shard messenger, replayed on reconnect.
    pub presence: SharedPresence,
    /// Pronoun resolution service (PronounDB v2 adapter with cache).
    pub pronoun_service: Option<Arc<crate::pronouns::PronounService>>,
    /// Construct nameplate service (construct-nameplates repo adapter with cache).
    pub nameplate_service: Option<Arc<crate::nameplates::NameplateService>>,
    /// Ingress ledger — records messages admitted by the gateway for egress verification.
    pub ingress_ledger: Arc<crate::ingress_ledger::IngressLedger>,
    /// PluralKit identity resolver for proxy webhook messages.
    pub pk_resolver: Option<Arc<crate::pluralkit::PkResolver>>,
}

impl Handler {
    async fn resolve_identity_enrichment(
        &self,
        user_id: u64,
        is_bot: bool,
        base_name: &str,
    ) -> String {
        if is_bot {
            if let Some(svc) = self.nameplate_service.as_ref()
                && !svc.is_excluded(user_id)
            {
                return svc.resolve_display_name(user_id, base_name).await;
            }
        } else if let Some(svc) = self.pronoun_service.as_ref()
            && !svc.is_excluded(user_id)
        {
            return svc.resolve_display_name(user_id, base_name).await;
        }
        base_name.to_string()
    }

    async fn resolve_pronoun_name(&self, msg: &Message) -> Option<PronounDisplayName> {
        let user_id = msg.author.id.get();
        let base_name = resolve_user_identity(Some(&display_name(msg)), Some(&msg.author.name));

        let resolved = self
            .resolve_identity_enrichment(user_id, msg.author.bot, &base_name)
            .await;

        if resolved == base_name {
            None
        } else {
            Some(PronounDisplayName(resolved))
        }
    }

    async fn emit_lifecycle_delete(
        &self,
        channel_id: ChannelId,
        message_id: MessageId,
        guild_id: Option<GuildId>,
    ) {
        emit_lifecycle_delete_to(
            &self.state,
            &self.ingress_ledger,
            &self.tx,
            channel_id,
            message_id,
            guild_id,
        )
        .await;
    }
}

async fn emit_lifecycle_delete_to(
    state: &crate::state::State,
    ingress_ledger: &crate::ingress_ledger::IngressLedger,
    tx: &tokio::sync::mpsc::Sender<NotificationEvent>,
    channel_id: ChannelId,
    message_id: MessageId,
    guild_id: Option<GuildId>,
) {
    if state
        .read()
        .await
        .recent_sent_ids
        .contains(&message_id.get())
    {
        return;
    }
    let context = guild_id.map_or(LifecycleContext::DirectMessage, LifecycleContext::Guild);
    let crate::ingress_ledger::TransitionResult::Admitted(admission) =
        ingress_ledger.transition_delete(message_id, channel_id, context)
    else {
        return;
    };
    let event = NotificationEvent::MessageDelete {
        chat_id: admission.channel_id(),
        message_id: admission.message_id(),
        thread_parent_id: admission.thread_parent_id(),
    };
    if let Err(error) = tx.send(event).await {
        tracing::warn!(%error, "failed to send message delete notification");
    }
}

/// Record a gateway-admitted message before attempting notification delivery.
///
/// Admission is durable only for this process. A failed channel send remains
/// warning-only and does not retract the evidence that the gateway admitted the
/// message.
async fn send_gateway_admitted_message(
    ingress_ledger: &crate::ingress_ledger::IngressLedger,
    tx: &tokio::sync::mpsc::Sender<NotificationEvent>,
    msg: &Message,
    thread_parent_id: Option<ChannelId>,
    event: NotificationEvent,
    delivery_kind: &'static str,
) {
    let context = msg
        .guild_id
        .map_or(LifecycleContext::DirectMessage, LifecycleContext::Guild);
    if !matches!(
        ingress_ledger.admit_direct_create(
            msg.id,
            msg.channel_id,
            context,
            msg.author.id,
            thread_parent_id,
            &msg.content,
            msg.timestamp,
        ),
        crate::ingress_ledger::TransitionResult::Admitted(_)
    ) {
        return;
    }
    if let Err(error) = tx.send(event).await {
        tracing::warn!(
            %error,
            delivery_kind,
            "failed to send gateway-admitted notification event"
        );
    }
}

// ── EventHandler impl ─────────────────────────────────────────────────────────

#[async_trait]
impl EventHandler for Handler {
    async fn ready(&self, ctx: Context, ready: Ready) {
        let id = ready.user.id.get();
        self.bot_user_id.store(id, Ordering::Relaxed);
        tracing::info!(
            user = %ready.user.name,
            id,
            "Discord gateway ready"
        );

        // Install the new shard messenger and replay any stored presence.
        self.presence.install(Arc::new(ctx.shard.clone())).await;

        // Take the command receiver (once) and spawn the command processor.
        // On reconnect, the existing processor continues — it reads the
        // shared sink, which was just updated above.
        if let Some(rx) = self.discord_cmd_rx.lock().await.take() {
            tokio::spawn(run_discord_commands(self.presence.clone(), rx));
        }
    }

    async fn message(&self, ctx: Context, msg: Message) {
        let config = crate::config::load_config(&self.state_dir);

        // Quick bot filter for non-webhook bots — no expensive lookups needed.
        let is_webhook = msg.webhook_id.is_some();
        if msg.author.bot && !config.is_allowed(msg.author.id.get()) && !is_webhook {
            return;
        }

        let bot_user_id = self.bot_user_id.load(Ordering::Relaxed);

        {
            let mut state = self.state.write().await;
            state.cache_username(msg.author.id.get(), display_name(&msg));
        }

        let is_dm = msg.guild_id.is_none();

        // DM policy authorizes direct Discord users only. Until a typed app-DM
        // policy exists, webhook transport in a DM must fail closed.
        if is_dm && !is_direct_dm_transport(is_dm, is_webhook) {
            return;
        }

        if is_dm {
            let sender_id = msg.author.id.get();
            // #369: only an ignored SENDER drops the DM. The reply-parent is
            // resolved lazily below, and only once we know the DM would be
            // delivered (packet: don't fetch for a bot that would reject the
            // sender anyway).
            let decision = InboundGate::check_dm(&config, sender_id);

            match decision {
                GateDecision::Deliver => {
                    // #369: resolve the reply-parent (3-tier) for redaction /
                    // fail-closed drop. Short-circuits with no fetch when no ids
                    // are ignored.
                    let reply_parent =
                        resolve_reply_parent_ignore(&config, &self.ingress_ledger, &ctx.http, &msg)
                            .await;
                    if reply_parent.drops() {
                        tracing::trace!(
                            sender_id,
                            "DM dropped: reply parent unresolvable (identity-ignore fail-closed)"
                        );
                        return;
                    }
                    let channel_id = msg.channel_id.get();

                    // Record DM channel mapping.
                    {
                        let mut state = self.state.write().await;
                        state.record_dm_channel(sender_id, channel_id);
                    }

                    let pronoun_name = self.resolve_pronoun_name(&msg).await;
                    let event = build_message_event(
                        &msg,
                        &config,
                        &self.ingress_ledger,
                        None,
                        MessageTargeting::DirectMessage,
                        pronoun_name,
                        reply_parent.redacts_preview(),
                    );
                    send_gateway_admitted_message(
                        &self.ingress_ledger,
                        &self.tx,
                        &msg,
                        None,
                        event,
                        "dm",
                    )
                    .await;
                }
                GateDecision::Queue => {
                    let max_pending = config.access_requests.max_pending;
                    let cooldown = std::time::Duration::from_secs(
                        config.access_requests.notify_cooldown_seconds,
                    );
                    let sender_name = display_name(&msg);
                    let request = AccessRequest {
                        user_id: sender_id,
                        username: sender_name.clone(),
                        message_preview: msg.content.clone(),
                        timestamp: chrono::Utc::now(),
                    };

                    let should_notify = {
                        let mut queue = self.queue.lock().await;
                        let added = queue.enqueue(request, max_pending);
                        if added && queue.should_notify_admin(cooldown) {
                            queue.mark_notified();
                            true
                        } else {
                            false
                        }
                    };

                    if should_notify {
                        for &admin_id in &config.admin_ids {
                            notify_admin_dm(
                                &ctx.http,
                                admin_id,
                                &sender_name,
                                sender_id,
                                &msg.content,
                            )
                            .await;
                        }
                    }
                }
                GateDecision::Drop => {
                    tracing::trace!(sender_id = msg.author.id.get(), "DM dropped by gate");
                }
            }
        } else {
            // Guild message.
            let message_mentions: Vec<u64> = msg.mentions.iter().map(|u| u.id.get()).collect();
            let referenced_author_id = msg.referenced_message.as_deref().map(|m| m.author.id.get());
            let mention_kind = MentionDetector::classify(
                bot_user_id,
                &message_mentions,
                &msg.content,
                referenced_author_id,
                config.mention_patterns.as_ref(),
            );

            let channel_id = msg.channel_id.get();

            let resolved =
                resolve_guild_channel(&ctx.http, &self.state, &config, channel_id, false).await;

            // Preflight: skip expensive proxy/PK resolution for ineligible messages.
            // Channel eligibility, guild mute, and mention checks are all O(1).
            // Every preflight drop is recorded in the drop ledger, and the
            // #361 reply-inheritance check runs before any of them, so a
            // reply chain rooted in any dropped message stays dropped.
            if !is_webhook {
                let reply_parent_id = msg
                    .message_reference
                    .as_ref()
                    .and_then(|r| r.message_id)
                    .or_else(|| msg.referenced_message.as_deref().map(|m| m.id));
                let policy = config.channel_policy(resolved.gate_channel_id);
                let guild_muted = msg.guild_id.map(|g| g.get()).is_some_and(|gid| {
                    crate::mute_store::global().is_some_and(|store| store.is_guild_muted(gid))
                });
                match guild_message_preflight(
                    crate::drop_ledger::global(),
                    ChannelId::new(resolved.gate_channel_id),
                    msg.id,
                    reply_parent_id,
                    policy.is_some(),
                    policy.is_some_and(|p| p.require_mention),
                    guild_muted,
                    mention_kind.is_some(),
                ) {
                    GuildPreflight::Proceed => {}
                    GuildPreflight::InheritedDrop => {
                        tracing::debug!(
                            channel_id,
                            parent_id = reply_parent_id.map(|id| id.get()),
                            "guild message dropped: direct reply to a dropped message"
                        );
                        return;
                    }
                    GuildPreflight::NotOptedIn => {
                        tracing::trace!(
                            channel_id,
                            "guild message dropped: channel not opted in (preflight)"
                        );
                        return;
                    }
                    GuildPreflight::GuildMuted => {
                        tracing::trace!(
                            channel_id,
                            "guild message dropped: guild muted (preflight)"
                        );
                        return;
                    }
                    GuildPreflight::MentionRequired => {
                        tracing::trace!(
                            channel_id,
                            "guild message dropped: mention required (preflight)"
                        );
                        return;
                    }
                }
            }

            if is_webhook {
                let delivery_msg = msg.clone();
                let resolution = async {
                    let action = crate::discord::verified_action::DiscordTransportVerifier::new()
                        .verify(&ctx.http, &self.state, msg)
                        .await?;
                    let pk_resolver = self.pk_resolver.as_deref()?;
                    Some(
                        BoundPrincipalResolver::new(pk_resolver)
                            .resolve(action)
                            .await,
                    )
                };
                let Some(plan) = admit_verified_create_after_wait(
                    &delivery_msg,
                    bot_user_id,
                    &self.ingress_ledger,
                    resolution,
                    resolve_thread_parent(&ctx.http, &self.state, channel_id),
                    || crate::config::load_config(&self.state_dir),
                    |guild_id, _gate_channel_id| {
                        crate::mute_store::global()
                            .is_some_and(|store| store.is_guild_muted(guild_id))
                    },
                )
                .await
                else {
                    return;
                };
                // #369: resolve the reply-parent (3-tier) for redaction /
                // fail-closed drop on the verified path too, using the same
                // post-wait config the admission was taken under.
                let reply_parent = resolve_reply_parent_ignore(
                    &plan.config,
                    &self.ingress_ledger,
                    &ctx.http,
                    &delivery_msg,
                )
                .await;
                if reply_parent.drops() {
                    tracing::trace!(
                        channel_id,
                        "verified guild message dropped: reply parent unresolvable (identity-ignore fail-closed)"
                    );
                    return;
                }
                let pronoun_name = self.resolve_pronoun_name(&delivery_msg).await;
                let event = build_verified_message_event(
                    &plan.admission,
                    &delivery_msg,
                    &plan.config,
                    &self.ingress_ledger,
                    plan.thread_parent_id,
                    plan.targeting,
                    pronoun_name,
                    reply_parent.redacts_preview(),
                );
                if let Err(error) = self.tx.send(event).await {
                    tracing::warn!(%error, "failed to send verified guild app action");
                }
                return;
            }

            // Resolve immutable topology before taking the policy snapshot.
            let thread_parent_id = resolve_thread_parent(&ctx.http, &self.state, channel_id).await;
            let config = crate::config::load_config(&self.state_dir);
            let resolved = ResolvedChannel {
                thread_parent_id,
                gate_channel_id: select_gate_channel(&config, channel_id, thread_parent_id),
            };
            let mention_kind = MentionDetector::classify(
                bot_user_id,
                &message_mentions,
                &msg.content,
                referenced_author_id,
                config.mention_patterns.as_ref(),
            );
            let reply_parent_id = msg
                .message_reference
                .as_ref()
                .and_then(|r| r.message_id)
                .or_else(|| msg.referenced_message.as_deref().map(|m| m.id));
            let guild_muted = msg.guild_id.map(|g| g.get()).is_some_and(|gid| {
                crate::mute_store::global().is_some_and(|store| store.is_guild_muted(gid))
            });

            // The single admission authority: delivery is possible only
            // through the targeting its Deliver variant carries. #369 v2: the
            // author-level ignore drop lives inside this authority; the
            // reply-parent is resolved only for an admitted message (below), so
            // an ignored parent redacts the preview rather than dropping.
            let admission = admit_direct_guild_message(
                crate::drop_ledger::global(),
                &config,
                ChannelId::new(resolved.gate_channel_id),
                msg.id,
                reply_parent_id,
                msg.guild_id.map(|g| g.get()),
                guild_muted,
                msg.author.bot,
                msg.author.id.get(),
                mention_kind,
            );

            match admission {
                DirectGuildAdmission::Deliver { targeting } => {
                    // #369: resolve the reply-parent (3-tier) for redaction /
                    // fail-closed drop. Only reached for an admitted message,
                    // and short-circuits with no fetch when no ids are ignored.
                    let reply_parent =
                        resolve_reply_parent_ignore(&config, &self.ingress_ledger, &ctx.http, &msg)
                            .await;
                    if reply_parent.drops() {
                        tracing::trace!(
                            channel_id,
                            "guild message dropped: reply parent unresolvable (identity-ignore fail-closed)"
                        );
                        return;
                    }
                    let pronoun_name = self.resolve_pronoun_name(&msg).await;
                    let event = build_message_event(
                        &msg,
                        &config,
                        &self.ingress_ledger,
                        resolved.thread_parent_id,
                        targeting,
                        pronoun_name,
                        reply_parent.redacts_preview(),
                    );
                    send_gateway_admitted_message(
                        &self.ingress_ledger,
                        &self.tx,
                        &msg,
                        resolved.thread_parent_id.map(ChannelId::new),
                        event,
                        "guild",
                    )
                    .await;
                }
                DirectGuildAdmission::Preflight(reason) => {
                    tracing::debug!(channel_id, ?reason, "guild message suppressed at admission");
                }
                DirectGuildAdmission::BotAuthor => {}
                DirectGuildAdmission::UnexpectedQueue => {
                    // Guild messages don't queue — this case shouldn't occur from check_guild.
                    tracing::debug!(channel_id, "guild message: unexpected Queue decision");
                }
                DirectGuildAdmission::GateDrop => {
                    tracing::trace!(
                        channel_id,
                        sender_id = msg.author.id.get(),
                        "guild message dropped by gate"
                    );
                }
                DirectGuildAdmission::IdentityIgnored => {
                    tracing::trace!(
                        channel_id,
                        sender_id = msg.author.id.get(),
                        "guild message dropped: identity ignore list"
                    );
                }
            }
        }
    }

    async fn reaction_add(&self, ctx: Context, reaction: Reaction) {
        let message_id = reaction.message_id.get();
        let channel_id = reaction.channel_id;
        let bot_id = self.bot_user_id.load(Ordering::Relaxed);

        // Guild mute check — suppress reaction delivery for muted guilds.
        if let Some(gid) = reaction.guild_id
            && let Some(store) = crate::mute_store::global()
            && store.is_guild_muted(gid.get())
        {
            tracing::debug!(guild_id = gid.get(), "reaction dropped: guild muted");
            return;
        }

        // Discard reactions with no user attribution or from the bot itself
        // before the potentially-expensive message authorship lookup.
        let Some(user_id) = gateway_reactor(reaction.user_id, bot_id) else {
            return;
        };

        // #400: an identity-ignored user's reaction must drop, exactly as their
        // messages do. Reactions were the one inbound ingress the ignore never
        // closed — the guild-mute check above suppressed muted-guild reactions,
        // but nothing consulted the reactor's identity. Checked here, before the
        // authorship lookup, so an ignored reactor costs no network fetch.
        //
        // The DECISION (check_reaction) is unit-tested in gate.rs; this call
        // site — the wiring — is NOT integration-testable (reaction_add needs a
        // live serenity Context), same residual class as #334. Do not delete or
        // reorder this block without an integration harness for reaction_add:
        // no unit test will catch its removal.
        let config = crate::config::load_config(&self.state_dir);
        if matches!(
            crate::gate::InboundGate::check_reaction(&config, user_id.get()),
            crate::gate::GateDecision::Drop
        ) {
            tracing::debug!(
                user_id = user_id.get(),
                "reaction dropped: reactor on identity ignore list (#400)"
            );
            return;
        }

        let cached = {
            let state = self.state.read().await;
            if state.recent_sent_ids.contains(&message_id) {
                Some(true)
            } else if state.non_bot_message_ids.contains(&message_id) {
                Some(false)
            } else {
                None
            }
        };

        match cached {
            Some(true) => {}
            Some(false) => return,
            None => match ctx.http.get_message(channel_id, reaction.message_id).await {
                Ok(msg) if msg.author.id.get() == bot_id => {
                    let mut state = self.state.write().await;
                    state.note_sent(message_id);
                }
                Ok(_) => {
                    let mut state = self.state.write().await;
                    state.note_non_bot(message_id);
                    return;
                }
                Err(e) => {
                    tracing::debug!(
                        message_id,
                        error = %e,
                        "could not verify reaction target authorship"
                    );
                    return;
                }
            },
        }

        let emoji = match &reaction.emoji {
            ReactionType::Unicode(s) => s.clone(),
            ReactionType::Custom { name, id, .. } => {
                name.clone().unwrap_or_else(|| id.get().to_string())
            }
            _ => return,
        };

        let cached_name = {
            let state = self.state.read().await;
            state.user_names.get(&user_id.get()).cloned()
        };

        // When we have no cached display name for the reactor, fall back to
        // their Discord username before defaulting to "dione" (#153). Resolve
        // the username from the event's member payload when present, else via a
        // direct user fetch. Skip the fetch entirely on the common cache-hit
        // path.
        let username = if is_blank(cached_name.as_deref()) {
            match &reaction.member {
                Some(member) => Some(member.user.name.clone()),
                None => ctx.http.get_user(user_id).await.map(|u| u.name).ok(),
            }
        } else {
            None
        };
        let user_name = resolve_user_identity(cached_name.as_deref(), username.as_deref());

        let event = NotificationEvent::Reaction {
            chat_id: channel_id,
            message_id: reaction.message_id,
            user: user_name,
            user_id,
            emoji,
            self_react: false,
        };

        if let Err(e) = self.tx.send(event).await {
            tracing::warn!(error = %e, "failed to send reaction notification event");
        }
    }

    async fn message_update(
        &self,
        ctx: Context,
        old_if_available: Option<Message>,
        new: Option<Message>,
        event: MessageUpdateEvent,
    ) {
        let Some(edited_ts) = event.edited_timestamp else {
            return;
        };

        let verified_candidate = match VerifiedUpdateCandidate::from_gateway(
            &event,
            old_if_available.as_ref(),
            new.as_ref(),
        ) {
            Ok(candidate) => candidate,
            Err(reason) => {
                tracing::debug!(
                    reason,
                    "message update dropped: gateway representations conflict"
                );
                return;
            }
        };

        let author = event
            .author
            .as_ref()
            .or_else(|| new.as_ref().map(|m| &m.author))
            .or_else(|| old_if_available.as_ref().map(|m| &m.author));
        let Some(author) = author else {
            return;
        };

        let config = crate::config::load_config(&self.state_dir);

        // Resolve webhook_id from event, new, or old (P2-2: fallback chain).
        let webhook_id = event
            .webhook_id
            .flatten()
            .map(|w| w.get())
            .or_else(|| new.as_ref().and_then(|m| m.webhook_id.map(|w| w.get())))
            .or_else(|| {
                old_if_available
                    .as_ref()
                    .and_then(|m| m.webhook_id.map(|w| w.get()))
            });
        let is_webhook = webhook_id.is_some();

        // Quick bot filter for non-webhook bots.
        if author.bot && !config.is_allowed(author.id.get()) && !is_webhook {
            return;
        }

        let new_content = event
            .content
            .clone()
            .or_else(|| new.as_ref().map(|m| m.content.clone()));
        let Some(new_content) = new_content else {
            return;
        };

        let channel_id = event.channel_id.get();

        let is_dm = event.guild_id.is_none();

        if is_dm && !is_direct_dm_transport(is_dm, is_webhook) {
            return;
        }

        // Check the ingress ledger: only deliver edits for messages that were
        // admitted by the create handler. Applies to both DMs and guild
        // messages — DM lifecycle events must also have admission evidence (P2).
        {
            let verify = self.ingress_ledger.verify(event.id, event.channel_id);
            if !matches!(verify, crate::ingress_ledger::VerifyResult::Admitted { .. }) {
                tracing::trace!(
                    channel_id,
                    message_id = event.id.get(),
                    ?verify,
                    "message edit dropped: original message not in admission ledger"
                );
                return;
            }
        }

        let lifecycle_context = event
            .guild_id
            .map_or(LifecycleContext::DirectMessage, LifecycleContext::Guild);
        let (final_config, admission) = if is_dm {
            // #369: author-level identity ignore still applies to DM edits.
            if !matches!(
                InboundGate::check_dm(&config, author.id.get()),
                GateDecision::Deliver
            ) {
                return;
            }
            // #369 v2: apply the reply-parent contract to the edited message
            // too. A `MessageEdit` carries no quoted preview, so redaction is
            // moot here; only a fail-closed *unresolvable* parent affects
            // delivery (an ignored parent is admitted). Resolve from the full
            // `new` message when the gateway provided it (best-effort).
            if let Some(new_msg) = new.as_ref() {
                let reply_parent =
                    resolve_reply_parent_ignore(&config, &self.ingress_ledger, &ctx.http, new_msg)
                        .await;
                if reply_parent.drops() {
                    tracing::trace!(
                        channel_id,
                        "DM edit dropped: reply parent unresolvable (identity-ignore fail-closed)"
                    );
                    return;
                }
            }
            let admission = match self.ingress_ledger.transition_passive_edit(
                event.id,
                event.channel_id,
                lifecycle_context,
                author.id,
                &new_content,
                edited_ts,
                |_| true,
            ) {
                crate::ingress_ledger::TransitionResult::Admitted(snapshot) => snapshot,
                crate::ingress_ledger::TransitionResult::Duplicate
                | crate::ingress_ledger::TransitionResult::Rejected
                | crate::ingress_ledger::TransitionResult::Unavailable => return,
            };
            (config, admission)
        } else if let Some(candidate) = verified_candidate {
            let Some(webhook_id) = webhook_id.filter(|id| *id != 0).map(WebhookId::new) else {
                return;
            };
            let resolution = async {
                let action = crate::discord::verified_action::DiscordTransportVerifier::new()
                    .verify_update(&ctx.http, &self.state, candidate)
                    .await?;
                let pk_resolver = self.pk_resolver.as_deref()?;
                Some(
                    BoundPrincipalResolver::new(pk_resolver)
                        .resolve(action)
                        .await,
                )
            };
            let Some(plan) = admit_verified_edit_after_wait(
                &event,
                new.as_ref(),
                old_if_available.as_ref(),
                author,
                &new_content,
                edited_ts,
                webhook_id,
                self.bot_user_id.load(Ordering::Relaxed),
                &self.ingress_ledger,
                resolution,
                resolve_thread_parent(&ctx.http, &self.state, channel_id),
                || crate::config::load_config(&self.state_dir),
                |guild_id, _gate_channel_id| {
                    crate::mute_store::global().is_some_and(|store| store.is_guild_muted(guild_id))
                },
            )
            .await
            else {
                return;
            };
            (plan.config, plan.admission)
        } else {
            let thread_parent_id = resolve_thread_parent(&ctx.http, &self.state, channel_id).await;
            let config = crate::config::load_config(&self.state_dir);
            let gate_channel_id = select_gate_channel(&config, channel_id, thread_parent_id);
            let message_mentions = event
                .mentions
                .as_ref()
                .map(|mentions| {
                    mentions
                        .iter()
                        .map(|user| user.id.get())
                        .collect::<Vec<_>>()
                })
                .or_else(|| {
                    new.as_ref()
                        .map(|message| message.mentions.iter().map(|user| user.id.get()).collect())
                })
                .unwrap_or_default();
            let referenced_author_id = new
                .as_ref()
                .and_then(|message| message.referenced_message.as_deref())
                .or_else(|| old_if_available.as_ref()?.referenced_message.as_deref())
                .map(|message| message.author.id.get());
            let mention_kind = MentionDetector::classify(
                self.bot_user_id.load(Ordering::Relaxed),
                &message_mentions,
                &new_content,
                referenced_author_id,
                config.mention_patterns.as_ref(),
            );
            let admission = match self.ingress_ledger.transition_passive_edit(
                event.id,
                event.channel_id,
                lifecycle_context,
                author.id,
                &new_content,
                edited_ts,
                |lineage| {
                    passive_edit_policy_allows(
                        &config,
                        gate_channel_id,
                        event.guild_id.map(|guild_id| guild_id.get()),
                        mention_kind,
                        lineage,
                    )
                },
            ) {
                crate::ingress_ledger::TransitionResult::Admitted(snapshot) => snapshot,
                crate::ingress_ledger::TransitionResult::Duplicate
                | crate::ingress_ledger::TransitionResult::Rejected
                | crate::ingress_ledger::TransitionResult::Unavailable => return,
            };
            (config, admission)
        };

        let sender_name = {
            let mut state = self.state.write().await;
            if is_dm {
                state.record_dm_channel(author.id.get(), channel_id);
            }
            let resolved = new
                .as_ref()
                .map(display_name)
                .unwrap_or_else(|| display_name_from_user(author));
            let base_name = resolve_user_identity(Some(&resolved), Some(&author.name));
            let sender_name = self
                .resolve_identity_enrichment(author.id.get(), author.bot, &base_name)
                .await;
            state.cache_username(author.id.get(), sender_name.clone());
            sender_name
        };

        let timestamp =
            final_config.localize_rfc3339(&serenity_ts_to_rfc3339("edited_ts", &edited_ts));

        // message_reference is Option<Option<MessageReference>> in update events:
        // outer Option = field present in update, inner Option = nullable value.
        let reply_to_message_id = event
            .message_reference
            .as_ref()
            .and_then(|outer| outer.as_ref())
            .and_then(reply_to_id);

        let ev = NotificationEvent::MessageEdit {
            chat_id: admission.channel_id(),
            message_id: admission.message_id(),
            user: sender_name,
            user_id: admission.effective_user_id(),
            new_content,
            timestamp,
            thread_parent_id: admission.thread_parent_id(),
            reply_to_message_id,
        };

        if let Err(e) = self.tx.send(ev).await {
            tracing::warn!(error = %e, "failed to send message edit notification");
        }
    }

    async fn message_delete(
        &self,
        _ctx: Context,
        channel_id: ChannelId,
        deleted_message_id: MessageId,
        guild_id: Option<GuildId>,
    ) {
        self.emit_lifecycle_delete(channel_id, deleted_message_id, guild_id)
            .await;
    }

    async fn message_delete_bulk(
        &self,
        _ctx: Context,
        channel_id: ChannelId,
        multiple_deleted_messages_ids: Vec<MessageId>,
        guild_id: Option<GuildId>,
    ) {
        for message_id in multiple_deleted_messages_ids {
            self.emit_lifecycle_delete(channel_id, message_id, guild_id)
                .await;
        }
    }

    async fn interaction_create(&self, ctx: Context, interaction: Interaction) {
        let Interaction::Component(component) = interaction else {
            return;
        };

        let custom_id = &component.data.custom_id;

        // Parse permission response pattern: "perm:allow:{request_id}" or "perm:deny:{request_id}"
        let (granted, request_id) = if let Some(rid) = custom_id.strip_prefix("perm:allow:") {
            (true, rid.to_string())
        } else if let Some(rid) = custom_id.strip_prefix("perm:deny:") {
            (false, rid.to_string())
        } else {
            return;
        };

        let config = crate::config::load_config(&self.state_dir);
        let sender_id = component.user.id.get();
        let is_admin = config.is_admin(sender_id);

        if !is_admin {
            // Respond with ephemeral "not authorized".
            let resp = CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content("Not authorized.")
                    .ephemeral(true),
            );
            if let Err(e) = component.create_response(&ctx.http, resp).await {
                tracing::warn!(error = %e, "failed to send ephemeral not-authorized response");
            }
            return;
        }

        // Acknowledge the interaction first — Discord requires a response within 3 seconds.
        // Only send the PermissionResponse event if the acknowledgment succeeds.
        let label = if granted { "Allowed" } else { "Denied" };
        let updated_content = format!(
            "Permission request `{request_id}` — **{label}** by <@{}>",
            component.user.id.get()
        );
        let resp = CreateInteractionResponse::UpdateMessage(
            CreateInteractionResponseMessage::new()
                .content(updated_content)
                .components(vec![]),
        );
        if let Err(e) = component.create_response(&ctx.http, resp).await {
            tracing::warn!(error = %e, "failed to update permission button message; not forwarding event");
            return;
        }

        // Remove all pending entries for this request_id (multi-admin: each admin
        // gets a separate DM, so there may be sibling messages to clean up).
        // Removal also re-marks the message IDs as bot-sent, so the gateway
        // message_delete events triggered by the cleanup below are suppressed
        // rather than delivered to the MCP client.
        let siblings = {
            let mut state = self.state.write().await;
            state.remove_permissions_by_request_id(&request_id)
        };

        // Delete all permission DMs — siblings from other admins and the
        // clicked message itself. The interaction response already acknowledged
        // the click, so the message can be cleaned up.
        for (channel_id, msg_id) in &siblings {
            if let Err(e) = ctx.http.delete_message(*channel_id, *msg_id, None).await {
                tracing::warn!(msg_id = msg_id.get(), error = %e, "failed to delete permission DM");
            }
        }

        // Only send the event if we actually owned this request (guard against
        // duplicate clicks after prune already cleared the entries).
        if siblings.is_empty() {
            return;
        }

        let event = NotificationEvent::PermissionResponse {
            request_id: request_id.clone(),
            granted,
        };
        if let Err(e) = self.tx.send(event).await {
            tracing::warn!(error = %e, "failed to send permission response event");
        }
    }
}

// ── Discord command processing ───────────────────────────────────────────────

/// Processes gateway-level commands sent from MCP tools.
///
/// Reads from the [`SharedPresence`] on each dispatch so that reconnects
/// (which install a new shard messenger via `ready()`) take effect immediately
/// without restarting the processor.
async fn run_discord_commands(
    presence: SharedPresence,
    mut rx: tokio::sync::mpsc::Receiver<DiscordCommand>,
) {
    tracing::debug!("discord command processor started");
    while let Some(cmd) = rx.recv().await {
        match cmd {
            DiscordCommand::SetPresence {
                online_status,
                activity_type,
                activity_name,
            } => {
                let status = online_status.to_serenity();
                let activity = match (activity_type, activity_name.as_deref()) {
                    (Some(kind), Some(name)) => Some(kind.to_activity(name)),
                    _ => None,
                };
                tracing::info!(
                    ?status,
                    ?activity_type,
                    activity_name = activity_name.as_deref(),
                    "setting presence"
                );

                presence.set_presence(activity, status).await;
            }
        }
    }
    tracing::debug!("discord command processor stopped (channel closed)");
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Why a guild message did not proceed past the cheap preflight checks, or
/// confirmation that it did. Every non-[`GuildPreflight::Proceed`] variant
/// has already been recorded in the drop ledger by
/// [`guild_message_preflight`], so direct replies inherit the drop (#361).
#[derive(Debug, PartialEq, Eq)]
enum GuildPreflight {
    /// Direct reply to a recorded dropped message: inherits the drop.
    InheritedDrop,
    /// The gate channel is not opted in to delivery.
    NotOptedIn,
    /// The guild is muted.
    GuildMuted,
    /// The channel requires a mention and the message carries none.
    MentionRequired,
    /// No preflight drop applies; continue to the full gate.
    Proceed,
}

/// The preflight authority for direct guild messages: the #361
/// reply-inheritance check runs **before** every policy check, and every
/// drop in a **configured** gate scope — inherited or root — is recorded in
/// the ledger so later replies inherit it.
///
/// Unconfigured channels record nothing: their replies are suppressed by the
/// same `NotOptedIn` check that suppressed the parent, and refusing to
/// record keeps untrusted traffic from minting ledger scopes (the drop
/// ledger's scope set is derived from trusted configuration only). If a
/// channel is configured mid-chain, replies to pre-configuration messages
/// fall back to the ordinary gate — the same fail-open contract as a
/// process restart.
#[expect(
    clippy::too_many_arguments,
    reason = "each argument is one preflight fact; bundling them would hide which check consumes which fact"
)]
fn guild_message_preflight(
    ledger: &crate::drop_ledger::DropLedger,
    gate_scope: ChannelId,
    message_id: MessageId,
    reply_parent_id: Option<MessageId>,
    channel_opted_in: bool,
    require_mention: bool,
    guild_muted: bool,
    mentioned: bool,
) -> GuildPreflight {
    if ledger.reply_inherits_drop(gate_scope, reply_parent_id) {
        ledger.record(gate_scope, message_id);
        return GuildPreflight::InheritedDrop;
    }
    if !channel_opted_in {
        return GuildPreflight::NotOptedIn;
    }
    if guild_muted {
        ledger.record(gate_scope, message_id);
        return GuildPreflight::GuildMuted;
    }
    if require_mention && !mentioned {
        ledger.record(gate_scope, message_id);
        return GuildPreflight::MentionRequired;
    }
    GuildPreflight::Proceed
}

/// Timeout for the tier-3 live parent fetch in [`resolve_reply_parent_ignore`].
///
/// Bounds identity-ignore resolution so a slow or unreachable Discord API
/// cannot stall ingest. Mirrors the deadline pattern used in
/// `verified_action_runtime`.
const IGNORE_PARENT_FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Resolve a reply's parent author and classify the identity-ignore action
/// (#369 v2), 3-tier ladder, first hit wins:
///   tier1: gateway-inlined `referenced_message.author.id` (any age)
///   tier2: ingress-ledger active snapshot for the parent (≤7d, survives a
///          Discord-side deletion) → its `effective_user_id`
///   tier3: one **bounded** live `get_message` on the reference's OWN channel,
///          wrapped in a timeout
///
/// then [`classify_reply_parent_ignore_default`] applies the flagged fail
/// policy to any unresolvable residual.
///
/// The ignore DECISION is always read from the live config snapshot
/// (`is_ignored`), so an un-ignore takes effect on the very next message. The
/// ledger is consulted ONLY to learn WHO authored the parent — an immutable
/// fact — never as the ignore authority.
///
/// NOTE(#369/#400): this path is the *message*-ingress ignore. Reaction ignore
/// is separate and has landed — see [`crate::gate::InboundGate::check_reaction`],
/// consulted in `reaction_add` (#400). "Everywhere" below refers to every
/// *message* ingress path; reactions are gated in their own handler, not here.
async fn resolve_reply_parent_ignore(
    config: &crate::config::LoadedConfig,
    ingress_ledger: &crate::ingress_ledger::IngressLedger,
    http: &serenity::http::Http,
    msg: &Message,
) -> ReplyParentAction {
    // Short-circuit: the feature is unused → no ledger lookup, no live fetch.
    if config.ignored_ids.is_empty() {
        return classify_reply_parent_ignore_default(ReplyParentResolution::Clear);
    }
    let resolution = resolve_reply_parent_resolution(config, ingress_ledger, http, msg).await;
    classify_reply_parent_ignore_default(resolution)
}

/// The impure half of [`resolve_reply_parent_ignore`]: run the 3-tier ladder
/// and report how the parent resolved (kept separate so the pure classifier
/// stays trivially testable).
async fn resolve_reply_parent_resolution(
    config: &crate::config::LoadedConfig,
    ingress_ledger: &crate::ingress_ledger::IngressLedger,
    http: &serenity::http::Http,
    msg: &Message,
) -> ReplyParentResolution {
    let ignored = |author: u64| {
        if config.is_ignored(author) {
            ReplyParentResolution::ParentIgnored
        } else {
            ReplyParentResolution::Clear
        }
    };

    // Establish that this is a genuine reply. A forward/crosspost is decided
    // explicitly as Unresolvable: we do not trust its reference to name an
    // author, and it carries no quoted preview to leak.
    match msg.message_reference.as_ref() {
        Some(reference) if !is_reply_reference(reference) => {
            return ReplyParentResolution::Unresolvable;
        }
        None if msg.referenced_message.is_none() => {
            return ReplyParentResolution::Clear;
        }
        _ => {}
    }

    // tier1: gateway-inlined parent (covers replies of any age; the common case).
    // A webhook/PK parent's `author.id` is the TRANSPORT id, not the principal.
    // Resolve the principal from the ledger snapshot, keyed by the REPLY's guild
    // context: Discord omits `guild_id` on the nested `referenced_message`, so the
    // parent's own is None -- a reply is same-channel, so `msg.guild_id` matches
    // the context the parent was admitted under. Only a Represented (proxied)
    // snapshot yields a human principal; an AppOnly snapshot's effective id is the
    // app itself (safe to check); an Unavailable snapshot (PK resolution failed)
    // collapses to the transport id, which must never drive the ignore decision,
    // so it -- and any absent/Direct snapshot -- resolves Unresolvable (fail-open
    // redacts, never leaks).
    if let Some(parent) = msg.referenced_message.as_deref() {
        if parent.webhook_id.is_some() {
            let context = msg
                .guild_id
                .map_or(LifecycleContext::DirectMessage, LifecycleContext::Guild);
            return match ingress_ledger.active_snapshot(parent.id, parent.channel_id, context) {
                Some(snapshot)
                    if matches!(
                        snapshot.provenance(),
                        Some(
                            LifecycleProvenance::Represented { .. } | LifecycleProvenance::AppOnly
                        )
                    ) =>
                {
                    ignored(snapshot.effective_user_id().get())
                }
                _ => ReplyParentResolution::Unresolvable,
            };
        }
        return ignored(parent.author.id.get());
    }

    // Parent not inlined: we need the reference's coordinates for tiers 2/3.
    let Some(reference) = msg.message_reference.as_ref() else {
        return ReplyParentResolution::Unresolvable;
    };
    let Some(parent_id) = reference.message_id else {
        return ReplyParentResolution::Unresolvable;
    };
    // Use the reference's OWN channel, not `msg.channel_id` — a cross-channel
    // reply otherwise 404s against the wrong channel (fable P2).
    let parent_channel = reference.channel_id;
    let context = msg
        .guild_id
        .map_or(LifecycleContext::DirectMessage, LifecycleContext::Guild);

    // tier2: ingress-ledger active snapshot — an immutable authorship fact,
    // retained ≤7d, surviving a Discord-side deletion of the parent.
    if let Some(snapshot) = ingress_ledger.active_snapshot(parent_id, parent_channel, context) {
        return ignored(snapshot.effective_user_id().get());
    }

    // tier3: one bounded, best-effort live fetch (no retries), with a timeout.
    match tokio::time::timeout(
        IGNORE_PARENT_FETCH_TIMEOUT,
        http.get_message(parent_channel, parent_id),
    )
    .await
    {
        // A live-fetched webhook/PK parent carries the transport author id, and
        // tier2's ledger lookup already missed, so no principal is resolvable here.
        Ok(Ok(parent)) if parent.webhook_id.is_some() => ReplyParentResolution::Unresolvable,
        Ok(Ok(parent)) => ignored(parent.author.id.get()),
        // Fetch error, or the timeout elapsed → parent author unknown.
        Ok(Err(_)) | Err(_) => ReplyParentResolution::Unresolvable,
    }
}

/// Effective Discord principal behind a verified (webhook/PK) action, mirroring
/// [`crate::ingress_ledger::LifecycleSnapshot::effective_user_id`]: a
/// represented (proxied) action collapses to the represented user; app-only or
/// unavailable actions retain the observed transport author.
fn verified_principal_id(provenance: &LifecycleProvenance, observed_author: UserId) -> u64 {
    match provenance {
        LifecycleProvenance::Represented {
            discord_user_id, ..
        } => discord_user_id.get(),
        LifecycleProvenance::AppOnly | LifecycleProvenance::Unavailable(_) => observed_author.get(),
    }
}

/// One admission decision for a direct (non-webhook) guild message. The
/// production handler builds a delivery **only** from the
/// [`DirectGuildAdmission::Deliver`] variant — its `targeting` exists
/// nowhere else — so this function's return necessarily controls delivery,
/// and tests that drive it exercise the same authority the handler obeys.
#[derive(Debug, PartialEq, Eq)]
enum DirectGuildAdmission {
    /// Deliver with the targeting derived from mention classification.
    Deliver { targeting: MessageTargeting },
    /// Suppressed by the preflight authority (recorded as that authority
    /// dictates).
    Preflight(GuildPreflight),
    /// Bot author outside the identity allow list (recorded for reply
    /// inheritance — the scope is configured once preflight proceeds).
    BotAuthor,
    /// Dropped by the inbound gate (recorded for reply inheritance).
    GateDrop,
    /// Dropped by the identity-level ignore list (#369). Deliberately **not**
    /// recorded in the drop-event ledger: identity ignore is stateless and
    /// re-evaluated from current config on every message, so caching it as a
    /// reply-inheritance root would let a stale entry survive an un-ignore.
    IdentityIgnored,
    /// `check_guild` returned Queue, which guild messages never should.
    UnexpectedQueue,
}

/// The full admission authority for direct guild messages: preflight
/// (inheritance + policy + mute + mention), bot-author filtering, and the
/// inbound gate, with every ledger write that reply inheritance depends on.
/// The handler's cheap early-return pass calls [`guild_message_preflight`]
/// first for cost; recording is idempotent, so running both is safe.
#[expect(
    clippy::too_many_arguments,
    reason = "each argument is one admission fact; bundling them would hide which check consumes which fact"
)]
fn admit_direct_guild_message(
    ledger: &crate::drop_ledger::DropLedger,
    config: &crate::config::LoadedConfig,
    gate_scope: ChannelId,
    message_id: MessageId,
    reply_parent_id: Option<MessageId>,
    guild_id: Option<u64>,
    guild_muted: bool,
    author_is_bot: bool,
    author_id: u64,
    mention_kind: Option<MentionKind>,
) -> DirectGuildAdmission {
    let policy = config.channel_policy(gate_scope.get());
    let preflight = guild_message_preflight(
        ledger,
        gate_scope,
        message_id,
        reply_parent_id,
        policy.is_some(),
        policy.is_some_and(|p| p.require_mention),
        guild_muted,
        mention_kind.is_some(),
    );
    if preflight != GuildPreflight::Proceed {
        return DirectGuildAdmission::Preflight(preflight);
    }
    if should_drop_bot_message(author_is_bot, author_id, config) {
        // Preflight returned Proceed above, so this scope is configured:
        // recording here cannot mint an untrusted scope. Identity-list
        // drops are roots like any other — replies inherit them.
        ledger.record(gate_scope, message_id);
        return DirectGuildAdmission::BotAuthor;
    }
    match InboundGate::check_guild(
        config,
        gate_scope.get(),
        author_id,
        mention_kind.is_some(),
        guild_id,
    ) {
        GateDecision::Deliver => DirectGuildAdmission::Deliver {
            targeting: mention_kind
                .map_or(MessageTargeting::Ambient, MessageTargeting::GuildDirected),
        },
        GateDecision::Queue => DirectGuildAdmission::UnexpectedQueue,
        GateDecision::Drop => {
            // #369: an identity-ignore drop is stateless — never cache it as a
            // #361 reply-inheritance root, or un-ignoring would not take effect
            // until the ledger entry aged out. Only non-identity gate drops
            // (e.g. sender outside `allow_from`) are recorded as roots. #369 v2:
            // only the AUTHOR triggers this; an ignored reply-parent redacts the
            // preview at the delivery layer and never reaches a drop here.
            if crate::gate::author_ignored(config, author_id) {
                return DirectGuildAdmission::IdentityIgnored;
            }
            // #361: remember the drop so a direct reply inherits it.
            ledger.record(gate_scope, message_id);
            DirectGuildAdmission::GateDrop
        }
    }
}

/// Result of resolving a channel's thread parentage and computing the
/// effective channel ID for gate decisions.
struct ResolvedChannel {
    /// If the message was in a thread, the parent channel ID.
    thread_parent_id: Option<u64>,
    /// The channel ID to use for gate decisions: parent if this is a thread,
    /// otherwise the original channel ID.
    gate_channel_id: u64,
}

struct VerifiedCreatePlan {
    admission: crate::ingress_ledger::LifecycleSnapshot,
    config: Arc<crate::config::LoadedConfig>,
    thread_parent_id: Option<u64>,
    targeting: MessageTargeting,
}

struct VerifiedEditPlan {
    admission: crate::ingress_ledger::LifecycleSnapshot,
    config: Arc<crate::config::LoadedConfig>,
}

async fn admit_verified_create_after_wait<RF, TF, LF, MF>(
    msg: &Message,
    bot_user_id: u64,
    ledger: &crate::ingress_ledger::IngressLedger,
    resolved_action: RF,
    topology: TF,
    load_fresh_config: LF,
    observe_current_mute: MF,
) -> Option<VerifiedCreatePlan>
where
    RF: std::future::Future<
            Output = Option<
                crate::discord::verified_action::VerifiedAppAction<
                    crate::discord::verified_action::Resolved,
                >,
            >,
        >,
    TF: std::future::Future<Output = Option<u64>>,
    LF: FnOnce() -> Arc<crate::config::LoadedConfig>,
    MF: FnOnce(u64, u64) -> bool,
{
    msg.guild_id?;
    let action = resolved_action.await?;
    let thread_parent_id = topology.await;
    let config = load_fresh_config();
    let channel_id = msg.channel_id.get();
    let gate_channel_id = select_gate_channel(&config, channel_id, thread_parent_id);
    let mentions = msg
        .mentions
        .iter()
        .map(|user| user.id.get())
        .collect::<Vec<_>>();
    let referenced_author_id = msg.referenced_message.as_deref().map(|m| m.author.id.get());
    let mention_kind = MentionDetector::classify(
        bot_user_id,
        &mentions,
        &msg.content,
        referenced_author_id,
        config.mention_patterns.as_ref(),
    );
    let muted = msg
        .guild_id
        .is_some_and(|guild_id| observe_current_mute(guild_id.get(), gate_channel_id));
    if !fresh_guild_envelope_allows_with_mute(&config, gate_channel_id, mention_kind, muted) {
        return None;
    }
    let policy = fresh_policy_snapshot(&config, gate_channel_id)?;
    let facts = match VerifiedActionGate::evaluate(action, policy) {
        VerifiedGateVerdict::Allow(facts) => facts,
        VerifiedGateVerdict::Deny => return None,
    };
    // #369 (P1 fix): the identity-ignore blocklist applies to the RESOLVED
    // PRINCIPAL behind the proxy (the represented Discord user), never the
    // webhook transport id. Checked before the ledger record so no admitted
    // entry is minted for content we are dropping.
    let lifecycle = facts.into_lifecycle();
    let principal_id = verified_principal_id(lifecycle.provenance(), msg.author.id);
    if config.is_ignored(principal_id) {
        tracing::trace!(
            principal_id,
            "verified guild create dropped: principal on identity ignore list"
        );
        return None;
    }
    let webhook_id = msg.webhook_id?;
    let context = msg
        .guild_id
        .map_or(LifecycleContext::DirectMessage, LifecycleContext::Guild);
    let admission = match ledger.admit_verified_create(
        lifecycle,
        msg.id,
        msg.channel_id,
        context,
        webhook_id,
        msg.author.id,
        thread_parent_id.map(ChannelId::new),
        &msg.content,
        msg.timestamp,
    ) {
        crate::ingress_ledger::TransitionResult::Admitted(snapshot) => snapshot,
        crate::ingress_ledger::TransitionResult::Duplicate
        | crate::ingress_ledger::TransitionResult::Rejected
        | crate::ingress_ledger::TransitionResult::Unavailable => return None,
    };
    Some(VerifiedCreatePlan {
        admission,
        config,
        thread_parent_id,
        targeting: mention_kind.map_or(MessageTargeting::Ambient, MessageTargeting::GuildDirected),
    })
}

#[expect(
    clippy::too_many_arguments,
    reason = "verified edit orchestration keeps immutable gateway evidence explicit"
)]
async fn admit_verified_edit_after_wait<RF, TF, LF, MF>(
    event: &MessageUpdateEvent,
    new: Option<&Message>,
    old: Option<&Message>,
    author: &User,
    new_content: &str,
    edited_ts: serenity::model::Timestamp,
    webhook_id: WebhookId,
    bot_user_id: u64,
    ledger: &crate::ingress_ledger::IngressLedger,
    resolved_action: RF,
    topology: TF,
    load_fresh_config: LF,
    observe_current_mute: MF,
) -> Option<VerifiedEditPlan>
where
    RF: std::future::Future<
            Output = Option<
                crate::discord::verified_action::VerifiedAppAction<
                    crate::discord::verified_action::Resolved,
                >,
            >,
        >,
    TF: std::future::Future<Output = Option<u64>>,
    LF: FnOnce() -> Arc<crate::config::LoadedConfig>,
    MF: FnOnce(u64, u64) -> bool,
{
    event.guild_id?;
    let action = resolved_action.await?;
    let thread_parent_id = topology.await;
    let config = load_fresh_config();
    let gate_channel_id = select_gate_channel(&config, event.channel_id.get(), thread_parent_id);
    let mentions = event
        .mentions
        .as_ref()
        .map(|mentions| {
            mentions
                .iter()
                .map(|user| user.id.get())
                .collect::<Vec<_>>()
        })
        .or_else(|| new.map(|message| message.mentions.iter().map(|user| user.id.get()).collect()))
        .unwrap_or_default();
    let referenced_author_id = new
        .and_then(|message| message.referenced_message.as_deref())
        .or_else(|| old?.referenced_message.as_deref())
        .map(|message| message.author.id.get());
    let mention_kind = MentionDetector::classify(
        bot_user_id,
        &mentions,
        new_content,
        referenced_author_id,
        config.mention_patterns.as_ref(),
    );
    let muted = event
        .guild_id
        .is_some_and(|guild_id| observe_current_mute(guild_id.get(), gate_channel_id));
    if !fresh_guild_envelope_allows_with_mute(&config, gate_channel_id, mention_kind, muted) {
        return None;
    }
    let policy = fresh_policy_snapshot(&config, gate_channel_id)?;
    let facts = match VerifiedActionGate::evaluate(action, policy) {
        VerifiedGateVerdict::Allow(facts) => facts,
        VerifiedGateVerdict::Deny => return None,
    };
    // #369 (P1 fix): the identity-ignore blocklist applies to the RESOLVED
    // PRINCIPAL behind the proxy, never the webhook transport id. An ignored
    // principal's edit of an already-admitted message must not slip through.
    let lifecycle = facts.into_lifecycle();
    let principal_id = verified_principal_id(lifecycle.provenance(), author.id);
    if config.is_ignored(principal_id) {
        tracing::trace!(
            principal_id,
            "verified guild edit dropped: principal on identity ignore list"
        );
        return None;
    }
    let context = event
        .guild_id
        .map_or(LifecycleContext::DirectMessage, LifecycleContext::Guild);
    let admission = match ledger.transition_verified_edit(
        lifecycle,
        event.id,
        event.channel_id,
        context,
        webhook_id,
        author.id,
        new_content,
        edited_ts,
    ) {
        crate::ingress_ledger::TransitionResult::Admitted(snapshot) => snapshot,
        crate::ingress_ledger::TransitionResult::Duplicate
        | crate::ingress_ledger::TransitionResult::Rejected
        | crate::ingress_ledger::TransitionResult::Unavailable => return None,
    };
    Some(VerifiedEditPlan { admission, config })
}

fn select_gate_channel(
    config: &crate::config::LoadedConfig,
    channel_id: u64,
    thread_parent_id: Option<u64>,
) -> u64 {
    if config.channel_policy(channel_id).is_some() {
        channel_id
    } else {
        thread_parent_id.unwrap_or(channel_id)
    }
}

#[cfg(test)]
fn fresh_guild_envelope_allows(
    config: &crate::config::LoadedConfig,
    gate_channel_id: u64,
    guild_id: Option<u64>,
    mention_kind: Option<MentionKind>,
) -> bool {
    let muted = guild_id.is_some_and(|guild_id| {
        crate::mute_store::global().is_some_and(|store| store.is_guild_muted(guild_id))
    });
    fresh_guild_envelope_allows_with_mute(config, gate_channel_id, mention_kind, muted)
}

fn fresh_guild_envelope_allows_with_mute(
    config: &crate::config::LoadedConfig,
    gate_channel_id: u64,
    mention_kind: Option<MentionKind>,
    muted: bool,
) -> bool {
    let Some(policy) = config.channel_policy(gate_channel_id) else {
        return false;
    };
    if muted {
        return false;
    }
    !policy.require_mention || mention_kind.is_some()
}

fn passive_edit_policy_allows(
    config: &crate::config::LoadedConfig,
    gate_channel_id: u64,
    guild_id: Option<u64>,
    mention_kind: Option<MentionKind>,
    lineage: &crate::ingress_ledger::LifecycleView<'_>,
) -> bool {
    // #369 (P1 fix): identity-ignore is a stateless blocklist that also governs
    // EDITS. An ignored author editing a previously-admitted message was a
    // bypass — resolve the effective author (represented principal for proxied
    // lineage, else the observed actor) and drop before any policy check.
    let effective_author = match lineage.provenance() {
        Some(LifecycleProvenance::Represented {
            discord_user_id, ..
        }) => discord_user_id.get(),
        _ => lineage.actor_id().get(),
    };
    if config.is_ignored(effective_author) {
        return false;
    }
    if guild_id.is_some_and(|guild_id| {
        crate::mute_store::global().is_some_and(|store| store.is_guild_muted(guild_id))
    }) {
        return false;
    }
    let Some(policy) = config.channel_policy(gate_channel_id) else {
        return false;
    };
    if policy.require_mention && mention_kind.is_none() {
        return false;
    }
    if !policy.has_identity_filter() {
        return true;
    }
    match lineage.provenance() {
        None => policy.allow_from.contains(&lineage.actor_id().get()),
        Some(LifecycleProvenance::Represented {
            discord_user_id,
            system_id,
            member_id,
        }) => {
            policy.allow_from.contains(&discord_user_id.get())
                || system_id.is_some_and(|id| policy.allow_pk_systems.contains(&id.to_string()))
                || member_id.is_some_and(|id| policy.allow_pk_members.contains(&id.to_string()))
        }
        Some(LifecycleProvenance::AppOnly) | Some(LifecycleProvenance::Unavailable(_)) => false,
    }
}

/// Resolves thread parentage for a channel and returns the effective gate
/// channel ID.
///
/// If the channel is directly configured, it is used as-is (no thread
/// resolution needed). Otherwise, we check whether the channel is a thread
/// and, if so, use its parent for gate decisions.
///
/// For DM channels (`is_dm = true`), thread resolution is skipped entirely.
async fn resolve_guild_channel(
    http: &serenity::http::Http,
    state: &crate::state::State,
    config: &crate::config::LoadedConfig,
    channel_id: u64,
    is_dm: bool,
) -> ResolvedChannel {
    let thread_parent_id = if !is_dm && config.channel_policy(channel_id).is_none() {
        resolve_thread_parent(http, state, channel_id).await
    } else {
        None
    };
    let gate_channel_id = thread_parent_id.unwrap_or(channel_id);
    ResolvedChannel {
        thread_parent_id,
        gate_channel_id,
    }
}

/// Converts a serenity [`Timestamp`] to an RFC 3339 string.
///
/// If `to_rfc3339()` returns `None` — which indicates the timestamp is broken
/// at the Discord API level — logs a warning and falls back to the current UTC
/// time so callers never receive an empty string.
fn serenity_ts_to_rfc3339(field: &str, ts: &serenity::model::Timestamp) -> String {
    match ts.to_rfc3339() {
        Some(s) => s,
        None => {
            let fallback = chrono::Utc::now().to_rfc3339();
            tracing::warn!(
                field,
                fallback = %fallback,
                "Discord timestamp failed to_rfc3339(); using current UTC time as fallback"
            );
            fallback
        }
    }
}

/// True when the reference denotes a genuine reply (not a forward/crosspost).
fn is_reply_reference(reference: &MessageReference) -> bool {
    matches!(reference.kind, MessageReferenceKind::Default)
}

/// Extracts the replied-to message ID from a Discord message reference.
///
/// A reference can exist without a message ID (for example a channel-only
/// forward or crosspost), so the inner `message_id` is itself optional.
fn reply_to_id(reference: &MessageReference) -> Option<MessageId> {
    is_reply_reference(reference)
        .then_some(reference.message_id)
        .flatten()
}

/// Max characters retained from a replied-to message preview.
const REPLY_PREVIEW_MAX_CHARS: usize = 100;

/// Best-effort extraction of reply author + content preview from the parent
/// message embedded by Discord. Returns `(user_id, user, content_preview)`,
/// each independently optional. Only populated for genuine replies
/// (`MessageReferenceKind::Default`); forwards/crossposts yield all `None`.
fn reply_context(
    msg: &Message,
    ingress_ledger: &crate::ingress_ledger::IngressLedger,
) -> (Option<UserId>, Option<String>, Option<String>) {
    let is_reply = msg
        .message_reference
        .as_ref()
        .is_some_and(is_reply_reference);
    if !is_reply {
        return (None, None, None);
    }
    let Some(parent) = msg.referenced_message.as_deref() else {
        return (None, None, None);
    };
    let preview = reply_preview(&parent.content);
    let reply_to_user_id = if parent.webhook_id.is_some() {
        // Same resolution as tier1 of `resolve_reply_parent_resolution`: key on the
        // REPLY's guild (the nested parent carries no `guild_id`), and trust only a
        // Represented/AppOnly snapshot -- an Unavailable one collapses to the
        // transport id.
        let context = msg
            .guild_id
            .map_or(LifecycleContext::DirectMessage, LifecycleContext::Guild);
        ingress_ledger
            .active_snapshot(parent.id, parent.channel_id, context)
            .filter(|snapshot| {
                matches!(
                    snapshot.provenance(),
                    Some(LifecycleProvenance::Represented { .. } | LifecycleProvenance::AppOnly)
                )
            })
            .map(|snapshot| snapshot.effective_user_id())
    } else {
        Some(parent.author.id)
    };
    (reply_to_user_id, Some(display_name(parent)), preview)
}

/// Resolves the best available display name for a message author.
///
/// Priority: server nickname > global display name > username.
/// For webhook messages (e.g. PluralKit), `author.name` is already the
/// alter's display name, so the fallback is correct.
fn display_name(msg: &Message) -> String {
    let raw = msg
        .member
        .as_ref()
        .and_then(|m| m.nick.as_ref())
        .or(msg.author.global_name.as_ref())
        .unwrap_or(&msg.author.name);
    strip_invisible(raw)
}

/// Display name from a bare `User` (no guild member context).
/// Falls back global_name > username.
fn display_name_from_user(user: &serenity::model::user::User) -> String {
    let raw = user.global_name.as_ref().unwrap_or(&user.name);
    strip_invisible(raw)
}

/// Literal identity used for an outbound notification's `user` field when
/// neither a resolved display name nor a Discord username is available.
const FALLBACK_USER_IDENTITY: &str = "dione";

/// Returns `true` when the candidate is absent or whitespace-only.
fn is_blank(candidate: Option<&str>) -> bool {
    candidate.map(str::trim).unwrap_or("").is_empty()
}

/// Resolves the `user` field for an outbound notification.
///
/// Fallback chain (see #153): resolved display name → Discord username →
/// the literal `"dione"`. Blank or whitespace-only candidates are treated as
/// absent so an empty display name never leaks into a notification (which
/// would otherwise surface as the substrate name "dione" downstream).
fn resolve_user_identity(display_name: Option<&str>, username: Option<&str>) -> String {
    [display_name, username]
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|s| !s.is_empty())
        .unwrap_or(FALLBACK_USER_IDENTITY)
        .to_string()
}

/// Strip invisible/zero-width characters that proxy bots (e.g. PluralKit)
/// pad onto short names to prevent Discord formatting issues.
fn strip_invisible(s: &str) -> String {
    s.chars()
        .filter(|c| {
            !matches!(
                *c,
                '\u{200B}'  // Zero Width Space
                | '\u{200C}' // Zero Width Non-Joiner
                | '\u{200D}' // Zero Width Joiner
                | '\u{FEFF}' // Zero Width No-Break Space
                | '\u{17B5}' // Khmer Vowel Inherent AA (PluralKit anti-ping)
                | '\u{034F}' // Combining Grapheme Joiner
            )
        })
        .collect()
}

/// UTF-8-safe truncation to `REPLY_PREVIEW_MAX_CHARS` chars, with an ellipsis
/// when truncated. Returns `None` for empty content (e.g. attachment-only parent).
fn reply_preview(content: &str) -> Option<String> {
    if content.is_empty() {
        return None;
    }
    let mut chars = content.chars();
    let preview: String = chars.by_ref().take(REPLY_PREVIEW_MAX_CHARS).collect();
    if chars.next().is_some() {
        Some(format!("{preview}\u{2026}"))
    } else {
        Some(preview)
    }
}

fn build_message_event(
    msg: &Message,
    config: &crate::config::LoadedConfig,
    ingress_ledger: &crate::ingress_ledger::IngressLedger,
    thread_parent_id: Option<u64>,
    targeting: MessageTargeting,
    pronoun_display_name: Option<PronounDisplayName>,
    redact_reply_preview: bool,
) -> NotificationEvent {
    build_message_event_with_coordinates(
        msg.channel_id,
        msg.id,
        msg.author.id,
        msg,
        config,
        ingress_ledger,
        thread_parent_id,
        targeting,
        pronoun_display_name,
        redact_reply_preview,
    )
}

#[expect(
    clippy::too_many_arguments,
    reason = "the verified event builder keeps immutable Discord coordinates plus the #369 redaction flag explicit at the delivery boundary"
)]
fn build_verified_message_event(
    admission: &crate::ingress_ledger::LifecycleSnapshot,
    msg: &Message,
    config: &crate::config::LoadedConfig,
    ingress_ledger: &crate::ingress_ledger::IngressLedger,
    thread_parent_id: Option<u64>,
    targeting: MessageTargeting,
    pronoun_display_name: Option<PronounDisplayName>,
    redact_reply_preview: bool,
) -> NotificationEvent {
    build_message_event_with_coordinates(
        admission.channel_id(),
        admission.message_id(),
        admission.effective_user_id(),
        msg,
        config,
        ingress_ledger,
        thread_parent_id,
        targeting,
        pronoun_display_name,
        redact_reply_preview,
    )
}

#[expect(
    clippy::too_many_arguments,
    reason = "the shared direct/verified event builder keeps immutable Discord coordinates explicit at the transport-neutral serialization boundary"
)]
fn build_message_event_with_coordinates(
    chat_id: ChannelId,
    message_id: MessageId,
    effective_user_id: UserId,
    msg: &Message,
    config: &crate::config::LoadedConfig,
    ingress_ledger: &crate::ingress_ledger::IngressLedger,
    thread_parent_id: Option<u64>,
    targeting: MessageTargeting,
    pronoun_display_name: Option<PronounDisplayName>,
    redact_reply_preview: bool,
) -> NotificationEvent {
    let attachments = msg
        .attachments
        .iter()
        .map(|a| AttachmentMeta {
            name: a.filename.clone(),
            content_type: a.content_type.clone(),
            size: u64::from(a.size),
        })
        .collect();

    let is_voice_message = msg
        .flags
        .map(|f| f.contains(MessageFlags::IS_VOICE_MESSAGE))
        .unwrap_or(false);

    let reply_to_message_id = msg.message_reference.as_ref().and_then(reply_to_id);
    let (reply_to_user_id, reply_to_user, mut reply_to_content_preview) =
        reply_context(msg, ingress_ledger);
    // #369 v2: when the resolved parent author is ignored (or unresolvable
    // under fail-open), strip ONLY the quoted content preview — the sole
    // content-leak vector a reply carries from the ignored person. The parent
    // user id/name are kept for threading (they are not the leak). Mirrors the
    // preview-None convention in coalesce.rs.
    if redact_reply_preview {
        reply_to_content_preview = None;
    }

    let resolved_name = pronoun_display_name
        .map(|p| p.0)
        .unwrap_or_else(|| resolve_user_identity(Some(&display_name(msg)), Some(&msg.author.name)));

    NotificationEvent::Message(MessageEvent {
        chat_id,
        message_id,
        user: resolved_name,
        user_id: effective_user_id,
        content: msg.content.clone(),
        targeting,
        timestamp: config
            .localize_rfc3339(&serenity_ts_to_rfc3339("msg.timestamp", &msg.timestamp)),
        attachments,
        is_voice_message,
        thread_parent_id: thread_parent_id.map(ChannelId::new),
        reply_to_message_id,
        reply_to_user_id,
        reply_to_user,
        reply_to_content_preview,
        bells: None,
        bells_status: None,
    })
}

/// Resolves the parent channel ID for a thread channel.
///
/// First checks the in-memory cache (including negative entries for channels
/// confirmed not to be threads), then falls back to an API call. Caches
/// the result either way. Returns `None` if the channel is not a thread.
async fn resolve_thread_parent(
    http: &serenity::http::Http,
    state: &crate::state::State,
    channel_id: u64,
) -> Option<u64> {
    // Check cache first — includes negative entries (Some(None)).
    {
        let state = state.read().await;
        if let Some(cached) = state.thread_parents.get(&channel_id) {
            return *cached;
        }
    }

    // Not cached — ask Discord.
    let channel = match http.get_channel(ChannelId::new(channel_id)).await {
        Ok(ch) => ch,
        Err(e) => {
            tracing::debug!(channel_id, error = %e, "failed to look up channel for thread detection");
            return None;
        }
    };

    let parent_id = match channel {
        serenity::model::channel::Channel::Guild(gc)
            if matches!(
                gc.kind,
                ChannelType::PublicThread | ChannelType::PrivateThread | ChannelType::NewsThread
            ) =>
        {
            gc.parent_id.map(|p| p.get())
        }
        _ => None,
    };

    // Cache the result — including None for non-threads (negative cache).
    {
        let mut state = state.write().await;
        state.record_thread_parent(channel_id, parent_id);
    }

    parent_id
}

/// Sends a DM to an admin about a pending access request.
async fn notify_admin_dm(
    http: &serenity::http::Http,
    admin_id: u64,
    requester_name: &str,
    requester_id: u64,
    message_preview: &str,
) {
    // Create DM channel with admin.
    let Some(admin_uid) = crate::mcp::ids::Snowflake::new(admin_id) else {
        tracing::warn!(admin_id, "skipping admin DM notification: admin_id is zero");
        return;
    };
    let channel = match create_dm_channel(http, admin_uid.user()).await {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(
                admin_id,
                error = %e,
                "failed to open DM with admin for access notification"
            );
            return;
        }
    };

    let preview: String = message_preview.chars().take(100).collect();
    let content = format!(
        "**Access request** from `{requester_name}` (<@{requester_id}>)\n\
         Preview: {preview}\n\
         Use the `list_access_requests` MCP tool to review and approve/deny."
    );

    let msg = CreateMessage::new().content(content);
    if let Err(e) = channel.id.send_message(http, msg).await {
        tracing::warn!(admin_id, error = %e, "failed to send access notification DM");
    }
}

/// Returns the attributed reactor for a gateway reaction, or `None` when the
/// reaction should be dropped: no user attribution, or the bot reacting to
/// itself (which would otherwise feed back into the notification stream).
///
/// Intentional bot self-reactions (e.g. contradictionary celebrate) never
/// arrive through this path — they are emitted by the tool layer with
/// `self_react: true` at the point where they are initiated. See
/// `crate::mcp::tools::messaging`.
fn gateway_reactor(user_id: Option<UserId>, bot_id: u64) -> Option<UserId> {
    user_id.filter(|id| id.get() != bot_id)
}

/// Returns `true` if the message should be dropped because the author is a bot
/// whose user ID is **not** in the `allow_from` list.
///
/// Human messages (is_bot = false) always pass through. Bot messages pass
/// through only if the bot's user ID is in the config's allow_from list.
fn should_drop_bot_message(
    is_bot: bool,
    user_id: u64,
    config: &crate::config::LoadedConfig,
) -> bool {
    is_bot && !config.is_allowed(user_id)
}

fn is_direct_dm_transport(is_dm: bool, is_webhook: bool) -> bool {
    is_dm && !is_webhook
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AccessConfig, Config, DmPolicy, LoadedConfig};

    fn mid(raw: u64) -> MessageId {
        MessageId::new(raw)
    }

    const SCOPE: u64 = 20;

    fn scope() -> ChannelId {
        ChannelId::new(SCOPE)
    }

    /// The configured drop roots — guild muted, mention required — are
    /// recorded, and a direct reply to any of them inherits the drop even
    /// when the reply itself would pass every policy check. Chains stay
    /// dropped hop by hop.
    #[test]
    fn preflight_records_configured_drop_roots_and_replies_inherit() {
        let ledger = crate::drop_ledger::DropLedger::new();
        assert_eq!(
            guild_message_preflight(&ledger, scope(), mid(2), None, true, false, true, false),
            GuildPreflight::GuildMuted
        );
        assert_eq!(
            guild_message_preflight(&ledger, scope(), mid(3), None, true, true, false, false),
            GuildPreflight::MentionRequired
        );
        // Replies to each recorded root inherit, even with a mention and a
        // fully clean policy state.
        for parent in [2u64, 3] {
            assert_eq!(
                guild_message_preflight(
                    &ledger,
                    scope(),
                    mid(100 + parent),
                    Some(mid(parent)),
                    true,
                    false,
                    false,
                    true
                ),
                GuildPreflight::InheritedDrop,
                "reply to dropped root {parent} must inherit the drop"
            );
        }
        // A reply to an inherited-drop hop stays dropped: the chain holds.
        assert_eq!(
            guild_message_preflight(
                &ledger,
                scope(),
                mid(201),
                Some(mid(102)),
                true,
                false,
                false,
                true
            ),
            GuildPreflight::InheritedDrop
        );
    }

    /// Unconfigured channels suppress without recording: untrusted traffic
    /// cannot mint ledger scopes (round-2 hardening — the scope set is
    /// derived from trusted configuration only). The reply to an
    /// unconfigured-channel drop is suppressed by the same `NotOptedIn`
    /// check, not by inheritance.
    #[test]
    fn unconfigured_channels_suppress_without_minting_scopes() {
        let ledger = crate::drop_ledger::DropLedger::new();
        assert_eq!(
            guild_message_preflight(&ledger, scope(), mid(1), None, false, false, false, false),
            GuildPreflight::NotOptedIn
        );
        assert!(!ledger.contains(scope(), mid(1)), "nothing recorded");
        // Same-channel reply while unconfigured: suppressed by NotOptedIn.
        assert_eq!(
            guild_message_preflight(
                &ledger,
                scope(),
                mid(2),
                Some(mid(1)),
                false,
                false,
                false,
                true
            ),
            GuildPreflight::NotOptedIn
        );
        // If the channel is configured mid-chain, the reply falls back to
        // the ordinary gate (same fail-open contract as a restart).
        assert_eq!(
            guild_message_preflight(
                &ledger,
                scope(),
                mid(3),
                Some(mid(1)),
                true,
                false,
                false,
                false
            ),
            GuildPreflight::Proceed
        );
    }

    /// Negative space: clean messages proceed, replies to undropped parents
    /// proceed, and proceeding records nothing — a reply to a delivered
    /// message is judged on its own.
    #[test]
    fn preflight_negatives_proceed_and_record_nothing() {
        let ledger = crate::drop_ledger::DropLedger::new();
        assert_eq!(
            guild_message_preflight(&ledger, scope(), mid(1), None, true, false, false, false),
            GuildPreflight::Proceed
        );
        assert_eq!(
            guild_message_preflight(
                &ledger,
                scope(),
                mid(2),
                Some(mid(999)),
                true,
                false,
                false,
                false
            ),
            GuildPreflight::Proceed
        );
        assert_eq!(
            guild_message_preflight(&ledger, scope(), mid(3), None, true, true, false, true),
            GuildPreflight::Proceed
        );
        // None of the proceeding messages were recorded: replying to them
        // does not inherit anything.
        assert_eq!(
            guild_message_preflight(
                &ledger,
                scope(),
                mid(4),
                Some(mid(1)),
                true,
                false,
                false,
                false
            ),
            GuildPreflight::Proceed
        );
    }

    // ── Direct-guild admission authority (round-2 seam) ─────────────────────

    fn admission_config(
        channel_id: u64,
        require_mention: bool,
        allow_from: Vec<&str>,
    ) -> LoadedConfig {
        let mut raw = Config {
            access: AccessConfig {
                dm_policy: DmPolicy::Queue,
                allow_from: vec![],
                ignore_from: vec![],
                admins: vec![],
                admin_only_mutations: false,
            },
            ..Default::default()
        };
        raw.channels.push(crate::config::ChannelConfig {
            id: channel_id.to_string(),
            require_mention,
            allow_from: allow_from.into_iter().map(String::from).collect(),
            ..Default::default()
        });
        LoadedConfig::from_raw(raw)
    }

    /// An open channel (delivers non-ignored senders ambiently) with an
    /// identity-level ignore list. #369.
    fn admission_config_ignored(channel_id: u64, ignored: Vec<&str>) -> LoadedConfig {
        let mut raw = Config {
            access: AccessConfig {
                dm_policy: DmPolicy::Queue,
                allow_from: vec![],
                ignore_from: ignored.into_iter().map(String::from).collect(),
                admins: vec![],
                admin_only_mutations: false,
            },
            ..Default::default()
        };
        raw.channels.push(crate::config::ChannelConfig {
            id: channel_id.to_string(),
            require_mention: false,
            allow_from: vec![],
            ..Default::default()
        });
        LoadedConfig::from_raw(raw)
    }

    /// #369: identity-ignore drops resolve to `IdentityIgnored` and are never
    /// written to the drop-event ledger, so they cannot become stale
    /// reply-inheritance roots. Un-ignoring takes effect on the next message.
    #[test]
    fn identity_ignore_drops_without_polluting_the_ledger() {
        let ledger = crate::drop_ledger::DropLedger::new();
        let ignored = admission_config_ignored(SCOPE, vec!["900"]);

        // Ignored author → IdentityIgnored, and nothing recorded.
        assert_eq!(
            admit_direct_guild_message(
                &ledger,
                &ignored,
                scope(),
                mid(1),
                None,
                Some(60),
                false,
                false,
                900,
                None,
            ),
            DirectGuildAdmission::IdentityIgnored
        );
        assert!(
            !ledger.contains(scope(), mid(1)),
            "identity-ignore drops must not be recorded as reply-inheritance roots"
        );

        // #369 v2: a NON-ignored sender replying to the ignored author's
        // message is DELIVERED (the preview is redacted at the delivery layer,
        // not dropped here). The admission authority is author-only, so nothing
        // is recorded either.
        assert_eq!(
            admit_direct_guild_message(
                &ledger,
                &ignored,
                scope(),
                mid(2),
                Some(mid(1)),
                Some(60),
                false,
                false,
                123,
                None,
            ),
            DirectGuildAdmission::Deliver {
                targeting: MessageTargeting::Ambient
            }
        );
        assert!(!ledger.contains(scope(), mid(2)));

        // Statelessness proof: once 900 is un-ignored, the same author delivers
        // — no stale ledger entry blocks it.
        let unignored = admission_config_ignored(SCOPE, vec![]);
        assert_eq!(
            admit_direct_guild_message(
                &ledger,
                &unignored,
                scope(),
                mid(3),
                None,
                Some(60),
                false,
                false,
                900,
                None,
            ),
            DirectGuildAdmission::Deliver {
                targeting: MessageTargeting::Ambient
            }
        );
    }

    /// The admission authority end to end with real config: every recorded
    /// drop root produces inheriting replies, clean messages produce the
    /// only `Deliver { targeting }` the handler can build a delivery from,
    /// and gate drops (sender outside `allow_from`) record for inheritance.
    #[test]
    fn admission_authority_controls_delivery_and_inheritance() {
        let ledger = crate::drop_ledger::DropLedger::new();
        let open = admission_config(SCOPE, false, vec![]);
        let restricted = admission_config(SCOPE, false, vec!["42"]);
        let mention_gated = admission_config(SCOPE, true, vec![]);
        let admit = |config: &LoadedConfig,
                     ledger: &crate::drop_ledger::DropLedger,
                     message_id: u64,
                     reply_to: Option<u64>,
                     muted: bool,
                     author: u64,
                     mentioned: bool| {
            admit_direct_guild_message(
                ledger,
                config,
                scope(),
                mid(message_id),
                reply_to.map(mid),
                Some(60),
                muted,
                false,
                author,
                mentioned.then_some(MentionKind::DirectMention),
            )
        };

        // Clean ambient message delivers; a mention delivers directed.
        assert_eq!(
            admit(&open, &ledger, 1, None, false, 42, false),
            DirectGuildAdmission::Deliver {
                targeting: MessageTargeting::Ambient
            }
        );
        assert_eq!(
            admit(&open, &ledger, 2, None, false, 42, true),
            DirectGuildAdmission::Deliver {
                targeting: MessageTargeting::GuildDirected(MentionKind::DirectMention)
            }
        );

        // Gate drop: author outside allow_from is recorded; the allowed
        // author's reply to it inherits the drop.
        assert_eq!(
            admit(&restricted, &ledger, 3, None, false, 7, false),
            DirectGuildAdmission::GateDrop
        );
        assert_eq!(
            admit(&restricted, &ledger, 4, Some(3), false, 42, true),
            DirectGuildAdmission::Preflight(GuildPreflight::InheritedDrop)
        );

        // Mention-required root is recorded; the mentioning reply inherits.
        let ledger2 = crate::drop_ledger::DropLedger::new();
        assert_eq!(
            admit(&mention_gated, &ledger2, 5, None, false, 42, false),
            DirectGuildAdmission::Preflight(GuildPreflight::MentionRequired)
        );
        assert_eq!(
            admit(&mention_gated, &ledger2, 6, Some(5), false, 42, true),
            DirectGuildAdmission::Preflight(GuildPreflight::InheritedDrop)
        );

        // Muted-guild root is recorded; the post-unmute reply inherits.
        let ledger3 = crate::drop_ledger::DropLedger::new();
        assert_eq!(
            admit(&open, &ledger3, 7, None, true, 42, false),
            DirectGuildAdmission::Preflight(GuildPreflight::GuildMuted)
        );
        assert_eq!(
            admit(&open, &ledger3, 8, Some(7), false, 42, true),
            DirectGuildAdmission::Preflight(GuildPreflight::InheritedDrop)
        );

        // Unconfigured channel: suppressed, nothing recorded.
        let ledger4 = crate::drop_ledger::DropLedger::new();
        let unconfigured = admission_config(999, false, vec![]);
        assert_eq!(
            admit(&unconfigured, &ledger4, 9, None, false, 42, false),
            DirectGuildAdmission::Preflight(GuildPreflight::NotOptedIn)
        );
        assert!(!ledger4.contains(scope(), mid(9)));

        // Disallowed-bot root in a configured scope is recorded (round-3
        // blocker): an allowed human replying immediately inherits the
        // drop, and the chain holds hop by hop.
        assert_eq!(
            admit_direct_guild_message(
                &ledger4,
                &open,
                scope(),
                mid(10),
                None,
                Some(60),
                false,
                true,
                999,
                None,
            ),
            DirectGuildAdmission::BotAuthor
        );
        assert!(
            ledger4.contains(scope(), mid(10)),
            "configured-scope bot-author root must be recorded"
        );
        assert_eq!(
            admit(&open, &ledger4, 11, Some(10), false, 42, true),
            DirectGuildAdmission::Preflight(GuildPreflight::InheritedDrop),
            "allowed human reply to a dropped bot message inherits"
        );
        assert_eq!(
            admit(&open, &ledger4, 12, Some(11), false, 42, true),
            DirectGuildAdmission::Preflight(GuildPreflight::InheritedDrop),
            "the chain rooted in a bot drop stays dropped hopwise"
        );
    }

    // ── Presence assembly (round-2 seam) ────────────────────────────────────

    /// The production wiring mints one authority: both handles observe the
    /// same state, and a write through the MCP-side handle is what the
    /// gateway-side handle replays onto a freshly installed sink.
    #[tokio::test]
    async fn wired_presence_handles_share_one_authority() {
        let (handler_handle, server_handle) = wire_shared_presence();
        let server_handle = server_handle.expect("gateway transport wires the MCP handle");
        assert!(handler_handle.is_same_authority(&server_handle));
        assert!(
            !handler_handle.is_same_authority(&SharedPresence::new()),
            "a separately constructed store must not read as the same authority"
        );

        // Write through the MCP-side handle…
        server_handle
            .set_presence(None, OnlineStatus::DoNotDisturb)
            .await;
        // …and the gateway-side handle replays it onto a new sink.
        let sink = Arc::new(MockPresenceSink::new());
        handler_handle
            .install(Arc::clone(&sink) as Arc<dyn PresenceSink>)
            .await;
        assert_eq!(sink.call_count(), 1, "install replays the MCP-side write");
        assert_eq!(
            handler_handle.snapshot().await.desired.map(|d| d.status),
            Some(OnlineStatus::DoNotDisturb)
        );
    }

    fn config_with_allow_from(ids: Vec<&str>) -> LoadedConfig {
        let raw = Config {
            access: AccessConfig {
                dm_policy: DmPolicy::Queue,
                allow_from: ids.into_iter().map(String::from).collect(),
                ignore_from: vec![],
                admins: vec![],
                admin_only_mutations: false,
            },
            ..Default::default()
        };
        LoadedConfig::from_raw(raw)
    }

    // ── Bot message filter tests ─────────────────────────────────────────────

    /// Bot whose user ID is NOT in allow_from must be dropped.
    #[test]
    fn test_bot_not_in_allow_from_is_dropped() {
        let config = config_with_allow_from(vec!["100"]);
        assert!(
            should_drop_bot_message(true, 999, &config),
            "bot user 999 is not in allow_from and must be dropped"
        );
    }

    /// Bot whose user ID IS in allow_from must pass through.
    #[test]
    fn test_bot_in_allow_from_passes_through() {
        let config = config_with_allow_from(vec!["100", "200"]);
        assert!(
            !should_drop_bot_message(true, 200, &config),
            "bot user 200 is in allow_from and must not be dropped"
        );
    }

    /// Human message (is_bot = false) is never dropped, regardless of allow_from.
    #[test]
    fn test_human_message_not_dropped() {
        let config = config_with_allow_from(vec!["100"]);
        // Human in allow_from.
        assert!(
            !should_drop_bot_message(false, 100, &config),
            "human user in allow_from must not be dropped"
        );
        // Human NOT in allow_from.
        assert!(
            !should_drop_bot_message(false, 999, &config),
            "human user not in allow_from must not be dropped by bot filter"
        );
    }

    /// Bot with empty allow_from list is always dropped.
    #[test]
    fn test_bot_with_empty_allow_from_is_dropped() {
        let config = config_with_allow_from(vec![]);
        assert!(
            should_drop_bot_message(true, 42, &config),
            "bot must be dropped when allow_from is empty"
        );
    }

    #[test]
    fn webhook_dm_create_and_update_fail_closed_before_direct_gate() {
        assert!(is_direct_dm_transport(true, false));
        assert!(!is_direct_dm_transport(true, true));
        assert!(!is_direct_dm_transport(false, true));
    }

    #[test]
    fn fresh_snapshot_controls_thread_mapping_and_mention_classification() {
        let config = |channel_id: &str, require_mention: bool, patterns: Vec<&str>| {
            let mut raw = Config::default();
            raw.channels.push(crate::config::ChannelConfig {
                id: channel_id.to_owned(),
                require_mention,
                ..Default::default()
            });
            raw.mentions.patterns = patterns.into_iter().map(str::to_owned).collect();
            LoadedConfig::from_raw(raw)
        };
        let before_wait = config("20", false, vec!["old-name"]);
        let after_wait = config("30", true, vec!["new-name"]);
        assert_eq!(select_gate_channel(&before_wait, 20, Some(30)), 20);
        assert_eq!(select_gate_channel(&after_wait, 20, Some(30)), 30);

        let mention = MentionDetector::classify(
            999,
            &[],
            "hello new-name",
            None,
            after_wait.mention_patterns.as_ref(),
        );
        assert!(mention.is_some());
        assert!(fresh_guild_envelope_allows(
            &after_wait,
            select_gate_channel(&after_wait, 20, Some(30)),
            Some(40),
            mention,
        ));
    }

    const TEST_PK_CREATOR: u64 = 466_378_653_216_014_359;

    fn verified_message(id: u64, content: &str, guild_id: Option<u64>) -> Message {
        let mut payload = wire_message_body(id, wire_author(500, "proxy"), content);
        payload["channel_id"] = serde_json::json!("20");
        payload["guild_id"] = guild_id.map_or(serde_json::Value::Null, |id| {
            serde_json::Value::String(id.to_string())
        });
        payload["webhook_id"] = serde_json::json!("40");
        message_from_wire(payload)
    }

    fn handler_policy_config(
        channel_id: u64,
        allow_from: &[u64],
        require_mention: bool,
        patterns: &[&str],
    ) -> Arc<LoadedConfig> {
        let mut raw = Config::default();
        raw.channels.push(crate::config::ChannelConfig {
            id: channel_id.to_string(),
            require_mention,
            allow_from: allow_from.iter().map(u64::to_string).collect(),
            ..Default::default()
        });
        raw.mentions.patterns = patterns.iter().map(|value| (*value).to_owned()).collect();
        Arc::new(LoadedConfig::from_raw(raw))
    }

    fn represented_test_facts(user_id: u64) -> crate::pluralkit::VerifiedPkFacts {
        crate::pluralkit::VerifiedPkFacts::Represented {
            discord_user_id: UserId::new(user_id),
            system_id: None,
            member_id: None,
        }
    }

    #[tokio::test]
    async fn verified_create_uses_only_post_wait_config_then_current_mute() {
        use arc_swap::ArcSwap;
        use std::sync::Mutex;

        let snapshots = Arc::new(ArcSwap::from(handler_policy_config(
            99,
            &[],
            false,
            &["old"],
        )));
        let after = handler_policy_config(30, &[42], true, &["new-name"]);
        let order = Arc::new(Mutex::new(Vec::new()));
        let msg = verified_message(100, "hello new-name", Some(60));
        let resolution_msg = msg.clone();
        let snapshots_for_wait = Arc::clone(&snapshots);
        let order_for_wait = Arc::clone(&order);
        let resolution = crate::discord::verified_action_runtime::resolve_test_create_with_sources(
            resolution_msg,
            async move {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                snapshots_for_wait.store(after);
                order_for_wait.lock().expect("order lock").push("provider");
                Some(TEST_PK_CREATOR)
            },
            std::future::ready(Ok(represented_test_facts(42))),
        );
        let order_for_config = Arc::clone(&order);
        let order_for_mute = Arc::clone(&order);
        let snapshots_for_load = Arc::clone(&snapshots);
        let plan = admit_verified_create_after_wait(
            &msg,
            999,
            &crate::ingress_ledger::IngressLedger::new(),
            resolution,
            std::future::ready(Some(30)),
            move || {
                order_for_config.lock().expect("order lock").push("config");
                snapshots_for_load.load_full()
            },
            move |guild_id, gate_channel_id| {
                order_for_mute.lock().expect("order lock").push("mute");
                assert_eq!((guild_id, gate_channel_id), (60, 30));
                false
            },
        )
        .await
        .expect("post-wait policy admits");

        assert_eq!(plan.thread_parent_id, Some(30));
        assert!(matches!(
            plan.targeting,
            MessageTargeting::GuildDirected(MentionKind::ConfiguredPattern)
        ));
        assert_eq!(
            order.lock().expect("order lock").as_slice(),
            ["provider", "config", "mute"]
        );
    }

    #[tokio::test]
    async fn verified_create_real_gate_covers_supported_and_unavailable_outcomes() {
        async fn admitted(
            id: u64,
            config: Arc<LoadedConfig>,
            facts: Result<crate::pluralkit::VerifiedPkFacts, crate::pluralkit::PkResolveError>,
        ) -> bool {
            let msg = verified_message(id, "ambient", Some(60));
            let action = crate::discord::verified_action_runtime::resolve_test_create_with_sources(
                msg.clone(),
                std::future::ready(Some(TEST_PK_CREATOR)),
                std::future::ready(facts),
            );
            admit_verified_create_after_wait(
                &msg,
                999,
                &crate::ingress_ledger::IngressLedger::new(),
                action,
                std::future::ready(None),
                move || config,
                |_, _| false,
            )
            .await
            .is_some()
        }

        assert!(
            admitted(
                110,
                handler_policy_config(20, &[42], false, &[]),
                Ok(represented_test_facts(42))
            )
            .await
        );
        assert!(
            !admitted(
                111,
                handler_policy_config(20, &[7], false, &[]),
                Ok(represented_test_facts(42))
            )
            .await
        );
        assert!(
            admitted(
                112,
                handler_policy_config(20, &[], false, &[]),
                Err(crate::pluralkit::PkResolveError::Timeout),
            )
            .await
        );
        assert!(
            !admitted(
                113,
                handler_policy_config(20, &[42], false, &[]),
                Err(crate::pluralkit::PkResolveError::Timeout),
            )
            .await
        );
    }

    #[tokio::test]
    async fn unsupported_creator_and_fresh_envelope_changes_fail_closed() {
        async fn attempt(
            id: u64,
            creator: Option<u64>,
            config: Arc<LoadedConfig>,
            muted: bool,
        ) -> bool {
            let msg = verified_message(id, "ambient", Some(60));
            let action = crate::discord::verified_action_runtime::resolve_test_create_with_sources(
                msg.clone(),
                std::future::ready(creator),
                std::future::ready(Ok(represented_test_facts(42))),
            );
            admit_verified_create_after_wait(
                &msg,
                999,
                &crate::ingress_ledger::IngressLedger::new(),
                action,
                std::future::ready(None),
                move || config,
                move |_, _| muted,
            )
            .await
            .is_some()
        }

        assert!(
            !attempt(
                120,
                Some(1),
                handler_policy_config(20, &[], false, &[]),
                false
            )
            .await
        );
        assert!(
            !attempt(
                121,
                Some(TEST_PK_CREATOR),
                handler_policy_config(30, &[], false, &[]),
                false
            )
            .await
        );
        assert!(
            !attempt(
                122,
                Some(TEST_PK_CREATOR),
                handler_policy_config(20, &[], true, &["required"]),
                false
            )
            .await
        );
        assert!(
            !attempt(
                123,
                Some(TEST_PK_CREATOR),
                handler_policy_config(20, &[], false, &[]),
                true
            )
            .await
        );
    }

    fn verified_update_event(
        id: u64,
        author_id: u64,
        guild_id: Option<u64>,
        webhook_id: Option<u64>,
    ) -> MessageUpdateEvent {
        serde_json::from_value(serde_json::json!({
            "id": id.to_string(),
            "channel_id": "20",
            "guild_id": guild_id.map(|id| id.to_string()),
            "webhook_id": webhook_id.map(|id| id.to_string()),
            "author": {
                "id": author_id.to_string(),
                "username": "proxy",
                "discriminator": "0",
                "bot": false
            },
            "content": "edited",
            "edited_timestamp": "2026-01-01T00:00:01Z"
        }))
        .expect("valid update")
    }

    #[tokio::test]
    async fn sufficient_partial_update_uses_verified_path_and_conflicts_fail_closed() {
        let ledger = crate::ingress_ledger::IngressLedger::new();
        let create = verified_message(130, "before", Some(60));
        let create_action =
            crate::discord::verified_action_runtime::resolve_test_create_with_sources(
                create.clone(),
                std::future::ready(Some(TEST_PK_CREATOR)),
                std::future::ready(Ok(represented_test_facts(42))),
            );
        assert!(
            admit_verified_create_after_wait(
                &create,
                999,
                &ledger,
                create_action,
                std::future::ready(None),
                || handler_policy_config(20, &[42], false, &[]),
                |_, _| false,
            )
            .await
            .is_some()
        );

        let event = verified_update_event(130, 500, Some(60), Some(40));
        let candidate = VerifiedUpdateCandidate::from_gateway(&event, None, None)
            .expect("consistent raw update")
            .expect("raw update is sufficient");
        let action = crate::discord::verified_action_runtime::resolve_test_update_with_sources(
            candidate,
            std::future::ready(Some(TEST_PK_CREATOR)),
            std::future::ready(Ok(represented_test_facts(42))),
        );
        assert!(
            admit_verified_edit_after_wait(
                &event,
                None,
                None,
                event.author.as_ref().expect("author"),
                event.content.as_deref().expect("content"),
                event.edited_timestamp.expect("edited timestamp"),
                WebhookId::new(40),
                999,
                &ledger,
                action,
                std::future::ready(None),
                || handler_policy_config(20, &[42], false, &[]),
                |_, _| false,
            )
            .await
            .is_some()
        );

        let conflicting_new = verified_message(130, "edited", Some(60));
        let conflict = verified_update_event(130, 501, Some(60), Some(40));
        assert!(
            VerifiedUpdateCandidate::from_gateway(&conflict, None, Some(&conflicting_new)).is_err()
        );
    }

    #[tokio::test]
    async fn dm_lineage_and_delete_bulk_emit_only_admitted_direct_messages() {
        let ledger = crate::ingress_ledger::IngressLedger::new();
        for id in [140, 141] {
            assert!(matches!(
                ledger.admit_direct_create(
                    MessageId::new(id),
                    ChannelId::new(20),
                    LifecycleContext::DirectMessage,
                    UserId::new(500),
                    None,
                    "dm",
                    serenity::model::Timestamp::parse("2026-01-01T00:00:00Z").unwrap(),
                ),
                crate::ingress_ledger::TransitionResult::Admitted(_)
            ));
        }
        assert!(matches!(
            ledger.transition_passive_edit(
                MessageId::new(140),
                ChannelId::new(20),
                LifecycleContext::DirectMessage,
                UserId::new(500),
                "dm edited",
                serenity::model::Timestamp::parse("2026-01-01T00:00:01Z").unwrap(),
                |_| true,
            ),
            crate::ingress_ledger::TransitionResult::Admitted(_)
        ));

        let state = Arc::new(tokio::sync::RwLock::new(crate::state::SharedState::new()));
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        for id in [140, 999, 141] {
            emit_lifecycle_delete_to(
                &state,
                &ledger,
                &tx,
                ChannelId::new(20),
                MessageId::new(id),
                None,
            )
            .await;
        }
        let first = rx.recv().await.expect("first admitted delete");
        let second = rx.recv().await.expect("second admitted delete");
        assert!(
            matches!(first, NotificationEvent::MessageDelete { message_id, .. } if message_id == MessageId::new(140))
        );
        assert!(
            matches!(second, NotificationEvent::MessageDelete { message_id, .. } if message_id == MessageId::new(141))
        );
        assert!(rx.try_recv().is_err());
        emit_lifecycle_delete_to(
            &state,
            &ledger,
            &tx,
            ChannelId::new(20),
            MessageId::new(140),
            None,
        )
        .await;
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn webhook_dm_create_and_update_stop_before_injected_sources() {
        use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};

        let invoked = Arc::new(AtomicBool::new(false));
        let invoked_create = Arc::clone(&invoked);
        let msg = verified_message(150, "dm", None);
        let action = crate::discord::verified_action_runtime::resolve_test_create_with_sources(
            msg.clone(),
            async move {
                invoked_create.store(true, AtomicOrdering::Relaxed);
                Some(TEST_PK_CREATOR)
            },
            std::future::ready(Ok(represented_test_facts(42))),
        );
        let ledger = crate::ingress_ledger::IngressLedger::new();
        assert!(
            admit_verified_create_after_wait(
                &msg,
                999,
                &ledger,
                action,
                std::future::ready(None),
                || handler_policy_config(20, &[], false, &[]),
                |_, _| false,
            )
            .await
            .is_none()
        );
        assert!(!invoked.load(AtomicOrdering::Relaxed));

        let event = verified_update_event(150, 500, None, Some(40));
        let candidate = VerifiedUpdateCandidate::from_gateway(&event, None, None)
            .expect("consistent")
            .expect("sufficient");
        let invoked_update = Arc::clone(&invoked);
        let action = crate::discord::verified_action_runtime::resolve_test_update_with_sources(
            candidate,
            async move {
                invoked_update.store(true, AtomicOrdering::Relaxed);
                Some(TEST_PK_CREATOR)
            },
            std::future::ready(Ok(represented_test_facts(42))),
        );
        assert!(
            admit_verified_edit_after_wait(
                &event,
                None,
                None,
                event.author.as_ref().expect("author"),
                event.content.as_deref().expect("content"),
                event.edited_timestamp.expect("edited timestamp"),
                WebhookId::new(40),
                999,
                &ledger,
                action,
                std::future::ready(None),
                || handler_policy_config(20, &[], false, &[]),
                |_, _| false,
            )
            .await
            .is_none()
        );
        assert!(!invoked.load(AtomicOrdering::Relaxed));
    }

    #[test]
    fn passive_verified_edit_reapplies_identity_and_mention_policy() {
        use crate::discord::verified_action::{LifecycleProvenance, test_lifecycle_facts};

        let mut raw = Config::default();
        raw.channels.push(crate::config::ChannelConfig {
            id: "100".to_owned(),
            require_mention: true,
            allow_from: vec!["600".to_owned()],
            ..Default::default()
        });
        let config = LoadedConfig::from_raw(raw);
        let ledger = crate::ingress_ledger::IngressLedger::new();
        let facts = test_lifecycle_facts(
            MessageId::new(10),
            ChannelId::new(100),
            GuildId::new(200),
            WebhookId::new(400),
            LifecycleProvenance::Represented {
                discord_user_id: UserId::new(600),
                system_id: None,
                member_id: None,
            },
        );
        assert!(matches!(
            ledger.admit_verified_create(
                facts,
                MessageId::new(10),
                ChannelId::new(100),
                LifecycleContext::Guild(GuildId::new(200)),
                WebhookId::new(400),
                UserId::new(500),
                None,
                "one",
                serenity::model::Timestamp::parse("2026-01-01T00:00:00Z").unwrap(),
            ),
            crate::ingress_ledger::TransitionResult::Admitted(_)
        ));
        assert_eq!(
            ledger.transition_passive_edit(
                MessageId::new(10),
                ChannelId::new(100),
                LifecycleContext::Guild(GuildId::new(200)),
                UserId::new(500),
                "two",
                serenity::model::Timestamp::parse("2026-01-01T00:00:01Z").unwrap(),
                |lineage| passive_edit_policy_allows(&config, 100, Some(200), None, lineage),
            ),
            crate::ingress_ledger::TransitionResult::Rejected
        );
        assert!(matches!(
            ledger.transition_passive_edit(
                MessageId::new(10),
                ChannelId::new(100),
                LifecycleContext::Guild(GuildId::new(200)),
                UserId::new(500),
                "two",
                serenity::model::Timestamp::parse("2026-01-01T00:00:01Z").unwrap(),
                |lineage| passive_edit_policy_allows(
                    &config,
                    100,
                    Some(200),
                    Some(MentionKind::DirectMention),
                    lineage,
                ),
            ),
            crate::ingress_ledger::TransitionResult::Admitted(_)
        ));
    }

    // ── Gateway reaction filter tests ────────────────────────────────────────

    /// The bot's own gateway reactions are dropped — intentional self-reacts
    /// (e.g. contradictionary celebrate) reach the construct via the tool
    /// layer instead, marked `self_react: true`.
    #[test]
    fn test_gateway_drops_bot_self_reactions() {
        assert_eq!(gateway_reactor(Some(UserId::new(42)), 42), None);
    }

    /// Other users' reactions pass through with attribution intact.
    #[test]
    fn test_gateway_keeps_other_users_reactions() {
        assert_eq!(
            gateway_reactor(Some(UserId::new(7)), 42),
            Some(UserId::new(7))
        );
    }

    /// Reactions with no user attribution are dropped.
    #[test]
    fn test_gateway_drops_unattributed_reactions() {
        assert_eq!(gateway_reactor(None, 42), None);
    }

    // ── proxy bot constant tests ───────────────────────────────────────────────

    // ── reply_to_id tests ──────────────────────────────────────────────────────

    #[test]
    fn reply_to_id_returns_message_id_when_present() {
        use serenity::model::{
            channel::{MessageReference, MessageReferenceKind},
            id::{ChannelId, MessageId},
        };

        let message_id = MessageId::new(42);
        let reference = MessageReference::new(MessageReferenceKind::Default, ChannelId::new(1))
            .message_id(message_id);

        assert_eq!(reply_to_id(&reference), Some(message_id));
    }

    #[test]
    fn reply_to_id_returns_none_when_message_id_absent() {
        use serenity::model::{
            channel::{MessageReference, MessageReferenceKind},
            id::ChannelId,
        };

        let reference = MessageReference::new(MessageReferenceKind::Default, ChannelId::new(1));

        assert_eq!(reply_to_id(&reference), None);
    }

    #[test]
    fn reply_to_id_returns_none_for_forward_reference() {
        use serenity::model::{
            channel::{MessageReference, MessageReferenceKind},
            id::{ChannelId, MessageId},
        };

        let message_id = MessageId::new(99);
        let reference = MessageReference::new(MessageReferenceKind::Forward, ChannelId::new(1))
            .message_id(message_id);

        assert_eq!(reply_to_id(&reference), None);
    }

    // ── reply_preview tests ───────────────────────────────────────────────────

    #[test]
    fn reply_preview_returns_none_for_empty() {
        assert_eq!(reply_preview(""), None);
    }

    #[test]
    fn reply_preview_passes_through_short_content() {
        assert_eq!(reply_preview("hello"), Some("hello".to_string()));
    }

    #[test]
    fn reply_preview_truncates_and_appends_ellipsis() {
        let content: String = "a".repeat(100);
        let preview = reply_preview(&content).expect("non-empty content");
        assert_eq!(preview.chars().count(), 100);
        assert!(!preview.ends_with('…'));

        let long: String = "b".repeat(101);
        let preview = reply_preview(&long).expect("non-empty content");
        assert_eq!(preview.chars().count(), 101);
        assert!(preview.ends_with('…'));

        let emoji: String = "🐌".repeat(101);
        let preview = reply_preview(&emoji).expect("emoji content");
        assert_eq!(preview.chars().count(), 101);
        assert!(preview.ends_with('…'));
    }

    // ── reply_context tests ───────────────────────────────────────────────────

    fn wire_author(id: u64, username: &str) -> serde_json::Value {
        serde_json::json!({
            "id": id.to_string(),
            "username": username,
            "discriminator": "0",
        })
    }

    fn wire_message_body(id: u64, author: serde_json::Value, content: &str) -> serde_json::Value {
        serde_json::json!({
            "id": id.to_string(),
            "type": 0,
            "channel_id": "1",
            "author": author,
            "content": content,
            "timestamp": "2026-01-01T00:00:00.000000+00:00",
            "edited_timestamp": null,
            "tts": false,
            "mention_everyone": false,
            "mentions": [],
            "mention_roles": [],
            "attachments": [],
            "embeds": [],
            "pinned": false,
            "flags": 0,
            "components": [],
        })
    }

    fn message_from_wire(payload: serde_json::Value) -> Message {
        serde_json::from_value(payload).expect("valid Message JSON")
    }

    #[test]
    fn verified_delivery_rejects_raw_coordinate_mismatch() {
        let mut payload = wire_message_body(11, wire_author(7, "proxy"), "hello");
        payload["guild_id"] = serde_json::json!("30");
        payload["webhook_id"] = serde_json::json!("40");
        let message = message_from_wire(payload);
        let facts = crate::discord::verified_action::test_admission_facts(
            MessageId::new(10),
            ChannelId::new(message.channel_id.get()),
            GuildId::new(30),
            WebhookId::new(40),
        );
        let ledger = crate::ingress_ledger::IngressLedger::new();
        assert_eq!(
            ledger.admit_verified_create(
                facts.into_lifecycle(),
                message.id,
                message.channel_id,
                LifecycleContext::Guild(GuildId::new(30)),
                WebhookId::new(40),
                message.author.id,
                None,
                &message.content,
                message.timestamp,
            ),
            crate::ingress_ledger::TransitionResult::Rejected
        );
    }

    fn wire_reply_message(
        content: &str,
        reference: serde_json::Value,
        referenced_message: Option<serde_json::Value>,
    ) -> Message {
        let mut payload = wire_message_body(100, wire_author(1, "alice"), content);
        payload["message_reference"] = reference;
        if let Some(parent) = referenced_message {
            payload["referenced_message"] = parent;
        }
        message_from_wire(payload)
    }

    #[tokio::test]
    async fn admitted_message_is_recorded_before_notification_delivery() {
        let config = LoadedConfig::from_raw(Config::default());
        let msg = message_from_wire(wire_message_body(
            123,
            wire_author(456, "alice"),
            "gateway payload",
        ));
        let ledger = crate::ingress_ledger::IngressLedger::new();
        let event = build_message_event(
            &msg,
            &config,
            &ledger,
            None,
            MessageTargeting::DirectMessage,
            None,
            false,
        );
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);

        send_gateway_admitted_message(&ledger, &tx, &msg, None, event, "test").await;

        let delivered = rx.recv().await.expect("notification event");
        let NotificationEvent::Message(delivered) = delivered else {
            panic!("expected message event");
        };
        assert_eq!(
            ledger.verify(delivered.message_id, delivered.chat_id),
            crate::ingress_ledger::VerifyResult::Admitted {
                channel: auspex_core::ChannelRef::new(delivered.chat_id.get()),
            }
        );
        assert_eq!(delivered.content, "gateway payload");
    }

    #[test]
    fn reply_context_returns_none_for_non_reply() {
        let ledger = crate::ingress_ledger::IngressLedger::new();
        let msg = message_from_wire(wire_message_body(
            100,
            wire_author(1, "alice"),
            "not a reply",
        ));
        assert_eq!(reply_context(&msg, &ledger), (None, None, None));
    }

    #[test]
    fn reply_context_returns_none_for_forward() {
        let ledger = crate::ingress_ledger::IngressLedger::new();
        let msg = wire_reply_message(
            "forwarded",
            serde_json::json!({
                "type": 1,
                "channel_id": "1",
                "message_id": "999",
            }),
            Some(wire_message_body(
                999,
                wire_author(500, "parent"),
                "parent text",
            )),
        );
        assert_eq!(reply_context(&msg, &ledger), (None, None, None));
    }

    #[test]
    fn reply_context_returns_none_when_referenced_message_absent() {
        let ledger = crate::ingress_ledger::IngressLedger::new();
        let msg = wire_reply_message(
            "reply without parent body",
            serde_json::json!({
                "type": 0,
                "channel_id": "1",
                "message_id": "999",
            }),
            None,
        );
        assert_eq!(reply_context(&msg, &ledger), (None, None, None));
    }

    #[test]
    fn reply_context_extracts_author_and_preview_for_reply() {
        let ledger = crate::ingress_ledger::IngressLedger::new();
        let msg = wire_reply_message(
            "my reply",
            serde_json::json!({
                "type": 0,
                "channel_id": "1",
                "message_id": "999",
            }),
            Some(wire_message_body(
                999,
                wire_author(500, "parentuser"),
                "parent message content",
            )),
        );
        let (uid, user, preview) = reply_context(&msg, &ledger);
        assert_eq!(uid, Some(UserId::new(500)));
        assert_eq!(user.as_deref(), Some("parentuser"));
        assert_eq!(preview.as_deref(), Some("parent message content"));
    }

    #[test]
    fn reply_context_omits_preview_for_empty_parent_content() {
        let ledger = crate::ingress_ledger::IngressLedger::new();
        let msg = wire_reply_message(
            "my reply",
            serde_json::json!({
                "type": 0,
                "channel_id": "1",
                "message_id": "999",
            }),
            Some(wire_message_body(999, wire_author(500, "parentuser"), "")),
        );
        let (uid, user, preview) = reply_context(&msg, &ledger);
        assert_eq!(uid, Some(UserId::new(500)));
        assert_eq!(user.as_deref(), Some("parentuser"));
        assert_eq!(preview, None);
    }

    #[test]
    fn reply_context_never_exposes_unverified_webhook_author() {
        let ledger = crate::ingress_ledger::IngressLedger::new();
        let mut parent = wire_message_body(999, wire_author(500, "proxy"), "parent");
        parent["guild_id"] = serde_json::json!("30");
        parent["webhook_id"] = serde_json::json!("40");
        let msg = wire_reply_message(
            "reply",
            serde_json::json!({
                "type": 0,
                "channel_id": "1",
                "message_id": "999",
            }),
            Some(parent),
        );

        let (uid, user, preview) = reply_context(&msg, &ledger);
        assert_eq!(uid, None);
        assert_eq!(user.as_deref(), Some("proxy"));
        assert_eq!(preview.as_deref(), Some("parent"));
    }

    #[test]
    fn reply_context_uses_admitted_represented_participant() {
        let ledger = crate::ingress_ledger::IngressLedger::new();
        let facts = crate::discord::verified_action::test_lifecycle_facts(
            MessageId::new(999),
            ChannelId::new(1),
            GuildId::new(30),
            WebhookId::new(40),
            LifecycleProvenance::Represented {
                discord_user_id: UserId::new(600),
                system_id: None,
                member_id: None,
            },
        );
        assert!(matches!(
            ledger.admit_verified_create(
                facts,
                MessageId::new(999),
                ChannelId::new(1),
                LifecycleContext::Guild(GuildId::new(30)),
                WebhookId::new(40),
                UserId::new(500),
                None,
                "parent",
                serenity::model::Timestamp::parse("2026-01-01T00:00:00Z").unwrap(),
            ),
            crate::ingress_ledger::TransitionResult::Admitted(_)
        ));
        // The reply carries guild_id (a top-level gateway field); its inlined
        // parent copy does NOT (Discord omits guild_id on referenced_message).
        // Context comes from the reply, so the Guild(30) snapshot resolves.
        let mut parent = wire_message_body(999, wire_author(500, "proxy"), "parent");
        parent["webhook_id"] = serde_json::json!("40");
        let mut msg = wire_message_body(100, wire_author(1, "alice"), "reply");
        msg["guild_id"] = serde_json::json!("30");
        msg["channel_id"] = serde_json::json!("1");
        msg["message_reference"] =
            serde_json::json!({ "type": 0, "channel_id": "1", "message_id": "999" });
        msg["referenced_message"] = parent;
        let msg = message_from_wire(msg);

        let (uid, _, _) = reply_context(&msg, &ledger);
        assert_eq!(uid, Some(UserId::new(600)));
        assert_ne!(uid, Some(UserId::new(500)));
    }

    // ── build_message_event tests ─────────────────────────────────────────────

    #[test]
    fn build_message_event_populates_reply_context_for_reply() {
        let config = LoadedConfig::from_raw(Config::default());
        let ledger = crate::ingress_ledger::IngressLedger::new();
        let msg = wire_reply_message(
            "my reply",
            serde_json::json!({
                "type": 0,
                "channel_id": "1",
                "message_id": "999",
            }),
            Some(wire_message_body(
                999,
                wire_author(500, "parentuser"),
                "parent message content",
            )),
        );

        let event = build_message_event(
            &msg,
            &config,
            &ledger,
            None,
            MessageTargeting::Ambient,
            None,
            false,
        );
        let NotificationEvent::Message(MessageEvent {
            reply_to_message_id,
            reply_to_user_id,
            reply_to_user,
            reply_to_content_preview,
            content,
            user,
            ..
        }) = event
        else {
            panic!("expected Message event");
        };

        assert_eq!(reply_to_message_id, Some(MessageId::new(999)));
        assert_eq!(reply_to_user_id, Some(UserId::new(500)));
        assert_eq!(reply_to_user.as_deref(), Some("parentuser"));
        assert_eq!(
            reply_to_content_preview.as_deref(),
            Some("parent message content")
        );
        assert_eq!(content, "my reply");
        assert_eq!(user, "alice");
    }

    // ── #369 v2 identity-ignore: resolution, redaction, per-path enforcement ──

    /// One opted-in channel plus an identity ignore list.
    fn ignore_channel_config(
        channel_id: u64,
        allow_from: &[u64],
        ignore_from: &[u64],
        require_mention: bool,
    ) -> LoadedConfig {
        let mut raw = Config::default();
        raw.channels.push(crate::config::ChannelConfig {
            id: channel_id.to_string(),
            require_mention,
            allow_from: allow_from.iter().map(u64::to_string).collect(),
            ..Default::default()
        });
        raw.access.ignore_from = ignore_from.iter().map(u64::to_string).collect();
        LoadedConfig::from_raw(raw)
    }

    /// A live-token-free HTTP handle. The 3-tier resolver only touches HTTP at
    /// tier3, so tier1/tier2/short-circuit tests never invoke the network.
    fn unused_http() -> serenity::http::Http {
        serenity::http::Http::new("Bot test-token-tier12-never-calls-network")
    }

    /// reply, parent ignored, resolved via the INLINE gateway copy (tier1) →
    /// admitted with the quoted preview stripped; the parent user id is kept for
    /// threading (it is not the leak — the content is).
    #[tokio::test]
    async fn reply_to_ignored_parent_inline_admits_and_redacts_preview() {
        let config = ignore_channel_config(1, &[], &[500], false);
        let ledger = crate::ingress_ledger::IngressLedger::new();
        let http = unused_http();
        let msg = wire_reply_message(
            "my reply",
            serde_json::json!({ "type": 0, "channel_id": "1", "message_id": "999" }),
            Some(wire_message_body(
                999,
                wire_author(500, "blocked"),
                "secret content",
            )),
        );

        let action = resolve_reply_parent_ignore(&config, &ledger, &http, &msg).await;
        assert_eq!(action, ReplyParentAction::AdmitRedactPreview);

        let event = build_message_event(
            &msg,
            &config,
            &ledger,
            None,
            MessageTargeting::Ambient,
            None,
            action.redacts_preview(),
        );
        let NotificationEvent::Message(m) = event else {
            panic!("expected message event");
        };
        assert_eq!(
            m.reply_to_content_preview, None,
            "the ignored parent's quoted content must be stripped"
        );
        assert_eq!(
            m.reply_to_user_id,
            Some(UserId::new(500)),
            "threading identity is preserved"
        );
    }

    /// reply, parent ignored, resolved via the LEDGER (tier2 — parent not
    /// inlined but present as an admitted record, an immutable authorship fact).
    #[tokio::test]
    async fn reply_to_ignored_parent_via_ledger_resolves_ignored() {
        let config = ignore_channel_config(1, &[], &[500], false);
        let ledger = crate::ingress_ledger::IngressLedger::new();
        let http = unused_http();
        // Record the parent (999, ch 1, author 500) as an admitted DM.
        ledger.note_admitted(
            MessageId::new(999),
            ChannelId::new(1),
            UserId::new(500),
            "parent",
        );
        // The reply carries only a reference — no inlined referenced_message.
        let msg = wire_reply_message(
            "my reply",
            serde_json::json!({ "type": 0, "channel_id": "1", "message_id": "999" }),
            None,
        );
        let resolution = resolve_reply_parent_resolution(&config, &ledger, &http, &msg).await;
        assert_eq!(
            resolution,
            ReplyParentResolution::ParentIgnored,
            "tier2 ledger snapshot resolves the ignored parent author"
        );
    }

    /// #369 (P1 fix): a reply whose INLINE parent is a webhook/PK message resolves
    /// the parent's represented PRINCIPAL via the ledger (keyed by the REPLY's
    /// guild, since Discord omits `guild_id` on the nested parent), never the raw
    /// webhook TRANSPORT author id. Transport author is 500 (not ignored); the
    /// represented principal is 77. Also pins that a resolved-but-not-ignored
    /// principal stays Clear (not merely Unresolvable-via-fail-open).
    #[tokio::test]
    async fn reply_to_ignored_webhook_parent_resolves_principal_not_transport() {
        let ledger = crate::ingress_ledger::IngressLedger::new();
        let http = unused_http();

        // The parent's own MESSAGE_CREATE carries guild_id (a top-level gateway
        // field): admit it as represented principal 77 while 77 is NOT ignored, so
        // the ledger records the snapshot under Guild(60).
        let parent_msg = {
            let mut p = wire_message_body(999, wire_author(500, "proxy"), "secret parent content");
            p["channel_id"] = serde_json::json!("20");
            p["guild_id"] = serde_json::json!("60");
            p["webhook_id"] = serde_json::json!("40");
            message_from_wire(p)
        };
        let create_action =
            crate::discord::verified_action_runtime::resolve_test_create_with_sources(
                parent_msg.clone(),
                std::future::ready(Some(TEST_PK_CREATOR)),
                std::future::ready(Ok(represented_test_facts(77))),
            );
        assert!(
            admit_verified_create_after_wait(
                &parent_msg,
                999,
                &ledger,
                create_action,
                std::future::ready(None),
                || handler_policy_config(20, &[], false, &[]),
                |_, _| false,
            )
            .await
            .is_some(),
            "parent admits while 77 is not ignored, recording its principal snapshot"
        );

        // The reply carries guild_id at the TOP level; its inlined parent copy does
        // NOT (Discord omits guild_id on referenced_message). Context must come from
        // the reply -- keying off the nested parent's (absent) guild_id would miss.
        let reply = {
            let mut r = wire_message_body(100, wire_author(1, "alice"), "my reply");
            r["guild_id"] = serde_json::json!("60");
            r["channel_id"] = serde_json::json!("20");
            r["message_reference"] =
                serde_json::json!({ "type": 0, "channel_id": "20", "message_id": "999" });
            let mut parent_inline =
                wire_message_body(999, wire_author(500, "proxy"), "secret parent content");
            parent_inline["channel_id"] = serde_json::json!("20");
            parent_inline["webhook_id"] = serde_json::json!("40");
            // deliberately NO guild_id on the nested parent (realistic wire shape)
            r["referenced_message"] = parent_inline;
            message_from_wire(r)
        };

        // 77 ignored -> ParentIgnored at the resolution level (not just the fail-open
        // action), and the action redacts the preview.
        let ignored_cfg = ignore_channel_config(20, &[], &[77], false);
        assert_eq!(
            resolve_reply_parent_resolution(&ignored_cfg, &ledger, &http, &reply).await,
            ReplyParentResolution::ParentIgnored,
            "webhook parent's ignored PRINCIPAL (77) is resolved via the ledger, not the transport id (500)"
        );
        assert_eq!(
            resolve_reply_parent_ignore(&ignored_cfg, &ledger, &http, &reply).await,
            ReplyParentAction::AdmitRedactPreview
        );

        // Control: a non-empty ignore list WITHOUT 77 must resolve Clear -- proving
        // the principal is actually resolved, not collapsed to Unresolvable.
        let other_cfg = ignore_channel_config(20, &[], &[888], false);
        assert_eq!(
            resolve_reply_parent_resolution(&other_cfg, &ledger, &http, &reply).await,
            ReplyParentResolution::Clear,
            "principal 77 resolves and, not being ignored, the parent is Clear (not Unresolvable)"
        );
        assert_eq!(
            resolve_reply_parent_ignore(&other_cfg, &ledger, &http, &reply).await,
            ReplyParentAction::Admit
        );
    }

    /// ignore_from empty → the resolver returns Admit with NO redaction, even
    /// for a reply whose inline parent WOULD be ignored were the list populated.
    /// (Behavioral facet of the short-circuit; the "no fetch/ledger" property is
    /// structural — with an empty list `is_ignored` is always false regardless.)
    #[tokio::test]
    async fn empty_ignore_list_admits_without_redaction() {
        let config = ignore_channel_config(1, &[], &[], false);
        let ledger = crate::ingress_ledger::IngressLedger::new();
        let http = unused_http();
        let msg = wire_reply_message(
            "my reply",
            serde_json::json!({ "type": 0, "channel_id": "1", "message_id": "999" }),
            Some(wire_message_body(
                999,
                wire_author(500, "whoever"),
                "content",
            )),
        );
        let action = resolve_reply_parent_ignore(&config, &ledger, &http, &msg).await;
        assert_eq!(action, ReplyParentAction::Admit);
        assert!(!action.redacts_preview());
    }

    /// forward/crosspost reference → resolved explicitly as Unresolvable (the
    /// reference's author is not trusted; a forward carries no quoted preview).
    #[tokio::test]
    async fn forward_reference_is_unresolvable() {
        let config = ignore_channel_config(1, &[], &[500], false);
        let ledger = crate::ingress_ledger::IngressLedger::new();
        let http = unused_http();
        let msg = wire_reply_message(
            "forwarded",
            serde_json::json!({ "type": 1, "channel_id": "1", "message_id": "999" }),
            Some(wire_message_body(
                999,
                wire_author(500, "blocked"),
                "content",
            )),
        );
        let resolution = resolve_reply_parent_resolution(&config, &ledger, &http, &msg).await;
        assert_eq!(resolution, ReplyParentResolution::Unresolvable);
    }

    /// Handler-level: an unresolvable parent under fail-CLOSED yields
    /// DropUnresolved, which every create/edit handler branches on
    /// (`if action.drops() { return }`) to suppress delivery. The shipped const
    /// is fail-OPEN, so the fail policy is exercised as a PARAMETER (fable P2
    /// dead-code fix): the real ladder produces Unresolvable, and the classifier
    /// drops under fail-closed / redacts under fail-open.
    #[tokio::test]
    async fn unresolvable_parent_drop_is_reachable_at_the_handler() {
        let config = ignore_channel_config(1, &[], &[500], false);
        let ledger = crate::ingress_ledger::IngressLedger::new();
        let http = unused_http();
        let msg = wire_reply_message(
            "forwarded",
            serde_json::json!({ "type": 1, "channel_id": "1", "message_id": "999" }),
            None,
        );
        let resolution = resolve_reply_parent_resolution(&config, &ledger, &http, &msg).await;
        assert_eq!(resolution, ReplyParentResolution::Unresolvable);

        let fail_closed = crate::gate::classify_reply_parent_ignore(resolution, true);
        assert!(
            fail_closed.drops(),
            "fail-closed suppresses delivery of the reply"
        );
        let fail_open = crate::gate::classify_reply_parent_ignore(resolution, false);
        assert!(
            !fail_open.drops() && fail_open.redacts_preview(),
            "fail-open admits but redacts"
        );
    }

    /// #369: un-ignore takes effect on the very next message — the resolver
    /// reads live config. A reply whose parent WAS ignored (preview redacted) is,
    /// once the id is removed, admitted with the preview intact.
    #[tokio::test]
    async fn un_ignore_restores_preview_on_next_reply() {
        let ledger = crate::ingress_ledger::IngressLedger::new();
        let http = unused_http();
        let msg = wire_reply_message(
            "my reply",
            serde_json::json!({ "type": 0, "channel_id": "1", "message_id": "999" }),
            Some(wire_message_body(
                999,
                wire_author(500, "formerly"),
                "content",
            )),
        );
        let ignored = ignore_channel_config(1, &[], &[500], false);
        assert_eq!(
            resolve_reply_parent_ignore(&ignored, &ledger, &http, &msg).await,
            ReplyParentAction::AdmitRedactPreview
        );
        let unignored = ignore_channel_config(1, &[], &[], false);
        assert_eq!(
            resolve_reply_parent_ignore(&unignored, &ledger, &http, &msg).await,
            ReplyParentAction::Admit,
            "removing the id delivers the very next reply with its preview restored"
        );
    }

    /// #369 (P1 fix): a verified/PK CREATE from an ignored PRINCIPAL is dropped —
    /// even when that principal is also allow-listed on the channel (ignore
    /// overrides allow_from on the webhook path too). This was a P1 bypass.
    #[tokio::test]
    async fn verified_create_drops_ignored_principal() {
        // 42 is allow-listed AND ignored → must drop on the principal.
        let config = Arc::new(ignore_channel_config(20, &[42], &[42], false));
        let msg = verified_message(140, "hello", Some(60));
        let action = crate::discord::verified_action_runtime::resolve_test_create_with_sources(
            msg.clone(),
            std::future::ready(Some(TEST_PK_CREATOR)),
            std::future::ready(Ok(represented_test_facts(42))),
        );
        let plan = admit_verified_create_after_wait(
            &msg,
            999,
            &crate::ingress_ledger::IngressLedger::new(),
            action,
            std::future::ready(None),
            move || config,
            |_, _| false,
        )
        .await;
        assert!(
            plan.is_none(),
            "an ignored principal's verified create must be dropped"
        );
    }

    /// #369 (P1 fix): a verified/PK EDIT from an ignored PRINCIPAL is dropped.
    /// The create is admitted while the principal is NOT ignored; the principal
    /// is then ignored and the edit of that already-admitted message must drop —
    /// exactly the previously-unchecked bypass.
    #[tokio::test]
    async fn verified_edit_drops_newly_ignored_principal() {
        let ledger = crate::ingress_ledger::IngressLedger::new();
        let create = verified_message(150, "before", Some(60));
        let create_action =
            crate::discord::verified_action_runtime::resolve_test_create_with_sources(
                create.clone(),
                std::future::ready(Some(TEST_PK_CREATOR)),
                std::future::ready(Ok(represented_test_facts(42))),
            );
        assert!(
            admit_verified_create_after_wait(
                &create,
                999,
                &ledger,
                create_action,
                std::future::ready(None),
                || handler_policy_config(20, &[42], false, &[]),
                |_, _| false,
            )
            .await
            .is_some(),
            "create admits while 42 is not ignored"
        );

        let event = verified_update_event(150, 500, Some(60), Some(40));
        let candidate = VerifiedUpdateCandidate::from_gateway(&event, None, None)
            .expect("consistent raw update")
            .expect("raw update is sufficient");
        let action = crate::discord::verified_action_runtime::resolve_test_update_with_sources(
            candidate,
            std::future::ready(Some(TEST_PK_CREATOR)),
            std::future::ready(Ok(represented_test_facts(42))),
        );
        let config = Arc::new(ignore_channel_config(20, &[42], &[42], false));
        let plan = admit_verified_edit_after_wait(
            &event,
            None,
            None,
            event.author.as_ref().expect("author"),
            event.content.as_deref().expect("content"),
            event.edited_timestamp.expect("edited timestamp"),
            WebhookId::new(40),
            999,
            &ledger,
            action,
            std::future::ready(None),
            move || config,
            |_, _| false,
        )
        .await;
        assert!(
            plan.is_none(),
            "an ignored principal's verified edit must be dropped (P1 bypass)"
        );
    }

    /// #369 (P1 fix): the passive guild EDIT path drops an edit whose effective
    /// (represented) author is ignored — previously unchecked. Mirrors
    /// `passive_verified_edit_reapplies_identity_and_mention_policy` but with the
    /// represented principal (600) on the ignore list, so even a mention-carrying
    /// edit by an otherwise-allowed principal is rejected.
    #[test]
    fn passive_verified_edit_drops_ignored_represented_principal() {
        use crate::discord::verified_action::{LifecycleProvenance, test_lifecycle_facts};

        let mut raw = Config::default();
        raw.channels.push(crate::config::ChannelConfig {
            id: "100".to_owned(),
            require_mention: true,
            allow_from: vec!["600".to_owned()],
            ..Default::default()
        });
        // The represented principal 600 is on the ignore blocklist.
        raw.access.ignore_from = vec!["600".to_owned()];
        let config = LoadedConfig::from_raw(raw);
        let ledger = crate::ingress_ledger::IngressLedger::new();
        let facts = test_lifecycle_facts(
            MessageId::new(10),
            ChannelId::new(100),
            GuildId::new(200),
            WebhookId::new(400),
            LifecycleProvenance::Represented {
                discord_user_id: UserId::new(600),
                system_id: None,
                member_id: None,
            },
        );
        assert!(matches!(
            ledger.admit_verified_create(
                facts,
                MessageId::new(10),
                ChannelId::new(100),
                LifecycleContext::Guild(GuildId::new(200)),
                WebhookId::new(400),
                UserId::new(500),
                None,
                "one",
                serenity::model::Timestamp::parse("2026-01-01T00:00:00Z").unwrap(),
            ),
            crate::ingress_ledger::TransitionResult::Admitted(_)
        ));
        // Even a mention-satisfying edit is rejected because 600 is ignored;
        // without the is_ignored guard in passive_edit_policy_allows this would
        // ADMIT (channel allow_from lists 600, and the mention is present).
        assert_eq!(
            ledger.transition_passive_edit(
                MessageId::new(10),
                ChannelId::new(100),
                LifecycleContext::Guild(GuildId::new(200)),
                UserId::new(500),
                "two",
                serenity::model::Timestamp::parse("2026-01-01T00:00:01Z").unwrap(),
                |lineage| passive_edit_policy_allows(
                    &config,
                    100,
                    Some(200),
                    Some(MentionKind::DirectMention),
                    lineage,
                ),
            ),
            crate::ingress_ledger::TransitionResult::Rejected
        );
    }

    #[test]
    fn represented_and_direct_messages_share_the_same_construct_envelope_shape() {
        use crate::mcp::notifications::IntoNotification;

        let config = LoadedConfig::from_raw(Config::default());
        let direct_ledger = crate::ingress_ledger::IngressLedger::new();
        let direct = message_from_wire(wire_message_body(200, wire_author(600, "alice"), "hello"));
        let direct = build_message_event(
            &direct,
            &config,
            &direct_ledger,
            None,
            MessageTargeting::Ambient,
            None,
            false,
        )
        .into_notification();

        let represented_ledger = crate::ingress_ledger::IngressLedger::new();
        let mut represented_message = wire_message_body(201, wire_author(500, "alice"), "hello");
        represented_message["guild_id"] = serde_json::json!("30");
        represented_message["webhook_id"] = serde_json::json!("40");
        let represented_message = message_from_wire(represented_message);
        let facts = crate::discord::verified_action::test_lifecycle_facts(
            represented_message.id,
            represented_message.channel_id,
            GuildId::new(30),
            WebhookId::new(40),
            LifecycleProvenance::Represented {
                discord_user_id: UserId::new(600),
                system_id: None,
                member_id: None,
            },
        );
        let crate::ingress_ledger::TransitionResult::Admitted(admission) = represented_ledger
            .admit_verified_create(
                facts,
                represented_message.id,
                represented_message.channel_id,
                LifecycleContext::Guild(GuildId::new(30)),
                WebhookId::new(40),
                represented_message.author.id,
                None,
                &represented_message.content,
                represented_message.timestamp,
            )
        else {
            panic!("represented message must admit");
        };
        let represented = build_verified_message_event(
            &admission,
            &represented_message,
            &config,
            &represented_ledger,
            None,
            MessageTargeting::Ambient,
            None,
            false,
        )
        .into_notification();

        let direct_meta = direct["params"]["meta"].as_object().expect("direct meta");
        let represented_meta = represented["params"]["meta"]
            .as_object()
            .expect("represented meta");
        assert_eq!(
            direct_meta.keys().collect::<Vec<_>>(),
            represented_meta.keys().collect::<Vec<_>>()
        );
        assert_eq!(direct_meta["user_id"], "600");
        assert_eq!(represented_meta["user_id"], "600");
        assert_ne!(represented_meta["user_id"], "500");
    }

    // ── display_name tests ───────────────────────────────────────────────────

    #[test]
    fn display_name_falls_back_to_username() {
        let msg = message_from_wire(wire_message_body(1, wire_author(1, "alice"), "hi"));
        assert_eq!(display_name(&msg), "alice");
    }

    #[test]
    fn display_name_prefers_global_name() {
        let mut author = wire_author(1, "alice");
        author["global_name"] = serde_json::json!("Alice Display");
        let msg = message_from_wire(wire_message_body(1, author, "hi"));
        assert_eq!(display_name(&msg), "Alice Display");
    }

    #[test]
    fn display_name_prefers_server_nick() {
        let mut author = wire_author(1, "alice");
        author["global_name"] = serde_json::json!("Alice Display");
        let mut payload = wire_message_body(1, author, "hi");
        payload["member"] = serde_json::json!({
            "roles": [],
            "joined_at": "2026-01-01T00:00:00.000000+00:00",
            "deaf": false,
            "mute": false,
            "nick": "Server Alice",
        });
        let msg = message_from_wire(payload);
        assert_eq!(display_name(&msg), "Server Alice");
    }

    #[test]
    fn display_name_from_user_falls_back_to_username() {
        let user_json = wire_author(1, "bob_iverse");
        let user: serenity::model::user::User =
            serde_json::from_value(user_json).expect("valid User JSON");
        assert_eq!(display_name_from_user(&user), "bob_iverse");
    }

    #[test]
    fn display_name_from_user_prefers_global_name() {
        let mut user_json = wire_author(1, "bob_iverse");
        user_json["global_name"] = serde_json::json!("Bob Iverse");
        let user: serenity::model::user::User =
            serde_json::from_value(user_json).expect("valid User JSON");
        assert_eq!(display_name_from_user(&user), "Bob Iverse");
    }

    // ── resolve_user_identity tests (#153) ───────────────────────────────────

    #[test]
    fn resolve_user_identity_prefers_display_name() {
        assert_eq!(
            resolve_user_identity(Some("Vesper"), Some("vesper_bot")),
            "Vesper"
        );
    }

    #[test]
    fn resolve_user_identity_falls_back_to_username_when_display_name_unset() {
        assert_eq!(
            resolve_user_identity(None, Some("vesper_bot")),
            "vesper_bot"
        );
    }

    #[test]
    fn resolve_user_identity_falls_back_to_username_when_display_name_blank() {
        // An empty (or whitespace-only) display name must not leak through; it
        // is treated as absent so the Discord username is used instead.
        assert_eq!(
            resolve_user_identity(Some(""), Some("vesper_bot")),
            "vesper_bot"
        );
        assert_eq!(
            resolve_user_identity(Some("   "), Some("vesper_bot")),
            "vesper_bot"
        );
    }

    #[test]
    fn resolve_user_identity_defaults_to_dione_when_neither_present() {
        assert_eq!(resolve_user_identity(None, None), "dione");
        assert_eq!(resolve_user_identity(Some(""), Some("")), "dione");
        assert_eq!(resolve_user_identity(Some("  "), None), "dione");
    }

    // ── strip_invisible tests ────────────────────────────────────────────────

    #[test]
    fn strip_invisible_removes_pk_padding() {
        assert_eq!(strip_invisible("B\u{17B5}"), "B");
    }

    #[test]
    fn strip_invisible_removes_zero_width_space() {
        assert_eq!(strip_invisible("test\u{200B}name"), "testname");
    }

    #[test]
    fn strip_invisible_preserves_normal_text() {
        assert_eq!(strip_invisible("Benedicta"), "Benedicta");
    }

    #[test]
    fn display_name_strips_pk_padding() {
        let mut author = wire_author(1, "B\u{17B5}");
        author["bot"] = serde_json::json!(true);
        let msg = message_from_wire(wire_message_body(1, author, "hello"));
        assert_eq!(display_name(&msg), "B");
    }

    // ── Presence sink mock & tests ──────────────────────────────────────────

    /// Records every `set_presence` call for assertion in tests.
    struct MockPresenceSink {
        calls: std::sync::Mutex<Vec<(Option<String>, OnlineStatus)>>,
    }

    impl MockPresenceSink {
        fn new() -> Self {
            Self {
                calls: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn call_count(&self) -> usize {
            self.calls.lock().unwrap().len()
        }
    }

    impl PresenceSink for MockPresenceSink {
        fn set_presence(&self, activity: Option<ActivityData>, status: OnlineStatus) {
            let name = activity.map(|a| a.name.clone());
            self.calls.lock().unwrap().push((name, status));
        }
    }

    /// Queue drain + last-write-wins: multiple commands dispatched in sequence
    /// all reach the same sink, and the desired presence reflects the final one.
    #[tokio::test]
    async fn presence_queue_drain_last_write_wins() {
        use crate::mcp::tools::bot_state::{ActivityType, DiscordCommand, OnlineStatus};

        let presence = SharedPresence::new();
        let (tx, rx) = tokio::sync::mpsc::channel::<DiscordCommand>(16);

        let sink = Arc::new(MockPresenceSink::new());
        // Use install — the same path ready() takes.
        presence.install(sink.clone()).await;

        let handle = tokio::spawn(run_discord_commands(presence.clone(), rx));

        // Send three commands in quick succession.
        for (status, name) in [
            (OnlineStatus::Online, "first"),
            (OnlineStatus::Idle, "second"),
            (OnlineStatus::Dnd, "third"),
        ] {
            tx.send(DiscordCommand::SetPresence {
                online_status: status,
                activity_type: Some(ActivityType::Playing),
                activity_name: Some(name.to_string()),
            })
            .await
            .unwrap();
        }

        // Close the channel and await the processor — deterministic drain,
        // no sleeps.
        drop(tx);
        handle.await.unwrap();

        assert_eq!(sink.call_count(), 3, "all three commands must dispatch");

        // Desired presence reflects the last write.
        let stored = presence
            .desired_for_test()
            .await
            .expect("desired presence stored");
        assert_eq!(
            stored.status,
            serenity::model::user::OnlineStatus::DoNotDisturb
        );
        assert_eq!(
            stored.activity.as_ref().map(|a| a.name.as_str()),
            Some("third")
        );
    }

    /// Reconnect dispatch: Ready(A) -> command -> Ready(B) -> replay.
    ///
    /// Uses `SharedPresence::install` — the same code path `ready()` takes —
    /// so deleting production replay logic would cause this test to fail.
    /// Asserts that reconnect replay reaches B exactly once, and no
    /// post-replacement dispatch leaks through A.
    #[tokio::test]
    async fn presence_reconnect_replays_to_new_sink_not_old() {
        use crate::mcp::tools::bot_state::{ActivityType, DiscordCommand, OnlineStatus};

        let presence = SharedPresence::new();
        let (tx, rx) = tokio::sync::mpsc::channel::<DiscordCommand>(16);

        // Install mock sink A (simulates first ready()).
        let sink_a = Arc::new(MockPresenceSink::new());
        presence.install(sink_a.clone()).await;

        // Spawn the command processor (once, like the real ready()).
        let handle = tokio::spawn(run_discord_commands(presence.clone(), rx));

        // Send a SetPresence command through the channel.
        tx.send(DiscordCommand::SetPresence {
            online_status: OnlineStatus::Idle,
            activity_type: Some(ActivityType::Playing),
            activity_name: Some("testing".to_string()),
        })
        .await
        .unwrap();

        // Close the channel and drain — deterministic, no sleeps.
        drop(tx);
        handle.await.unwrap();

        // Sink A received exactly one set_presence call.
        assert_eq!(sink_a.call_count(), 1, "sink A must receive one dispatch");
        {
            let calls = sink_a.calls.lock().unwrap();
            assert_eq!(calls[0].0.as_deref(), Some("testing"));
            assert_eq!(calls[0].1, serenity::model::user::OnlineStatus::Idle);
        }

        // Simulate reconnect: install mock sink B
        // (the same code path ready() takes — tests the real wiring).
        let sink_b = Arc::new(MockPresenceSink::new());
        presence.install(sink_b.clone()).await;

        // Sink B received exactly one set_presence call (replay).
        assert_eq!(sink_b.call_count(), 1, "sink B must receive replay");
        {
            let calls = sink_b.calls.lock().unwrap();
            assert_eq!(calls[0].0.as_deref(), Some("testing"));
            assert_eq!(calls[0].1, serenity::model::user::OnlineStatus::Idle);
        }

        // Sink A has no additional calls after replacement.
        assert_eq!(
            sink_a.call_count(),
            1,
            "sink A must not receive post-replacement dispatch"
        );
    }
}
