//! Administrator permission prompts and their concrete Discord interaction lifecycle.
//!
//! Sending records successful prompt deliveries in shared Discord state. Interaction
//! handling checks the current config snapshot, acknowledges before cleanup, and
//! emits one decision only when it removed pending siblings. Request-id matching
//! is not a new correlation or ownership guarantee. HTTP failures preserve the
//! existing partial-delivery and failed-ack behavior; state is transient.
#![warn(missing_docs)]

use dione_discord_events::NotificationEvent;
use dione_discord_state::{PendingPermission, State};
use serenity::{
    builder::{
        CreateActionRow, CreateButton, CreateInteractionResponse, CreateInteractionResponseMessage,
        CreateMessage,
    },
    model::{application::Interaction, channel::ReactionType},
};
use thiserror::Error;

// ── Error type ────────────────────────────────────────────────────────────────

/// Failure before permission delivery begins.
#[derive(Debug, Error)]
pub enum PermissionError {
    /// No administrators were configured, so no HTTP request was attempted.
    #[error("no admins configured")]
    NoAdmins,
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Sends a permission request DM to all configured admins.
///
/// Creates Allow/Deny buttons on the message. The button `custom_id` values are
/// `perm:allow:{request_id}` and `perm:deny:{request_id}` so the interaction
/// owner can parse them. Individual HTTP failures log and continue; success
/// does not promise that every administrator received a message.
pub async fn send_permission_request(
    http: &serenity::http::Http,
    config: &dione_config::LoadedConfig,
    state: &State,
    request_id: &str,
    tool_name: &str,
    description: &str,
    input_preview: &str,
) -> Result<(), PermissionError> {
    if config.admin_ids.is_empty() {
        return Err(PermissionError::NoAdmins);
    }

    let content = format!(
        "**Permission request** — tool `{tool_name}`\n\
         Description: {description}\n\
         Input preview:\n```\n{input_preview}\n```\n\
         Request ID: `{request_id}`"
    );

    let allow_button = CreateButton::new(format!("perm:allow:{request_id}"))
        .label("Allow")
        .style(serenity::model::application::ButtonStyle::Success)
        .emoji(ReactionType::Unicode("✅".to_string()));

    let deny_button = CreateButton::new(format!("perm:deny:{request_id}"))
        .label("Deny")
        .style(serenity::model::application::ButtonStyle::Danger)
        .emoji(ReactionType::Unicode("❌".to_string()));

    let row = CreateActionRow::Buttons(vec![allow_button, deny_button]);

    for &admin_id in &config.admin_ids {
        let dm_body = serde_json::json!({ "recipient_id": admin_id.to_string() });
        let channel = match http.create_private_channel(&dm_body).await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(admin_id, error = %e, "failed to open DM with admin");
                continue;
            }
        };

        let msg = CreateMessage::new()
            .content(&content)
            .components(vec![row.clone()]);

        match channel.id.send_message(http, msg).await {
            Ok(sent_msg) => {
                let mut locked = state.write().await;
                locked.pending_permissions.insert(
                    sent_msg.id.get(),
                    PendingPermission {
                        request_id: request_id.to_string(),
                        channel_id: channel.id,
                        created_at: chrono::Utc::now(),
                    },
                );
                // Record the prompt as a bot-sent message so its eventual
                // cleanup deletion is suppressed by the message_delete handler
                // instead of being delivered as a useless notification.
                locked.note_sent(sent_msg.id.get());
            }
            Err(e) => {
                tracing::warn!(admin_id, error = %e, "failed to send permission request DM");
            }
        }
    }

    Ok(())
}

/// Handle permission components using current administrator policy.
///
/// Unrecognized interactions return without HTTP. Unauthorized components receive
/// an ephemeral response. Failed acknowledgements retain pending state and emit
/// no event; successful acknowledgements clean all siblings before forwarding a
/// decision. Duplicate clicks can acknowledge but cannot forward another decision.
pub async fn handle_interaction(
    http: &serenity::http::Http,
    interaction: Interaction,
    state: &State,
    state_dir: &camino::Utf8Path,
    event_tx: &tokio::sync::mpsc::Sender<NotificationEvent>,
) {
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

    let config = dione_config::load_config(state_dir);
    let sender_id = component.user.id.get();
    let is_admin = config.is_admin(sender_id);

    if !is_admin {
        // Respond with ephemeral "not authorized".
        let resp = CreateInteractionResponse::Message(
            CreateInteractionResponseMessage::new()
                .content("Not authorized.")
                .ephemeral(true),
        );
        if let Err(e) = component.create_response(http, resp).await {
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
    if let Err(e) = component.create_response(http, resp).await {
        tracing::warn!(error = %e, "failed to update permission button message; not forwarding event");
        return;
    }

    // Remove all pending entries for this request_id (multi-admin: each admin
    // gets a separate DM, so there may be sibling messages to clean up).
    // Removal also re-marks the message IDs as bot-sent, so the gateway
    // message_delete events triggered by the cleanup below are suppressed
    // rather than delivered to the MCP client.
    let siblings = {
        let mut state = state.write().await;
        state.remove_permissions_by_request_id(&request_id)
    };

    // Delete all permission DMs — siblings from other admins and the
    // clicked message itself. The interaction response already acknowledged
    // the click, so the message can be cleaned up.
    for (channel_id, msg_id) in &siblings {
        if let Err(e) = http.delete_message(*channel_id, *msg_id, None).await {
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
    if let Err(e) = event_tx.send(event).await {
        tracing::warn!(error = %e, "failed to send permission response event");
    }
}
