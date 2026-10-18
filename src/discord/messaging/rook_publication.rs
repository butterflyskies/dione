//! A single Discord message POST for the durable Rook publication owner.

use super::{
    MessagingCtx, MessagingErrorKind, OutboundDraft, OutboundSurface, PreSendOptions, TargetError,
    TargetOperation, TargetPolicy, auto_send_records, channel_coordinator, check_outbound,
    prepare_outbound, send_side_records, verify_message_target,
};
use crate::{
    contradictionary::{Action, DiaryRecord, append_diary_record},
    coordination::{ClaimOutcome, ClaimProgress, Coordinator},
    core::{
        ConversationId, ConversationRef, MessageRef, ModuleInstanceId, ModuleSlotId, ProviderId,
    },
    no_rly::judge::{OutboundJudge, Verdict},
    rook::{
        CanonicalPublication, DispatchOutcome, MentionPolicy, MessageKey, PreparedPublication,
        PublicationCapabilities, PublicationDispatch, PublicationProvider, PublicationReceipt,
        PublicationRejection, PublicationTarget, PublicationUncertainty,
    },
};
use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};
use serenity::{
    http::{LightMethod, Request, Route},
    model::id::{ChannelId, MessageId},
};
use std::{num::NonZeroU64, sync::Arc, time::Duration};
use thiserror::Error;

const CODEC: &str = "discord.rook.text";
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);

const RESPONSE_LIMIT: usize = 65_536;

/// Failure constructing the private single-attempt HTTP transport.
#[derive(Debug, Error)]
pub enum DiscordPublicationInitError {
    /// The configured module slot is invalid.
    #[error("invalid discord module slot")]
    InvalidSlot,
    /// The HTTP client could not be constructed.
    #[error("failed to construct publication http client")]
    Client(#[source] reqwest::Error),
}

/// Text publication using current Discord policy and one non-retrying POST.
///
/// The caller supplies current configuration for each preparation and final check.
/// Attachments, automatic follow-up messages, and celebrate reactions are unsupported.
/// Only the durable Rook owner should call this trusted provider boundary.
pub struct DiscordPublicationProvider {
    source: ModuleInstanceId,
    context: Arc<dyn Fn() -> MessagingCtx + Send + Sync>,
    client: reqwest::Client,
    #[cfg(test)]
    proxy: Option<String>,
}

impl DiscordPublicationProvider {
    /// Bind a module slot and current-configuration factory without contacting Discord.
    pub fn new(
        slot: &str,
        context: Arc<dyn Fn() -> MessagingCtx + Send + Sync>,
    ) -> Result<Self, DiscordPublicationInitError> {
        let provider =
            ProviderId::new("discord").map_err(|_| DiscordPublicationInitError::InvalidSlot)?;
        let slot = ModuleSlotId::new(slot).map_err(|_| DiscordPublicationInitError::InvalidSlot)?;
        let client = reqwest::Client::builder()
            .use_rustls_tls()
            .retry(reqwest::retry::never())
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .connect_timeout(Duration::from_secs(3))
            .timeout(HTTP_TIMEOUT)
            .build()
            .map_err(DiscordPublicationInitError::Client)?;
        Ok(Self {
            source: ModuleInstanceId::new(provider, slot),
            context,
            client,
            #[cfg(test)]
            proxy: None,
        })
    }

    fn coordinates(
        &self,
        request: &CanonicalPublication,
    ) -> Result<(ChannelId, Option<MessageId>), PublicationRejection> {
        let room = request.target().room();
        if !room.matches_source(&self.source) {
            return Err(PublicationRejection::Unsupported);
        }
        let channel = room.numeric().ok_or(PublicationRejection::InvalidTarget)?;
        let reply = match request.target() {
            PublicationTarget::Room(_) => None,
            PublicationTarget::Reply(message) => Some(MessageId::new(
                message
                    .numeric()
                    .ok_or(PublicationRejection::InvalidTarget)?
                    .get(),
            )),
        };
        Ok((ChannelId::new(channel.get()), reply))
    }

    async fn check(
        &self,
        ctx: &MessagingCtx,
        channel: ChannelId,
        reply: Option<MessageId>,
    ) -> Result<(), PublicationRejection> {
        check_outbound(ctx, channel)
            .await
            .map_err(|error| match error.kind {
                MessagingErrorKind::OutboundDenied(_) => PublicationRejection::AccessDenied,
                MessagingErrorKind::Observation(_)
                | MessagingErrorKind::Target(_)
                | MessagingErrorKind::Pipeline(_)
                | MessagingErrorKind::PipelineTask(_)
                | MessagingErrorKind::HooksNotRegistered
                | MessagingErrorKind::HookHalted { .. }
                | MessagingErrorKind::RedirectUnsupported(_)
                | MessagingErrorKind::CaptionlessHookOverrides
                | MessagingErrorKind::RawSentexLocators
                | MessagingErrorKind::SentexInsideQuoteOrFence
                | MessagingErrorKind::SentexRequiresSingleMessage
                | MessagingErrorKind::SentexCannotBeHeld
                | MessagingErrorKind::DmDisabled
                | MessagingErrorKind::CreateDm(_)
                | MessagingErrorKind::CoordinationUnavailable(_)
                | MessagingErrorKind::CoordinationBusy { .. }
                | MessagingErrorKind::Delivery(_)
                | MessagingErrorKind::EmptyRephrase
                | MessagingErrorKind::Handle(_)
                | MessagingErrorKind::Discord(_)
                | MessagingErrorKind::SendAttachment(_)
                | MessagingErrorKind::ReadFile(_)
                | MessagingErrorKind::FetchMessage(_)
                | MessagingErrorKind::CreateInbox(_)
                | MessagingErrorKind::ConflictingPagination
                | MessagingErrorKind::NoAttachments
                | MessagingErrorKind::RelativeFilePath
                | MessagingErrorKind::FileNotPermitted
                | MessagingErrorKind::ZeroEmojiId(_) => PublicationRejection::AccessUnavailable,
            })?;
        if let Some(reply) = reply {
            let own = ctx.state.read().await.is_own_send(reply.get());
            let mut policy = ctx.config.as_ref().clone();
            // Canary messages would escape this operation's one-message ledger.
            policy.phantom_canary_channel = None;
            verify_message_target(
                &ctx.ingress_ledger,
                &ctx.http,
                &policy,
                reply,
                channel,
                TargetOperation::ReplyTo,
                TargetPolicy::CanonicalReplyOrReact(own),
            )
            .await
            .map_err(|error| match error {
                TargetError::MessageLookup(_) | TargetError::GuildLookup(_) => {
                    PublicationRejection::AccessUnavailable
                }
                TargetError::IngressRequired
                | TargetError::CanonicalRequiresIngress
                | TargetError::CanonicalMismatch
                | TargetError::NotFound { .. }
                | TargetError::ChannelMismatch { .. } => PublicationRejection::InvalidTarget,
            })?;
        }
        Ok(())
    }

    async fn prepare_text(
        &self,
        request: &CanonicalPublication,
    ) -> Result<PreparedPublication, PublicationRejection> {
        let (channel, reply) = self.coordinates(request)?;
        let ctx = (self.context)();
        self.check(&ctx, channel, reply).await?;
        let prepared = prepare_outbound(
            &ctx,
            OutboundDraft::channel(
                channel,
                request.text(),
                reply,
                PreSendOptions {
                    surface: OutboundSurface::Reply,
                    bypasses: &[],
                },
            ),
        )
        .await
        .map_err(|_| PublicationRejection::Policy)?;
        if prepared.channel_id() != Some(channel) || prepared.reply_to != reply {
            return Err(PublicationRejection::InvalidTarget);
        }
        if prepared.text.is_empty() || prepared.text.encode_utf16().count() > 2000 {
            return Err(PublicationRejection::TooLarge);
        }
        if let Some(judge) = &ctx.config.contradictionary
            && let Verdict::Bounce(reason) = judge.judge(&prepared.text)
        {
            write_diary(
                ctx.state_dir.clone(),
                vec![DiaryRecord::held_now(&reason.to_string(), &prepared.text)],
            )
            .await;
            return Err(PublicationRejection::Policy);
        }
        let records = match &prepared.auto {
            Some(auto) => auto_send_records(&ctx, &auto.rewrites, &auto.written, &prepared.text),
            None => match &ctx.config.contradictionary {
                Some(judge) => {
                    send_side_records(judge, &judge.check(&prepared.text), &prepared.text)
                }
                None => Vec::new(),
            },
        };
        let wire = WireMessage::new(channel, reply, prepared.text, request.mentions())?;
        let payload = serde_json::to_vec(&PreparedText {
            generation: ctx.config.generation(),
            wire,
            diary: records.into_iter().map(DiaryEntry::from).collect(),
        })
        .map_err(|_| PublicationRejection::InvalidPreparation)?;
        PreparedPublication::new(request.clone(), CODEC.into(), 1, payload)
            .map_err(|_| PublicationRejection::InvalidPreparation)
    }

    async fn dispatch_text(&self, prepared: &PreparedPublication) -> PublicationDispatch {
        let not_sent = |reason| PublicationDispatch::new(DispatchOutcome::NotSent(reason));
        let invalid = || not_sent(PublicationRejection::InvalidPreparation);
        if prepared.codec() != CODEC || prepared.version() != 1 {
            return invalid();
        }
        let mut decoder = serde_json::Deserializer::from_slice(prepared.payload());
        let Ok(payload) = serde_path_to_error::deserialize::<_, PreparedText>(&mut decoder) else {
            return invalid();
        };
        if decoder.end().is_err() {
            return invalid();
        }
        let Ok((channel, reply)) = self.coordinates(prepared.request()) else {
            return invalid();
        };
        let Ok(expected) = WireMessage::new(
            channel,
            reply,
            payload.wire.content.clone(),
            prepared.request().mentions(),
        ) else {
            return invalid();
        };
        if payload.wire != expected
            || payload.wire.content.is_empty()
            || payload.wire.content.encode_utf16().count() > 2000
        {
            return invalid();
        }
        let Ok(body) = serde_json::to_vec(&payload.wire) else {
            return invalid();
        };
        let ctx = (self.context)();
        if ctx.config.generation() != payload.generation {
            return not_sent(PublicationRejection::AuthorityChanged);
        }
        if let Err(reason) = self.check(&ctx, channel, reply).await {
            return not_sent(reason);
        }
        if let Some(judge) = &ctx.config.contradictionary
            && matches!(judge.judge(&payload.wire.content), Verdict::Bounce(_))
        {
            return not_sent(PublicationRejection::Policy);
        }
        let coordinator = match reply {
            Some(_) => channel_coordinator(&ctx, channel)
                .await
                .map(|(_, coordinator)| {
                    let mut config = coordinator.config().clone();
                    config.connect_timeout_ms = config.connect_timeout_ms.min(1000);
                    Coordinator::shared(config, coordinator.bot_id())
                }),
            None => None,
        };
        let reference = reply.map(|id| id.get().to_string());
        let mut claimed = false;
        if let (Some(coordinator), Some(reference)) = (&coordinator, reference.as_deref()) {
            let attempt = coordinator
                .claim_with_progress(&channel.get().to_string(), reference)
                .await;
            if attempt.progress == ClaimProgress::WriteUncertain {
                return not_sent(PublicationRejection::CoordinationUncertain);
            }
            match attempt.outcome {
                ClaimOutcome::Proceed { .. } => claimed = true,
                ClaimOutcome::Unavailable { claim_sent, .. } if coordinator.config().fail_open => {
                    claimed = claim_sent
                }
                ClaimOutcome::Unavailable {
                    claim_sent: true, ..
                } => {
                    return not_sent(if coordinator.release_checked(reference).await.is_ok() {
                        PublicationRejection::Coordination
                    } else {
                        PublicationRejection::CoordinationUncertain
                    });
                }
                ClaimOutcome::Wait { .. }
                | ClaimOutcome::Unavailable {
                    claim_sent: false, ..
                } => {
                    return not_sent(PublicationRejection::Coordination);
                }
            }
        }
        let current = (self.context)();
        let mut final_check = if current.config.generation() != payload.generation {
            Err(PublicationRejection::AuthorityChanged)
        } else {
            self.check(&current, channel, reply).await
        };
        if (self.context)().config.generation() != payload.generation {
            final_check = Err(PublicationRejection::AuthorityChanged);
        }
        if let Err(reason) = final_check {
            if claimed
                && let (Some(coordinator), Some(reference)) = (&coordinator, reference.as_deref())
                && coordinator.release_checked(reference).await.is_err()
            {
                return not_sent(PublicationRejection::CoordinationUncertain);
            }
            return not_sent(reason);
        }
        tracing::debug!(channel = %channel, "dispatching single rook message");
        let outcome = post_once(
            &self.client,
            current.http.token(),
            self.test_proxy(),
            channel,
            body,
        )
        .await;
        match outcome {
            NativeOutcome::Published(id) => {
                let room = ConversationRef::new(
                    self.source.clone(),
                    ConversationId::numeric(channel.into()),
                );
                let receipt = PublicationReceipt {
                    message: MessageKey::from(&MessageRef::new(
                        room,
                        crate::core::MessageId::numeric(id),
                    )),
                };
                PublicationDispatch::with_after_commit(
                    DispatchOutcome::Published(receipt),
                    Box::pin(async move {
                        current.state.write().await.note_sent(id.get());
                        if claimed
                            && let (Some(coordinator), Some(reference)) =
                                (&coordinator, reference.as_deref())
                        {
                            coordinator.done(reference, &id.get().to_string()).await;
                        }
                        write_diary(
                            current.state_dir,
                            payload.diary.into_iter().map(DiaryRecord::from).collect(),
                        )
                        .await;
                    }),
                )
            }
            NativeOutcome::Refused => PublicationDispatch::with_after_commit(
                DispatchOutcome::Refused(PublicationRejection::ProviderRefused),
                Box::pin(async move {
                    if claimed
                        && let (Some(coordinator), Some(reference)) =
                            (&coordinator, reference.as_deref())
                        && coordinator.release_checked(reference).await.is_err()
                    {
                        tracing::warn!(channel = %channel, reference, "rook refused message left coordinator cleanup unresolved");
                    }
                }),
            ),
            NativeOutcome::NotSent => {
                if claimed
                    && let (Some(coordinator), Some(reference)) =
                        (&coordinator, reference.as_deref())
                    && coordinator.release_checked(reference).await.is_err()
                {
                    return not_sent(PublicationRejection::CoordinationUncertain);
                }
                not_sent(PublicationRejection::InvalidPreparation)
            }
            NativeOutcome::Unknown(reason) => {
                PublicationDispatch::new(DispatchOutcome::Unknown(reason))
            }
        }
    }

    fn test_proxy(&self) -> Option<&str> {
        #[cfg(test)]
        {
            self.proxy.as_deref()
        }
        #[cfg(not(test))]
        {
            None
        }
    }
}

impl PublicationProvider for DiscordPublicationProvider {
    fn capabilities(&self) -> PublicationCapabilities {
        PublicationCapabilities {
            text: true,
            max_text_bytes: 32_768,
            attachments: false,
        }
    }
    fn prepare<'a>(
        &'a self,
        request: &'a CanonicalPublication,
    ) -> BoxFuture<'a, Result<PreparedPublication, PublicationRejection>> {
        Box::pin(self.prepare_text(request))
    }
    fn dispatch<'a>(
        &'a self,
        prepared: &'a PreparedPublication,
    ) -> BoxFuture<'a, PublicationDispatch> {
        Box::pin(self.dispatch_text(prepared))
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PreparedText {
    generation: u64,
    wire: WireMessage,
    diary: Vec<DiaryEntry>,
}

#[derive(Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct WireMessage {
    content: String,
    allowed_mentions: AllowedMentions,
    #[serde(skip_serializing_if = "Option::is_none")]
    message_reference: Option<ReplyReference>,
}
#[derive(Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct AllowedMentions {
    parse: Vec<String>,
    users: Vec<String>,
    roles: Vec<String>,
    replied_user: bool,
}
#[derive(Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct ReplyReference {
    message_id: String,
    channel_id: String,
    fail_if_not_exists: bool,
}
impl WireMessage {
    fn new(
        channel: ChannelId,
        reply: Option<MessageId>,
        content: String,
        mentions: &MentionPolicy,
    ) -> Result<Self, PublicationRejection> {
        let (users, replied_user) = match mentions {
            MentionPolicy::SuppressAll => (Vec::new(), false),
            MentionPolicy::ExplicitUsers {
                users,
                reply_author,
            } => (
                users
                    .iter()
                    .map(|actor| {
                        actor
                            .numeric()
                            .map(|id| id.get().to_string())
                            .ok_or(PublicationRejection::Unsupported)
                    })
                    .collect::<Result<Vec<_>, _>>()?,
                *reply_author,
            ),
        };
        Ok(Self {
            content,
            allowed_mentions: AllowedMentions {
                parse: Vec::new(),
                users,
                roles: Vec::new(),
                replied_user,
            },
            message_reference: reply.map(|id| ReplyReference {
                message_id: id.get().to_string(),
                channel_id: channel.get().to_string(),
                fail_if_not_exists: true,
            }),
        })
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DiaryEntry {
    timestamp: crate::timestamp::Timestamp,
    pattern: String,
    message: String,
    action: Action,
    overridden: bool,
    sent: Option<String>,
    matched: Option<String>,
    replacement: Option<String>,
}
impl From<DiaryRecord> for DiaryEntry {
    fn from(record: DiaryRecord) -> Self {
        Self {
            timestamp: record.timestamp,
            pattern: record.pattern,
            message: record.message,
            action: record.action,
            overridden: record.overridden,
            sent: record.sent,
            matched: record.matched,
            replacement: record.replacement,
        }
    }
}
impl From<DiaryEntry> for DiaryRecord {
    fn from(record: DiaryEntry) -> Self {
        Self {
            timestamp: record.timestamp,
            pattern: record.pattern,
            message: record.message,
            action: record.action,
            overridden: record.overridden,
            sent: record.sent,
            matched: record.matched,
            replacement: record.replacement,
        }
    }
}
async fn write_diary(directory: camino::Utf8PathBuf, records: Vec<DiaryRecord>) {
    if records.is_empty() {
        return;
    }
    if let Err(error) = tokio::task::spawn_blocking(move || {
        for record in records {
            if let Err(error) = append_diary_record(directory.as_std_path(), &record) {
                tracing::warn!(%error, "rook publication diary append failed");
            }
        }
    })
    .await
    {
        tracing::warn!(%error, "rook publication diary worker failed");
    }
}

enum NativeOutcome {
    NotSent,
    Published(NonZeroU64),
    Refused,
    Unknown(PublicationUncertainty),
}
#[derive(Deserialize)]
struct NativeReceipt {
    id: String,
    channel_id: String,
}
async fn post_once(
    client: &reqwest::Client,
    token: &str,
    proxy: Option<&str>,
    channel: ChannelId,
    body: Vec<u8>,
) -> NativeOutcome {
    let request = Request::new(
        Route::ChannelMessages {
            channel_id: channel,
        },
        LightMethod::Post,
    )
    .body(Some(body));
    let request = match request.build(client, token, proxy) {
        Ok(builder) => builder,
        Err(_) => return NativeOutcome::NotSent,
    };
    let request = match request.build() {
        Ok(request) => request,
        Err(_) => return NativeOutcome::NotSent,
    };
    let mut response = match client.execute(request).await {
        Ok(response) => response,
        Err(_) => return NativeOutcome::Unknown(PublicationUncertainty::Transport),
    };
    if !response.status().is_success() {
        return if matches!(
            response.status().as_u16(),
            400 | 401 | 403 | 404 | 405 | 413 | 429
        ) {
            NativeOutcome::Refused
        } else {
            NativeOutcome::Unknown(PublicationUncertainty::Transport)
        };
    }
    let mut bytes = Vec::new();
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) if bytes.len().saturating_add(chunk.len()) <= RESPONSE_LIMIT => {
                bytes.extend_from_slice(&chunk)
            }
            Ok(Some(_)) => return NativeOutcome::Unknown(PublicationUncertainty::InvalidReceipt),
            Ok(None) => break,
            Err(_) => return NativeOutcome::Unknown(PublicationUncertainty::Transport),
        }
    }
    let mut decoder = serde_json::Deserializer::from_slice(&bytes);
    let Ok(receipt) = serde_path_to_error::deserialize::<_, NativeReceipt>(&mut decoder) else {
        return NativeOutcome::Unknown(PublicationUncertainty::InvalidReceipt);
    };
    if decoder.end().is_err() || receipt.channel_id != channel.get().to_string() {
        return NativeOutcome::Unknown(PublicationUncertainty::InvalidReceipt);
    }
    match receipt.id.parse::<NonZeroU64>() {
        Ok(id) if id.get().to_string() == receipt.id => NativeOutcome::Published(id),
        _ => NativeOutcome::Unknown(PublicationUncertainty::InvalidReceipt),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::{ChannelConfig, Config, LoadedConfig, ReplyToMode},
        ingress_ledger::IngressLedger,
        no_rly::consent::ConsentGate,
    };
    use serde_json::{Value, json};
    use serenity::http::Http;
    use std::sync::Mutex;
    use tokio::{
        io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
        net::TcpListener,
    };

    struct Fixture {
        provider: DiscordPublicationProvider,
        config: Arc<Mutex<LoadedConfig>>,
        requests: Arc<Mutex<Vec<Value>>>,
        state: crate::state::State,
        server: tokio::task::JoinHandle<()>,
        _directory: tempfile::TempDir,
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            self.server.abort();
        }
    }
    impl Fixture {
        async fn new(
            status: &'static str,
            headers: &'static str,
            response: impl Into<String>,
        ) -> Self {
            let response = response.into();
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let requests = Arc::new(Mutex::new(Vec::new()));
            let seen = requests.clone();
            let server = tokio::spawn(async move {
                loop {
                    let (socket, _) = listener.accept().await.unwrap();
                    let mut reader = BufReader::new(socket);
                    let mut request = String::new();
                    reader.read_line(&mut request).await.unwrap();
                    let mut length = 0;
                    loop {
                        let mut header = String::new();
                        reader.read_line(&mut header).await.unwrap();
                        if header == "\r\n" {
                            break;
                        }
                        if let Some(value) =
                            header.to_ascii_lowercase().strip_prefix("content-length:")
                        {
                            length = value.trim().parse::<usize>().unwrap();
                        }
                    }
                    let mut body = vec![0; length];
                    reader.read_exact(&mut body).await.unwrap();
                    seen.lock().unwrap().push(json!({"request":request, "body":serde_json::from_slice::<Value>(&body).unwrap()}));
                    if status == "drop" {
                        continue;
                    }
                    let reply = format!(
                        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n{response}",
                        response.len()
                    );
                    let _ = reader.get_mut().write_all(reply.as_bytes()).await;
                }
            });
            let mut raw = Config::default();
            raw.channels.push(ChannelConfig {
                id: "10".into(),
                ..Default::default()
            });
            raw.delivery.reply_to_mode = ReplyToMode::Off;
            let config = Arc::new(Mutex::new(LoadedConfig::from_raw(raw)));
            let current = config.clone();
            let state = crate::state::new_state();
            let shared_state = state.clone();
            let directory = tempfile::TempDir::new().unwrap();
            let path = camino::Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).unwrap();
            let ledger = Arc::new(IngressLedger::new());
            ledger.note_admitted(
                MessageId::new(30),
                ChannelId::new(10),
                serenity::model::id::UserId::new(20),
                "event",
            );
            let consent = Arc::new(ConsentGate::new(&path));
            let http = Arc::new(Http::new("fixture"));
            let mut provider = DiscordPublicationProvider::new(
                "primary",
                Arc::new(move || {
                    MessagingCtx::new(
                        http.clone(),
                        shared_state.clone(),
                        Arc::new(current.lock().unwrap().clone()),
                        path.clone(),
                        consent.clone(),
                        ledger.clone(),
                    )
                }),
            )
            .unwrap();
            provider.proxy = Some(format!("http://{address}"));
            Self {
                provider,
                config,
                requests,
                state,
                server,
                _directory: directory,
            }
        }
        fn request(&self, text: &str) -> CanonicalPublication {
            let message = MessageKey::from(&MessageRef::new(
                ConversationRef::new(
                    self.provider.source.clone(),
                    ConversationId::numeric(NonZeroU64::new(10).unwrap()),
                ),
                crate::core::MessageId::numeric(NonZeroU64::new(30).unwrap()),
            ));
            serde_json::from_value(json!({"version":1, "event":{"message":message,"occurrence":{"kind":"created"}},"target":{"kind":"reply","target":message}, "text":text, "mentions":{"kind":"suppress_all"}})).unwrap()
        }
        async fn send(&self) -> DispatchOutcome {
            let prepared = self
                .provider
                .prepare(&self.request("hello <@20> @everyone <@&40>"))
                .await
                .unwrap();
            self.dispatch(&prepared).await
        }
        async fn dispatch(&self, prepared: &PreparedPublication) -> DispatchOutcome {
            let (outcome, completion) = self.provider.dispatch(prepared).await.into_parts();
            if let Some(completion) = completion {
                completion.await;
            }
            outcome
        }
    }

    #[tokio::test]
    async fn exact_reply_suppresses_mentions_even_when_legacy_reply_mode_is_off() {
        let fixture = Fixture::new("200 OK", "", r#"{"id":"31","channel_id":"10"}"#).await;
        let DispatchOutcome::Published(receipt) = fixture.send().await else {
            panic!("published receipt")
        };
        assert_eq!(receipt.message.numeric().unwrap().get(), 31);
        assert!(fixture.state.read().await.is_own_send(31));
        let requests = fixture.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert!(
            requests[0]["request"]
                .as_str()
                .unwrap()
                .starts_with("POST ")
        );
        assert_eq!(
            requests[0]["body"]["message_reference"],
            json!({"channel_id":"10","message_id":"30","fail_if_not_exists":true})
        );
        assert_eq!(
            requests[0]["body"]["allowed_mentions"],
            json!({"parse":[],"users":[],"roles":[],"replied_user":false})
        );
    }

    #[tokio::test]
    async fn rate_headers_and_redirects_never_repeat_the_post() {
        for (status, headers, refused) in [
            (
                "429 Too Many Requests",
                "X-RateLimit-Remaining: invalid\r\nRetry-After: 0\r\n",
                true,
            ),
            ("403 Forbidden", "", true),
            (
                "302 Found",
                "Location: https://example.invalid/retry\r\n",
                false,
            ),
            ("503 Service Unavailable", "", false),
        ] {
            let fixture = Fixture::new(
                status,
                headers,
                r#"{"code":50001,"message":"fixture refusal"}"#,
            )
            .await;
            let outcome = fixture.send().await;
            assert_eq!(matches!(outcome, DispatchOutcome::Refused(_)), refused);
            if !refused {
                assert!(matches!(outcome, DispatchOutcome::Unknown(_)));
            }
            assert_eq!(fixture.requests.lock().unwrap().len(), 1);
        }
        let fixture = Fixture::new(
            "200 OK",
            "X-RateLimit-Remaining: invalid\r\n",
            r#"{"id":"31","channel_id":"10"}"#,
        )
        .await;
        assert!(matches!(
            fixture.send().await,
            DispatchOutcome::Published(_)
        ));
        assert_eq!(fixture.requests.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn lost_or_malformed_success_is_unknown_without_another_post() {
        for (status, body) in [
            ("drop", ""),
            ("200 OK", "not json"),
            ("200 OK", r#"{"id":"31","channel_id":"11"}"#),
            ("200 OK", r#"{"id":"0","channel_id":"10"}"#),
        ] {
            let fixture = Fixture::new(status, "", body).await;
            assert!(matches!(fixture.send().await, DispatchOutcome::Unknown(_)));
            assert_eq!(fixture.requests.lock().unwrap().len(), 1);
        }
    }

    #[tokio::test]
    async fn changed_native_policy_and_oversized_text_never_post() {
        let fixture = Fixture::new("200 OK", "", r#"{"id":"31","channel_id":"10"}"#).await;
        let prepared = fixture
            .provider
            .prepare(&fixture.request("hello"))
            .await
            .unwrap();
        *fixture.config.lock().unwrap() = LoadedConfig::from_raw(Config::default());
        assert!(matches!(
            fixture.dispatch(&prepared).await,
            DispatchOutcome::NotSent(PublicationRejection::AuthorityChanged)
        ));
        assert!(fixture.requests.lock().unwrap().is_empty());
        let fixture = Fixture::new("200 OK", "", r#"{"id":"31","channel_id":"10"}"#).await;
        assert_eq!(
            fixture
                .provider
                .prepare(&fixture.request(&"x".repeat(2001)))
                .await
                .unwrap_err(),
            PublicationRejection::TooLarge
        );
        assert!(fixture.requests.lock().unwrap().is_empty());
        assert!(matches!(
            fixture.send().await,
            DispatchOutcome::Published(_)
        ));
    }

    mod policy_tests {
        use super::Fixture;
        use super::*;
        use crate::{
            contradictionary::{Entry, MatchMode},
            pre_send::{
                AuditTrail, ConstructFeedback, HookContext, HookDecision, HookName, HookOutput,
                PipelineMode, PreSendHook, PreSendPipeline,
            },
        };
        use serde_json::json;

        struct DecisionHook(HookDecision);
        impl PreSendHook for DecisionHook {
            fn name(&self) -> HookName {
                HookName::parse("rook-fixture").unwrap()
            }
            fn execute(&self, _: &HookContext) -> HookOutput {
                HookOutput::new(
                    self.0.clone(),
                    ConstructFeedback::default(),
                    AuditTrail::default(),
                )
            }
        }

        #[tokio::test]
        async fn configured_hook_halt_and_redirect_never_escape_the_publication_slot() {
            for decision in [
                HookDecision::Halt {
                    reason: "fixture halt".into(),
                },
                HookDecision::Redirect {
                    channel_id: ChannelId::new(11),
                },
            ] {
                let mut fixture =
                    Fixture::new("200 OK", "", r#"{"id":"31","channel_id":"10"}"#).await;
                let original = fixture.provider.context.clone();
                let pipeline = Arc::new(
                    PreSendPipeline::new(vec![Box::new(DecisionHook(decision))])
                        .unwrap()
                        .with_mode(PipelineMode::Enforce),
                );
                fixture.provider.context =
                    Arc::new(move || original().with_pre_send_pipeline(pipeline.clone()));
                assert!(
                    fixture
                        .provider
                        .prepare(&fixture.request("hello"))
                        .await
                        .is_err()
                );
                assert!(fixture.requests.lock().unwrap().is_empty());
            }
        }

        #[tokio::test]
        async fn rewrites_are_persisted_and_blocked_content_creates_no_held_handle() {
            let fixture = Fixture::new("200 OK", "", r#"{"id":"31","channel_id":"10"}"#).await;
            let mut raw = fixture.config.lock().unwrap().raw.clone();
            raw.contradictionary.enabled = true;
            raw.contradictionary.entries = vec![
                Entry {
                    pattern: "utilize".into(),
                    action: Action::Auto,
                    match_mode: MatchMode::Word,
                    replace: Some("use".into()),
                    reason: None,
                },
                Entry {
                    pattern: "blocked".into(),
                    action: Action::Block,
                    match_mode: MatchMode::Word,
                    replace: None,
                    reason: Some("fixture".into()),
                },
            ];
            *fixture.config.lock().unwrap() = crate::config::LoadedConfig::from_raw(raw);
            assert_eq!(
                fixture
                    .provider
                    .prepare(&fixture.request("blocked"))
                    .await
                    .unwrap_err(),
                PublicationRejection::Policy
            );
            assert_eq!((fixture.provider.context)().no_rly.pending().await, 0);
            assert!(fixture.requests.lock().unwrap().is_empty());
            let prepared = fixture
                .provider
                .prepare(&fixture.request("we utilize it"))
                .await
                .unwrap();
            assert_eq!(prepared.request().text(), "we utilize it");
            let payload: PreparedText = serde_json::from_slice(prepared.payload()).unwrap();
            assert_eq!(payload.wire.content, "we use it");
            assert!(!payload.diary.is_empty());
            assert!(matches!(
                fixture.dispatch(&prepared).await,
                DispatchOutcome::Published(_)
            ));
            assert_eq!(
                fixture.requests.lock().unwrap()[0]["body"]["content"],
                "we use it"
            );
        }

        #[tokio::test]
        async fn deliberate_mentions_and_room_publication_are_separate_controls() {
            let fixture = Fixture::new("200 OK", "", r#"{"id":"31","channel_id":"10"}"#).await;
            let request = fixture.request("hello <@20>");
            let mut value = serde_json::to_value(&request).unwrap();
            value["mentions"] = json!({"kind":"explicit_users", "users":[{"source":{"provider":"discord","slot":"primary"},"local":{"kind":"numeric","value":20}}],"reply_author":true});
            let request: CanonicalPublication = serde_json::from_value(value).unwrap();
            let prepared = fixture.provider.prepare(&request).await.unwrap();
            assert!(matches!(
                fixture.dispatch(&prepared).await,
                DispatchOutcome::Published(_)
            ));
            assert_eq!(
                fixture.requests.lock().unwrap()[0]["body"]["allowed_mentions"],
                json!({"parse":[],"users":["20"],"roles":[],"replied_user":true})
            );
            let mut value = serde_json::to_value(fixture.request("room message")).unwrap();
            value["target"] = json!({"kind":"room", "target":request.target().room()});
            let request = serde_json::from_value(value).unwrap();
            let prepared = fixture.provider.prepare(&request).await.unwrap();
            assert!(matches!(
                fixture.dispatch(&prepared).await,
                DispatchOutcome::Published(_)
            ));
            assert!(
                fixture.requests.lock().unwrap()[1]["body"]
                    .get("message_reference")
                    .is_none()
            );
        }
    }

    mod coordination_tests {
        use super::*;
        use crate::coordination::CoordinationConfig;
        use serenity::model::id::UserId;

        #[derive(Clone, Copy)]
        enum ClaimReply {
            Proceed,
            Malformed,
            Delayed,
        }
        struct CoordinatorFixture {
            address: String,
            messages: Arc<Mutex<Vec<String>>>,
            server: tokio::task::JoinHandle<()>,
        }
        impl Drop for CoordinatorFixture {
            fn drop(&mut self) {
                self.server.abort();
            }
        }
        impl CoordinatorFixture {
            async fn new(claim: ClaimReply, cleanup_ok: bool) -> Self {
                let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
                let address = listener.local_addr().unwrap().to_string();
                let messages = Arc::new(Mutex::new(Vec::new()));
                let log = messages.clone();
                let server = tokio::spawn(async move {
                    loop {
                        let (socket, _) = listener.accept().await.unwrap();
                        let mut reader = BufReader::new(socket);
                        loop {
                            let mut line = String::new();
                            if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                                break;
                            }
                            let request: Value = serde_json::from_str(&line).unwrap();
                            let method = request["msg"].as_str().unwrap();
                            log.lock().unwrap().push(method.to_owned());
                            let response = match method {
                                "hello" => {
                                    if matches!(claim, ClaimReply::Delayed) {
                                        tokio::time::sleep(Duration::from_millis(1500)).await;
                                    }
                                    json!({"ok":true})
                                }
                                "claim" => match claim {
                                    ClaimReply::Malformed => json!({"malformed":true}),
                                    ClaimReply::Proceed => {
                                        json!({"status":"proceed","lease_ms":10_000})
                                    }
                                    ClaimReply::Delayed => {
                                        tokio::time::sleep(Duration::from_millis(1600)).await;
                                        json!({"status":"proceed","lease_ms":10_000})
                                    }
                                },
                                "done" | "release" => json!({"ok":cleanup_ok}),
                                _ => panic!("unexpected method"),
                            };
                            if reader
                                .get_mut()
                                .write_all(format!("{response}\n").as_bytes())
                                .await
                                .is_err()
                            {
                                break;
                            }
                        }
                    }
                });
                Self {
                    address,
                    messages,
                    server,
                }
            }
            fn configure(&self, fixture: &Fixture, fail_open: bool) {
                let mut raw = fixture.config.lock().unwrap().raw.clone();
                raw.channels[0].coordinate = Some("rook-coordinator".into());
                raw.pre_send.author_id = Some(UserId::new(50));
                raw.coordination.insert(
                    "rook-coordinator".into(),
                    CoordinationConfig {
                        addr: self.address.clone(),
                        lease_ms: 10_000,
                        connect_timeout_ms: 1000,
                        fail_open,
                    },
                );
                *fixture.config.lock().unwrap() = LoadedConfig::from_raw(raw);
            }
            fn count(&self, method: &str) -> usize {
                self.messages
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|name| name.as_str() == method)
                    .count()
            }
        }

        #[tokio::test]
        async fn fail_closed_completed_claim_write_requires_acknowledged_release() {
            for cleanup_ok in [true, false] {
                let coordinator = CoordinatorFixture::new(ClaimReply::Malformed, cleanup_ok).await;
                let fixture = Fixture::new("200 OK", "", r#"{"id":"31","channel_id":"10"}"#).await;
                coordinator.configure(&fixture, false);
                let expected = if cleanup_ok {
                    PublicationRejection::Coordination
                } else {
                    PublicationRejection::CoordinationUncertain
                };
                assert!(
                    matches!(fixture.send().await, DispatchOutcome::NotSent(reason) if reason == expected)
                );
                assert_eq!(coordinator.count("claim"), 1);
                assert_eq!(coordinator.count("release"), 1);
                assert_eq!(coordinator.count("done"), 0);
                assert!(fixture.requests.lock().unwrap().is_empty());
            }
        }

        #[tokio::test]
        async fn fail_open_unanswered_claim_publishes_once_and_reports_done() {
            let coordinator = CoordinatorFixture::new(ClaimReply::Malformed, true).await;
            let fixture = Fixture::new("200 OK", "", r#"{"id":"31","channel_id":"10"}"#).await;
            coordinator.configure(&fixture, true);
            assert!(matches!(
                fixture.send().await,
                DispatchOutcome::Published(_)
            ));
            assert_eq!(coordinator.count("claim"), 1);
            assert_eq!(coordinator.count("done"), 1);
            assert_eq!(coordinator.count("release"), 0);
            assert_eq!(fixture.requests.lock().unwrap().len(), 1);
        }

        #[tokio::test]
        async fn delayed_hello_and_claim_preserve_acquisition_past_three_seconds() {
            let coordinator = CoordinatorFixture::new(ClaimReply::Delayed, true).await;
            let fixture = Fixture::new("200 OK", "", r#"{"id":"31","channel_id":"10"}"#).await;
            coordinator.configure(&fixture, false);
            assert!(matches!(
                fixture.send().await,
                DispatchOutcome::Published(_)
            ));
            assert_eq!(coordinator.count("claim"), 1);
            assert_eq!(coordinator.count("done"), 1);
            assert_eq!(fixture.requests.lock().unwrap().len(), 1);
        }

        #[tokio::test]
        async fn authoritative_receipt_precedes_blocked_or_failed_bookkeeping() {
            let coordinator = CoordinatorFixture::new(ClaimReply::Proceed, false).await;
            let fixture = Fixture::new("200 OK", "", r#"{"id":"31","channel_id":"10"}"#).await;
            coordinator.configure(&fixture, false);
            let prepared = fixture
                .provider
                .prepare(&fixture.request("hello"))
                .await
                .unwrap();
            let (outcome, completion) = fixture.provider.dispatch(&prepared).await.into_parts();
            assert!(
                matches!(&outcome, DispatchOutcome::Published(receipt) if receipt.message.numeric().unwrap().get() == 31)
            );
            assert_eq!(coordinator.count("done"), 0);
            assert!(!fixture.state.read().await.is_own_send(31));
            let lock = fixture.state.write().await;
            let mut completion = completion.unwrap();
            assert!(
                tokio::time::timeout(Duration::from_millis(20), &mut completion)
                    .await
                    .is_err()
            );
            assert!(matches!(&outcome, DispatchOutcome::Published(_)));
            drop(lock);
            completion.await;
            assert!(matches!(&outcome, DispatchOutcome::Published(_)));
            assert!(fixture.state.read().await.is_own_send(31));
            assert_eq!(coordinator.count("done"), 1);
            assert_eq!(fixture.requests.lock().unwrap().len(), 1);
        }

        #[tokio::test]
        async fn oversized_success_is_unknown_and_error_bodies_are_not_decoded() {
            for (status, refused) in [
                ("200 OK", false),
                ("403 Forbidden", true),
                ("503 Service Unavailable", false),
            ] {
                let fixture = Fixture::new(status, "", "x".repeat(RESPONSE_LIMIT * 16)).await;
                let outcome = fixture.send().await;
                if refused {
                    assert!(matches!(outcome, DispatchOutcome::Refused(_)));
                } else {
                    assert!(matches!(outcome, DispatchOutcome::Unknown(_)));
                }
                assert_eq!(fixture.requests.lock().unwrap().len(), 1);
            }
        }
    }
}
