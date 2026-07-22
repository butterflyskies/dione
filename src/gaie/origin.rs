use crate::gaie::{Event, EventKind};
use serde_json::Value;
use sha2::{Digest as _, Sha256};

pub(crate) const DISCORD_HTTP_ADAPTER_NAME: &str = "discord-http-message-page";
pub(crate) const DISCORD_HTTP_ADAPTER_VERSION: &str = "1";
pub(crate) const DISCORD_COLLECTOR_VERSION: &str = "gaie-alpha-0.8.0";

#[derive(Debug)]
pub(crate) struct DiscordAttachmentProjection {
    pub(crate) id: String,
    pub(crate) filename: String,
    pub(crate) media_type: Option<String>,
    pub(crate) size: u64,
    pub(crate) url: String,
}

#[derive(Debug)]
pub(crate) struct DiscordMessageProjection {
    pub(crate) message_id: String,
    pub(crate) channel_id: Option<String>,
    pub(crate) guild_id: Option<String>,
    pub(crate) actor_id: Option<String>,
    pub(crate) created_at: Option<String>,
    pub(crate) edited_at: Option<String>,
    pub(crate) embedded_thread_id: Option<String>,
    pub(crate) content: Option<String>,
    pub(crate) content_sha256: Option<String>,
    pub(crate) attachments: Vec<DiscordAttachmentProjection>,
    pub(crate) reply_to_message_id: Option<String>,
    pub(crate) history_status: String,
    pub(crate) raw_payload_sha256: String,
}

#[derive(Debug)]
pub(crate) struct DiscordReactionProjection {
    pub(crate) emoji_name: String,
    pub(crate) emoji_id: Option<String>,
    pub(crate) key_sha256: String,
    pub(crate) count: u64,
    pub(crate) normal_count: u64,
    pub(crate) burst_count: u64,
    pub(crate) raw_payload_sha256: String,
}

pub(crate) fn project_discord_message(value: &Value) -> Option<DiscordMessageProjection> {
    let message_id = value.get("id")?.as_str()?.to_owned();
    let edited_at = optional_string(value, "edited_timestamp");
    let content = optional_string(value, "content");
    Some(DiscordMessageProjection {
        message_id,
        channel_id: optional_string(value, "channel_id"),
        guild_id: optional_string(value, "guild_id"),
        actor_id: value
            .pointer("/author/id")
            .and_then(Value::as_str)
            .map(str::to_owned),
        created_at: optional_string(value, "timestamp"),
        edited_at: edited_at.clone(),
        embedded_thread_id: value
            .pointer("/thread/id")
            .and_then(Value::as_str)
            .map(str::to_owned),
        content_sha256: content.as_deref().map(string_hash),
        content,
        attachments: value
            .get("attachments")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|attachment| DiscordAttachmentProjection {
                id: attachment
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                filename: attachment
                    .get("filename")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_owned(),
                media_type: attachment
                    .get("content_type")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                size: attachment.get("size").and_then(Value::as_u64).unwrap_or(0),
                url: attachment
                    .get("url")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
            })
            .collect(),
        reply_to_message_id: value
            .pointer("/message_reference/message_id")
            .and_then(Value::as_str)
            .map(str::to_owned),
        history_status: if edited_at.is_some() {
            "unknown_prior"
        } else {
            "complete"
        }
        .to_owned(),
        raw_payload_sha256: discord_value_hash(value, false),
    })
}

pub(crate) fn project_discord_reaction(value: &Value) -> DiscordReactionProjection {
    let emoji_name = value
        .pointer("/emoji/name")
        .and_then(Value::as_str)
        .unwrap_or("?")
        .to_owned();
    let emoji_id = value
        .pointer("/emoji/id")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let key = emoji_id
        .as_ref()
        .map_or_else(|| emoji_name.clone(), |id| format!("{id}:{emoji_name}"));
    let count = value.get("count").and_then(Value::as_u64).unwrap_or(0);
    DiscordReactionProjection {
        emoji_name,
        emoji_id,
        key_sha256: string_hash(&key),
        count,
        normal_count: value
            .pointer("/count_details/normal")
            .and_then(Value::as_u64)
            .unwrap_or(count),
        burst_count: value
            .pointer("/count_details/burst")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        raw_payload_sha256: discord_value_hash(value, true),
    }
}

pub(crate) fn validate_discord_projection(
    page: &Value,
    selector: &str,
    event: &Event,
) -> Result<(), &'static str> {
    let parts: Vec<_> = selector.split('/').collect();
    let (message_index, reaction_index) = match (event.event_kind, parts.as_slice()) {
        (EventKind::MessageCreate, ["", message_index]) => (*message_index, None),
        (EventKind::ReactionSnapshot, ["", message_index, "reactions", reaction_index]) => {
            (*message_index, Some(*reaction_index))
        }
        _ => return Err("selector is not canonical for the event kind"),
    };
    let message_index = canonical_index(message_index).ok_or("message index is invalid")?;
    let message = page
        .get(message_index)
        .ok_or("selector message does not resolve")?;
    let message_projection =
        project_discord_message(message).ok_or("selected message has no string id")?;
    if event.source.message_id != message_projection.message_id {
        return Err("selector does not identify the event message");
    }
    validate_common(event, &message_projection)?;
    match reaction_index {
        None => validate_message_event(event, &message_projection),
        Some(reaction_index) => {
            let reaction_index =
                canonical_index(reaction_index).ok_or("reaction index is invalid")?;
            let reaction = message
                .pointer(&format!("/reactions/{reaction_index}"))
                .ok_or("selector reaction does not resolve")?;
            validate_reaction_event(event, &project_discord_reaction(reaction))
        }
    }
}

fn validate_common(
    event: &Event,
    message: &DiscordMessageProjection,
) -> Result<(), &'static str> {
    if event.schema_version != "1"
        || event.source.platform != "discord"
        || event.ingest.collector_version != DISCORD_COLLECTOR_VERSION
    {
        return Err("event constants do not match the adapter contract");
    }
    if message
        .channel_id
        .as_ref()
        .is_some_and(|channel_id| event.source.channel_id != *channel_id)
        || message
            .guild_id
            .as_ref()
            .is_some_and(|guild_id| event.source.guild_id != *guild_id)
    {
        return Err("response-supplied capture context does not match the event");
    }
    if let Some(thread_id) = &message.embedded_thread_id
        && (event.source.thread_id.as_ref() != Some(thread_id)
            || event.relations.thread_parent_channel_id.as_deref()
                != Some(event.source.channel_id.as_str()))
    {
        return Err("embedded thread projection does not match the event");
    }
    Ok(())
}

fn validate_message_event(
    event: &Event,
    expected: &DiscordMessageProjection,
) -> Result<(), &'static str> {
    let attachments_match = event.payload.attachments.len() == expected.attachments.len()
        && event
            .payload
            .attachments
            .iter()
            .zip(&expected.attachments)
            .all(|(actual, expected)| {
                actual.id == expected.id
                    && actual.filename == expected.filename
                    && actual.media_type == expected.media_type
                    && actual.size == expected.size
                    && actual.url == expected.url
            });
    if event.source.actor_id != expected.actor_id
        || event.source.created_at != expected.created_at
        || event.source.edited_at != expected.edited_at
        || event.payload.content != expected.content
        || event.payload.content_sha256 != expected.content_sha256
        || !attachments_match
        || event.payload.emoji_id.is_some()
        || event.payload.count.is_some()
        || event.payload.normal_count.is_some()
        || event.payload.burst_count.is_some()
        || event.relations.reply_to_message_id != expected.reply_to_message_id
        || event.lineage.observed_version_ordinal.is_some()
        || event.lineage.predecessor_event_id.is_some()
        || event.lineage.history_status != expected.history_status
        || event.ingest.raw_payload_sha256 != expected.raw_payload_sha256
    {
        return Err("message projection does not match the selected source JSON");
    }
    Ok(())
}

fn validate_reaction_event(
    event: &Event,
    expected: &DiscordReactionProjection,
) -> Result<(), &'static str> {
    if event.source.actor_id.is_some()
        || event.source.created_at.is_some()
        || event.source.edited_at.is_some()
        || event.payload.content.as_deref() != Some(expected.emoji_name.as_str())
        || event.payload.content_sha256.as_deref() != Some(expected.key_sha256.as_str())
        || !event.payload.attachments.is_empty()
        || event.payload.emoji_id != expected.emoji_id
        || event.payload.count != Some(expected.count)
        || event.payload.normal_count != Some(expected.normal_count)
        || event.payload.burst_count != Some(expected.burst_count)
        || event.relations.reply_to_message_id.is_some()
        || event.lineage.observed_version_ordinal.is_some()
        || event.lineage.predecessor_event_id.is_some()
        || event.lineage.history_status != "complete"
        || event.ingest.raw_payload_sha256 != expected.raw_payload_sha256
    {
        return Err("reaction projection does not match the selected source JSON");
    }
    Ok(())
}

fn canonical_index(value: &str) -> Option<usize> {
    value
        .parse::<usize>()
        .ok()
        .filter(|index| index.to_string() == value)
}

fn optional_string(value: &Value, field: &str) -> Option<String> {
    value.get(field).and_then(Value::as_str).map(str::to_owned)
}

fn string_hash(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))
}

pub(crate) fn discord_value_hash(value: &Value, ensure_ascii: bool) -> String {
    format!(
        "{:x}",
        Sha256::digest(python_json(value, ensure_ascii).as_bytes())
    )
}

fn python_json(value: &Value, ensure_ascii: bool) -> String {
    match value {
        Value::Null => "null".to_owned(),
        Value::Bool(value) => if *value { "true" } else { "false" }.to_owned(),
        Value::Number(value) => value.to_string(),
        Value::String(value) => json_string(value, ensure_ascii),
        Value::Array(values) => format!(
            "[{}]",
            values
                .iter()
                .map(|value| python_json(value, ensure_ascii))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Value::Object(values) => {
            let mut entries: Vec<_> = values.iter().collect();
            entries.sort_by_key(|(key, _)| *key);
            format!(
                "{{{}}}",
                entries
                    .into_iter()
                    .map(|(key, value)| {
                        format!(
                            "{}: {}",
                            json_string(key, ensure_ascii),
                            python_json(value, ensure_ascii)
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        }
    }
}

fn json_string(value: &str, ensure_ascii: bool) -> String {
    let encoded = serde_json::to_string(value).unwrap_or_default();
    if !ensure_ascii {
        return encoded;
    }
    let mut ascii = String::new();
    for character in encoded.chars() {
        if character.is_ascii() {
            ascii.push(character);
        } else {
            let code = character as u32;
            if code <= 0xffff {
                ascii.push_str(&format!("\\u{code:04x}"));
            } else {
                let adjusted = code - 0x1_0000;
                ascii.push_str(&format!(
                    "\\u{:04x}\\u{:04x}",
                    0xd800 + (adjusted >> 10),
                    0xdc00 + (adjusted & 0x3ff)
                ));
            }
        }
    }
    ascii
}
