use serenity::builder::{CreateActionRow, CreateButton, CreateMessage};
use serenity::model::channel::ReactionType;
use thiserror::Error;

use crate::state::{PendingPermission, State};

// ── Error type ────────────────────────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum PermissionError {
    #[error("no admins configured")]
    NoAdmins,
    #[error("Discord HTTP error: {0}")]
    Http(String),
    #[error("permission request not found")]
    NotFound,
    #[error("user is not an admin")]
    NotAdmin,
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Sends a permission request DM to all configured admins.
///
/// Creates Allow/Deny buttons on the message. The button `custom_id` values are
/// `perm:allow:{request_id}` and `perm:deny:{request_id}` so the interaction
/// handler in `events.rs` can parse them.
pub async fn send_permission_request(
    http: &serenity::http::Http,
    config: &crate::config::LoadedConfig,
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
                        channel_id: channel.id.get(),
                        created_at: chrono::Utc::now(),
                    },
                );
            }
            Err(e) => {
                tracing::warn!(admin_id, error = %e, "failed to send permission request DM");
            }
        }
    }

    Ok(())
}

/// Validates that `user_id` is a configured admin and looks up the `request_id`
/// associated with `message_id`.
///
/// Returns the `request_id` string. The caller is responsible for supplying
/// the actual `granted` boolean from the interaction.
pub fn validate_response(
    state: &crate::state::SharedState,
    config: &crate::config::LoadedConfig,
    message_id: u64,
    user_id: u64,
) -> Result<String, PermissionError> {
    if !config.is_admin(user_id) {
        return Err(PermissionError::NotAdmin);
    }

    let pending = state
        .pending_permissions
        .get(&message_id)
        .ok_or(PermissionError::NotFound)?;

    Ok(pending.request_id.clone())
}
