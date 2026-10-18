//! Authorized Discord reads, message observations, and attachment downloads.

use super::{MessagingCtx, MessagingError, MessagingErrorKind, check_outbound};
use crate::{
    config::LoadedConfig,
    core::{
        self, ModuleInstanceId,
        context::{
            AttachmentSummary, Availability, ContextMetadata, ContextObservation, HistoryDirection,
            HistoryQuery, ObservationExtent, ObservationOrigin, PageMetadata, ReferenceConsistency,
            ReferenceObservation, ReferenceRelation, ResolvedReference, SenderObservation,
            UnavailableReason,
        },
    },
    evidence::{OfferedSentex, parse_offered_sentexes},
    timestamp::Timestamp,
    vaelii::{GetMessageReceipt, VaeliiError},
};
use camino::{Utf8Path, Utf8PathBuf};
use chrono::Utc;
use serenity::{
    http::MessagePagination,
    model::{
        channel::{Attachment, Message, MessageReference, MessageReferenceKind, MessageType},
        id::{ApplicationId, ChannelId, GuildId, MessageId, UserId, WebhookId},
    },
};
use std::io;
use thiserror::Error;
use tokio::io::AsyncWriteExt;

/// A provider observation that cannot satisfy a requested read.
#[derive(Debug, Error)]
#[error("invalid Discord observation for channel {channel_id}, message {message_id}: {kind}")]
pub struct ObservationError {
    /// Channel reported by the provider.
    pub channel_id: ChannelId,
    /// Message reported by the provider.
    pub message_id: MessageId,
    /// The failed observation constraint.
    #[source]
    pub kind: ObservationErrorKind,
}

/// Invalid identity or timestamp in a provider observation.
#[derive(Debug, Error)]
pub enum ObservationErrorKind {
    /// The exact read returned a different target.
    #[error("expected channel {channel_id}, message {message_id}")]
    ExactTarget {
        /// Requested channel.
        channel_id: ChannelId,
        /// Requested message.
        message_id: MessageId,
    },
    /// A page contains a message from another channel.
    #[error("expected page channel {0}")]
    PageChannel(ChannelId),
    /// The provider timestamp cannot be represented as RFC 3339.
    #[error("timestamp cannot be formatted as RFC 3339")]
    TimestampFormat,
    /// The formatted timestamp cannot be parsed.
    #[error("timestamp cannot be parsed: {0}")]
    TimestampParse(#[source] chrono::ParseError),
    /// An edited timestamp cannot be represented as RFC 3339.
    #[error("edited timestamp cannot be formatted as RFC 3339")]
    EditedTimestampFormat,
    /// A formatted edited timestamp cannot be parsed.
    #[error("edited timestamp cannot be parsed: {0}")]
    EditedTimestampParse(#[source] chrono::ParseError),
}

fn observation_error(message: &Message, kind: ObservationErrorKind) -> ObservationError {
    ObservationError {
        channel_id: message.channel_id,
        message_id: message.id,
        kind,
    }
}

fn check_exact_target(
    message: &Message,
    channel_id: ChannelId,
    message_id: MessageId,
) -> Result<(), ObservationError> {
    if message.channel_id != channel_id || message.id != message_id {
        return Err(observation_error(
            message,
            ObservationErrorKind::ExactTarget {
                channel_id,
                message_id,
            },
        ));
    }
    Ok(())
}

fn check_page_channel(messages: &[Message], channel_id: ChannelId) -> Result<(), ObservationError> {
    for message in messages {
        if message.channel_id != channel_id {
            return Err(observation_error(
                message,
                ObservationErrorKind::PageChannel(channel_id),
            ));
        }
    }
    Ok(())
}

/// An attachment size measured in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AttachmentBytes(u32);

impl AttachmentBytes {
    /// Returns the byte count reported by Discord.
    pub fn get(self) -> u32 {
        self.0
    }
}

/// An attachment observed on a Discord message.
#[derive(Debug)]
pub struct MessageAttachment {
    name: String,
    url: String,
    size: AttachmentBytes,
    content_type: Option<String>,
}

impl MessageAttachment {
    /// Returns the original filename.
    pub fn name(&self) -> &str {
        &self.name
    }
    /// Returns the URL supplied by Discord.
    pub fn url(&self) -> &str {
        &self.url
    }
    /// Returns the attachment size.
    pub fn size(&self) -> AttachmentBytes {
        self.size
    }
    /// Returns the media type when Discord supplies one.
    pub fn content_type(&self) -> Option<&str> {
        self.content_type.as_deref()
    }
}

/// Message content and metadata observed from Discord.
#[derive(Debug)]
pub struct MessageView {
    id: MessageId,
    author_id: UserId,
    author_name: String,
    content: String,
    timestamp: Timestamp,
    attachments: Vec<MessageAttachment>,
    reply_to_message_id: Option<MessageId>,
    reply_to_user_id: Option<UserId>,
    offered_sentexes: Vec<OfferedSentex>,
    native: DiscordNativeObservation,
    origin: ObservationOrigin,
}

impl MessageView {
    pub(crate) fn from_discord(
        config: &LoadedConfig,
        message: &Message,
        origin: ObservationOrigin,
    ) -> Result<Self, ObservationError> {
        let formatted = message
            .timestamp
            .to_rfc3339()
            .ok_or_else(|| observation_error(message, ObservationErrorKind::TimestampFormat))?;
        let timestamp = Timestamp::parse(&formatted).map_err(|source| {
            observation_error(message, ObservationErrorKind::TimestampParse(source))
        })?;
        let timestamp = if config.tz.is_some() {
            let utc = timestamp.as_datetime().with_timezone(&Utc);
            config.localize_utc(&utc)
        } else {
            timestamp
        };
        Ok(Self {
            native: DiscordNativeObservation::capture(message),
            origin,
            id: message.id,
            author_id: message.author.id,
            author_name: message.author.name.clone(),
            content: message.content.clone(),
            timestamp,
            attachments: message
                .attachments
                .iter()
                .map(|attachment| MessageAttachment {
                    name: attachment.filename.clone(),
                    url: attachment.url.clone(),
                    size: AttachmentBytes(attachment.size),
                    content_type: attachment.content_type.clone(),
                })
                .collect(),
            reply_to_message_id: message
                .message_reference
                .as_ref()
                .and_then(|reference| reference.message_id)
                .or_else(|| {
                    message
                        .referenced_message
                        .as_deref()
                        .map(|reference| reference.id)
                }),
            reply_to_user_id: message
                .referenced_message
                .as_deref()
                .map(|reference| reference.author.id),
            offered_sentexes: if config.delivery.evidence_markers_enabled {
                parse_offered_sentexes(&message.content, message.author.id)
            } else {
                Vec::new()
            },
        })
    }
    /// Returns the message identity.
    pub fn id(&self) -> MessageId {
        self.id
    }
    /// Returns the Discord author identity.
    pub fn author_id(&self) -> UserId {
        self.author_id
    }
    /// Returns the author's username.
    pub fn author_name(&self) -> &str {
        &self.author_name
    }
    /// Returns the message content.
    pub fn content(&self) -> &str {
        &self.content
    }
    /// Returns the timestamp in the configured timezone.
    pub fn timestamp(&self) -> &Timestamp {
        &self.timestamp
    }
    /// Returns attachments in Discord's order.
    pub fn attachments(&self) -> &[MessageAttachment] {
        &self.attachments
    }
    /// Returns the referenced message identity, preferring the explicit reference.
    pub fn reply_to_message_id(&self) -> Option<MessageId> {
        self.reply_to_message_id
    }
    /// Returns the resolved reference's author, when available.
    pub fn reply_to_user_id(&self) -> Option<UserId> {
        self.reply_to_user_id
    }
    pub(crate) fn offered_sentexes(&self) -> &[OfferedSentex] {
        &self.offered_sentexes
    }
}

fn sorted_messages(
    config: &LoadedConfig,
    mut messages: Vec<Message>,
    origin: ObservationOrigin,
) -> Result<Vec<MessageView>, ObservationError> {
    messages.sort_unstable_by_key(|message| message.id);
    messages
        .iter()
        .map(|message| MessageView::from_discord(config, message, origin))
        .collect()
}

/// A channel read ordered from oldest to newest, optionally using a cursor.
#[derive(Debug)]
pub struct FetchedMessages {
    messages: Vec<MessageView>,
    limit: Option<u8>,
    query: DiscordHistoryQuery,
}
impl FetchedMessages {
    #[cfg(test)]
    fn new(
        config: &LoadedConfig,
        messages: Vec<Message>,
        limit: Option<u8>,
    ) -> Result<Self, ObservationError> {
        let channel_id = messages
            .first()
            .map_or(ChannelId::new(1), |message| message.channel_id);
        Self::observed(
            config,
            messages,
            limit,
            DiscordHistoryQuery {
                channel_id,
                direction: DiscordHistoryDirection::Recent,
                limit: limit.unwrap_or(50),
            },
        )
    }
    fn observed(
        config: &LoadedConfig,
        messages: Vec<Message>,
        limit: Option<u8>,
        query: DiscordHistoryQuery,
    ) -> Result<Self, ObservationError> {
        Ok(Self {
            messages: sorted_messages(config, messages, ObservationOrigin::History)?,
            limit,
            query,
        })
    }
    /// Returns the messages in ascending ID order.
    pub fn messages(&self) -> &[MessageView] {
        &self.messages
    }
    /// Returns a continuation hint only for cursor reads.
    pub fn has_more(&self) -> Option<bool> {
        self.limit
            .map(|limit| limit > 0 && self.messages.len() == usize::from(limit))
    }
}

/// Messages newer than the requested cursor, ordered from oldest to newest.
#[derive(Debug)]
pub struct NewMessages {
    messages: Vec<MessageView>,
    limit: u8,
}
impl NewMessages {
    fn new(
        config: &LoadedConfig,
        messages: Vec<Message>,
        limit: u8,
    ) -> Result<Self, ObservationError> {
        Ok(Self {
            messages: sorted_messages(config, messages, ObservationOrigin::NewSince)?,
            limit,
        })
    }
    /// Returns the messages in ascending ID order.
    pub fn messages(&self) -> &[MessageView] {
        &self.messages
    }
    /// Returns whether the requested page is full and may have a successor.
    pub fn has_more(&self) -> bool {
        self.limit > 0 && self.messages.len() == usize::from(self.limit)
    }
}

/// Pinned messages ordered from oldest to newest.
#[derive(Debug)]
pub struct PinnedMessages {
    messages: Vec<MessageView>,
}
impl PinnedMessages {
    fn new(config: &LoadedConfig, messages: Vec<Message>) -> Result<Self, ObservationError> {
        Ok(Self {
            messages: sorted_messages(config, messages, ObservationOrigin::Pins)?,
        })
    }
    /// Returns the messages in ascending ID order.
    pub fn messages(&self) -> &[MessageView] {
        &self.messages
    }
}

/// An explicit message read with its optional provenance receipt outcome.
#[derive(Debug)]
pub struct RetrievedMessage {
    message: MessageView,
    receipt: Option<Result<GetMessageReceipt, VaeliiError>>,
}
impl RetrievedMessage {
    /// Returns the retrieved message.
    pub fn message(&self) -> &MessageView {
        &self.message
    }
    /// Returns the receipt outcome without treating receipt failure as read failure.
    pub fn receipt(&self) -> Option<Result<&GetMessageReceipt, &VaeliiError>> {
        self.receipt.as_ref().map(Result::as_ref)
    }
}

fn resolve_pagination(
    before: Option<MessageId>,
    after: Option<MessageId>,
) -> Result<Option<MessagePagination>, MessagingError> {
    match (before, after) {
        (Some(_), Some(_)) => Err(MessagingErrorKind::ConflictingPagination.into()),
        (Some(id), None) => Ok(Some(MessagePagination::Before(id))),
        (None, Some(id)) => Ok(Some(MessagePagination::After(id))),
        (None, None) => Ok(None),
    }
}

/// Fetch a permitted channel in ascending message ID order.
pub async fn fetch_messages(
    ctx: &MessagingCtx,
    channel_id: ChannelId,
    before: Option<MessageId>,
    after: Option<MessageId>,
    limit: u8,
) -> Result<FetchedMessages, MessagingError> {
    check_outbound(ctx, channel_id).await?;

    let pagination = resolve_pagination(before, after)?;
    let paginating = before.is_some() || after.is_some();

    match ctx
        .http
        .get_messages(channel_id, pagination, Some(limit))
        .await
    {
        Ok(messages) => {
            check_page_channel(&messages, channel_id)?;
            Ok(FetchedMessages::observed(
                &ctx.config,
                messages,
                paginating.then_some(limit),
                DiscordHistoryQuery {
                    channel_id,
                    direction: match (before, after) {
                        (Some(id), _) => DiscordHistoryDirection::Before(id),
                        (_, Some(id)) => DiscordHistoryDirection::After(id),
                        _ => DiscordHistoryDirection::Recent,
                    },
                    limit,
                },
            )?)
        }
        Err(e) => Err(MessagingErrorKind::Discord(e.into()).into()),
    }
}

/// Fetch all visible pins in ascending message ID order.
pub async fn fetch_pins(
    ctx: &MessagingCtx,
    channel_id: ChannelId,
) -> Result<PinnedMessages, MessagingError> {
    check_outbound(ctx, channel_id).await?;

    match ctx.http.get_pins(channel_id).await {
        Ok(messages) => {
            check_page_channel(&messages, channel_id)?;
            Ok(PinnedMessages::new(&ctx.config, messages)?)
        }
        Err(e) => Err(MessagingErrorKind::Discord(e.into()).into()),
    }
}

/// Fetch messages newer than the cursor in ascending message ID order.
pub async fn fetch_new_since(
    ctx: &MessagingCtx,
    channel_id: ChannelId,
    after_message_id: MessageId,
    limit: u8,
) -> Result<NewMessages, MessagingError> {
    check_outbound(ctx, channel_id).await?;

    match ctx
        .http
        .get_messages(
            channel_id,
            Some(MessagePagination::After(after_message_id)),
            Some(limit),
        )
        .await
    {
        Ok(messages) => {
            check_page_channel(&messages, channel_id)?;
            Ok(NewMessages::new(&ctx.config, messages, limit)?)
        }
        Err(e) => Err(MessagingErrorKind::Discord(e.into()).into()),
    }
}

/// Download each available attachment into the inbox, returning successful paths.
pub async fn download_attachment(
    ctx: &MessagingCtx,
    channel_id: ChannelId,
    message_id: MessageId,
) -> Result<Vec<Utf8PathBuf>, MessagingError> {
    check_outbound(ctx, channel_id).await?;

    let msg = match ctx.http.get_message(channel_id, message_id).await {
        Ok(m) => m,
        Err(e) => return Err(MessagingErrorKind::FetchMessage(e.into()).into()),
    };

    check_exact_target(&msg, channel_id, message_id)?;

    if msg.attachments.is_empty() {
        return Err(MessagingErrorKind::NoAttachments.into());
    }

    let inbox_dir = ctx.state_dir.join("inbox");
    if let Err(e) = tokio::fs::create_dir_all(&inbox_dir).await {
        return Err(MessagingErrorKind::CreateInbox(e).into());
    }

    let mut saved_paths = Vec::new();

    for attachment in &msg.attachments {
        let safe_name = crate::gate::sanitize_filename(&attachment.filename);
        match download_url(&attachment.url).await {
            Ok(bytes) => match save_attachment(&inbox_dir, &safe_name, &bytes).await {
                Ok(dest) => saved_paths.push(dest),
                Err(e) => {
                    tracing::warn!(
                        name = %safe_name,
                        error = %e,
                        "failed to write attachment to inbox"
                    );
                }
            },
            Err(e) => {
                tracing::warn!(url = %attachment.url, error = %e, "failed to download attachment");
            }
        }
    }

    Ok(saved_paths)
}

async fn save_attachment(
    inbox_dir: &Utf8Path,
    safe_name: &str,
    bytes: &[u8],
) -> io::Result<Utf8PathBuf> {
    let mut suffix = 0u64;
    loop {
        let dest = inbox_dir.join(if suffix == 0 {
            safe_name.to_owned()
        } else {
            let prefix = format!("{suffix}-");
            let budget = 255 - prefix.len();
            let extension = Utf8Path::new(safe_name)
                .extension()
                .filter(|extension| extension.len() + 1 < budget)
                .map_or("", |extension| {
                    &safe_name[safe_name.len() - extension.len() - 1..]
                });
            let stem = &safe_name[..safe_name.len() - extension.len()];
            let mut end = stem.len().min(budget - extension.len());
            while !stem.is_char_boundary(end) {
                end -= 1;
            }
            format!("{prefix}{}{extension}", &stem[..end])
        });
        let mut file = match tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&dest)
            .await
        {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                suffix += 1;
                continue;
            }
            Err(error) => return Err(error),
        };
        let written = async {
            file.write_all(bytes).await?;
            file.flush().await
        }
        .await;
        if let Err(error) = written {
            drop(file);
            let _ = tokio::fs::remove_file(&dest).await;
            return Err(error);
        }
        return Ok(dest);
    }
}

const MAX_ATTACHMENT_BYTES: u64 = 25 * 1024 * 1024;

#[derive(Debug, Error)]
#[error("{kind}")]
struct DownloadError {
    #[source]
    kind: DownloadErrorKind,
}
#[derive(Debug, Error)]
enum DownloadErrorKind {
    #[error("{0}")]
    Request(#[from] reqwest::Error),
    #[error("attachment too large: {bytes} bytes (max {MAX_ATTACHMENT_BYTES})")]
    TooLarge { bytes: u64 },
}
impl From<reqwest::Error> for DownloadError {
    fn from(source: reqwest::Error) -> Self {
        Self {
            kind: DownloadErrorKind::Request(source),
        }
    }
}
async fn download_url(url: &str) -> Result<Vec<u8>, DownloadError> {
    let resp = reqwest::Client::builder()
        .use_rustls_tls()
        .build()?
        .get(url)
        .send()
        .await?
        .error_for_status()?;

    if let Some(len) = resp.content_length()
        && len > MAX_ATTACHMENT_BYTES
    {
        return Err(DownloadError {
            kind: DownloadErrorKind::TooLarge { bytes: len },
        });
    }

    let bytes = resp.bytes().await?;
    if bytes.len() as u64 > MAX_ATTACHMENT_BYTES {
        return Err(DownloadError {
            kind: DownloadErrorKind::TooLarge {
                bytes: bytes.len() as u64,
            },
        });
    }

    Ok(bytes.to_vec())
}

/// Fetch a permitted message and attempt its configured provenance receipt.
pub async fn get_message(
    ctx: &MessagingCtx,
    channel_id: ChannelId,
    message_id: MessageId,
) -> Result<RetrievedMessage, MessagingError> {
    check_outbound(ctx, channel_id).await?;
    let message = ctx
        .http
        .get_message(channel_id, message_id)
        .await
        .map_err(|error| MessagingError::from(MessagingErrorKind::Discord(error.into())))?;
    check_exact_target(&message, channel_id, message_id)?;
    let message = MessageView::from_discord(&ctx.config, &message, ObservationOrigin::Exact)?;
    let receipt = if ctx.config.vaelii.is_enabled() {
        match crate::vaelii::write_get_message_receipt(
            &ctx.config.vaelii,
            message_id.get(),
            ctx.construct_id.as_str(),
        )
        .await
        {
            Ok(receipt) => receipt.map(Ok),
            Err(error) => {
                tracing::warn!(%error, message_id = message_id.get(), "Vaelii receipt write failed");
                Some(Err(error))
            }
        }
    } else {
        None
    };
    Ok(RetrievedMessage { message, receipt })
}

/// Native reference coordinates and sender, without recursively retained message bodies.
#[derive(Debug)]
pub(crate) struct DiscordResolvedReference {
    channel_id: ChannelId,
    message_id: MessageId,
    guild_id: Option<GuildId>,
    author_id: UserId,
}
impl DiscordResolvedReference {
    fn capture(message: &Message) -> Self {
        Self {
            channel_id: message.channel_id,
            message_id: message.id,
            guild_id: message.guild_id,
            author_id: message.author.id,
        }
    }
    pub(crate) fn channel_id(&self) -> ChannelId {
        self.channel_id
    }
    pub(crate) fn message_id(&self) -> MessageId {
        self.message_id
    }
    pub(crate) fn guild_id(&self) -> Option<GuildId> {
        self.guild_id
    }
    pub(crate) fn author_id(&self) -> UserId {
        self.author_id
    }
}

/// Metadata captured from the SDK at the existing read boundary.
#[derive(Debug)]
pub(crate) struct DiscordNativeObservation {
    message_type: MessageType,
    channel_id: ChannelId,
    message_id: MessageId,
    guild_id: Option<GuildId>,
    author_id: UserId,
    author_name: String,
    author_bot: bool,
    webhook_id: Option<WebhookId>,
    application_id: Option<ApplicationId>,
    reference: Option<MessageReference>,
    resolved_reference: Option<DiscordResolvedReference>,
    attachments: Vec<Attachment>,
    edited_at: Option<serenity::model::Timestamp>,
}
impl DiscordNativeObservation {
    fn capture(message: &Message) -> Self {
        Self {
            message_type: message.kind,
            channel_id: message.channel_id,
            message_id: message.id,
            guild_id: message.guild_id,
            author_id: message.author.id,
            author_name: message.author.name.clone(),
            author_bot: message.author.bot,
            webhook_id: message.webhook_id,
            application_id: message.application_id,
            reference: message.message_reference.clone(),
            resolved_reference: message
                .referenced_message
                .as_deref()
                .map(DiscordResolvedReference::capture),
            attachments: message.attachments.clone(),
            edited_at: message.edited_timestamp,
        }
    }
    pub(crate) fn message_type(&self) -> MessageType {
        self.message_type
    }
    pub(crate) fn channel_id(&self) -> ChannelId {
        self.channel_id
    }
    pub(crate) fn message_id(&self) -> MessageId {
        self.message_id
    }
    pub(crate) fn guild_id(&self) -> Option<GuildId> {
        self.guild_id
    }
    pub(crate) fn author_id(&self) -> UserId {
        self.author_id
    }
    pub(crate) fn author_bot(&self) -> bool {
        self.author_bot
    }
    pub(crate) fn webhook_id(&self) -> Option<WebhookId> {
        self.webhook_id
    }
    pub(crate) fn application_id(&self) -> Option<ApplicationId> {
        self.application_id
    }
    pub(crate) fn author_name(&self) -> &str {
        &self.author_name
    }
    pub(crate) fn reference(&self) -> Option<&MessageReference> {
        self.reference.as_ref()
    }
    pub(crate) fn resolved_reference(&self) -> Option<&DiscordResolvedReference> {
        self.resolved_reference.as_ref()
    }
    pub(crate) fn attachments(&self) -> &[Attachment] {
        &self.attachments
    }
    pub(crate) fn edited_at(&self) -> Option<serenity::model::Timestamp> {
        self.edited_at
    }
}

fn conversation_ref(source: &ModuleInstanceId, channel_id: ChannelId) -> core::ConversationRef {
    core::ConversationRef::new(
        source.clone(),
        core::ConversationId::numeric(channel_id.into()),
    )
}
fn message_ref(
    source: &ModuleInstanceId,
    channel_id: ChannelId,
    message_id: MessageId,
) -> core::MessageRef {
    core::MessageRef::new(
        conversation_ref(source, channel_id),
        core::MessageId::numeric(message_id.into()),
    )
}
fn actor_ref(source: &ModuleInstanceId, user_id: UserId) -> core::ActorRef {
    core::ActorRef::new(source.clone(), core::ActorId::numeric(user_id.into()))
}

impl DiscordNativeObservation {
    fn relation(&self, source: &ModuleInstanceId) -> Availability<ReferenceObservation> {
        let explicit = self.reference.as_ref();
        let resolved = self.resolved_reference.as_ref();
        if explicit.is_none() && resolved.is_none() {
            return Availability::Unavailable(UnavailableReason::NotReported);
        }
        let kind = match explicit.map(|reference| reference.kind) {
            Some(MessageReferenceKind::Forward) => ReferenceRelation::Forward,
            Some(MessageReferenceKind::Default)
                if self.message_type == MessageType::InlineReply =>
            {
                ReferenceRelation::Reply
            }
            _ => ReferenceRelation::Unknown,
        };
        let target = explicit
            .and_then(|reference| {
                reference
                    .message_id
                    .map(|id| message_ref(source, reference.channel_id, id))
            })
            .map_or(
                Availability::Unavailable(UnavailableReason::NotReported),
                Availability::Known,
            );
        let resolution = resolved
            .map(|reference| {
                ResolvedReference::new(
                    message_ref(source, reference.channel_id, reference.message_id),
                    actor_ref(source, reference.author_id),
                )
            })
            .map_or(
                Availability::Unavailable(UnavailableReason::NotResolved),
                Availability::Known,
            );
        let consistency = match (explicit, resolved) {
            (_, None) => ReferenceConsistency::NoResolvedObservation,
            (None, Some(_)) => ReferenceConsistency::ResolvedOnly,
            (Some(explicit), Some(resolved)) => {
                let conflict = explicit.channel_id != resolved.channel_id
                    || explicit
                        .message_id
                        .is_some_and(|id| id != resolved.message_id)
                    || explicit
                        .guild_id
                        .zip(resolved.guild_id)
                        .is_some_and(|(a, b)| a != b);
                if conflict {
                    ReferenceConsistency::Conflicts
                } else if explicit.message_id.is_none() {
                    ReferenceConsistency::InsufficientEvidence
                } else {
                    ReferenceConsistency::Matches
                }
            }
        };
        Availability::Known(ReferenceObservation::new(
            kind,
            target,
            resolution,
            consistency,
        ))
    }
}

impl MessageView {
    pub(crate) fn into_native(self) -> DiscordNativeObservation {
        self.native
    }

    pub(crate) fn native(&self) -> &DiscordNativeObservation {
        &self.native
    }

    pub(crate) fn context(
        &self,
        source: &ModuleInstanceId,
    ) -> Result<ContextObservation, ObservationError> {
        let invalid = |kind| ObservationError {
            channel_id: self.native.channel_id,
            message_id: self.id,
            kind,
        };
        let edited_at = self
            .native
            .edited_at
            .map(|timestamp| {
                let text = timestamp
                    .to_rfc3339()
                    .ok_or_else(|| invalid(ObservationErrorKind::EditedTimestampFormat))?;
                Timestamp::parse(&text)
                    .map(|parsed| parsed.as_datetime().with_timezone(&Utc))
                    .map_err(|error| invalid(ObservationErrorKind::EditedTimestampParse(error)))
            })
            .transpose()?;
        let workspace = self
            .native
            .guild_id
            .map(|id| {
                core::WorkspaceRef::new(source.clone(), core::WorkspaceId::numeric(id.into()))
            })
            .map_or(
                Availability::Unavailable(UnavailableReason::NotReported),
                Availability::Known,
            );
        let attachments = self
            .native
            .attachments
            .iter()
            .map(|attachment| {
                AttachmentSummary::new(
                    attachment.filename.clone(),
                    attachment.content_type.clone().map_or(
                        Availability::Unavailable(UnavailableReason::NotReported),
                        Availability::Known,
                    ),
                    u64::from(attachment.size),
                )
            })
            .collect();
        Ok(ContextObservation::new(
            message_ref(source, self.native.channel_id, self.id),
            SenderObservation::new(
                actor_ref(source, self.author_id),
                self.author_name.clone(),
                Availability::Known(self.native.author_bot),
                Availability::Unavailable(UnavailableReason::NotResolved),
            ),
            self.content.clone(),
            self.timestamp.as_datetime().with_timezone(&Utc),
            ContextMetadata::new(
                workspace,
                Availability::Known(edited_at),
                self.native.relation(source),
                Availability::Known(attachments),
                ObservationExtent::new(self.origin),
            ),
        ))
    }
}

#[derive(Debug)]
enum DiscordHistoryDirection {
    Recent,
    Before(MessageId),
    After(MessageId),
}
#[derive(Debug)]
struct DiscordHistoryQuery {
    channel_id: ChannelId,
    direction: DiscordHistoryDirection,
    limit: u8,
}
impl FetchedMessages {
    pub(crate) fn context_page(&self, source: &ModuleInstanceId) -> PageMetadata {
        let query = &self.query;
        let direction = match query.direction {
            DiscordHistoryDirection::Recent => HistoryDirection::Recent,
            DiscordHistoryDirection::Before(id) => {
                HistoryDirection::Before(message_ref(source, query.channel_id, id))
            }
            DiscordHistoryDirection::After(id) => {
                HistoryDirection::After(message_ref(source, query.channel_id, id))
            }
        };
        let observed = self
            .messages
            .iter()
            .map(|message| message_ref(source, message.native.channel_id, message.id))
            .collect::<Vec<_>>();
        PageMetadata::from_observed(
            HistoryQuery::new(
                conversation_ref(source, query.channel_id),
                direction,
                query.limit,
            ),
            &observed,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{
        super::tests::{
            fake_discord_http, messaging_ctx, messaging_ctx_with_http, test_config, wire_message,
        },
        *,
    };
    use crate::{
        config::{ChannelConfig, Config},
        discord::query_wire,
        gate::OutboundGate,
        no_rly::consent::ConsentGate,
    };
    use serde_json::{Value, json};
    use std::{collections::BTreeMap, fmt, sync::Arc};
    use tokio::{
        io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };
    use tracing::instrument::WithSubscriber;
    use tracing_subscriber::layer::SubscriberExt;
    async fn attachment_download_fixture(
        filenames: Vec<String>,
        concurrent_downloads: bool,
    ) -> (MessagingCtx, tempfile::TempDir, tokio::task::JoinHandle<()>) {
        let directory = tempfile::tempdir().expect("attachment inbox fixture");
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let barrier = concurrent_downloads.then(|| Arc::new(tokio::sync::Barrier::new(2)));
        let server = tokio::spawn(async move {
            let mut requests = tokio::task::JoinSet::new();
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let filenames = filenames.clone();
                let barrier = barrier.clone();
                requests.spawn(async move {
                    let mut reader = tokio::io::BufReader::new(stream);
                    let mut request_line = String::new();
                    reader.read_line(&mut request_line).await.unwrap();
                    loop {
                        let mut header = String::new();
                        if reader.read_line(&mut header).await.unwrap() == 0 || header == "\r\n" {
                            break;
                        }
                    }
                    let path = request_line.split_whitespace().nth(1).unwrap();
                    let (status, body) = if path.contains("/messages/") {
                        let message_id = path.rsplit('/').next().unwrap().parse::<u64>().unwrap();
                        let attachments = filenames.iter().enumerate().map(|(index, filename)| {
                            let url = format!("http://{address}/bytes/{message_id}/{index}");
                            json!({
                                "id": (index + 1).to_string(),
                                "filename": filename,
                                "size": 3,
                                "url": url,
                                "proxy_url": url,
                            })
                        }).collect::<Vec<_>>();
                        let mut message = wire_message(
                            message_id,
                            "",
                            "2026-08-15T09:00:00.000000+00:00",
                            json!(attachments),
                        );
                        message["channel_id"] = json!("42");
                        ("200 OK", message.to_string())
                    } else {
                        if let Some(barrier) = barrier {
                            barrier.wait().await;
                        }
                        let index = path.rsplit('/').next().unwrap().parse::<usize>().unwrap();
                        if filenames[index] == "missing.png" {
                            ("404 Not Found", "missing".to_owned())
                        } else {
                            ("200 OK", path.to_owned())
                        }
                    };
                    let response = format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                    reader.get_mut().write_all(response.as_bytes()).await.unwrap();
                });
                while requests.try_join_next().is_some() {}
            }
        });
        let http = serenity::http::HttpBuilder::new("fake")
            .proxy(format!("http://{address}"))
            .ratelimiter_disabled(true)
            .build();
        let mut raw = Config::default();
        raw.channels.push(ChannelConfig {
            id: "42".into(),
            ..Default::default()
        });
        let mut ctx = messaging_ctx_with_http(LoadedConfig::from_raw(raw), Arc::new(http));
        ctx.state_dir = Utf8PathBuf::from_path_buf(directory.path().to_owned()).unwrap();
        (ctx, directory, server)
    }

    #[tokio::test]
    async fn attachment_download_repeated_names_preserve_returned_bytes() {
        let (ctx, _directory, server) =
            attachment_download_fixture(vec!["image.png".into()], false).await;
        let mut saved = Vec::new();
        for message_id in [1, 1, 2, 3] {
            let result = query_wire::downloaded(
                download_attachment(&ctx, ChannelId::new(42), MessageId::new(message_id)).await,
            );
            let paths = result["saved"].as_array().expect("saved array");
            assert_eq!(paths.len(), 1, "{result}");
            saved.push((
                paths[0].as_str().unwrap().to_owned(),
                format!("/bytes/{message_id}/0"),
            ));
        }
        for (path, expected) in &saved {
            assert!(OutboundGate::check_file_send(
                camino::Utf8Path::new(path),
                &ctx.state_dir
            ));
            assert_eq!(
                tokio::fs::read_to_string(path).await.unwrap(),
                *expected,
                "previously returned {path} was overwritten"
            );
        }
        let distinct = saved
            .iter()
            .map(|(path, _)| path)
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(
            distinct.len(),
            saved.len(),
            "repeated downloads are independent copies"
        );
        server.abort();
    }

    #[tokio::test]
    async fn attachment_download_concurrent_names_preserve_returned_bytes() {
        let (ctx, _directory, server) =
            attachment_download_fixture(vec!["image.png".into()], true).await;
        let inbox = ctx.state_dir.join("inbox");
        tokio::fs::create_dir_all(&inbox).await.unwrap();
        tokio::fs::write(inbox.join("image.png"), b"existing")
            .await
            .unwrap();
        let (first, second) = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            tokio::join!(
                download_attachment(&ctx, ChannelId::new(42), MessageId::new(1)),
                download_attachment(&ctx, ChannelId::new(42), MessageId::new(2)),
            )
        })
        .await
        .expect("concurrent fixture requests completed");
        let first = query_wire::downloaded(first);
        let second = query_wire::downloaded(second);
        for (result, message_id) in [(&first, 1), (&second, 2)] {
            let paths = result["saved"].as_array().expect("saved array");
            assert_eq!(paths.len(), 1, "{result}");
            let path = paths[0].as_str().unwrap();
            assert!(OutboundGate::check_file_send(
                camino::Utf8Path::new(path),
                &ctx.state_dir
            ));
            assert_eq!(
                tokio::fs::read_to_string(path).await.unwrap(),
                format!("/bytes/{message_id}/0"),
                "previously returned {path} was overwritten"
            );
        }
        assert_ne!(first["saved"][0], second["saved"][0]);
        assert_eq!(
            tokio::fs::read_to_string(inbox.join("image.png"))
                .await
                .unwrap(),
            "existing"
        );
        server.abort();
    }

    #[tokio::test]
    async fn attachment_download_long_names_preserve_returned_bytes() {
        for filename in [
            format!("{}.png", "a".repeat(250)),
            format!("{}.png", "a".repeat(251)),
            "a".repeat(255),
            format!("a{}.png", "猫".repeat(83)),
            format!("{}.rs", "猫".repeat(84)),
            format!("a.{}", "x".repeat(253)),
        ] {
            let (ctx, _directory, server) =
                attachment_download_fixture(vec![filename.clone()], false).await;
            let mut saved = Vec::new();
            for message_id in 1..=12 {
                let result = query_wire::downloaded(
                    download_attachment(&ctx, ChannelId::new(42), MessageId::new(message_id)).await,
                );
                let paths = result["saved"].as_array().expect("saved array");
                assert_eq!(
                    paths.len(),
                    1,
                    "download {message_id}, filename {filename}: {result}"
                );
                let path = Utf8PathBuf::from(paths[0].as_str().unwrap());
                let name = path.file_name().unwrap();
                assert!(name.len() <= 255, "{name}");
                if message_id == 1 {
                    assert_eq!(name, filename);
                } else {
                    assert!(name.starts_with(&format!("{}-", message_id - 1)), "{name}");
                }
                if filename.ends_with(".png") || filename.ends_with(".rs") {
                    assert_eq!(path.extension(), Utf8Path::new(&filename).extension());
                }
                assert!(OutboundGate::check_file_send(&path, &ctx.state_dir));
                saved.push((path, format!("/bytes/{message_id}/0")));
            }
            assert_eq!(
                saved
                    .iter()
                    .map(|(path, _)| path)
                    .collect::<std::collections::HashSet<_>>()
                    .len(),
                saved.len()
            );
            for (path, expected) in saved {
                assert_eq!(
                    tokio::fs::read_to_string(&path).await.unwrap(),
                    expected,
                    "{path}"
                );
            }
            server.abort();
        }
    }

    #[tokio::test]
    async fn attachment_download_long_names_preserve_concurrent_and_existing_bytes() {
        let filename = format!("{}.png", "a".repeat(251));
        let (ctx, _directory, server) =
            attachment_download_fixture(vec![filename.clone()], true).await;
        let inbox = ctx.state_dir.join("inbox");
        tokio::fs::create_dir_all(&inbox).await.unwrap();
        tokio::fs::write(inbox.join(&filename), b"existing")
            .await
            .unwrap();
        let (first, second) = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            tokio::join!(
                download_attachment(&ctx, ChannelId::new(42), MessageId::new(1)),
                download_attachment(&ctx, ChannelId::new(42), MessageId::new(2)),
            )
        })
        .await
        .expect("concurrent fixture requests completed");
        let first = query_wire::downloaded(first);
        let second = query_wire::downloaded(second);
        for (result, message_id) in [(&first, 1), (&second, 2)] {
            let paths = result["saved"].as_array().expect("saved array");
            assert_eq!(paths.len(), 1, "{result}");
            let path = Utf8Path::new(paths[0].as_str().unwrap());
            assert!(path.file_name().unwrap().len() <= 255);
            assert_eq!(path.extension(), Some("png"));
            assert!(OutboundGate::check_file_send(path, &ctx.state_dir));
            assert_eq!(
                tokio::fs::read_to_string(path).await.unwrap(),
                format!("/bytes/{message_id}/0")
            );
        }
        assert_ne!(first["saved"][0], second["saved"][0]);
        assert_eq!(
            tokio::fs::read_to_string(inbox.join(filename))
                .await
                .unwrap(),
            "existing"
        );
        server.abort();
    }

    #[derive(Default)]
    struct AttachmentWarningFields(BTreeMap<String, String>);

    impl tracing::field::Visit for AttachmentWarningFields {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn fmt::Debug) {
            self.0.insert(field.name().to_owned(), format!("{value:?}"));
        }

        fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
            self.0.insert(field.name().to_owned(), value.to_owned());
        }
    }

    struct AttachmentWarningCapture(tokio::sync::mpsc::UnboundedSender<BTreeMap<String, String>>);

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for AttachmentWarningCapture {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            if *event.metadata().level() == tracing::Level::WARN {
                let mut fields = AttachmentWarningFields::default();
                event.record(&mut fields);
                let _ = self.0.send(fields.0);
            }
        }
    }

    #[tokio::test]
    async fn attachment_download_sanitizes_names_and_keeps_partial_success() {
        let (ctx, _directory, server) = attachment_download_fixture(
            vec![
                "../[image];.png".into(),
                "missing.png".into(),
                "x".repeat(300),
                "image.png".into(),
            ],
            false,
        )
        .await;
        let (warnings_tx, mut warnings_rx) = tokio::sync::mpsc::unbounded_channel();
        let subscriber = tracing_subscriber::registry().with(AttachmentWarningCapture(warnings_tx));
        let result = download_attachment(&ctx, ChannelId::new(42), MessageId::new(1))
            .with_subscriber(subscriber)
            .await;
        let warnings: Vec<_> = std::iter::from_fn(|| warnings_rx.try_recv().ok()).collect();
        assert_eq!(warnings.len(), 2, "one warning per failed attachment");
        assert!(
            warnings.iter().any(|fields| fields
                .get("url")
                .is_some_and(|url| url.contains("/bytes/1/1"))
                && fields.contains_key("error")),
            "download warning identifies the failed URL: {warnings:?}"
        );
        assert!(
            warnings.iter().any(|fields| fields
                .get("name")
                .is_some_and(|name| name.contains(&"x".repeat(300)))
                && fields.contains_key("error")),
            "write warning identifies the failed filename: {warnings:?}"
        );
        let result = query_wire::downloaded(result);
        assert_eq!(
            result.as_object().unwrap().len(),
            1,
            "saved-only response contract"
        );
        let paths = result["saved"].as_array().expect("saved array");
        assert_eq!(paths.len(), 2, "{result}");
        for (path, index) in paths.iter().zip([0, 3]) {
            let path = camino::Utf8Path::new(path.as_str().unwrap());
            assert_eq!(path.parent(), Some(ctx.state_dir.join("inbox").as_path()));
            assert!(path.file_name().unwrap().ends_with("image.png"));
            assert!(OutboundGate::check_file_send(path, &ctx.state_dir));
            assert_eq!(
                tokio::fs::read_to_string(path).await.unwrap(),
                format!("/bytes/1/{index}")
            );
        }
        server.abort();
    }

    #[tokio::test]
    async fn attachment_download_skips_existing_directories() {
        let (ctx, _directory, server) =
            attachment_download_fixture(vec!["image.png".into()], false).await;
        let inbox = ctx.state_dir.join("inbox");
        for name in ["image.png", "0-image.png", "1-image.png"] {
            let occupied = inbox.join(name);
            tokio::fs::create_dir_all(&occupied).await.unwrap();
            tokio::fs::write(occupied.join("keep"), b"existing")
                .await
                .unwrap();
        }
        let result = query_wire::downloaded(
            download_attachment(&ctx, ChannelId::new(42), MessageId::new(1)).await,
        );
        assert_eq!(result["saved"].as_array().unwrap().len(), 1, "{result}");
        let path = camino::Utf8Path::new(result["saved"][0].as_str().unwrap());
        assert!(OutboundGate::check_file_send(path, &ctx.state_dir));
        assert_eq!(tokio::fs::read_to_string(path).await.unwrap(), "/bytes/1/0");
        for name in ["image.png", "0-image.png", "1-image.png"] {
            assert_eq!(
                tokio::fs::read_to_string(inbox.join(name).join("keep"))
                    .await
                    .unwrap(),
                "existing"
            );
        }
        server.abort();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn attachment_download_does_not_follow_existing_symlinks() {
        let (ctx, _directory, server) =
            attachment_download_fixture(vec!["image.png".into()], false).await;
        let inbox = ctx.state_dir.join("inbox");
        tokio::fs::create_dir_all(&inbox).await.unwrap();
        let outside = ctx.state_dir.join("outside.png");
        std::os::unix::fs::symlink(&outside, inbox.join("image.png")).unwrap();
        std::os::unix::fs::symlink(&outside, inbox.join("1-image.png")).unwrap();
        let result = query_wire::downloaded(
            download_attachment(&ctx, ChannelId::new(42), MessageId::new(1)).await,
        );
        assert_eq!(result["saved"].as_array().unwrap().len(), 1, "{result}");
        assert!(
            !outside.exists(),
            "publication must not follow a dangling symlink"
        );
        assert!(OutboundGate::check_file_send(
            camino::Utf8Path::new(result["saved"][0].as_str().unwrap()),
            &ctx.state_dir
        ));
        assert_eq!(
            tokio::fs::read_to_string(result["saved"][0].as_str().unwrap())
                .await
                .unwrap(),
            "/bytes/1/0"
        );
        server.abort();
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

        let retrieved = get_message(&ctx, ChannelId::new(42), MessageId::new(9001))
            .await
            .unwrap();
        assert_eq!(retrieved.message().id(), MessageId::new(9001));
        assert!(retrieved.receipt().is_none());
        let projected = query_wire::retrieved(Ok(retrieved));
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

        let retrieved = get_message(&ctx, ChannelId::new(42), MessageId::new(9001))
            .await
            .unwrap();
        assert_eq!(
            retrieved.receipt().unwrap().unwrap().invocation,
            "Check9001"
        );
        let projected = query_wire::retrieved(Ok(retrieved));
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

        let retrieved = get_message(&ctx, ChannelId::new(42), MessageId::new(9001))
            .await
            .unwrap();
        assert_eq!(retrieved.message().content(), "explicit read");
        assert!(
            matches!(retrieved.receipt(), Some(Err(VaeliiError::Http(status))) if *status == reqwest::StatusCode::INTERNAL_SERVER_ERROR)
        );
        let projected = query_wire::retrieved(Ok(retrieved));
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

        let projected =
            query_wire::fetched(fetch_messages(&ctx, ChannelId::new(42), None, None, 10).await);
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

    #[test]
    fn get_message_and_fetch_share_role_separated_sentex_projection() {
        let content = "grounded [🔍=v2:claim:AAAAAAAAAAw] [🔍=v1:AAAAAAAAACI]";
        let messages = from_wire(json!([wire_message(
            3001,
            content,
            "2026-06-09T12:00:00.000000+00:00",
            json!([])
        )]));

        let fetched = query_wire::message(
            &MessageView::from_discord(&test_config(), &messages[0], ObservationOrigin::Exact)
                .unwrap(),
        );
        let single = query_wire::retrieved(Ok(RetrievedMessage {
            message: MessageView::from_discord(
                &test_config(),
                &messages[0],
                ObservationOrigin::Exact,
            )
            .unwrap(),
            receipt: None,
        }));
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

        let projected = query_wire::message(
            &MessageView::from_discord(&disabled, &messages[0], ObservationOrigin::Exact).unwrap(),
        );
        assert_eq!(projected["content"], content);
        assert!(projected.get("claim_locators").is_none());
        assert!(projected.get("citation_locators").is_none());
    }

    fn from_wire(payload: Value) -> Vec<Message> {
        serde_json::from_value(payload).expect("captured payload must deserialize as Vec<Message>")
    }

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
        let resp = query_wire::new_since(Ok(NewMessages::new(
            &test_config(),
            newest_first_batch(),
            20,
        )
        .unwrap()));
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
        let resp = query_wire::new_since(Ok(NewMessages::new(
            &test_config(),
            newest_first_batch(),
            20,
        )
        .unwrap()));
        assert_eq!(resp["count"], 3);
        assert_eq!(
            resp["messages"].as_array().expect("messages array").len(),
            3
        );
    }

    #[test]
    fn new_since_has_more_set_at_exactly_limit() {
        let resp = query_wire::new_since(Ok(NewMessages::new(
            &test_config(),
            newest_first_batch(),
            3,
        )
        .unwrap()));
        assert_eq!(
            resp["has_more"],
            json!(true),
            "a full page (count == limit) must signal that more may follow"
        );
    }

    #[test]
    fn new_since_has_more_unset_below_limit() {
        let resp = query_wire::new_since(Ok(NewMessages::new(
            &test_config(),
            newest_first_batch(),
            4,
        )
        .unwrap()));
        assert_eq!(
            resp["has_more"],
            json!(false),
            "a partial page must signal that the caller is caught up"
        );
    }

    #[test]
    fn new_since_empty_when_caught_up() {
        let resp =
            query_wire::new_since(Ok(NewMessages::new(&test_config(), Vec::new(), 20).unwrap()));
        assert_eq!(resp["messages"], json!([]));
        assert_eq!(resp["count"], 0);
        assert_eq!(resp["has_more"], json!(false));
    }

    #[test]
    fn new_since_zero_limit_never_signals_more() {
        let resp =
            query_wire::new_since(Ok(NewMessages::new(&test_config(), Vec::new(), 0).unwrap()));
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
        let resp = query_wire::new_since(Ok(NewMessages::new(&test_config(), batch, 20).unwrap()));
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

        let reply_json = query_wire::message(
            &MessageView::from_discord(&test_config(), &messages[0], ObservationOrigin::Exact)
                .unwrap(),
        );
        assert_eq!(reply_json["reply_to_message_id"], "3001");
        assert_eq!(reply_json["reply_to_user_id"], "333333333333333333");

        let plain_json = query_wire::message(
            &MessageView::from_discord(&test_config(), &messages[1], ObservationOrigin::Exact)
                .unwrap(),
        );
        assert!(
            plain_json.get("reply_to_message_id").is_none()
                && plain_json.get("reply_to_user_id").is_none(),
            "non-replies must not carry reply keys"
        );
    }

    #[test]
    fn query_contract_query_collection_payloads_preserve_complete_wire_contracts() {
        let messages = from_wire(json!([
            wire_message(12, "later", "2026-06-09T12:01:00Z", json!([])),
            wire_message(
                11,
                "earlier",
                "2026-06-09T12:00:00Z",
                json!([{
                    "id": "111", "filename": "photo.png", "size": 2048,
                    "url": "https://cdn.discordapp.com/photo.png",
                    "proxy_url": "https://media.discordapp.net/photo.png",
                    "content_type": "image/png"
                }])
            )
        ]));
        let expected = json!([
            {"id":"11","author":"example-user","author_id":"210987654321098765",
             "content":"earlier","timestamp":"2026-06-09T12:00:00+00:00",
             "attachments":[{"name":"photo.png","url":"https://cdn.discordapp.com/photo.png","size":2048}]},
            {"id":"12","author":"example-user","author_id":"210987654321098765",
             "content":"later","timestamp":"2026-06-09T12:01:00+00:00","attachments":[]}
        ]);
        let config = test_config();
        assert_eq!(
            query_wire::fetched(Ok(
                FetchedMessages::new(&config, messages.clone(), None).unwrap()
            )),
            json!({"messages":expected})
        );
        assert_eq!(
            query_wire::fetched(Ok(
                FetchedMessages::new(&config, messages.clone(), Some(2)).unwrap()
            )),
            json!({"messages":expected,"count":2,"has_more":true})
        );
        assert_eq!(
            query_wire::new_since(Ok(NewMessages::new(&config, messages.clone(), 3).unwrap())),
            json!({"messages":expected,"count":2,"has_more":false})
        );
        assert_eq!(
            query_wire::pins(Ok(PinnedMessages::new(&config, messages).unwrap())),
            json!({"messages":expected,"count":2})
        );
        assert_eq!(
            query_wire::fetched(Ok(
                FetchedMessages::new(&config, Vec::new(), Some(0)).unwrap()
            )),
            json!({"messages":[],"count":0,"has_more":false})
        );
        assert_eq!(
            query_wire::new_since(Ok(NewMessages::new(&config, Vec::new(), 0).unwrap())),
            json!({"messages":[],"count":0,"has_more":false})
        );
        assert_eq!(
            query_wire::pins(Ok(PinnedMessages::new(&config, Vec::new()).unwrap())),
            json!({"messages":[],"count":0})
        );
    }

    #[test]
    fn query_contract_retrieved_message_payload_preserves_present_and_null_content_types() {
        let message = serde_json::from_value(wire_message(11, "files", "2026-06-09T12:00:00Z", json!([
            {"id":"111","filename":"photo.png","size":2048,"url":"https://cdn.discordapp.com/photo.png","proxy_url":"https://media.discordapp.net/photo.png","content_type":"image/png"},
            {"id":"112","filename":"data","size":3,"url":"https://cdn.discordapp.com/data","proxy_url":"https://media.discordapp.net/data"}
        ]))).unwrap();
        assert_eq!(
            query_wire::retrieved(Ok(RetrievedMessage {
                message: MessageView::from_discord(
                    &test_config(),
                    &message,
                    ObservationOrigin::Exact
                )
                .unwrap(),
                receipt: None
            })),
            json!({
                "id":"11","author":"example-user","author_id":"210987654321098765",
                "content":"files","timestamp":"2026-06-09T12:00:00+00:00",
                "attachments":[
                    {"name":"photo.png","url":"https://cdn.discordapp.com/photo.png","size":2048,"content_type":"image/png"},
                    {"name":"data","url":"https://cdn.discordapp.com/data","size":3,"content_type":null}
                ]
            })
        );
    }

    #[test]
    fn query_contract_reply_projection_preserves_independent_reference_and_resolved_metadata() {
        let mut reply = wire_message(12, "reply", "2026-06-09T12:01:00Z", json!([]));
        reply["message_reference"] = json!({"message_id":"10","channel_id":"1080000000000000001"});
        reply["referenced_message"] =
            wire_message(11, "resolved", "2026-06-09T12:00:00Z", json!([]));
        reply["referenced_message"]["author"]["id"] = json!("99");
        let message = serde_json::from_value(reply.clone()).unwrap();
        assert_eq!(
            query_wire::message(
                &MessageView::from_discord(&test_config(), &message, ObservationOrigin::Exact)
                    .unwrap()
            ),
            json!({
                "id":"12","author":"example-user","author_id":"210987654321098765",
                "content":"reply","timestamp":"2026-06-09T12:01:00+00:00","attachments":[],
                "reply_to_message_id":"10","reply_to_user_id":"99"
            })
        );
        reply["message_reference"] = Value::Null;
        let message = serde_json::from_value(reply.clone()).unwrap();
        assert_eq!(
            query_wire::message(
                &MessageView::from_discord(&test_config(), &message, ObservationOrigin::Exact)
                    .unwrap()
            ),
            json!({
                "id":"12","author":"example-user","author_id":"210987654321098765",
                "content":"reply","timestamp":"2026-06-09T12:01:00+00:00","attachments":[],
                "reply_to_message_id":"11","reply_to_user_id":"99"
            })
        );
        reply["message_reference"] = json!({"message_id":"10","channel_id":"1080000000000000001"});
        reply["referenced_message"] = Value::Null;
        let message = serde_json::from_value(reply).unwrap();
        assert_eq!(
            query_wire::message(
                &MessageView::from_discord(&test_config(), &message, ObservationOrigin::Exact)
                    .unwrap()
            ),
            json!({
                "id":"12","author":"example-user","author_id":"210987654321098765",
                "content":"reply","timestamp":"2026-06-09T12:01:00+00:00","attachments":[],
                "reply_to_message_id":"10"
            })
        );
    }

    #[tokio::test]
    async fn query_contract_fetch_authorizes_before_rejecting_conflicting_cursors() {
        let mut raw = Config::default();
        raw.channels.push(ChannelConfig {
            id: "42".into(),
            ..Default::default()
        });
        let ctx = messaging_ctx(LoadedConfig::from_raw(raw));
        let before = Some(MessageId::new(10));
        let after = Some(MessageId::new(11));
        assert_eq!(
            query_wire::fetched(fetch_messages(&ctx, ChannelId::new(99), before, after, 2).await),
            json!({"error":"channel 99 is not a permitted outbound target"})
        );
        assert_eq!(
            query_wire::fetched(fetch_messages(&ctx, ChannelId::new(42), before, after, 2).await),
            json!({"error":"cannot specify both 'before' and 'after'"})
        );
    }

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

        let resp =
            query_wire::new_since(fetch_new_since(&ctx, channel_id, after_message_id, 20).await);
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
            #[test]
            fn new_since_response_invariants(ids in ids_strategy(), limit in 0u8..=100) {
                let resp = query_wire::new_since(Ok(NewMessages::new(&test_config(), batch_from_ids(&ids), limit).unwrap()));

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
        assert!(err.to_string().contains("cannot specify both"));
    }

    #[test]
    fn build_fetch_response_sorts_oldest_first() {
        let msgs = newest_first_batch();
        let resp =
            query_wire::fetched(Ok(FetchedMessages::new(&test_config(), msgs, None).unwrap()));
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
        let resp = query_wire::pins(Ok(PinnedMessages::new(
            &test_config(),
            newest_first_batch(),
        )
        .unwrap()));
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
        let resp = query_wire::pins(Ok(PinnedMessages::new(&test_config(), Vec::new()).unwrap()));

        assert_eq!(resp["messages"], json!([]));
        assert_eq!(resp["count"], 0);
    }

    #[test]
    fn build_fetch_response_no_metadata_without_pagination() {
        let msgs = newest_first_batch();
        let resp =
            query_wire::fetched(Ok(FetchedMessages::new(&test_config(), msgs, None).unwrap()));
        assert!(resp.get("count").is_none());
        assert!(resp.get("has_more").is_none());
    }

    #[test]
    fn build_fetch_response_includes_metadata_with_pagination() {
        let msgs = newest_first_batch();
        let resp = query_wire::fetched(Ok(
            FetchedMessages::new(&test_config(), msgs, Some(20)).unwrap()
        ));
        assert_eq!(resp["count"], 3);
        assert_eq!(resp["has_more"], false);
    }

    #[test]
    fn build_fetch_response_has_more_when_full_page() {
        let msgs = newest_first_batch();
        let resp = query_wire::fetched(Ok(
            FetchedMessages::new(&test_config(), msgs, Some(3)).unwrap()
        ));
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
        let resp = query_wire::fetched(Ok(
            FetchedMessages::new(&test_config(), msgs, Some(20)).unwrap()
        ));
        assert_eq!(resp["count"], 1);
        assert_eq!(resp["has_more"], false);
    }

    #[test]
    fn build_fetch_response_empty_page() {
        let resp = query_wire::fetched(Ok(
            FetchedMessages::new(&test_config(), vec![], Some(20)).unwrap()
        ));
        assert_eq!(resp["count"], 0);
        assert_eq!(resp["has_more"], false);
        assert!(resp["messages"].as_array().unwrap().is_empty());
    }
}

#[cfg(test)]
mod observation_tests {
    use super::{
        super::tests::{test_config, wire_message},
        *,
    };
    use serde_json::json;

    #[test]
    fn observation_timestamp_preserves_offset_instant_and_nanoseconds() {
        let message: Message = serde_json::from_value(wire_message(
            7,
            "",
            "2026-07-15T12:34:56.123456789+05:30",
            json!([]),
        ))
        .unwrap();
        let original =
            MessageView::from_discord(&test_config(), &message, ObservationOrigin::Exact).unwrap();
        assert_eq!(
            original.timestamp().to_rfc3339(),
            "2026-07-15T12:34:56.123456789+05:30"
        );
        let config = LoadedConfig::from_raw(crate::config::Config {
            timezone: Some("UTC".into()),
            ..Default::default()
        });
        let utc = MessageView::from_discord(&config, &message, ObservationOrigin::Exact).unwrap();
        assert_eq!(
            utc.timestamp().to_rfc3339(),
            "2026-07-15T07:04:56.123456789+00:00"
        );
        let config = LoadedConfig::from_raw(crate::config::Config {
            timezone: Some("America/Chicago".into()),
            ..Default::default()
        });
        let local = MessageView::from_discord(&config, &message, ObservationOrigin::Exact).unwrap();
        assert_eq!(
            local.timestamp().to_rfc3339(),
            "2026-07-15T02:04:56.123456789-05:00"
        );
    }

    #[test]
    fn observation_unformattable_timestamp_is_an_error_instead_of_now() {
        let mut message: Message = serde_json::from_value(wire_message(
            7,
            "private content",
            "2026-07-15T12:00:00Z",
            json!([]),
        ))
        .unwrap();
        message.timestamp =
            serenity::model::Timestamp::from_unix_timestamp(-62_167_219_201).unwrap();
        let error = MessageView::from_discord(&test_config(), &message, ObservationOrigin::Exact)
            .unwrap_err();
        assert!(matches!(error.kind, ObservationErrorKind::TimestampFormat));
        assert_eq!(
            error.to_string(),
            "invalid Discord observation for channel 1080000000000000001, message 7: timestamp cannot be formatted as RFC 3339"
        );
        assert!(FetchedMessages::new(&test_config(), vec![message.clone()], None).is_err());
        assert!(NewMessages::new(&test_config(), vec![message.clone()], 20).is_err());
        assert!(PinnedMessages::new(&test_config(), vec![message]).is_err());
    }
}

#[cfg(test)]
mod context_tests {
    use super::{
        super::tests::{test_config, wire_message},
        *,
    };
    use crate::discord::query_wire;
    use serde_json::json;

    fn source() -> ModuleInstanceId {
        ModuleInstanceId::new(
            core::ProviderId::new("discord").unwrap(),
            core::ModuleSlotId::new("secondary").unwrap(),
        )
    }

    #[test]
    fn malformed_edit_is_rejected_only_by_context_projection() {
        let mut message: Message = serde_json::from_value(wire_message(
            7,
            "observed",
            "2026-07-15T12:00:00Z",
            json!([]),
        ))
        .unwrap();
        message.edited_timestamp =
            Some(serenity::model::Timestamp::from_unix_timestamp(-62_167_219_201).unwrap());
        let view =
            MessageView::from_discord(&test_config(), &message, ObservationOrigin::Exact).unwrap();
        assert_eq!(query_wire::message(&view)["content"], "observed");
        assert!(matches!(
            view.context(&source()).unwrap_err().kind,
            ObservationErrorKind::EditedTimestampFormat
        ));
        let exact = query_wire::retrieved_v2(
            Ok(RetrievedMessage {
                message: view,
                receipt: None,
            }),
            &source(),
        );
        assert_eq!(exact["error"]["code"], "invalid_observation");
        assert!(exact.get("message").is_none());
        let legacy_page =
            FetchedMessages::new(&test_config(), vec![message.clone()], None).unwrap();
        assert_eq!(
            query_wire::fetched(Ok(legacy_page))["messages"][0]["content"],
            "observed"
        );
        let page = FetchedMessages::new(&test_config(), vec![message], None).unwrap();
        assert_eq!(
            query_wire::fetched_v2(Ok(page), &source())["error"]["code"],
            "invalid_observation"
        );
    }

    #[test]
    fn context_normalizes_instants_without_losing_precision_or_source_scope() {
        let mut raw = wire_message(
            7,
            "unchanged",
            "2026-07-15T12:34:56.123456789+05:30",
            json!([]),
        );
        raw["edited_timestamp"] = json!("2026-07-15T12:35:57.987654321+05:30");
        let message: Message = serde_json::from_value(raw).unwrap();
        let context = MessageView::from_discord(&test_config(), &message, ObservationOrigin::Exact)
            .unwrap()
            .context(&source())
            .unwrap();
        assert_eq!(
            context
                .created_at()
                .to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true),
            "2026-07-15T07:04:56.123456789Z"
        );
        let Availability::Known(Some(edited)) = context.edited_at() else {
            panic!("expected observed edit")
        };
        assert_eq!(
            edited.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true),
            "2026-07-15T07:05:57.987654321Z"
        );
        assert_eq!(context.reference().conversation().module(), &source());
        assert_eq!(context.content(), "unchanged");
        assert_eq!(context.attachments(), &Availability::Known(vec![]));
        assert_eq!(
            context.relation(),
            &Availability::Unavailable(UnavailableReason::NotReported)
        );
    }

    #[test]
    fn conflicting_reference_does_not_reassign_resolved_sender() {
        let mut raw = wire_message(7, "reply", "2026-07-15T12:00:00Z", json!([]));
        raw["type"] = json!(19);
        raw["message_reference"] =
            json!({"type":0,"message_id":"3","channel_id":"100","guild_id":"500"});
        raw["referenced_message"] = wire_message(
            4,
            "private reference body",
            "2026-07-15T11:00:00Z",
            json!([]),
        );
        let message: Message = serde_json::from_value(raw).unwrap();
        let view =
            MessageView::from_discord(&test_config(), &message, ObservationOrigin::Exact).unwrap();
        let context = view.context(&source()).unwrap();
        let Availability::Known(relation) = context.relation() else {
            panic!("expected reference")
        };
        assert_eq!(relation.kind(), ReferenceRelation::Reply);
        assert_eq!(relation.consistency(), ReferenceConsistency::Conflicts);
        let Availability::Known(resolved) = relation.resolved() else {
            panic!("expected resolved reference")
        };
        assert_eq!(resolved.reference().local().as_numeric().unwrap().get(), 4);
        assert_eq!(
            resolved.sender().local().as_numeric().unwrap().get(),
            message.author.id.get()
        );
        let output = query_wire::retrieved_v2(
            Ok(RetrievedMessage {
                message: view,
                receipt: None,
            }),
            &source(),
        );
        assert!(!output.to_string().contains("private reference body"));
    }
}
