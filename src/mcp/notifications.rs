//! Conversion from Discord [`NotificationEvent`]s to MCP JSON-RPC notifications.

use serde_json::{Value, json};

use crate::discord::events::NotificationEvent;

/// Convert a [`NotificationEvent`] into a JSON-RPC 2.0 notification object.
pub(crate) fn event_to_notification(event: NotificationEvent) -> Value {
    match event {
        NotificationEvent::Message {
            chat_id,
            message_id,
            user,
            user_id,
            content,
            timestamp,
            attachments,
            is_voice_message,
        } => {
            let mut meta = json!({
                "chat_id": chat_id,
                "message_id": message_id,
                "user": user,
                "user_id": user_id,
                "ts": timestamp,
            });
            if is_voice_message {
                meta["is_voice_message"] = json!(true);
            }
            if !attachments.is_empty() {
                meta["attachment_count"] = json!(attachments.len().to_string());
                let att_desc: Vec<String> = attachments
                    .iter()
                    .map(|a| {
                        let ct = a.content_type.as_deref().unwrap_or("unknown");
                        let kb = a.size / 1024;
                        format!("{} ({ct}, {kb}KB)", a.name)
                    })
                    .collect();
                meta["attachments"] = json!(att_desc.join("; "));
            }
            json!({
                "jsonrpc": "2.0",
                "method": "notifications/claude/channel",
                "params": {
                    "content": content,
                    "meta": meta,
                }
            })
        }
        NotificationEvent::Reaction {
            chat_id,
            message_id,
            user,
            user_id,
            emoji,
        } => {
            json!({
                "jsonrpc": "2.0",
                "method": "notifications/claude/channel",
                "params": {
                    "content": format!("reacted with {emoji}"),
                    "meta": {
                        "chat_id": chat_id,
                        "message_id": message_id,
                        "user": user,
                        "user_id": user_id,
                        "type": "reaction",
                        "emoji": emoji,
                    },
                }
            })
        }
        NotificationEvent::PermissionResponse {
            request_id,
            granted,
        } => {
            let behavior = if granted { "allow" } else { "deny" };
            json!({
                "jsonrpc": "2.0",
                "method": "notifications/claude/channel/permission",
                "params": {
                    "request_id": request_id,
                    "behavior": behavior,
                }
            })
        }
        NotificationEvent::Trace {
            level,
            target,
            message,
            fields,
        } => {
            let mut content = message;
            if !fields.is_empty() {
                let kvs: Vec<String> = fields.iter().map(|(k, v)| format!("{k}={v}")).collect();
                content = format!("{content} {{ {} }}", kvs.join(", "));
            }
            json!({
                "jsonrpc": "2.0",
                "method": "notifications/claude/channel",
                "params": {
                    "content": content,
                    "meta": {
                        "type": "trace",
                        "level": level,
                        "target": target,
                    },
                }
            })
        }
        NotificationEvent::ConfigError { error } => {
            json!({
                "jsonrpc": "2.0",
                "method": "notifications/claude/channel",
                "params": {
                    "content": format!("config.toml parse error — running on last valid config. Error: {error}"),
                    "meta": {
                        "type": "config_error",
                    },
                }
            })
        }
    }
}
