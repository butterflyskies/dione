//! Native Bell eligibility and shadow/live enrichment.
//!
//! Generic retrieval, sessions, ranking, and rendering remain in dione-bell-rings.
//! Admission, source revalidation, and delivery authorization remain with callers.
#![warn(missing_docs)]
use dione_bell_rings::{Bell, BellEvaluator, BellStatus, render_bells};
use dione_config::BellMode;
use dione_config::BellRingsConfig;
use dione_config::BellTrigger;
use dione_discord_events::NotificationEvent;
use std::sync::Arc;

/// Evaluate native message eligibility and retrieve bells without mutating metadata.
///
/// Disabled, nonmessage, empty-provider, and undirected cases under the Directed
/// trigger skip initialization. The All trigger also permits ambient messages.
/// These supplied observations do not establish delivery authorization.
pub async fn evaluate_native(
    event: NotificationEvent,
    config: &BellRingsConfig,
    evaluator: &BellEvaluator,
) -> (NotificationEvent, Vec<Bell>, Option<BellStatus>) {
    let eligible = match &event {
        NotificationEvent::Message(message) if config.enabled => {
            let channel_id = message.chat_id.get().to_string();
            match config.trigger_for_channel(&channel_id) {
                BellTrigger::Directed => message.targeting.is_directed(),
                BellTrigger::All => true,
            }
        }
        _ => false,
    };
    if eligible
        && !config.providers.is_empty()
        && let NotificationEvent::Message(message) = &event
        && let Some(result) = evaluator
            .evaluate(&message.content, &config.providers, &config.recall)
            .await
    {
        return (event, result.bells, Some(result.status));
    }
    (event, Vec::new(), None)
}

/// Enrich an event according to the configured shadow/live mode.
///
/// Shadow detaches evaluation and preserves the original event, without a new
/// cancellation or drain guarantee. Live awaits evaluation, replaces status only
/// when supplied, and injects only nonempty rendered bells. Existing metadata is
/// retained when those conditions do not replace it.
pub async fn prepare_bells(
    mut event: NotificationEvent,
    cfg: &dione_config::LoadedConfig,
    evaluator: &Arc<BellEvaluator>,
) -> NotificationEvent {
    match cfg.bell_rings.mode {
        BellMode::Shadow => {
            let evaluator = evaluator.clone();
            let shadow_event = event.clone();
            let shadow_config = cfg.bell_rings.clone();
            tokio::spawn(async move {
                let _ = evaluate_native(shadow_event, &shadow_config, &evaluator).await;
            });
        }
        BellMode::Live => {
            let (returned, bells, status) =
                evaluate_native(event, &cfg.bell_rings, evaluator).await;
            event = returned;
            if let NotificationEvent::Message(message) = &mut event {
                if let Some(status) = status {
                    message.bells_status = Some(status);
                }
                if !bells.is_empty() {
                    message.bells = Some(render_bells(&bells));
                }
            }
        }
    }
    event
}
