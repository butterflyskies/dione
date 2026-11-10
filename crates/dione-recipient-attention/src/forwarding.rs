//! Complete recipient admission, forwarding and shutdown lifecycle.
use crate::runtime::AttentionRuntime;
#[cfg(test)]
use camino::Utf8PathBuf;
use dione_attention::delivery::AttentionGuardFailure;
use dione_attention::types::{Admission, DeliveryState};
use dione_bell_rings::BellEvaluator;
use dione_contracts::ModuleInstanceId;
use dione_delivery_scheduler::BufferResult;
use dione_discord_bells::prepare_bells;
use dione_discord_events::{MessageEvent, NotificationEvent};
use dione_discord_scheduling::DeliveryBuffer;
#[cfg(test)]
use dione_harness::AttentionPolicy;
use dione_harness::{HarnessConnection, HarnessError};
use dione_messages::delivery::{Delivery, DeliveryKind, EvidenceVisibility, PresentationContext};
use dione_outbound_consent::consent::ConsentGate;
use dione_rate_limiter::{ChannelRef, ParticipantId, RateLimitDecision, RateLimiter};
#[cfg(test)]
use serde_json::{Value, json};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Runs recipient delivery until cancellation or input closure, joining owned work before flushing.
pub async fn run_notifications(
    attention: Arc<AttentionRuntime>,
    notification_sink: HarnessConnection,
    notice_no_rly: Arc<ConsentGate>,
    notice_event_tx: Option<mpsc::Sender<NotificationEvent>>,
    event_rx: mpsc::Receiver<NotificationEvent>,
    cancel_notif: CancellationToken,
) {
    // Both rate limiter and delivery buffer configs are live-reloadable
    // via the ArcSwap config cache. Rate limiter config is refreshed per
    // event; existing bucket state is preserved across config changes.
    let config = dione_config::load_config(attention.resolver.directory());
    let mut rate_limiter = RateLimiter::new(config.rate_limit_runtime().clone());
    let mut delivery_buffer = DeliveryBuffer::new(attention.authority_module().clone());
    let bell_evaluator = Arc::new(BellEvaluator::new());

    // Lazy-init: providers connect on first message, no pre-warming.

    // Resolve timezone once at startup so `deliver_flushed` doesn't need to
    // load config just for the tz. Updated opportunistically when we already
    // load config per-event for the rate limiter.
    let initial_tz = config.tz;
    let initial_evidence_markers_enabled = config.delivery.evidence_markers_enabled;

    let state_dir_notif = attention.resolver.directory().clone();

    let mut rx = event_rx;
    let mut events_since_prune: u64 = 0;
    let mut tz = initial_tz;
    let mut evidence_markers_enabled = initial_evidence_markers_enabled;
    const PRUNE_INTERVAL: u64 = 100;
    let mut admissions = dione_attention_admission::AdmissionController::default();
    let mut evaluations = tokio::task::JoinSet::new();
    let mut attention_config = dione_config::subscribe_generation();
    let mut maintenance = tokio::task::JoinSet::new();
    let mut notices = tokio::task::JoinSet::new();
    let mut attention_tick = tokio::time::interval(Duration::from_secs(1));
    attention_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut next_maintenance = tokio::time::Instant::now();

    loop {
        let flush_deadline = delivery_buffer.next_flush_deadline();

        tokio::select! {
            biased;

            // Cancellation takes priority — break to drain path.
            _ = cancel_notif.cancelled() => {
                tracing::debug!("notif_task: cancellation received, draining buffer");
                break;
            }

            _ = attention_config.changed() => {
                let ready = attention_transition(&mut admissions, &attention).await;
                if let Err(error) = forward_attention_results(
                    ready,
                    &mut admissions,
                    &attention,
                    &mut delivery_buffer,
                    &bell_evaluator,
                    &notification_sink,
                )
                .await
                {
                    tracing::error!(%error, "attention transition delivery failed"); cancel_notif.cancel(); break;
                }
            }
            completed = evaluations.join_next(), if !evaluations.is_empty() => {
                match completed {
                    Some(Ok(record)) => {
                        let cfg = dione_config::load_config(&state_dir_notif);
                        let ready = admissions.complete(record, &cfg);
                        if let Err(error) = forward_attention_results(
                            ready,
                            &mut admissions,
                            &attention,
                            &mut delivery_buffer,
                            &bell_evaluator,
                            &notification_sink,
                        )
                        .await
                        {
                            tracing::error!(%error, "attention admission delivery failed"); cancel_notif.cancel(); break;
                        }
                    }
                    Some(Err(_)) => {
                        attention.health.unknown("attention worker failed");
                        let ready = attention_transition(&mut admissions, &attention).await;
                        if let Err(error) = forward_attention_results(
                            ready,
                            &mut admissions,
                            &attention,
                            &mut delivery_buffer,
                            &bell_evaluator,
                            &notification_sink,
                        )
                        .await
                        {
                            tracing::error!(%error, "attention worker fallback failed"); cancel_notif.cancel(); break;
                        }
                    }
                    None => {}
                }
            }

            _ = attention_tick.tick() => {
                let ready = admissions.retry_ready();
                if let Err(error) = forward_attention_results(
                    ready,
                    &mut admissions,
                    &attention,
                    &mut delivery_buffer,
                    &bell_evaluator,
                    &notification_sink,
                )
                .await
                {
                    tracing::error!(%error, "attention retry delivery failed"); cancel_notif.cancel(); break;
                }
                let now = tokio::time::Instant::now();
                if maintenance.is_empty() && now >= next_maintenance {
                    let runtime = attention.clone();
                    let incarnation = admissions.incarnation().to_owned();
                    let cfg = dione_config::load_config(&state_dir_notif);
                    next_maintenance = now + Duration::from_millis(cfg.raw.attention.revalidate_ms);
                    maintenance.spawn(async move {
                        runtime.clone().maintain().await;
                        runtime.recover_pending(incarnation).await
                    });
                }
                if notices.is_empty() {
                    notices.spawn(attention.clone().publish_notice(
                        notice_no_rly.clone(), notice_event_tx.clone(),
                    ));
                }
            }
            completed = maintenance.join_next(), if !maintenance.is_empty() => {
                match completed {
                    Some(Ok(ready)) => {
                        if let Err(error) = forward_attention_results(
                            ready,
                            &mut admissions,
                            &attention,
                            &mut delivery_buffer,
                            &bell_evaluator,
                            &notification_sink,
                        )
                        .await
                        {
                            tracing::error!(%error, "attention recovery delivery failed"); cancel_notif.cancel(); break;
                        }
                    }
                    Some(Err(_)) => attention.health.unknown("attention recovery worker failed"),
                    None => {}
                }
            }
            _ = notices.join_next(), if !notices.is_empty() => {}

            // Flush deadline fires — drain and coalesce buffered events.
            _ = async {
                match flush_deadline {
                    Some(deadline) => tokio::time::sleep_until(deadline).await,
                    None => std::future::pending::<()>().await,
                }
            } => {
                let now = tokio::time::Instant::now();
                let flushed = delivery_buffer.flush_ready(now);
                let outcome = deliver_flushed(
                    &notification_sink,
                    delivery_buffer.module(),
                    flushed,
                    tz,
                    evidence_markers_enabled,
                ).await;
                // Requeue undelivered events so flush_all picks them up.
                delivery_buffer.requeue(outcome.undelivered);
                if let Some(error) = outcome.error {
                    tracing::error!(error = %error, "inbound delivery failed; shutting down");
                    cancel_notif.cancel();
                    break;
                }
            }

            // New event arrives from Discord.
            event = rx.recv() => {
                let Some(event) = event else { break };

                // Reload config from ArcSwap (cheap Arc pointer load).
                let cfg = dione_config::load_config(&state_dir_notif);

                // Keep tz in sync with config changes so flushes use
                // the current value without a separate config load.
                tz = cfg.tz;
                evidence_markers_enabled = cfg.delivery.evidence_markers_enabled;

                // Live-reload rate limiter config before the check so
                // changes apply to the current event, not the next one.
                let new_rl_config = cfg.rate_limit_runtime();
                if new_rl_config != rate_limiter.config_ref() {
                    tracing::info!("rate limiter config changed, applying");
                    rate_limiter.update_config(new_rl_config.clone());
                }


                // MessageEvent.user_id is already the admitted effective
                // participant, independent of transport.
                if let Some(key) = message_rate_limit_key(&event) {
                    let now = Instant::now();
                    match rate_limiter.check_message(&key.participant, &key.channel, &[], now) {
                        RateLimitDecision::Allowed { remaining, .. } => {
                            tracing::trace!(
                                user_id = key.user_id,
                                chat_id = key.chat_id,
                                remaining,
                                "rate limiter: message allowed"
                            );
                        }
                        RateLimitDecision::Unavailable { reason } => {
                            tracing::warn!(%reason, "rate limiter unavailable, dropping message");
                            continue;
                        }
                        RateLimitDecision::Denied { retry_after, overflow: _ } => {
                            // All denied messages are dropped for now.
                            // OverflowPolicy::Buffer is accepted by config but not
                            // yet implemented — see #79 for sender class wiring.
                            tracing::info!(
                                user_id = key.user_id,
                                chat_id = key.chat_id,
                                retry_after_ms = %retry_after.as_millis(),
                                "rate limiter: message denied, dropping"
                            );
                            continue;
                        }
                    }
                }

                use dione_attention_admission::Submission;
                if let NotificationEvent::MessageEdit { chat_id, message_id, .. }
                    | NotificationEvent::MessageDelete { chat_id, message_id, .. } = &event {
                    let source = dione_attention::types::SourceKey { channel_id: *chat_id, message_id: *message_id };
                    let ready = admissions.invalidate(source);
                    if let Err(error) = attention.invalidate(source).await { attention.health.unknown(error.to_string()); }
                    let scoped_source = dione_contracts::MessageRef::new(
                        dione_contracts::ConversationRef::new(attention.authority_module().clone(), dione_contracts::ConversationId::numeric(source.channel_id.into())),
                        dione_contracts::MessageId::numeric(source.message_id.into()),
                    );
                    if let Err(error) = notification_sink.invalidate_attention(scoped_source).await {
                        attention.health.unknown(error.to_string());
                    }
                    if let Err(error) = forward_attention_results(
                        ready,
                        &mut admissions,
                        &attention,
                        &mut delivery_buffer,
                        &bell_evaluator,
                        &notification_sink,
                    )
                    .await
                    {
                        tracing::error!(%error, "attention lifecycle delivery failed"); cancel_notif.cancel(); break;
                    }
                }
                let submission = begin_attention(&mut admissions, attention.as_ref(), event, &cfg);
                let ordinary = match submission {
                    Submission::Ordinary(event) => Some(event),
                    Submission::Unknown { event, .. } => Some(event),
                    Submission::Duplicate => None,
                    Submission::Judge { work, ordinary } => {
                        evaluations.spawn(attention.clone().evaluate(*work));
                        ordinary
                    }
                    Submission::Saturated { event } => Some(event),
                };
                if let Some(event) = ordinary
                    && let Err(error) = forward_ordinary(event, &cfg, &mut delivery_buffer, &bell_evaluator, &notification_sink).await {
                    tracing::error!(%error, "inbound delivery failed; shutting down"); cancel_notif.cancel(); break;
                }

                // Periodically prune idle rate limiter buckets to bound memory.
                events_since_prune += 1;
                if events_since_prune >= PRUNE_INTERVAL {
                    events_since_prune = 0;
                    rate_limiter.prune_idle(Instant::now());
                }
            }
        }
    }
    drop(admissions);
    evaluations.abort_all();
    maintenance.abort_all();
    notices.abort_all();
    while maintenance.join_next().await.is_some() {}
    while notices.join_next().await.is_some() {}
    while evaluations.join_next().await.is_some() {}

    // Channel closed — flush any remaining buffered events.
    let remaining = delivery_buffer.flush_all();
    let outcome = deliver_flushed(
        &notification_sink,
        delivery_buffer.module(),
        remaining,
        tz,
        evidence_markers_enabled,
    )
    .await;
    if !outcome.undelivered.is_empty() {
        tracing::error!(
            dropped = outcome.undelivered.len(),
            "final flush: {} evidence events could not be delivered",
            outcome.undelivered.len()
        );
    }
    if let Some(error) = outcome.error {
        tracing::error!(error = %error, "failed to persist final inbound events");
        cancel_notif.cancel();
    }
}

/// Outcome of attempting to deliver a batch of flushed events.
struct DeliverOutcome {
    /// First error encountered, if any.
    error: Option<HarnessError>,
    /// Evidence events that could not be delivered due to a mid-FIFO sink
    /// error. The caller should re-buffer these to prevent silent drops.
    undelivered: Vec<NotificationEvent>,
}

impl DeliverOutcome {
    fn ok() -> Self {
        Self {
            error: None,
            undelivered: Vec::new(),
        }
    }

    #[cfg(test)]
    fn into_result(self) -> Result<(), HarnessError> {
        match self.error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

fn presentation(tz: Option<chrono_tz::Tz>, evidence: bool) -> PresentationContext {
    PresentationContext::new(
        tz,
        if evidence {
            EvidenceVisibility::Enabled
        } else {
            EvidenceVisibility::Disabled
        },
    )
}

async fn deliver_flushed(
    sink: &HarnessConnection,
    module: &ModuleInstanceId,
    events: Vec<NotificationEvent>,
    tz: Option<chrono_tz::Tz>,
    evidence_markers_enabled: bool,
) -> DeliverOutcome {
    let mut iterator = events.iter().cloned();
    let Some(first) = iterator.next() else {
        return DeliverOutcome::ok();
    };
    let context = presentation(tz, evidence_markers_enabled);
    let delivery = match dione_discord_events::batch(module, first, iterator.collect(), context) {
        Ok(delivery) => delivery,
        Err(error) => {
            return DeliverOutcome {
                error: Some(HarnessError::source(error)),
                undelivered: events,
            };
        }
    };
    if evidence_markers_enabled
        && events.iter().any(NotificationEvent::has_offered_evidence)
        && let DeliveryKind::Ordinary { first, rest } = delivery.kind()
    {
        let mut events = events;
        for (index, event) in std::iter::once(first).chain(rest).enumerate() {
            let single = Delivery::single(event.clone(), context);
            if let Err(error) = sink.deliver(&single).await {
                let retry_from = if error.completion_is_unknown() {
                    index + 1
                } else {
                    index
                };
                return DeliverOutcome {
                    error: Some(error),
                    undelivered: events.split_off(retry_from),
                };
            }
        }
        return DeliverOutcome::ok();
    }
    match sink.deliver(&delivery).await {
        Ok(()) => DeliverOutcome::ok(),
        Err(error) => DeliverOutcome {
            error: Some(error),
            undelivered: Vec::new(),
        },
    }
}

struct MessageRateLimitKey {
    participant: ParticipantId,
    channel: ChannelRef,
    user_id: u64,
    chat_id: u64,
}

fn message_rate_limit_key(event: &NotificationEvent) -> Option<MessageRateLimitKey> {
    let NotificationEvent::Message(MessageEvent {
        user_id, chat_id, ..
    }) = event
    else {
        return None;
    };
    let user_id = user_id.get();
    let chat_id = chat_id.get();
    Some(MessageRateLimitKey {
        participant: ParticipantId::new(user_id.to_string()),
        channel: ChannelRef::new(chat_id.to_string()),
        user_id,
        chat_id,
    })
}

/// Extracts the configured duration at the Discord configuration boundary.
///
/// Returns the configured delay for channel events (Message, MessageEdit,
/// MessageDelete, Reaction). Control events have no delay.
fn extract_delay(event: &NotificationEvent, config: &dione_config::LoadedConfig) -> Duration {
    let channel_id = match event {
        NotificationEvent::Message(msg) => Some(msg.chat_id.get()),
        NotificationEvent::MessageEdit { chat_id, .. }
        | NotificationEvent::MessageDelete { chat_id, .. }
        | NotificationEvent::Reaction { chat_id, .. } => Some(chat_id.get()),
        NotificationEvent::PermissionResponse { .. }
        | NotificationEvent::Trace { .. }
        | NotificationEvent::ConfigError { .. }
        | NotificationEvent::Coordination(_) => None,
    };
    match channel_id {
        Some(id) => Duration::from_millis(config.delivery_delay_ms(id)),
        None => Duration::ZERO,
    }
}

/// The same forwarding unit serves off, log, direct, and failure fallback paths.
async fn forward_ordinary(
    event: NotificationEvent,
    cfg: &dione_config::LoadedConfig,
    buffer: &mut DeliveryBuffer,
    evaluator: &Arc<BellEvaluator>,
    sink: &HarnessConnection,
) -> Result<(), HarnessError> {
    let event = prepare_bells(event, cfg, evaluator).await;
    let evidence = cfg.delivery.evidence_markers_enabled;
    let delay = extract_delay(&event, cfg);
    match buffer.buffer_event_with_evidence(event, delay, evidence) {
        BufferResult::Immediate(event) => {
            let delivery = match dione_discord_events::single(
                buffer.module(),
                (*event).clone(),
                presentation(cfg.tz, evidence),
            ) {
                Ok(delivery) => delivery,
                Err(error) => {
                    buffer.requeue(vec![*event]);
                    return Err(HarnessError::source(error));
                }
            };
            sink.deliver(&delivery).await
        }
        BufferResult::FlushThenImmediate { preceding, event } => {
            let outcome = deliver_flushed(sink, buffer.module(), preceding, cfg.tz, evidence).await;
            if let Some(error) = outcome.error {
                // Requeue is a prepend: keep retained predecessors before the
                // new event, excluding any completion-unknown predecessor.
                let mut retained = outcome.undelivered;
                retained.push(*event);
                buffer.requeue(retained);
                return Err(error);
            }
            buffer.requeue(outcome.undelivered);
            let delivery = match dione_discord_events::single(
                buffer.module(),
                (*event).clone(),
                presentation(cfg.tz, evidence),
            ) {
                Ok(delivery) => delivery,
                Err(error) => {
                    buffer.requeue(vec![*event]);
                    return Err(HarnessError::source(error));
                }
            };
            sink.deliver(&delivery).await
        }
        BufferResult::Buffered => Ok(()),
        BufferResult::Rejected { event, error } => {
            buffer.requeue(vec![*event]);
            Err(HarnessError::source(error))
        }
    }
}

async fn forward_attention_results(
    results: Vec<dione_attention_admission::AdmissionResult>,
    admissions: &mut dione_attention_admission::AdmissionController,
    attention: &Arc<crate::runtime::AttentionRuntime>,
    buffer: &mut DeliveryBuffer,
    evaluator: &Arc<BellEvaluator>,
    sink: &HarnessConnection,
) -> Result<(), HarnessError> {
    let mut results = results.into_iter();
    while let Some(mut result) = results.next() {
        let cfg = dione_config::load_config(attention.resolver.directory());
        if result.record.recipient != cfg.raw.attention.recipient {
            continue;
        }
        if result.record.delivery == DeliveryState::Invalidated {
            continue;
        }
        result.event.reply_to_content_preview = None;
        let Some(source) = result.record.sources.first() else {
            continue;
        };
        match attention.resolver.ledger().delivery_check(
            source.key.message_id,
            source.key.channel_id,
            &source.content_hash,
            &cfg,
            result.event.targeting,
        ) {
            Ok(()) => {}
            Err(dione_discord_runtime::SourceDeliveryFailure::Unavailable) => {
                attention
                    .health
                    .unknown("pending source authority is unavailable");
                let mut deferred = Vec::with_capacity(results.len() + 1);
                deferred.push(result);
                deferred.extend(results);
                admissions.defer(deferred);
                return Ok(());
            }
            Err(dione_discord_runtime::SourceDeliveryFailure::Invalidated) => {
                if let Err(error) = attention
                    .mark_delivery(result.record.id, DeliveryState::Invalidated)
                    .await
                {
                    attention.health.unknown(error.to_string());
                }
                continue;
            }
        }
        match result.record.actual {
            Admission::RetrievalOnly => {}
            Admission::Prompt | Admission::NextTurn => {
                if !attention.enforcement_supported {
                    attention
                        .health
                        .unknown("consumer has no safe-turn capability");
                    continue;
                }
                // Retain the original source-bound event until the nonblocking final
                // guard accepts it; temporary store contention returns it to the FIFO.
                let mut message = result.event.clone();
                message.reply_to_content_preview = None;
                let event =
                    prepare_bells(NotificationEvent::Message(message), &cfg, evaluator).await;
                // Keep the managed envelope separate while draining older same-room
                // ordinary work. Direct sink delivery would overtake its buffer.
                let event = match buffer.buffer_event_with_evidence(
                    event,
                    Duration::from_millis(0),
                    cfg.delivery.evidence_markers_enabled,
                ) {
                    BufferResult::Immediate(event) => *event,
                    BufferResult::FlushThenImmediate { preceding, event } => {
                        let outcome = deliver_flushed(
                            sink,
                            buffer.module(),
                            preceding,
                            cfg.tz,
                            cfg.delivery.evidence_markers_enabled,
                        )
                        .await;
                        buffer.requeue(outcome.undelivered);
                        if let Some(error) = outcome.error {
                            return Err(error);
                        }
                        *event
                    }
                    BufferResult::Buffered | BufferResult::Rejected { .. } => {
                        unreachable!("zero-delay events are immediate")
                    }
                };
                let NotificationEvent::Message(managed_message) = event else {
                    return Err(HarnessError::source(std::io::Error::other(
                        "managed attention delivery lost its source message",
                    )));
                };
                let directive = if result.record.actual == Admission::Prompt {
                    dione_attention::delivery::AttentionDirective::Prompt
                } else {
                    dione_attention::delivery::AttentionDirective::NextTurn
                };
                let notification = Delivery::managed(
                    dione_discord_events::message(attention.authority_module(), managed_message),
                    directive,
                    result.record.id.clone(),
                    presentation(cfg.tz, cfg.delivery.evidence_markers_enabled),
                );
                let Some(descriptor) = notification.attention() else {
                    return Err(HarnessError::source(std::io::Error::other(
                        "managed attention descriptor is unavailable",
                    )));
                };
                // A managed event must remain a source-bound envelope; coalescing
                // it with an ordinary event would erase its lifecycle fence.
                match dione_attention::delivery::AttentionDeliveryGuard::check(
                    attention.as_ref(),
                    &descriptor,
                ) {
                    Ok(()) => sink.deliver(&notification).await?,
                    Err(AttentionGuardFailure::Unavailable) => {
                        attention
                            .health
                            .unknown("pending source authority is unavailable");
                        let mut deferred = Vec::with_capacity(results.len() + 1);
                        deferred.push(result);
                        deferred.extend(results);
                        admissions.defer(deferred);
                        return Ok(());
                    }
                    Err(AttentionGuardFailure::Invalidated) => {
                        if let Err(error) = attention
                            .mark_delivery(result.record.id, DeliveryState::Invalidated)
                            .await
                        {
                            attention.health.unknown(error.to_string());
                        }
                    }
                }
            }
            Admission::Ordinary | Admission::Unknown => {
                forward_ordinary(
                    NotificationEvent::Message(result.event),
                    &cfg,
                    buffer,
                    evaluator,
                    sink,
                )
                .await?;
                // Observed means the ordinary queue owns delivery, not that a model
                // or Discord recipient has acknowledged seeing the event.
                if let Err(error) = attention
                    .mark_delivery(result.record.id, DeliveryState::Observed)
                    .await
                {
                    attention.health.unknown(error.to_string());
                }
            }
        }
    }
    Ok(())
}

async fn attention_transition(
    admissions: &mut dione_attention_admission::AdmissionController,
    attention: &Arc<crate::runtime::AttentionRuntime>,
) -> Vec<dione_attention_admission::AdmissionResult> {
    let _publication = dione_config::attention_effect_guard().await;
    admissions.cancel_work();
    let incarnation = admissions.incarnation().to_owned();
    let recipient = dione_config::load_config(attention.resolver.directory())
        .raw
        .attention
        .recipient
        .clone();
    let committed = attention
        .with_store(move |store| {
            Ok(store
                .list_records(&recipient)
                .into_iter()
                .filter(|record| {
                    record.incarnation == incarnation
                        && record.judgment.is_some()
                        && matches!(
                            record.delivery,
                            DeliveryState::Admitted
                                | DeliveryState::Deferred
                                | DeliveryState::Observed
                        )
                })
                .cloned()
                .collect())
        })
        .await;
    let committed = match committed {
        Ok(records) => records,
        Err(_) => {
            attention
                .health
                .unknown("attention transition metadata unavailable");
            Vec::new()
        }
    };
    admissions.configuration_changed(committed)
}

fn begin_attention(
    admissions: &mut dione_attention_admission::AdmissionController,
    attention: &crate::runtime::AttentionRuntime,
    event: NotificationEvent,
    config: &dione_config::LoadedConfig,
) -> dione_attention_admission::Submission {
    use dione_attention_admission::Submission;
    let duplicate = match &event {
        NotificationEvent::Message(message) => {
            admissions.owns_source(&dione_attention::types::SourceKey {
                channel_id: message.chat_id,
                message_id: message.message_id,
            })
        }
        _ => false,
    };
    let submission = if duplicate {
        Submission::Duplicate
    } else if attention.health.probe_due() {
        admissions.submit(
            event,
            config,
            dione_discord_runtime::now_ms(),
            attention.enforcement_supported,
        )
    } else {
        Submission::Ordinary(event)
    };
    match &submission {
        Submission::Unknown { reason, .. } => attention.health.unknown(*reason),
        Submission::Saturated { .. } => attention
            .health
            .unknown("attention evaluation capacity reached"),
        _ => {}
    }
    submission
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness_test_support::FixtureNotification;
    use dione_config::ChannelConfig;
    use dione_config::Config;
    use dione_config::LoadedConfig;
    use dione_discord_scheduling::DeliveryBuffer;
    use dione_messages::timestamp::Timestamp;
    use dione_provider_delivery::ReplyHandle;
    use dione_rate_limiter::OverflowPolicy;
    use dione_rate_limiter::RateLimitConfig;
    use dione_rate_limiter::ScopeConfig;
    use serenity::model::id::{ChannelId, MessageId, UserId};
    use std::collections::HashMap;

    fn config_with_channel_delay(channel_id: u64, delay_ms: u64) -> LoadedConfig {
        let mut raw = Config::default();
        raw.channels.push(ChannelConfig {
            id: channel_id.to_string(),
            delivery_delay_ms: Some(delay_ms),
            ..Default::default()
        });
        LoadedConfig::try_from_raw(raw).expect("valid test configuration")
    }

    fn config_with_global_delay(delay_ms: u64) -> LoadedConfig {
        let mut raw = Config::default();
        raw.delivery.delivery_delay_ms = delay_ms;
        LoadedConfig::try_from_raw(raw).expect("valid test configuration")
    }

    fn message_event(channel: u64) -> NotificationEvent {
        NotificationEvent::Message(MessageEvent {
            chat_id: ChannelId::new(channel),
            message_id: MessageId::new(1),
            user: "u".into(),
            user_id: UserId::new(1),
            author_kind: dione_attention::types::SourceAuthorKind::DirectHuman,
            content: "c".into(),
            targeting: dione_discord_events::MessageTargeting::Ambient,
            timestamp: Timestamp::parse("2026-01-01T00:00:00Z").unwrap(),
            attachments: vec![],
            is_voice_message: false,
            thread_parent_id: None,
            reply_to_message_id: None,
            reply_to_user_id: None,
            reply_to_user: None,
            reply_to_content_preview: None,
            bells: None,
            bells_status: None,
        })
    }

    pub(super) fn evidence_message_event(index: u64) -> NotificationEvent {
        NotificationEvent::Message(MessageEvent {
            chat_id: ChannelId::new(42),
            message_id: MessageId::new(100 + index),
            user: format!("user-{index}"),
            user_id: UserId::new(200 + index),
            author_kind: dione_attention::types::SourceAuthorKind::DirectHuman,
            content: format!("claim-{index} [🔍=v1:AAAAAAAAAAw] [🔍=v1:AAAAAAAAACI]"),
            targeting: dione_discord_events::MessageTargeting::Ambient,
            timestamp: Timestamp::parse("2026-01-01T00:00:00Z").unwrap(),
            attachments: vec![],
            is_voice_message: false,
            thread_parent_id: None,
            reply_to_message_id: None,
            reply_to_user_id: None,
            reply_to_user: None,
            reply_to_content_preview: None,
            bells: None,
            bells_status: None,
        })
    }

    #[tokio::test]
    async fn ordinary_failed_predecessor_flush_keeps_tail_before_current() {
        for unknown in [false, true] {
            let mut raw = Config::default();
            raw.delivery.evidence_markers_enabled = true;
            raw.channels.push(ChannelConfig {
                id: "42".to_owned(),
                delivery_delay_ms: Some(500),
                ..Default::default()
            });
            let config = LoadedConfig::try_from_raw(raw).unwrap();
            let module = ModuleInstanceId::new(
                ProviderId::new("discord").unwrap(),
                ModuleSlotId::new("primary").unwrap(),
            );
            let mut buffer = DeliveryBuffer::new(module.clone());
            buffer.requeue(vec![evidence_message_event(0), evidence_message_event(1)]);
            let (writer, reader) = tokio::io::duplex(4096);
            drop(reader);
            let output = dione_harness::ProtocolOutput::new(writer);
            if !unknown {
                // The first failed frame is conservatively unknown. Once poisoned,
                // later frames are rejected before any new write starts.
                assert!(
                    output
                        .write(&serde_json::json!({"probe":true}))
                        .await
                        .unwrap_err()
                        .completion_is_unknown()
                );
            }
            let sink = dione_claude::connect(output, dione_codex::TOOL_NAMES);
            let error = forward_ordinary(
                evidence_message_event(2),
                &config,
                &mut buffer,
                &Arc::new(BellEvaluator::new()),
                &sink,
            )
            .await
            .unwrap_err();
            assert_eq!(error.completion_is_unknown(), unknown);
            let retained = buffer.flush_all();
            let ids: Vec<_> = retained
                .iter()
                .map(|event| match event {
                    NotificationEvent::Message(message) => message.message_id.get(),
                    _ => panic!("message retained"),
                })
                .collect();
            let expected = if unknown {
                vec![101, 102]
            } else {
                vec![100, 101, 102]
            };
            assert_eq!(ids, expected);
            let (writer, reader) = tokio::io::duplex(8192);
            let recovered = dione_claude::connect(
                dione_harness::ProtocolOutput::new(writer),
                dione_codex::TOOL_NAMES,
            );
            let mut lines = tokio::io::AsyncBufReadExt::lines(tokio::io::BufReader::new(reader));
            let outcome = deliver_flushed(&recovered, &module, retained, None, true).await;
            assert!(outcome.error.is_none());
            assert!(outcome.undelivered.is_empty());
            for id in expected {
                let delivered: Value =
                    serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
                assert_eq!(delivered["params"]["meta"]["message_id"], id.to_string());
            }
        }
    }

    #[tokio::test]
    async fn unknown_admission_does_not_requeue_ambiguous_event_but_retains_tail() {
        let mut owner = crate::codex_test_process::OwnerProcess::start().await;
        let module = ModuleInstanceId::new(
            ProviderId::new("discord").unwrap(),
            ModuleSlotId::new("primary").unwrap(),
        );
        let sink = owner.connection();
        owner
            .command(serde_json::json!({"command":"pause_completion","lose_response":true}))
            .await;
        let events = vec![evidence_message_event(0), evidence_message_event(1)];
        let first_module = module.clone();
        let producer =
            tokio::spawn(
                async move { deliver_flushed(&sink, &first_module, events, None, true).await },
            );
        owner
            .command(serde_json::json!({"command":"completion_reached"}))
            .await;
        owner
            .command(serde_json::json!({"command":"completion_release"}))
            .await;
        let outcome = producer.await.unwrap();
        assert!(outcome.error.as_ref().unwrap().completion_is_unknown());
        assert_eq!(outcome.undelivered.len(), 1);
        let NotificationEvent::Message(tail) = &outcome.undelivered[0] else {
            panic!("literal message tail")
        };
        assert_eq!(tail.message_id, MessageId::new(101));
        assert_eq!(
            owner.command(serde_json::json!({"command":"status"})).await["queued"],
            1
        );
        let committed = owner
            .command(serde_json::json!({"command":"queued_payloads"}))
            .await;
        assert_eq!(committed[0]["params"]["meta"]["message_id"], "100");
        let recovered = deliver_flushed(
            &owner.connection(),
            &module,
            outcome.undelivered,
            None,
            true,
        )
        .await;
        assert!(recovered.error.is_none());
        assert_eq!(
            owner.command(serde_json::json!({"command":"status"})).await["queued"],
            2
        );
        let committed = owner
            .command(serde_json::json!({"command":"queued_payloads"}))
            .await;
        assert_eq!(committed[0]["params"]["meta"]["message_id"], "100");
        assert_eq!(committed[1]["params"]["meta"]["message_id"], "101");
        owner.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn evidence_burst_overflow_reaches_real_sink_losslessly_in_fifo_order() {
        let dir = tempfile::TempDir::new().unwrap();
        let state_dir = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        let queue = dione_codex::CodexEventQueue::load(
            &state_dir,
            dione_codex::LegacyDiscordScope::new(dione_contracts::ModuleInstanceId::new(
                dione_contracts::ProviderId::new("discord").unwrap(),
                dione_contracts::ModuleSlotId::new("primary").unwrap(),
            )),
        )
        .unwrap();
        let consumer = queue
            .register_consumer(
                "evidence overflow test".to_owned(),
                Duration::from_secs(60),
                true,
                true,
            )
            .await
            .unwrap()
            .consumer_id;
        let sink = test_connection_for_queue(queue.clone());
        let mut buffer = DeliveryBuffer::new(ModuleInstanceId::new(
            ProviderId::new("discord").unwrap(),
            ModuleSlotId::new("primary").unwrap(),
        ));

        for index in 0..6 {
            match buffer.buffer_event(evidence_message_event(index), Duration::from_millis(500)) {
                BufferResult::Immediate(event) => {
                    sink.deliver(
                        &dione_discord_events::single(
                            buffer.module(),
                            *event,
                            presentation(None, true),
                        )
                        .unwrap(),
                    )
                    .await
                    .unwrap();
                }
                BufferResult::Buffered => {
                    assert!(index >= 4, "only budget overflow may be delayed");
                }
                BufferResult::Rejected { error, .. } => {
                    panic!("valid fixture delay rejected: {error}")
                }
                BufferResult::FlushThenImmediate { .. } => {
                    panic!("one uninterrupted burst has no buffered predecessor recovery")
                }
            }
        }

        tokio::time::advance(Duration::from_millis(500)).await;
        let overflow = buffer.flush_ready(tokio::time::Instant::now());
        assert_eq!(overflow.len(), 2);
        let outcome = deliver_flushed(&sink, buffer.module(), overflow, None, true).await;
        assert!(outcome.undelivered.is_empty());
        outcome.into_result().unwrap();

        for index in 0..6 {
            let leased = queue
                .next_event(&consumer, Duration::ZERO, Duration::from_secs(60))
                .await
                .unwrap()
                .expect("all six events must reach the final sink");
            assert_eq!(
                leased.event["params"]["content"],
                format!("claim-{index} [🔍=v1:AAAAAAAAAAw] [🔍=v1:AAAAAAAAACI]")
            );
            let citations = leased.event["params"]["meta"]["citation_locators"]
                .as_array()
                .expect("legacy evidence must survive final delivery as citations");
            assert!(
                leased.event["params"]["meta"]
                    .get("claim_locators")
                    .is_none()
            );
            assert_eq!(citations.len(), 2);
            assert_eq!(citations[0]["locator"], "v1:AAAAAAAAAAw");
            assert_eq!(citations[1]["locator"], "v1:AAAAAAAAACI");
            assert_eq!(citations[0]["author_id"], (200 + index).to_string());
            assert_eq!(citations[1]["author_id"], (200 + index).to_string());
            queue
                .acknowledge(&consumer, &leased.delivery_token)
                .await
                .unwrap();
        }
        assert_eq!(queue.status().await.queued, 0);
    }

    #[tokio::test]
    async fn disabled_evidence_delivery_preserves_text_without_projection() {
        let dir = tempfile::TempDir::new().unwrap();
        let state_dir = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        let queue = dione_codex::CodexEventQueue::load(
            &state_dir,
            dione_codex::LegacyDiscordScope::new(dione_contracts::ModuleInstanceId::new(
                dione_contracts::ProviderId::new("discord").unwrap(),
                dione_contracts::ModuleSlotId::new("primary").unwrap(),
            )),
        )
        .unwrap();
        let consumer = queue
            .register_consumer(
                "disabled evidence test".to_owned(),
                Duration::from_secs(60),
                true,
                true,
            )
            .await
            .unwrap()
            .consumer_id;
        let sink = test_connection_for_queue(queue.clone());
        let event = evidence_message_event(0);
        let expected_content = match &event {
            NotificationEvent::Message(message) => message.content.clone(),
            _ => unreachable!(),
        };

        deliver_flushed(
            &sink,
            &ModuleInstanceId::new(
                ProviderId::new("discord").unwrap(),
                ModuleSlotId::new("primary").unwrap(),
            ),
            vec![event],
            None,
            false,
        )
        .await
        .into_result()
        .unwrap();
        let leased = queue
            .next_event(&consumer, Duration::ZERO, Duration::from_secs(60))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(leased.event["params"]["content"], expected_content);
        assert!(leased.event["params"]["meta"].get("evidence").is_none());
        assert!(
            leased.event["params"]["meta"]
                .get("claim_locators")
                .is_none()
        );
        assert!(
            leased.event["params"]["meta"]
                .get("citation_locators")
                .is_none()
        );
    }

    #[test]
    fn extract_delay_message_uses_channel_config() {
        let config = config_with_channel_delay(42, 500);
        assert_eq!(
            extract_delay(&message_event(42), &config),
            Duration::from_millis(500)
        );
    }

    #[tokio::test]
    async fn rejected_rate_limit_reload_keeps_next_admitted_message_on_previous_budget() {
        let dir = tempfile::TempDir::new().unwrap();
        let state = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        let path = state.join("config.toml");
        let runtime = dione_config::ConfigRuntime::new(state.clone());
        std::fs::write(
            &path,
            "[rate_limit]\nenabled = true\nmax_tokens = 2\nwindow_secs = 60\ncooldown_secs = 30\n",
        )
        .unwrap();
        assert!(runtime.reload().await.1.is_none());
        let before = dione_config::load_config(&state);
        let lkg_before = std::fs::read(dione_config::lkg_path(&path)).unwrap();
        let mut limiter = RateLimiter::new(before.rate_limit_runtime().clone());
        let event = message_event(42);
        let key =
            message_rate_limit_key(&event).expect("admitted message retains effective identity");
        let now = Instant::now();
        assert!(matches!(
            limiter.check_message(&key.participant, &key.channel, &[], now),
            RateLimitDecision::Allowed { remaining: 1, .. }
        ));
        let mut invalid_settings = vec!["max_tokens = 0"];
        if now
            .checked_add(Duration::from_secs(i64::MAX as u64))
            .is_none()
        {
            invalid_settings.extend([
                "window_secs = 9223372036854775807",
                "cooldown_secs = 9223372036854775807",
            ]);
        }
        for setting in invalid_settings {
            let invalid = format!("[rate_limit]\nenabled = true\n{setting}\n");
            std::fs::write(&path, &invalid).unwrap();
            assert!(runtime.reload().await.1.unwrap().starts_with("rate_limit."));
            let current = dione_config::load_config(&state);
            assert!(std::sync::Arc::ptr_eq(&before, &current));
            assert_eq!(std::fs::read_to_string(&path).unwrap(), invalid);
            assert_eq!(
                std::fs::read(dione_config::lkg_path(&path)).unwrap(),
                lkg_before
            );
            if current.rate_limit_runtime() != limiter.config_ref() {
                limiter.update_config(current.rate_limit_runtime().clone());
            }
        }
        assert!(matches!(
            limiter.check_message(&key.participant, &key.channel, &[], now),
            RateLimitDecision::Allowed { remaining: 0, .. }
        ));
        assert!(matches!(
            limiter.check_message(&key.participant, &key.channel, &[], now),
            RateLimitDecision::Denied { .. }
        ));
        let mut manual_invalid = before.rate_limit_runtime().clone();
        manual_invalid.default.max_tokens = 0;
        limiter.update_config(manual_invalid);
        assert!(matches!(
            limiter.check_message(&key.participant, &key.channel, &[], now),
            RateLimitDecision::Unavailable { .. }
        ));
    }

    #[test]
    fn direct_and_represented_same_human_share_the_actual_rate_limit_bucket() {
        let mut limiter = RateLimiter::new(RateLimitConfig {
            enabled: true,
            default: ScopeConfig {
                max_tokens: 1,
                window: Duration::from_secs(3600),
                cooldown: Duration::from_secs(3600),
                overflow: OverflowPolicy::Drop { notify: true },
            },
            classes: Vec::new(),
            individuals: HashMap::new(),
            channels: HashMap::new(),
        });
        let direct = message_event(42);
        let mut represented = message_event(42);
        let NotificationEvent::Message(represented_message) = &mut represented else {
            unreachable!("message_event always builds a message")
        };
        represented_message.message_id = MessageId::new(2);
        represented_message.user = "visible PK member".into();

        let direct_key = message_rate_limit_key(&direct).expect("direct message key");
        let represented_key =
            message_rate_limit_key(&represented).expect("represented message key");
        let now = Instant::now();

        assert!(matches!(
            limiter.check_message(&direct_key.participant, &direct_key.channel, &[], now),
            RateLimitDecision::Allowed { remaining: 0, .. }
        ));
        assert!(matches!(
            limiter.check_message(
                &represented_key.participant,
                &represented_key.channel,
                &[],
                now
            ),
            RateLimitDecision::Denied { .. }
        ));
    }

    #[test]
    fn extract_delay_reaction_uses_channel_config() {
        let config = config_with_channel_delay(7, 200);
        let event = NotificationEvent::Reaction {
            chat_id: ChannelId::new(7),
            message_id: MessageId::new(1),
            user: "u".into(),
            user_id: UserId::new(1),
            emoji: "👍".into(),
            self_react: false,
        };
        assert_eq!(extract_delay(&event, &config), Duration::from_millis(200));
    }

    #[test]
    fn extract_delay_message_edit_uses_channel_config() {
        let config = config_with_channel_delay(99, 300);
        let event = NotificationEvent::MessageEdit {
            chat_id: ChannelId::new(99),
            message_id: MessageId::new(1),
            user: "u".into(),
            user_id: UserId::new(1),
            new_content: "edited".into(),
            timestamp: Timestamp::parse("2026-01-01T00:00:00Z").unwrap(),
            thread_parent_id: None,
            reply_to_message_id: None,
        };
        assert_eq!(extract_delay(&event, &config), Duration::from_millis(300));
    }

    #[test]
    fn extract_delay_message_delete_uses_channel_config() {
        let config = config_with_channel_delay(55, 100);
        let event = NotificationEvent::MessageDelete {
            chat_id: ChannelId::new(55),
            message_id: MessageId::new(1),
            thread_parent_id: None,
        };
        assert_eq!(extract_delay(&event, &config), Duration::from_millis(100));
    }

    #[test]
    fn extract_delay_non_channel_event_is_zero() {
        let config = config_with_channel_delay(1, 999);
        let event = NotificationEvent::ConfigError {
            error: "oops".into(),
        };
        assert_eq!(extract_delay(&event, &config), Duration::from_millis(0));
    }

    #[test]
    fn extract_delay_unconfigured_channel_falls_back_to_global() {
        let config = config_with_global_delay(750);
        assert_eq!(
            extract_delay(&message_event(9999), &config),
            Duration::from_millis(750)
        );
    }

    #[tokio::test]
    async fn immediate_buffer_result_dereferences_to_notification() {
        let mut buf = DeliveryBuffer::new(ModuleInstanceId::new(
            ProviderId::new("discord").unwrap(),
            ModuleSlotId::new("primary").unwrap(),
        ));
        let result = buf.buffer_event(message_event(42), Duration::from_millis(0));
        let notification = match result {
            BufferResult::Immediate(event) => (*event).fixture_notification().await,
            BufferResult::FlushThenImmediate { .. } => {
                panic!("delay=0 cannot require a preceding flush")
            }
            BufferResult::Buffered => panic!("expected Immediate for delay=0"),
            BufferResult::Rejected { error, .. } => panic!("zero delay rejected: {error}"),
        };
        assert_eq!(notification["jsonrpc"], "2.0");
        assert_eq!(notification["method"], "notifications/claude/channel");
        assert_eq!(notification["params"]["meta"]["chat_id"], "42");
    }
}

#[cfg(test)]
#[path = "attention_tests.rs"]
mod attention_tests;
#[cfg(test)]
#[path = "scheduling_tests.rs"]
mod scheduling_tests;

#[cfg(test)]
fn test_connection_for_queue(queue: dione_codex::CodexEventQueue) -> HarnessConnection {
    let (binding, _receiver) = tokio::sync::watch::channel(None);
    dione_codex::connect(queue, binding)
}
