//! MCP projections of typed Discord messaging outcomes.
use crate::{
    contradictionary::AutoRewrite,
    discord::messaging::{
        DeliveryReceipt, DmDeliveryRoute, DmOutcome, MessagingError, MessagingErrorKind,
        ReplyOutcome, Report, TargetError,
    },
    evidence::SentexRole,
    no_rly::{
        consent::{BounceTicket, RejectedHandle, Released, Rephrased},
        delivery::{DeliveryProgress, PostedMessages},
        queue::HoldHandle,
    },
};
use serde_json::{Value, json};
use serenity::model::id::MessageId;
/// Make every `auto` rewrite visible to the sender: one
/// `auto: <match> → <replace>` line each, under `auto_rewrites`.
fn add_auto_rewrite_lines(response: &mut Value, rewrites: &[AutoRewrite]) {
    if !rewrites.is_empty() {
        response["auto_rewrites"] =
            json!(rewrites.iter().map(AutoRewrite::line).collect::<Vec<_>>());
    }
}

/// The construct-facing shape of a bounce: the error names the reason, and
/// the `held` block carries the handle plus the three verbs.
pub(crate) fn bounce_json(ticket: &BounceTicket) -> Value {
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

/// Map a handle rejection to the construct-facing error shape. A still-live
/// handle (failed send) says so explicitly, so the construct knows a retry
/// is possible.
pub(crate) fn rejected_handle_json(error: RejectedHandle) -> Value {
    match error {
        RejectedHandle::SendFailed {
            ref handle,
            ref expires_in,
            ref progress,
            ..
        } => {
            let mut value = json!({
                "error": error.to_string(),
                "handle_still_live": handle,
                "expires_in_secs": expires_in.as_secs(),
            });
            match progress {
                DeliveryProgress::None => {}
                DeliveryProgress::Partial(sent) => add_partial_receipt(&mut value, sent),
            }
            value
        }
        RejectedHandle::Unknown(_) | RejectedHandle::Expired { .. } => {
            json!({ "error": error.to_string() })
        }
    }
}

fn add_partial_receipt(value: &mut Value, sent: &PostedMessages) {
    value["partially_delivered"] = json!(true);
    value["message_ids"] = json!(sent.iter().map(|id| id.get()).collect::<Vec<_>>());
}

fn receipt(delivery: DeliveryReceipt) -> Value {
    let mut value = json!({"ok": true, "message_ids": delivery.message_ids.iter().map(|id| id.get()).collect::<Vec<_>>()});
    for (role, key) in [
        (SentexRole::Claim, "claim_locators"),
        (SentexRole::Citation, "citation_locators"),
    ] {
        let locators = delivery
            .locators
            .iter()
            .filter(|locator| locator.role() == role)
            .map(crate::evidence::SentexLocator::as_str)
            .collect::<Vec<_>>();
        if !locators.is_empty() {
            value[key] = json!(locators);
        }
    }
    value
}

pub(crate) fn error(error: MessagingError) -> Value {
    let mut value = match error.kind {
        MessagingErrorKind::Target(target) => target_error(target),
        MessagingErrorKind::Handle(handle) => rejected_handle_json(handle),
        MessagingErrorKind::HookHalted { reason, feedback } => {
            json!({"error": reason, "pre_send_feedback": feedback})
        }
        cause @ (MessagingErrorKind::OutboundDenied(_)
        | MessagingErrorKind::Pipeline(_)
        | MessagingErrorKind::PipelineTask(_)
        | MessagingErrorKind::HooksNotRegistered
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
        | MessagingErrorKind::Discord(_)
        | MessagingErrorKind::SendAttachment(_)
        | MessagingErrorKind::ReadFile(_)
        | MessagingErrorKind::Observation(_)
        | MessagingErrorKind::FetchMessage(_)
        | MessagingErrorKind::CreateInbox(_)
        | MessagingErrorKind::ConflictingPagination
        | MessagingErrorKind::NoAttachments
        | MessagingErrorKind::RelativeFilePath
        | MessagingErrorKind::FileNotPermitted
        | MessagingErrorKind::ZeroEmojiId(_)) => json!({"error": cause.to_string()}),
    };
    add_auto_rewrite_lines(&mut value, &error.auto_rewrites);
    value
}

pub(crate) fn target_error(error: TargetError) -> Value {
    let mut value = json!({"error": error.to_string()});
    value["reason"] = json!(match error {
        TargetError::IngressRequired => "ingress_required",
        TargetError::CanonicalRequiresIngress => "canonical_requires_ingress",
        TargetError::CanonicalMismatch => "canonical_mismatch",
        TargetError::GuildLookup(_) | TargetError::MessageLookup(_) => "canonical_lookup_failed",
        TargetError::NotFound { .. } => "target_not_found",
        TargetError::ChannelMismatch {
            message,
            admitted,
            claimed,
            operation,
        } => {
            value["message_id"] = json!(message.to_string());
            value["admitted_channel_id"] = json!(admitted.to_string());
            value["claimed_channel_id"] = json!(claimed.to_string());
            value["operation"] = json!(operation.as_str());
            value["blocked"] = json!(true);
            "channel_mismatch"
        }
    });
    value
}

pub(crate) fn reply(result: Result<Report<ReplyOutcome>, MessagingError>) -> Value {
    match result {
        Ok(report) => {
            let mut value = match report.outcome {
                ReplyOutcome::Delivered(delivery) => receipt(delivery),
                ReplyOutcome::Held(ticket) => bounce_json(&ticket),
                ReplyOutcome::PartiallyDelivered(partial) => {
                    let mut value = json!({"error": partial.cause.to_string()});
                    add_partial_receipt(&mut value, &partial.sent);
                    value
                }
            };
            add_auto_rewrite_lines(&mut value, &report.auto_rewrites);
            value
        }
        Err(cause) => error(cause),
    }
}

pub(crate) fn dm(result: Result<Report<DmOutcome>, MessagingError>) -> Value {
    match result {
        Ok(report) => {
            let mut value = match report.outcome {
                DmOutcome::Delivered { delivery, route } => {
                    let mut value = receipt(delivery);
                    match route {
                        DmDeliveryRoute::Direct(channel) => {
                            value["channel_id"] = json!(channel.to_string())
                        }
                        DmDeliveryRoute::Redirected => {}
                    }
                    value
                }
                DmOutcome::Held(ticket) => bounce_json(&ticket),
                DmOutcome::PartiallyDelivered(partial) => {
                    let mut value = json!({"error": partial.cause.to_string()});
                    add_partial_receipt(&mut value, &partial.sent);
                    value
                }
            };
            add_auto_rewrite_lines(&mut value, &report.auto_rewrites);
            value
        }
        Err(cause) => error(cause),
    }
}

pub(crate) fn release(handle: &HoldHandle, result: Result<Released, RejectedHandle>) -> Value {
    match result {
        Ok(released) => {
            json!({"ok": true, "message_ids": released.message_ids.iter().map(|id| id.get()).collect::<Vec<_>>(), "released": handle, "latency_ms": released.latency_ms})
        }
        Err(cause) => rejected_handle_json(cause),
    }
}
pub(crate) fn rephrase(
    handle: &HoldHandle,
    result: Result<Report<Rephrased>, MessagingError>,
) -> Value {
    match result {
        Ok(report) => {
            let mut value = match report.outcome {
                Rephrased::Sent { message_ids } => {
                    json!({"ok": true, "message_ids": message_ids.iter().map(|id| id.get()).collect::<Vec<_>>(), "rephrased": handle})
                }
                Rephrased::ReBounced(ticket) => bounce_json(&ticket),
            };
            add_auto_rewrite_lines(&mut value, &report.auto_rewrites);
            value
        }
        Err(cause) => error(cause),
    }
}

pub(crate) fn message(result: Result<MessageId, MessagingError>) -> Value {
    match result {
        Ok(id) => json!({"ok": true, "message_id": id.to_string()}),
        Err(cause) => error(cause),
    }
}
pub(crate) fn unit(result: Result<(), MessagingError>) -> Value {
    match result {
        Ok(()) => json!({"ok": true}),
        Err(cause) => error(cause),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        discord::messaging::PartialReceipt,
        no_rly::{
            delivery::{DeliveryCause, DeliveryProgress, PostedMessages},
            judge::{ReasonEntry, RejectReason},
        },
    };
    use std::time::Duration;
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
    #[test]
    fn delivery_ids_are_numeric_and_dm_route_is_success_only() {
        for (route, expected) in [
            (
                DmDeliveryRoute::Direct(serenity::model::id::ChannelId::new(42)),
                json!({"ok":true,"message_ids":[101],"channel_id":"42"}),
            ),
            (
                DmDeliveryRoute::Redirected,
                json!({"ok":true,"message_ids":[101]}),
            ),
        ] {
            let result = Ok(Report {
                outcome: DmOutcome::Delivered {
                    delivery: DeliveryReceipt {
                        message_ids: vec![MessageId::new(101)],
                        locators: Vec::new(),
                    },
                    route,
                },
                auto_rewrites: Vec::new(),
            });
            assert_eq!(dm(result), expected);
        }
        let partial = crate::discord::messaging::PartialReceipt {
            sent: crate::no_rly::delivery::PostedMessages::new(MessageId::new(101), vec![]),
            cause: crate::no_rly::delivery::DeliveryCause::ChunkSend {
                index: 1,
                source: serenity::Error::Other("connection lost").into(),
            },
        };
        assert_eq!(
            dm(Ok(Report {
                outcome: DmOutcome::PartiallyDelivered(partial),
                auto_rewrites: vec![]
            })),
            json!({"error":"failed to send chunk 1: connection lost","partially_delivered":true,"message_ids":[101]})
        );
    }

    #[test]
    fn retry_failure_preserves_partial_progress_and_live_handle() {
        let failure = RejectedHandle::SendFailed {
            handle: HoldHandle::new("nr-test-1"),
            error: crate::no_rly::delivery::DeliveryCause::ChunkSend {
                index: 1,
                source: serenity::Error::Other("connection lost").into(),
            },
            progress: crate::no_rly::delivery::DeliveryProgress::Partial(
                crate::no_rly::delivery::PostedMessages::new(MessageId::new(101), vec![]),
            ),
            expires_in: std::time::Duration::from_secs(30),
        };
        assert_eq!(
            rejected_handle_json(failure),
            json!({"error":"send failed: failed to send chunk 1: connection lost (handle nr-test-1 still live for 30s)","handle_still_live":"nr-test-1","expires_in_secs":30,"partially_delivered":true,"message_ids":[101]})
        );
    }

    #[test]
    fn reply_partial_receipt_preserves_order_and_rewrites() {
        assert_eq!(
            reply(Ok(Report {
                outcome: ReplyOutcome::PartiallyDelivered(PartialReceipt {
                    sent: PostedMessages::new(MessageId::new(u64::MAX), vec![MessageId::new(101)]),
                    cause: DeliveryCause::ChunkSend {
                        index: 2,
                        source: serenity::Error::Other("connection lost").into(),
                    },
                }),
                auto_rewrites: vec![AutoRewrite {
                    pattern: "easy".into(),
                    matched: "easy".into(),
                    replacement: "possible".into(),
                }],
            })),
            json!({"error":"failed to send chunk 2: connection lost","partially_delivered":true,"message_ids":[18_446_744_073_709_551_615_u64,101],"auto_rewrites":["auto: easy → possible"]})
        );
    }

    #[test]
    fn release_and_rephrase_preserve_only_current_attempt_progress() {
        for partial in [false, true] {
            for rephrased in [false, true] {
                let handle = HoldHandle::new("nr-test-2");
                let failure = RejectedHandle::SendFailed {
                    handle: handle.clone(),
                    error: DeliveryCause::ChunkSend {
                        index: 2,
                        source: serenity::Error::Other("connection lost").into(),
                    },
                    progress: if partial {
                        DeliveryProgress::Partial(PostedMessages::new(
                            MessageId::new(103),
                            vec![MessageId::new(101)],
                        ))
                    } else {
                        DeliveryProgress::None
                    },
                    expires_in: Duration::from_secs(30),
                };
                let projected = if rephrased {
                    rephrase(&handle, Err(MessagingErrorKind::Handle(failure).into()))
                } else {
                    release(&handle, Err(failure))
                };
                let expected = if partial {
                    json!({"error":"send failed: failed to send chunk 2: connection lost (handle nr-test-2 still live for 30s)","handle_still_live":"nr-test-2","expires_in_secs":30,"partially_delivered":true,"message_ids":[103,101]})
                } else {
                    json!({"error":"send failed: failed to send chunk 2: connection lost (handle nr-test-2 still live for 30s)","handle_still_live":"nr-test-2","expires_in_secs":30})
                };
                assert_eq!(projected, expected);
            }
        }
    }

    #[test]
    fn reply_complete_held_and_rejected_do_not_claim_partial_delivery() {
        assert_eq!(
            reply(Ok(Report {
                outcome: ReplyOutcome::Delivered(DeliveryReceipt {
                    message_ids: vec![MessageId::new(101)],
                    locators: vec![],
                }),
                auto_rewrites: vec![],
            })),
            json!({"ok":true,"message_ids":[101]})
        );
        assert_eq!(
            reply(Err(MessagingErrorKind::OutboundDenied(
                serenity::model::id::ChannelId::new(42),
            )
            .into())),
            json!({"error":"channel 42 is not a permitted outbound target"})
        );
        assert_eq!(
            reply(Ok(Report {
                outcome: ReplyOutcome::Held(BounceTicket {
                    handle: HoldHandle::new("nr-3f92-7"),
                    reason: bounce_reason(),
                    expires_in: Duration::from_secs(180),
                    parent: None,
                }),
                auto_rewrites: vec![],
            })),
            json!({
                "error":"⚠️ held by contradictionary: straightforward (nothing is ever straightforward)",
                "held": {
                    "handle":"nr-3f92-7",
                    "reason":{"matches":[{"pattern":"straightforward","reason":"nothing is ever straightforward"}]},
                    "expires_in_secs":180,
                    "next":"no_rly(handle) sends it verbatim; rephrase(handle, content) sends a replacement (re-checked); ignoring it lets it expire"
                }
            })
        );
    }

    #[test]
    fn halted_feedback_is_present_even_when_empty() {
        assert_eq!(
            error(
                MessagingErrorKind::HookHalted {
                    reason: "declined".into(),
                    feedback: vec![]
                }
                .into()
            ),
            json!({"error":"declined","pre_send_feedback":[]})
        );
        assert_eq!(
            error(MessagingErrorKind::EmptyRephrase.into()),
            json!({"error":"rephrase content must not be empty"})
        );
    }

    #[test]
    fn target_operation_spellings_and_mismatch_fields_are_literal() {
        use crate::{discord::messaging::TargetOperation, ingress_ledger::ChannelRef};
        for (operation, spelling) in [
            (TargetOperation::ReplyTo, "reply_to"),
            (TargetOperation::React, "react"),
            (TargetOperation::EditMessage, "edit_message"),
            (TargetOperation::DeleteMessage, "delete_message"),
            (TargetOperation::PinMessage, "pin_message"),
            (TargetOperation::UnpinMessage, "unpin_message"),
            (TargetOperation::CreateThread, "create_thread"),
        ] {
            let rendered = target_error(TargetError::ChannelMismatch {
                message: MessageId::new(7),
                admitted: ChannelRef::new(42),
                claimed: ChannelRef::new(43),
                operation,
            });
            assert_eq!(
                rendered,
                json!({"error":format!("message 7 was admitted in channel 42, not claimed channel 43; {spelling} blocked"),"reason":"channel_mismatch","message_id":"7","admitted_channel_id":"42","claimed_channel_id":"43","operation":spelling,"blocked":true})
            );
        }
    }
}
