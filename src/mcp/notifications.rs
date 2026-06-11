//! Conversion from Discord [`NotificationEvent`]s to MCP JSON-RPC notifications.
//!
//! The [`IntoNotification`] trait defines how a single event becomes a
//! full JSON-RPC notification. Each event is emitted as its own notification
//! line — no batch wrapping.

use serde_json::{Value, json};

use crate::discord::events::NotificationEvent;

// ── Trait ────────────────────────────────────────────────────────────────────

/// Convert a value into an MCP JSON-RPC notification.
pub(crate) trait IntoNotification {
    /// Full JSON-RPC 2.0 notification with `jsonrpc`, `method`, and `params`.
    fn into_notification(self) -> Value;
}

// ── Impl for NotificationEvent ──────────────────────────────────────────────

impl IntoNotification for NotificationEvent {
    fn into_notification(self) -> Value {
        match self {
            NotificationEvent::Message {
                chat_id,
                message_id,
                user,
                user_id,
                content,
                timestamp,
                attachments,
                is_voice_message,
                thread_parent_id,
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
                if let Some(parent_id) = thread_parent_id {
                    meta["thread_parent_id"] = json!(parent_id);
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
            NotificationEvent::MessageEdit {
                chat_id,
                message_id,
                user,
                user_id,
                new_content,
                timestamp,
                thread_parent_id,
            } => {
                let mut meta = json!({
                    "chat_id": chat_id,
                    "message_id": message_id,
                    "user": user,
                    "user_id": user_id,
                    "type": "message_edit",
                    "ts": timestamp,
                });
                if let Some(parent_id) = thread_parent_id {
                    meta["thread_parent_id"] = json!(parent_id);
                }
                json!({
                    "jsonrpc": "2.0",
                    "method": "notifications/claude/channel",
                    "params": {
                        "content": new_content,
                        "meta": meta,
                    }
                })
            }
            NotificationEvent::MessageDelete {
                chat_id,
                message_id,
                thread_parent_id,
            } => {
                let mut meta = json!({
                    "chat_id": chat_id,
                    "message_id": message_id,
                    "type": "message_delete",
                });
                if let Some(parent_id) = thread_parent_id {
                    meta["thread_parent_id"] = json!(parent_id);
                }
                json!({
                    "jsonrpc": "2.0",
                    "method": "notifications/claude/channel",
                    "params": {
                        "content": "message deleted",
                        "meta": meta,
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_message_edit_includes_thread_parent_id() {
        let event = NotificationEvent::MessageEdit {
            chat_id: "100".into(),
            message_id: "200".into(),
            user: "alice".into(),
            user_id: "300".into(),
            new_content: "edited text".into(),
            timestamp: "2026-01-01T00:00:00Z".into(),
            thread_parent_id: Some("400".into()),
        };
        let json = event.into_notification();
        let meta = &json["params"]["meta"];
        assert_eq!(meta["thread_parent_id"], "400");
        assert_eq!(meta["type"], "message_edit");
    }

    #[test]
    fn test_message_edit_omits_thread_parent_id_when_none() {
        let event = NotificationEvent::MessageEdit {
            chat_id: "100".into(),
            message_id: "200".into(),
            user: "alice".into(),
            user_id: "300".into(),
            new_content: "edited text".into(),
            timestamp: "2026-01-01T00:00:00Z".into(),
            thread_parent_id: None,
        };
        let json = event.into_notification();
        let meta = &json["params"]["meta"];
        assert!(meta.get("thread_parent_id").is_none());
    }

    #[test]
    fn test_message_delete_includes_thread_parent_id() {
        let event = NotificationEvent::MessageDelete {
            chat_id: "100".into(),
            message_id: "200".into(),
            thread_parent_id: Some("500".into()),
        };
        let json = event.into_notification();
        let meta = &json["params"]["meta"];
        assert_eq!(meta["thread_parent_id"], "500");
        assert_eq!(meta["type"], "message_delete");
    }

    #[test]
    fn test_message_delete_omits_thread_parent_id_when_none() {
        let event = NotificationEvent::MessageDelete {
            chat_id: "100".into(),
            message_id: "200".into(),
            thread_parent_id: None,
        };
        let json = event.into_notification();
        let meta = &json["params"]["meta"];
        assert!(meta.get("thread_parent_id").is_none());
    }

    #[test]
    fn test_message_omits_thread_parent_id_when_none() {
        let event = NotificationEvent::Message {
            chat_id: "100".into(),
            message_id: "200".into(),
            user: "bob".into(),
            user_id: "300".into(),
            content: "hello from channel".into(),
            timestamp: "2026-01-01T00:00:00Z".into(),
            attachments: vec![],
            is_voice_message: false,
            thread_parent_id: None,
        };
        let json = event.into_notification();
        let meta = &json["params"]["meta"];
        assert!(meta.get("thread_parent_id").is_none());
    }

    #[test]
    fn test_message_includes_thread_parent_id() {
        let event = NotificationEvent::Message {
            chat_id: "100".into(),
            message_id: "200".into(),
            user: "bob".into(),
            user_id: "300".into(),
            content: "hello from thread".into(),
            timestamp: "2026-01-01T00:00:00Z".into(),
            attachments: vec![],
            is_voice_message: false,
            thread_parent_id: Some("600".into()),
        };
        let json = event.into_notification();
        let meta = &json["params"]["meta"];
        assert_eq!(meta["thread_parent_id"], "600");
    }
}
