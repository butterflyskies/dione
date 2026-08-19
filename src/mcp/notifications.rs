//! Conversion from Discord [`NotificationEvent`]s to MCP JSON-RPC notifications.
//!
//! The `IntoNotification` trait defines how a single event becomes a
//! full JSON-RPC notification. Each event is emitted as its own notification
//! line — no batch wrapping.

use crate::discord::events::{MessageEvent, NotificationEvent};
use crate::evidence::project_evidence;
use serde_json::{Value, json};

// ── Trait ────────────────────────────────────────────────────────────────────

/// Convert a value into an MCP JSON-RPC notification.
pub(crate) trait IntoNotification {
    /// Full JSON-RPC 2.0 notification with `jsonrpc`, `method`, and `params`.
    fn into_notification(self) -> Value
    where
        Self: Sized,
    {
        self.into_notification_with_evidence(true)
    }

    fn into_notification_with_evidence(self, evidence_markers_enabled: bool) -> Value;
}

// ── Impl for NotificationEvent ──────────────────────────────────────────────

impl IntoNotification for NotificationEvent {
    fn into_notification_with_evidence(self, evidence_markers_enabled: bool) -> Value {
        match self {
            NotificationEvent::Message(MessageEvent {
                chat_id,
                message_id,
                user,
                user_id,
                content,
                timestamp,
                attachments,
                is_voice_message,
                thread_parent_id,
                reply_to_message_id,
                reply_to_user_id,
                reply_to_user,
                reply_to_content_preview,
                bells,
                bells_status,
                ..
            }) => {
                let mut meta = json!({
                    "chat_id": chat_id.get().to_string(),
                    "message_id": message_id.get().to_string(),
                    "user": user,
                    "user_id": user_id.get().to_string(),
                    "ts": timestamp,
                });
                if is_voice_message {
                    meta["is_voice_message"] = json!(true);
                }
                if let Some(parent_id) = thread_parent_id {
                    meta["thread_parent_id"] = json!(parent_id.get().to_string());
                }
                if let Some(reply_id) = reply_to_message_id {
                    meta["reply_to_message_id"] = json!(reply_id.get().to_string());
                }
                if let Some(reply_uid) = reply_to_user_id {
                    meta["reply_to_user_id"] = json!(reply_uid.get().to_string());
                }
                if let Some(reply_user) = reply_to_user {
                    meta["reply_to_user"] = json!(reply_user);
                }
                if let Some(preview) = reply_to_content_preview {
                    meta["reply_to_content_preview"] = json!(preview);
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
                if let Some(bells) = bells {
                    meta["bells"] = json!(bells);
                }
                if let Some(status) = bells_status {
                    meta["bells_status"] = json!(status.as_str());
                }
                if evidence_markers_enabled {
                    project_evidence(&mut meta, &content, user_id);
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
                self_react,
            } => {
                let mut meta = json!({
                    "chat_id": chat_id.get().to_string(),
                    "message_id": message_id.get().to_string(),
                    "user": user,
                    "user_id": user_id.get().to_string(),
                    "type": "reaction",
                    "emoji": emoji,
                });
                if self_react {
                    meta["self_react"] = json!(true);
                }
                json!({
                    "jsonrpc": "2.0",
                    "method": "notifications/claude/channel",
                    "params": {
                        "content": format!("reacted with {emoji}"),
                        "meta": meta,
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
                reply_to_message_id,
            } => {
                let mut meta = json!({
                    "chat_id": chat_id.get().to_string(),
                    "message_id": message_id.get().to_string(),
                    "user": user,
                    "user_id": user_id.get().to_string(),
                    "type": "message_edit",
                    "ts": timestamp,
                });
                if let Some(parent_id) = thread_parent_id {
                    meta["thread_parent_id"] = json!(parent_id.get().to_string());
                }
                if let Some(reply_id) = reply_to_message_id {
                    meta["reply_to_message_id"] = json!(reply_id.get().to_string());
                }
                if evidence_markers_enabled {
                    project_evidence(&mut meta, &new_content, user_id);
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
                    "chat_id": chat_id.get().to_string(),
                    "message_id": message_id.get().to_string(),
                    "type": "message_delete",
                });
                if let Some(parent_id) = thread_parent_id {
                    meta["thread_parent_id"] = json!(parent_id.get().to_string());
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
    use crate::bell_rings::BellStatus;
    use crate::timestamp::Timestamp;
    use serenity::model::id::{ChannelId, MessageId, UserId};

    #[test]
    fn test_message_edit_includes_thread_parent_id() {
        let event = NotificationEvent::MessageEdit {
            chat_id: ChannelId::new(100),
            message_id: MessageId::new(200),
            user: "alice".into(),
            user_id: UserId::new(300),
            new_content: "edited text".into(),
            timestamp: Timestamp::parse("2026-01-01T00:00:00Z").unwrap(),
            thread_parent_id: Some(ChannelId::new(400)),
            reply_to_message_id: None,
        };
        let json = event.into_notification();
        let meta = &json["params"]["meta"];
        assert_eq!(meta["thread_parent_id"], "400");
        assert_eq!(meta["type"], "message_edit");
    }

    #[test]
    fn test_message_edit_omits_thread_parent_id_when_none() {
        let event = NotificationEvent::MessageEdit {
            chat_id: ChannelId::new(100),
            message_id: MessageId::new(200),
            user: "alice".into(),
            user_id: UserId::new(300),
            new_content: "edited text".into(),
            timestamp: Timestamp::parse("2026-01-01T00:00:00Z").unwrap(),
            thread_parent_id: None,
            reply_to_message_id: None,
        };
        let json = event.into_notification();
        let meta = &json["params"]["meta"];
        assert!(meta.get("thread_parent_id").is_none());
    }

    #[test]
    fn message_edit_projects_current_evidence_and_removal_clears_it() {
        let edit = |new_content: &str| NotificationEvent::MessageEdit {
            chat_id: ChannelId::new(100),
            message_id: MessageId::new(200),
            user: "alice".into(),
            user_id: UserId::new(300),
            new_content: new_content.into(),
            timestamp: Timestamp::parse("2026-01-01T00:00:00Z").unwrap(),
            thread_parent_id: None,
            reply_to_message_id: None,
        };

        let added = edit("edited [🔍=v1:AAAAAAAAAAw]").into_notification();
        assert_eq!(
            added["params"]["meta"]["evidence"],
            json!([{
                "locator": "v1:AAAAAAAAAAw",
                "author_id": "300",
            }])
        );

        let removed = edit("edited").into_notification();
        assert!(removed["params"]["meta"].get("evidence").is_none());
    }

    #[test]
    fn disabled_evidence_projection_preserves_visible_content_only() {
        let event = NotificationEvent::MessageEdit {
            chat_id: ChannelId::new(100),
            message_id: MessageId::new(200),
            user: "alice".into(),
            user_id: UserId::new(300),
            new_content: "edited [🔍=v1:AAAAAAAAAAw]".into(),
            timestamp: Timestamp::parse("2026-01-01T00:00:00Z").unwrap(),
            thread_parent_id: None,
            reply_to_message_id: None,
        };

        let json = event.into_notification_with_evidence(false);
        assert_eq!(json["params"]["content"], "edited [🔍=v1:AAAAAAAAAAw]");
        assert!(json["params"]["meta"].get("evidence").is_none());
    }

    #[test]
    fn test_message_delete_includes_thread_parent_id() {
        let event = NotificationEvent::MessageDelete {
            chat_id: ChannelId::new(100),
            message_id: MessageId::new(200),
            thread_parent_id: Some(ChannelId::new(500)),
        };
        let json = event.into_notification();
        let meta = &json["params"]["meta"];
        assert_eq!(meta["thread_parent_id"], "500");
        assert_eq!(meta["type"], "message_delete");
    }

    #[test]
    fn test_message_delete_omits_thread_parent_id_when_none() {
        let event = NotificationEvent::MessageDelete {
            chat_id: ChannelId::new(100),
            message_id: MessageId::new(200),
            thread_parent_id: None,
        };
        let json = event.into_notification();
        let meta = &json["params"]["meta"];
        assert!(meta.get("thread_parent_id").is_none());
    }

    #[test]
    fn test_message_omits_thread_parent_id_when_none() {
        let event = NotificationEvent::Message(MessageEvent {
            chat_id: ChannelId::new(100),
            message_id: MessageId::new(200),
            user: "bob".into(),
            user_id: UserId::new(300),
            content: "hello from channel".into(),
            targeting: crate::discord::events::MessageTargeting::Ambient,
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
        });
        let json = event.into_notification();
        let meta = &json["params"]["meta"];
        assert!(meta.get("thread_parent_id").is_none());
    }

    #[test]
    fn test_message_includes_thread_parent_id() {
        let event = NotificationEvent::Message(MessageEvent {
            chat_id: ChannelId::new(100),
            message_id: MessageId::new(200),
            user: "bob".into(),
            user_id: UserId::new(300),
            content: "hello from thread".into(),
            targeting: crate::discord::events::MessageTargeting::Ambient,
            timestamp: Timestamp::parse("2026-01-01T00:00:00Z").unwrap(),
            attachments: vec![],
            is_voice_message: false,
            thread_parent_id: Some(ChannelId::new(600)),
            reply_to_message_id: None,
            reply_to_user_id: None,
            reply_to_user: None,
            reply_to_content_preview: None,
            bells: None,
            bells_status: None,
        });
        let json = event.into_notification();
        let meta = &json["params"]["meta"];
        assert_eq!(meta["thread_parent_id"], "600");
    }

    // ── Reply-to-message-id tests ─────────────────────────────────────────────

    #[test]
    fn test_message_includes_reply_to_message_id() {
        let event = NotificationEvent::Message(MessageEvent {
            chat_id: ChannelId::new(100),
            message_id: MessageId::new(200),
            user: "alice".into(),
            user_id: UserId::new(300),
            content: "replying to you".into(),
            targeting: crate::discord::events::MessageTargeting::Ambient,
            timestamp: Timestamp::parse("2026-01-01T00:00:00Z").unwrap(),
            attachments: vec![],
            is_voice_message: false,
            thread_parent_id: None,
            reply_to_message_id: Some(MessageId::new(999)),
            reply_to_user_id: None,
            reply_to_user: None,
            reply_to_content_preview: None,
            bells: None,
            bells_status: None,
        });
        let json = event.into_notification();
        let meta = &json["params"]["meta"];
        assert_eq!(meta["reply_to_message_id"], "999");
    }

    #[test]
    fn test_message_omits_reply_to_message_id_when_none() {
        let event = NotificationEvent::Message(MessageEvent {
            chat_id: ChannelId::new(100),
            message_id: MessageId::new(200),
            user: "alice".into(),
            user_id: UserId::new(300),
            content: "not a reply".into(),
            targeting: crate::discord::events::MessageTargeting::Ambient,
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
        });
        let json = event.into_notification();
        let meta = &json["params"]["meta"];
        assert!(meta.get("reply_to_message_id").is_none());
    }

    #[test]
    fn test_message_includes_reply_context() {
        let event = NotificationEvent::Message(MessageEvent {
            chat_id: ChannelId::new(100),
            message_id: MessageId::new(200),
            user: "alice".into(),
            user_id: UserId::new(300),
            content: "replying to you".into(),
            targeting: crate::discord::events::MessageTargeting::Ambient,
            timestamp: Timestamp::parse("2026-01-01T00:00:00Z").unwrap(),
            attachments: vec![],
            is_voice_message: false,
            thread_parent_id: None,
            reply_to_message_id: Some(MessageId::new(999)),
            reply_to_user_id: Some(UserId::new(888)),
            reply_to_user: Some("bob".into()),
            reply_to_content_preview: Some("original message".into()),
            bells: None,
            bells_status: None,
        });
        let json = event.into_notification();
        let meta = &json["params"]["meta"];
        assert_eq!(meta["reply_to_user_id"], "888");
        assert_eq!(meta["reply_to_user"], "bob");
        assert_eq!(meta["reply_to_content_preview"], "original message");
    }

    #[test]
    fn test_message_omits_reply_context_when_none() {
        let event = NotificationEvent::Message(MessageEvent {
            chat_id: ChannelId::new(100),
            message_id: MessageId::new(200),
            user: "alice".into(),
            user_id: UserId::new(300),
            content: "not a reply".into(),
            targeting: crate::discord::events::MessageTargeting::Ambient,
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
        });
        let json = event.into_notification();
        let meta = &json["params"]["meta"];
        assert!(meta.get("reply_to_user_id").is_none());
        assert!(meta.get("reply_to_user").is_none());
        assert!(meta.get("reply_to_content_preview").is_none());
    }

    #[test]
    fn test_message_omits_reply_context_but_keeps_reply_to_message_id() {
        let event = NotificationEvent::Message(MessageEvent {
            chat_id: ChannelId::new(100),
            message_id: MessageId::new(200),
            user: "alice".into(),
            user_id: UserId::new(300),
            content: "reply without hydrated parent".into(),
            targeting: crate::discord::events::MessageTargeting::Ambient,
            timestamp: Timestamp::parse("2026-01-01T00:00:00Z").unwrap(),
            attachments: vec![],
            is_voice_message: false,
            thread_parent_id: None,
            reply_to_message_id: Some(MessageId::new(999)),
            reply_to_user_id: None,
            reply_to_user: None,
            reply_to_content_preview: None,
            bells: None,
            bells_status: None,
        });
        let json = event.into_notification();
        let meta = &json["params"]["meta"];
        assert_eq!(meta["reply_to_message_id"], "999");
        assert!(meta.get("reply_to_user_id").is_none());
        assert!(meta.get("reply_to_user").is_none());
        assert!(meta.get("reply_to_content_preview").is_none());
    }

    #[test]
    fn test_message_includes_bells_when_present() {
        let event = NotificationEvent::Message(MessageEvent {
            chat_id: ChannelId::new(100),
            message_id: MessageId::new(200),
            user: "alice".into(),
            user_id: UserId::new(300),
            content: "hello with bells".into(),
            targeting: crate::discord::events::MessageTargeting::Ambient,
            timestamp: Timestamp::parse("2026-01-01T00:00:00Z").unwrap(),
            attachments: vec![],
            is_voice_message: false,
            thread_parent_id: None,
            reply_to_message_id: None,
            reply_to_user_id: None,
            reply_to_user: None,
            reply_to_content_preview: None,
            bells: Some("3s lain/person-pace;2b lain/feedback-no-platitudes".into()),
            bells_status: Some(BellStatus::Ok),
        });
        let json = event.into_notification();
        let meta = &json["params"]["meta"];
        assert_eq!(
            meta["bells"],
            "3s lain/person-pace;2b lain/feedback-no-platitudes"
        );
        assert_eq!(meta["bells_status"], "ok");
    }

    #[test]
    fn test_message_omits_bells_when_none() {
        let event = NotificationEvent::Message(MessageEvent {
            chat_id: ChannelId::new(100),
            message_id: MessageId::new(200),
            user: "alice".into(),
            user_id: UserId::new(300),
            content: "hello without bells".into(),
            targeting: crate::discord::events::MessageTargeting::Ambient,
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
        });
        let json = event.into_notification();
        let meta = &json["params"]["meta"];
        assert!(meta.get("bells").is_none());
    }

    #[test]
    fn test_message_omits_preview_but_keeps_author() {
        let event = NotificationEvent::Message(MessageEvent {
            chat_id: ChannelId::new(100),
            message_id: MessageId::new(200),
            user: "alice".into(),
            user_id: UserId::new(300),
            content: "reply to attachment-only parent".into(),
            targeting: crate::discord::events::MessageTargeting::Ambient,
            timestamp: Timestamp::parse("2026-01-01T00:00:00Z").unwrap(),
            attachments: vec![],
            is_voice_message: false,
            thread_parent_id: None,
            reply_to_message_id: Some(MessageId::new(999)),
            reply_to_user_id: Some(UserId::new(888)),
            reply_to_user: Some("bob".into()),
            reply_to_content_preview: None,
            bells: None,
            bells_status: None,
        });
        let json = event.into_notification();
        let meta = &json["params"]["meta"];
        assert_eq!(meta["reply_to_user_id"], "888");
        assert_eq!(meta["reply_to_user"], "bob");
        assert!(meta.get("reply_to_content_preview").is_none());
    }

    #[test]
    fn test_message_edit_includes_reply_to_message_id() {
        let event = NotificationEvent::MessageEdit {
            chat_id: ChannelId::new(100),
            message_id: MessageId::new(200),
            user: "alice".into(),
            user_id: UserId::new(300),
            new_content: "edited reply".into(),
            timestamp: Timestamp::parse("2026-01-01T00:00:00Z").unwrap(),
            thread_parent_id: None,
            reply_to_message_id: Some(MessageId::new(888)),
        };
        let json = event.into_notification();
        let meta = &json["params"]["meta"];
        assert_eq!(meta["reply_to_message_id"], "888");
    }

    #[test]
    fn test_message_edit_omits_reply_to_message_id_when_none() {
        let event = NotificationEvent::MessageEdit {
            chat_id: ChannelId::new(100),
            message_id: MessageId::new(200),
            user: "alice".into(),
            user_id: UserId::new(300),
            new_content: "edited non-reply".into(),
            timestamp: Timestamp::parse("2026-01-01T00:00:00Z").unwrap(),
            thread_parent_id: None,
            reply_to_message_id: None,
        };
        let json = event.into_notification();
        let meta = &json["params"]["meta"];
        assert!(meta.get("reply_to_message_id").is_none());
    }

    #[test]
    fn message_envelope_has_no_application_transport_tells() {
        use crate::discord::events::MessageTargeting;

        let event = NotificationEvent::Message(MessageEvent {
            chat_id: ChannelId::new(100),
            message_id: MessageId::new(200),
            user: "alice".into(),
            user_id: UserId::new(4242),
            content: "hello".into(),
            targeting: MessageTargeting::Ambient,
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
        });
        let notification = event.into_notification();
        let meta = &notification["params"]["meta"];
        assert_eq!(meta["user_id"], "4242");
        for forbidden in [
            "app_action_provenance",
            "app_action_state",
            "app_action_provider",
            "represented_sender_id",
        ] {
            assert!(meta.get(forbidden).is_none(), "leaked {forbidden}");
        }
    }
}
