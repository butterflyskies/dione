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

/// Convert a [`NotificationEvent`] into a notification item (params only, no
/// JSON-RPC envelope). Used as a building block for batch notifications.
fn event_to_notification_item(event: NotificationEvent) -> Value {
    // Batch notifications should only contain channel events. Non-channel
    // events (Trace, PermissionResponse, ConfigError) take the Immediate
    // path in the delivery buffer and never reach the batch path.
    debug_assert!(
        matches!(
            event,
            NotificationEvent::Message { .. }
                | NotificationEvent::MessageEdit { .. }
                | NotificationEvent::MessageDelete { .. }
                | NotificationEvent::Reaction { .. }
        ),
        "non-channel event in batch path: {event:?}"
    );

    let full = event_to_notification(event);
    // Extract the params and method from the full notification to build
    // a compact item that preserves the event's method and content.
    let method = full["method"]
        .as_str()
        .unwrap_or("notifications/claude/channel");
    let mut item = json!({ "method": method });
    if let Some(params) = full.get("params") {
        item["params"] = params.clone();
    }
    item
}

/// Convert multiple buffered events into a single batch MCP notification.
///
/// Each flush produces exactly one JSON-RPC line on stdout regardless of how
/// many events were buffered. Single-event flushes use the same batch format
/// for consistency.
pub(crate) fn events_to_batch_notification(events: Vec<NotificationEvent>) -> Value {
    let items: Vec<Value> = events.into_iter().map(event_to_notification_item).collect();
    json!({
        "jsonrpc": "2.0",
        "method": "notifications/claude/channel/batch",
        "params": {
            "events": items,
        }
    })
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
        let json = event_to_notification(event);
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
        let json = event_to_notification(event);
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
        let json = event_to_notification(event);
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
        let json = event_to_notification(event);
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
        let json = event_to_notification(event);
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
        let json = event_to_notification(event);
        let meta = &json["params"]["meta"];
        assert_eq!(meta["thread_parent_id"], "600");
    }

    // ── Batch notification tests ─────────────────────────────────────────────

    #[test]
    fn test_batch_notification_single_event() {
        let events = vec![NotificationEvent::Message {
            chat_id: "100".into(),
            message_id: "200".into(),
            user: "alice".into(),
            user_id: "300".into(),
            content: "hello".into(),
            timestamp: "2026-01-01T00:00:00Z".into(),
            attachments: vec![],
            is_voice_message: false,
            thread_parent_id: None,
        }];
        let batch = events_to_batch_notification(events);
        assert_eq!(batch["jsonrpc"], "2.0");
        assert_eq!(batch["method"], "notifications/claude/channel/batch");
        let items = batch["params"]["events"].as_array().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["method"], "notifications/claude/channel");
        assert_eq!(items[0]["params"]["content"], "hello");
    }

    #[test]
    fn test_batch_notification_multiple_events_preserves_order() {
        let events = vec![
            NotificationEvent::Message {
                chat_id: "100".into(),
                message_id: "1".into(),
                user: "alice".into(),
                user_id: "300".into(),
                content: "first".into(),
                timestamp: "2026-01-01T00:00:00Z".into(),
                attachments: vec![],
                is_voice_message: false,
                thread_parent_id: None,
            },
            NotificationEvent::Reaction {
                chat_id: "100".into(),
                message_id: "1".into(),
                user: "bob".into(),
                user_id: "400".into(),
                emoji: "👍".into(),
            },
            NotificationEvent::MessageEdit {
                chat_id: "100".into(),
                message_id: "1".into(),
                user: "alice".into(),
                user_id: "300".into(),
                new_content: "edited".into(),
                timestamp: "2026-01-01T00:00:01Z".into(),
                thread_parent_id: None,
            },
        ];
        let batch = events_to_batch_notification(events);
        let items = batch["params"]["events"].as_array().unwrap();
        assert_eq!(items.len(), 3);
        assert_eq!(items[0]["params"]["content"], "first");
        assert_eq!(items[1]["params"]["content"], "reacted with 👍");
        assert_eq!(items[2]["params"]["content"], "edited");
    }

    #[test]
    fn test_batch_notification_preserves_per_event_method() {
        let events = vec![
            NotificationEvent::Message {
                chat_id: "100".into(),
                message_id: "1".into(),
                user: "alice".into(),
                user_id: "300".into(),
                content: "msg".into(),
                timestamp: "2026-01-01T00:00:00Z".into(),
                attachments: vec![],
                is_voice_message: false,
                thread_parent_id: None,
            },
            NotificationEvent::MessageDelete {
                chat_id: "100".into(),
                message_id: "2".into(),
                thread_parent_id: None,
            },
        ];
        let batch = events_to_batch_notification(events);
        let items = batch["params"]["events"].as_array().unwrap();
        // Both channel events use the same method.
        assert_eq!(items[0]["method"], "notifications/claude/channel");
        assert_eq!(items[1]["method"], "notifications/claude/channel");
    }

    #[test]
    fn test_batch_notification_empty_events() {
        let batch = events_to_batch_notification(vec![]);
        assert_eq!(batch["method"], "notifications/claude/channel/batch");
        let items = batch["params"]["events"].as_array().unwrap();
        assert!(items.is_empty());
    }
}
