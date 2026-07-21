use crate::gaie::{Attachment, Event, EventKind};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap};
use thiserror::Error;

/// Errors that make a latest-state replay unsafe to continue.
#[derive(Debug, Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum ReplayError {
    /// A reaction-add event exceeded the representable aggregate count.
    #[error("reaction count overflow for message `{message_id}` and emoji `{emoji}`")]
    ReactionCountOverflow { message_id: String, emoji: String },
}

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
pub fn build_latest_state(events: &[Event]) -> Result<HashMap<String, LatestMessage>, ReplayError> {
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
            EventKind::ReactionAdd => change_reaction(&mut messages, event, true)?,
            EventKind::ReactionRemove => change_reaction(&mut messages, event, false)?,
        }
    }
    Ok(messages)
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

fn change_reaction(
    messages: &mut HashMap<String, LatestMessage>,
    event: &Event,
    add: bool,
) -> Result<(), ReplayError> {
    if let Some(message) = messages.get_mut(&event.source.message_id) {
        let key = reaction_key(event);
        let count = message.reactions.get(&key).copied().unwrap_or(0);
        match reaction_count_after(count, add).ok_or_else(|| {
            ReplayError::ReactionCountOverflow {
                message_id: event.source.message_id.clone(),
                emoji: key.clone(),
            }
        })? {
            Some(count) => {
                message.reactions.insert(key, count);
            }
            None => {
                message.reactions.remove(&key);
            }
        }
    }
    Ok(())
}

fn reaction_count_after(current: u64, add: bool) -> Option<Option<u64>> {
    if add {
        current.checked_add(1).map(Some)
    } else if current > 1 {
        Some(Some(current - 1))
    } else {
        Some(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gaie::{Ingest, Lineage, Payload, Relations, Source};
    use proptest::prelude::*;

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

    // The oracle stores one nonnegative integer and does not reuse the
    // production map transition.
    proptest! {
        #![proptest_config(ProptestConfig::with_cases(128))]

        #[test]
        fn prop_gaie_archive_reaction_replay_matches_nonnegative_model(
            operations in prop::collection::vec(any::<bool>(), 0..128),
        ) {
            let mut events = vec![event(EventKind::MessageCreate, 1)];
            let mut expected = 0_u64;
            for (index, add) in operations.into_iter().enumerate() {
                events.push(event(
                    if add { EventKind::ReactionAdd } else { EventKind::ReactionRemove },
                    index as u64 + 2,
                ));
                expected = if add { expected + 1 } else { expected.saturating_sub(1) };
            }
            let first = build_latest_state(&events).unwrap();
            let actual = first["3"].reactions.get("💜").copied().unwrap_or(0);
            prop_assert_eq!(actual, expected);
        }
    }

    #[test]
    fn test_gaie_archive_reaction_overflow_fails_closed() {
        let mut snapshot = event(EventKind::ReactionSnapshot, 2);
        snapshot.payload.count = Some(u64::MAX);
        snapshot.payload.content = Some("💜".to_owned());
        let events = [
            event(EventKind::MessageCreate, 1),
            snapshot,
            event(EventKind::ReactionAdd, 3),
        ];
        assert!(matches!(
            build_latest_state(&events),
            Err(ReplayError::ReactionCountOverflow { .. })
        ));
    }
}

#[cfg(kani)]
mod proofs {
    use super::reaction_count_after;

    #[kani::proof]
    fn reaction_transition_satisfies_checked_counter_contract() {
        let current: u64 = kani::any();
        let add: bool = kani::any();
        let actual = reaction_count_after(current, add);
        if add {
            if current == u64::MAX {
                assert_eq!(actual, None);
            } else {
                assert_eq!(actual, Some(Some(current + 1)));
            }
        } else if current > 1 {
            assert_eq!(actual, Some(Some(current - 1)));
        } else {
            assert_eq!(actual, Some(None));
        }
    }
}
