//! MCP response shapes for typed Discord query results.

use crate::{
    core::{
        ActorRef, ConversationRef, LocalIdentityRef, MessageRef, ModuleInstanceId, WorkspaceRef,
        context::{
            Availability, HistoryDirection, LocalTruncation, ObservationOrigin, PageMetadata,
            ProviderCompleteness, ReferenceConsistency, ReferenceObservation, ReferenceRelation,
            UnavailableReason,
        },
    },
    discord::{
        messaging::{
            MessagingError, MessagingErrorKind,
            queries::{
                DiscordNativeObservation, FetchedMessages, MessageView, NewMessages,
                PinnedMessages, RetrievedMessage,
            },
        },
        messaging_wire,
    },
    evidence::project_offered_sentexes,
    vaelii::{GetMessageReceipt, VaeliiError},
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use camino::Utf8PathBuf;
use serde_json::{Value, json};
use serenity::http::HttpError;

/// Projects an exact read with explicit observation and receipt states.
pub(crate) fn retrieved_v2(
    result: Result<RetrievedMessage, MessagingError>,
    source: &ModuleInstanceId,
) -> Value {
    let result = match result {
        Ok(result) => result,
        Err(error) => return query_error(&error),
    };
    match context_message(result.message(), source) {
        Ok(message) => json!({
            "response_version": 2, "status": "ok", "message": message,
            "receipt": receipt_v2(result.receipt()),
        }),
        Err(_) => ContextError::InvalidObservation.project(),
    }
}

/// Projects history with the actual requested direction and effective limit.
pub(crate) fn fetched_v2(
    result: Result<FetchedMessages, MessagingError>,
    source: &ModuleInstanceId,
) -> Value {
    let result = match result {
        Ok(result) => result,
        Err(error) => return query_error(&error),
    };
    let messages = result
        .messages()
        .iter()
        .map(|message| context_message(message, source))
        .collect::<Result<Vec<_>, _>>();
    match messages {
        Ok(messages) => json!({
            "response_version": 2, "status": "ok",
            "page": page(&result.context_page(source)), "messages": messages,
        }),
        Err(_) => ContextError::InvalidObservation.project(),
    }
}

fn receipt_v2(receipt: Option<Result<&GetMessageReceipt, &VaeliiError>>) -> Value {
    match receipt {
        None => json!({"status": "not_requested"}),
        Some(Ok(receipt)) => json!({
            "status": "recorded", "invocation": receipt.invocation, "receipt": receipt.receipt,
        }),
        Some(Err(_)) => json!({
            "status": "failed", "code": "receipt_failed",
            "message": "The read succeeded, but its evidence receipt could not be recorded.",
        }),
    }
}

enum ContextError {
    LocalAccessDenied,
    ProviderAccessDenied,
    ProviderNotFound,
    ProviderTimeout,
    ProviderError,
    InvalidObservation,
    InvalidQuery,
    QueryFailed,
}

impl ContextError {
    fn project(self) -> Value {
        let (code, message) = match self {
            Self::LocalAccessDenied => (
                "local_access_denied",
                "Current policy does not permit this read.",
            ),
            Self::ProviderAccessDenied => {
                ("provider_access_denied", "The provider denied this read.")
            }
            Self::ProviderNotFound => (
                "provider_not_found",
                "The provider did not find the requested resource.",
            ),
            Self::ProviderTimeout => ("provider_timeout", "The provider request timed out."),
            Self::ProviderError => ("provider_error", "The provider request failed."),
            Self::InvalidObservation => (
                "invalid_observation",
                "The provider observation cannot be represented safely.",
            ),
            Self::InvalidQuery => ("invalid_query", "The query arguments conflict."),
            Self::QueryFailed => ("query_failed", "The read could not be completed."),
        };
        json!({"response_version": 2, "status": "error", "error": {"code": code, "message": message}})
    }
}

fn query_error(error: &MessagingError) -> Value {
    match &error.kind {
        MessagingErrorKind::OutboundDenied(_) => ContextError::LocalAccessDenied,
        MessagingErrorKind::Observation(_) => ContextError::InvalidObservation,
        MessagingErrorKind::ConflictingPagination => ContextError::InvalidQuery,
        MessagingErrorKind::Discord(error) | MessagingErrorKind::FetchMessage(error) => {
            provider_error(error)
        }
        MessagingErrorKind::Target(_)
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
        | MessagingErrorKind::SendAttachment(_)
        | MessagingErrorKind::ReadFile(_)
        | MessagingErrorKind::CreateInbox(_)
        | MessagingErrorKind::NoAttachments
        | MessagingErrorKind::RelativeFilePath
        | MessagingErrorKind::FileNotPermitted
        | MessagingErrorKind::ZeroEmojiId(_) => ContextError::QueryFailed,
    }
    .project()
}

fn provider_error(error: &serenity::Error) -> ContextError {
    match error {
        serenity::Error::Http(HttpError::UnsuccessfulRequest(response)) => {
            match response.status_code.as_u16() {
                403 => ContextError::ProviderAccessDenied,
                404 => ContextError::ProviderNotFound,
                _ => ContextError::ProviderError,
            }
        }
        serenity::Error::Http(HttpError::Request(error)) if error.is_timeout() => {
            ContextError::ProviderTimeout
        }
        _ => ContextError::ProviderError,
    }
}

fn source_reference(source: &ModuleInstanceId) -> Value {
    json!({"provider": source.provider().as_str(), "slot": source.slot().as_str()})
}

fn local_identity(identity: LocalIdentityRef<'_>) -> Value {
    match identity {
        LocalIdentityRef::Numeric(value) => {
            json!({"kind": "numeric", "value": value.get().to_string()})
        }
        LocalIdentityRef::Opaque(value) => json!({"kind": "opaque", "value": value}),
    }
}

fn conversation_reference(reference: &ConversationRef) -> Value {
    json!({"source": source_reference(reference.module()),
        "conversation": local_identity(reference.local().identity())})
}

fn message_reference(reference: &MessageRef) -> Value {
    let mut value = conversation_reference(reference.conversation());
    value["message"] = local_identity(reference.local().identity());
    value
}

fn actor_reference(reference: &ActorRef) -> Value {
    json!({"source": source_reference(reference.module()),
        "actor": local_identity(reference.local().identity())})
}

fn workspace_reference(reference: &WorkspaceRef) -> Value {
    json!({"source": source_reference(reference.module()),
        "workspace": local_identity(reference.local().identity())})
}

fn availability<T>(value: &Availability<T>, project: impl FnOnce(&T) -> Value) -> Value {
    match value {
        Availability::Known(value) => json!({"status": "known", "value": project(value)}),
        Availability::Unavailable(reason) => {
            json!({"status": "unavailable", "reason": match reason {
                UnavailableReason::NotReported => "not_reported",
                UnavailableReason::NotResolved => "not_resolved",
                UnavailableReason::Unsupported => "unsupported",
            }})
        }
    }
}

fn relation(value: &ReferenceObservation) -> Value {
    json!({
        "kind": match value.kind() { ReferenceRelation::Reply => "reply", ReferenceRelation::Forward => "forward", ReferenceRelation::Unknown => "unknown" },
        "target": availability(value.target(), message_reference),
        "resolved": availability(value.resolved(), |resolved| json!({"reference": message_reference(resolved.reference()), "sender": actor_reference(resolved.sender())})),
        "consistency": match value.consistency() {
            ReferenceConsistency::NoResolvedObservation => "no_resolved_observation",
            ReferenceConsistency::ResolvedOnly => "resolved_only",
            ReferenceConsistency::InsufficientEvidence => "insufficient_evidence",
            ReferenceConsistency::Matches => "matches",
            ReferenceConsistency::Conflicts => "conflicts",
        },
    })
}

fn context_message(
    message: &MessageView,
    source: &ModuleInstanceId,
) -> Result<Value, crate::discord::messaging::queries::ObservationError> {
    let context = message.context(source)?;
    let sender = context.sender();
    Ok(json!({
        "reference": message_reference(context.reference()),
        "workspace": availability(context.workspace(), workspace_reference),
        "sender": {"reference": actor_reference(sender.reference()), "name": sender.name(),
            "bot": availability(sender.bot(), |value| json!(value)),
            "represented_identity": availability(sender.represented_identity(), actor_reference)},
        "content": context.content(),
        "created_at": context.created_at().to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true),
        "edited_at": availability(context.edited_at(), |value| json!(value.as_ref().map(|time| time.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true)))),
        "relation": availability(context.relation(), relation),
        "attachments": availability(context.attachments(), |attachments| json!(attachments.iter().map(|attachment| json!({
            "name": attachment.name(), "media_type": availability(attachment.media_type(), |media_type| json!(media_type)), "size_bytes": attachment.size_bytes(),
        })).collect::<Vec<_>>())),
        "observation": {"origin": match context.observation().origin() {
            ObservationOrigin::GatewayCreate => "gateway_create", ObservationOrigin::GatewayEdit => "gateway_edit", ObservationOrigin::Exact => "exact_read", ObservationOrigin::History => "history", ObservationOrigin::Pins => "pins", ObservationOrigin::NewSince => "new_since", ObservationOrigin::Search => "search",
        }, "local_truncation": match context.observation().local_truncation() { LocalTruncation::None => "none" },
        "provider_completeness": match context.observation().provider_completeness() { ProviderCompleteness::Unknown => "unknown" }},
        "native": native(message.native()),
    }))
}

fn page(page: &PageMetadata) -> Value {
    let query = page.query();
    let (direction, anchor) = match query.direction() {
        HistoryDirection::Recent => ("recent", None),
        HistoryDirection::Before(anchor) => ("before", Some(message_reference(anchor))),
        HistoryDirection::After(anchor) => ("after", Some(message_reference(anchor))),
    };
    json!({"conversation": conversation_reference(query.conversation()),
        "query": {"direction": direction, "requested_anchor": anchor, "effective_limit": query.limit()},
        "count": page.count(), "oldest_observed": page.oldest().map(message_reference),
        "newest_observed": page.newest().map(message_reference), "has_more_hint": page.has_more_hint()})
}

fn native(native: &DiscordNativeObservation) -> Value {
    json!({"provider": "discord", "message_type": u8::from(native.message_type()),
        "channel_id": native.channel_id().get().to_string(), "message_id": native.message_id().get().to_string(),
        "guild_id": native.guild_id().map(|id| id.get().to_string()),
        "author": {"id": native.author_id().get().to_string(), "username": native.author_name(), "bot": native.author_bot(),
            "webhook_id": native.webhook_id().map(|id| id.get().to_string()), "application_id": native.application_id().map(|id| id.get().to_string())},
        "reference": native.reference().map(|reference| json!({"kind": u8::from(reference.kind),
            "channel_id": reference.channel_id.get().to_string(), "message_id": reference.message_id.map(|id| id.get().to_string()),
            "guild_id": reference.guild_id.map(|id| id.get().to_string()), "fail_if_not_exists": reference.fail_if_not_exists})),
        "resolved_reference": native.resolved_reference().map(|reference| json!({
            "channel_id": reference.channel_id().get().to_string(), "message_id": reference.message_id().get().to_string(),
            "guild_id": reference.guild_id().map(|id| id.get().to_string()), "author_id": reference.author_id().get().to_string()})),
        "attachments": native.attachments().iter().map(|attachment| json!({
            "id": attachment.id.get().to_string(), "filename": attachment.filename, "description": attachment.description,
            "height": attachment.height, "width": attachment.width, "proxy_url": attachment.proxy_url, "size": attachment.size,
            "url": attachment.url, "content_type": attachment.content_type, "ephemeral": attachment.ephemeral,
            "duration_secs": attachment.duration_secs, "waveform": attachment.waveform.as_ref().map(|bytes| json!({"encoding": "base64", "value": STANDARD.encode(bytes)})),
        })).collect::<Vec<_>>()})
}

/// Projects fetched history while preserving optional cursor metadata.
pub(crate) fn fetched(result: Result<FetchedMessages, MessagingError>) -> Value {
    match result {
        Ok(result) => {
            let mut projected =
                json!({"messages": result.messages().iter().map(message).collect::<Vec<_>>()});
            if let Some(has_more) = result.has_more() {
                projected["count"] = json!(result.messages().len());
                projected["has_more"] = json!(has_more);
            }
            projected
        }
        Err(error) => messaging_wire::error(error),
    }
}

/// Projects a forward history page and its pagination hint.
pub(crate) fn new_since(result: Result<NewMessages, MessagingError>) -> Value {
    match result {
        Ok(result) => json!({
            "messages": result.messages().iter().map(message).collect::<Vec<_>>(),
            "count": result.messages().len(),
            "has_more": result.has_more(),
        }),
        Err(error) => messaging_wire::error(error),
    }
}

/// Projects pinned observations and their count.
pub(crate) fn pins(result: Result<PinnedMessages, MessagingError>) -> Value {
    match result {
        Ok(result) => json!({
            "messages": result.messages().iter().map(message).collect::<Vec<_>>(),
            "count": result.messages().len(),
        }),
        Err(error) => messaging_wire::error(error),
    }
}

/// Projects successfully saved attachment paths.
pub(crate) fn downloaded(result: Result<Vec<Utf8PathBuf>, MessagingError>) -> Value {
    match result {
        Ok(paths) => json!({"saved": paths}),
        Err(error) => messaging_wire::error(error),
    }
}

/// Projects a single observation and its optional evidence receipt.
pub(crate) fn retrieved(result: Result<RetrievedMessage, MessagingError>) -> Value {
    let result = match result {
        Ok(result) => result,
        Err(error) => return messaging_wire::error(error),
    };
    let mut projected = project_message(result.message(), AttachmentProjection::Details);
    match result.receipt() {
        Some(Ok(receipt)) => {
            projected["vaelii_receipt"] = json!({
                "ok": true, "invocation": receipt.invocation, "receipt": receipt.receipt,
            })
        }
        Some(Err(error)) => {
            projected["vaelii_receipt"] = json!({"ok": false, "error": error.to_string()})
        }
        None => {}
    }
    projected
}

enum AttachmentProjection {
    Summary,
    Details,
}

/// Projects an observed message with summary attachment metadata.
pub(crate) fn message(message: &MessageView) -> Value {
    project_message(message, AttachmentProjection::Summary)
}

fn project_message(message: &MessageView, attachments: AttachmentProjection) -> Value {
    let mut projected = json!({
        "id": message.id().get().to_string(),
        "author": message.author_name(),
        "author_id": message.author_id().get().to_string(),
        "content": message.content(),
        "timestamp": message.timestamp(),
        "attachments": message.attachments().iter().map(|attachment| match attachments {
            AttachmentProjection::Summary => json!({
                "name": attachment.name(), "url": attachment.url(), "size": attachment.size().get(),
            }),
            AttachmentProjection::Details => json!({
                "name": attachment.name(), "url": attachment.url(), "size": attachment.size().get(),
                "content_type": attachment.content_type(),
            }),
        }).collect::<Vec<_>>(),
    });
    if let Some(id) = message.reply_to_message_id() {
        projected["reply_to_message_id"] = json!(id.get().to_string());
    }
    if let Some(id) = message.reply_to_user_id() {
        projected["reply_to_user_id"] = json!(id.get().to_string());
    }
    project_offered_sentexes(&mut projected, message.offered_sentexes());
    projected
}

#[cfg(test)]
mod context_tests {
    use super::*;
    use crate::core::{ConversationId, MessageId, ModuleSlotId, ProviderId};
    use std::time::Duration;

    fn source() -> ModuleInstanceId {
        ModuleInstanceId::new(
            ProviderId::new("discord").unwrap(),
            ModuleSlotId::new("secondary").unwrap(),
        )
    }

    #[test]
    fn context_identity_preserves_numeric_and_opaque_forms_and_slot() {
        let numeric = MessageRef::new(
            ConversationRef::new(source(), ConversationId::numeric(100.try_into().unwrap())),
            MessageId::numeric(1002.try_into().unwrap()),
        );
        let opaque = MessageRef::new(
            ConversationRef::new(source(), ConversationId::opaque("100").unwrap()),
            MessageId::opaque("1002").unwrap(),
        );
        assert_eq!(
            message_reference(&numeric),
            json!({"source": {"provider": "discord", "slot": "secondary"}, "conversation": {"kind": "numeric", "value": "100"}, "message": {"kind": "numeric", "value": "1002"}})
        );
        assert_eq!(
            message_reference(&opaque)["conversation"],
            json!({"kind": "opaque", "value": "100"})
        );
        assert_eq!(
            message_reference(&opaque)["message"],
            json!({"kind": "opaque", "value": "1002"})
        );
    }

    #[test]
    fn context_receipt_failure_never_projects_private_error_text() {
        let error = VaeliiError::InvalidSymbol {
            field: "private",
            value: "https://private.invalid/token message body".into(),
        };
        assert_eq!(
            receipt_v2(Some(Err(&error))),
            json!({"status": "failed", "code": "receipt_failed", "message": "The read succeeded, but its evidence receipt could not be recorded."})
        );
        assert_eq!(receipt_v2(None), json!({"status": "not_requested"}));
        let receipt = GetMessageReceipt {
            invocation: "fixture-invocation".into(),
            receipt: "fixture-receipt".into(),
        };
        assert_eq!(
            receipt_v2(Some(Ok(&receipt))),
            json!({"status": "recorded", "invocation": "fixture-invocation", "receipt": "fixture-receipt"})
        );
    }

    #[test]
    fn context_errors_use_fixed_messages_instead_of_provider_or_owner_text() {
        let hostile = serenity::Error::Other("https://private.invalid/token message body");
        let error: MessagingError = MessagingErrorKind::FetchMessage(Box::new(hostile)).into();
        assert_eq!(
            query_error(&error),
            json!({"response_version": 2, "status": "error", "error": {"code": "provider_error", "message": "The provider request failed."}})
        );
        let error: MessagingError =
            MessagingErrorKind::CoordinationUnavailable("secret".into()).into();
        assert_eq!(
            query_error(&error)["error"],
            json!({"code": "query_failed", "message": "The read could not be completed."})
        );
        let error: MessagingError = MessagingErrorKind::ConflictingPagination.into();
        assert_eq!(
            query_error(&error)["error"],
            json!({"code": "invalid_query", "message": "The query arguments conflict."})
        );
        let error: MessagingError =
            MessagingErrorKind::OutboundDenied(serenity::all::ChannelId::new(100)).into();
        assert_eq!(
            query_error(&error)["error"],
            json!({"code": "local_access_denied", "message": "Current policy does not permit this read."})
        );
    }

    #[tokio::test]
    async fn context_timeout_classifies_the_typed_request_cause() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_millis(20))
            .build()
            .unwrap();
        let (accepted, response) = tokio::time::timeout(Duration::from_secs(2), async {
            tokio::join!(
                listener.accept(),
                client.get(format!("http://{address}/private-token")).send(),
            )
        })
        .await
        .expect("the request has its own bounded timeout");
        let (_stream, _) = accepted.unwrap();
        let error = response.unwrap_err();
        assert!(error.is_timeout());
        let error: MessagingError = MessagingErrorKind::Discord(Box::new(serenity::Error::Http(
            serenity::http::HttpError::Request(error),
        )))
        .into();
        assert_eq!(
            query_error(&error),
            json!({"response_version": 2, "status": "error", "error": {"code": "provider_timeout", "message": "The provider request timed out."}})
        );
        drop(listener);
    }

    #[test]
    fn context_availability_preserves_known_null_empty_and_unavailable() {
        assert_eq!(
            availability(&Availability::Known(None::<bool>), |value| json!(value)),
            json!({"status": "known", "value": null})
        );
        assert_eq!(
            availability(&Availability::Known(Vec::<bool>::new()), |value| json!(
                value
            )),
            json!({"status": "known", "value": []})
        );
        assert_eq!(
            availability::<bool>(
                &Availability::Unavailable(UnavailableReason::Unsupported),
                |value| json!(value)
            ),
            json!({"status": "unavailable", "reason": "unsupported"})
        );
    }
}
