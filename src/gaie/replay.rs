use crate::gaie::{Attachment, Event, EventKind};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap};

/// The Python-v11-compatible latest view of one Discord message.
#[derive(Debug, Clone, Serialize)]
pub struct LatestMessage {
    pub message_id: String,
    pub author_id: Option<String>,
    pub content: Option<String>,
    pub created_at: Option<String>,
    pub edited_at: Option<String>,
    pub channel_id: String,
    pub thread_id: Option<String>,
    pub attachments: Vec<Attachment>,
    pub reply_to: Option<String>,
    pub reactions: BTreeMap<String, u64>,
    pub deleted: bool,
    pub versions: u64,
    pub history_status: String,
}

/// Replays committed create/edit/delete and reaction events into latest state.
pub fn build_latest_state(events: &[Event]) -> HashMap<String, LatestMessage> {
    let mut messages = HashMap::new();
    for event in events {
        let message_id = &event.source.message_id;
        match event.event_kind {
            EventKind::MessageCreate => {
                messages.insert(
                    message_id.clone(),
                    from_event(event, &event.lineage.history_status),
                );
            }
            EventKind::MessageEdit => {
                if let Some(message) = messages.get_mut(message_id) {
                    message.content.clone_from(&event.payload.content);
                    message.edited_at.clone_from(&event.source.edited_at);
                    message.versions += 1;
                } else {
                    messages.insert(message_id.clone(), from_event(event, "unknown_prior"));
                }
            }
            EventKind::MessageDelete => {
                if let Some(message) = messages.get_mut(message_id) {
                    message.deleted = true;
                }
            }
            EventKind::ReactionSnapshot => set_reaction(&mut messages, event),
            EventKind::ReactionAdd => change_reaction(&mut messages, event, true),
            EventKind::ReactionRemove => change_reaction(&mut messages, event, false),
        }
    }
    messages
}

fn from_event(event: &Event, history_status: &str) -> LatestMessage {
    LatestMessage {
        message_id: event.source.message_id.clone(),
        author_id: event.source.actor_id.clone(),
        content: event.payload.content.clone(),
        created_at: event.source.created_at.clone(),
        edited_at: event.source.edited_at.clone(),
        channel_id: event.source.channel_id.clone(),
        thread_id: event.source.thread_id.clone(),
        attachments: event.payload.attachments.clone(),
        reply_to: event.relations.reply_to_message_id.clone(),
        reactions: BTreeMap::new(),
        deleted: false,
        versions: 1,
        history_status: history_status.to_owned(),
    }
}

fn reaction_key(event: &Event) -> String {
    match &event.payload.emoji_id {
        Some(id) => format!("{id}:{}", event.payload.content.as_deref().unwrap_or("?")),
        None => event
            .payload
            .content
            .clone()
            .unwrap_or_else(|| "?".to_owned()),
    }
}

fn set_reaction(messages: &mut HashMap<String, LatestMessage>, event: &Event) {
    if let Some(message) = messages.get_mut(&event.source.message_id) {
        message
            .reactions
            .insert(reaction_key(event), event.payload.count.unwrap_or(1));
    }
}

fn change_reaction(messages: &mut HashMap<String, LatestMessage>, event: &Event, add: bool) {
    if let Some(message) = messages.get_mut(&event.source.message_id) {
        let key = reaction_key(event);
        let count = message.reactions.get(&key).copied().unwrap_or(0);
        if add {
            message.reactions.insert(key, count + 1);
        } else if count > 1 {
            message.reactions.insert(key, count - 1);
        } else {
            message.reactions.remove(&key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gaie::{Ingest, Lineage, Payload, Relations, Source};

    fn event(kind: EventKind, sequence: u64) -> Event {
        Event {
            schema_version: "1".into(),
            corpus_id: "fixture".into(),
            archive_seq: sequence,
            event_id: format!("event-{sequence}"),
            event_kind: kind,
            observed_at: "2026-01-01T00:00:00Z".into(),
            source: Source {
                platform: "discord".into(),
                guild_id: "1".into(),
                channel_id: "2".into(),
                thread_id: None,
                message_id: "3".into(),
                actor_id: Some("4".into()),
                created_at: Some("2026-01-01T00:00:00Z".into()),
                edited_at: None,
            },
            payload: Payload {
                content: Some(
                    if matches!(kind, EventKind::ReactionAdd | EventKind::ReactionRemove) {
                        "💜"
                    } else {
                        "hello"
                    }
                    .into(),
                ),
                content_sha256: None,
                attachments: vec![],
                emoji_id: None,
                count: None,
                normal_count: None,
                burst_count: None,
            },
            relations: Relations {
                reply_to_message_id: None,
                thread_parent_channel_id: None,
            },
            lineage: Lineage {
                observed_version_ordinal: None,
                predecessor_event_id: None,
                history_status: "complete".into(),
            },
            ingest: Ingest {
                collector_version: "fixture".into(),
                raw_payload_sha256: "00".into(),
            },
        }
    }

    #[test]
    fn test_replay_is_idempotent_for_same_committed_snapshot() {
        let events = vec![
            event(EventKind::MessageCreate, 1),
            event(EventKind::ReactionAdd, 2),
            event(EventKind::ReactionRemove, 3),
        ];
        assert_eq!(
            serde_json::to_value(build_latest_state(&events)).unwrap(),
            serde_json::to_value(build_latest_state(&events)).unwrap()
        );
    }
}
