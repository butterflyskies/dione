//! Guild message search: wraps Discord's `GET /guilds/{guild_id}/messages/search`.

use std::collections::{BTreeMap, HashSet};
use std::fmt;

use serde::Deserialize;
use serde_json::{Value, json};
use serenity::model::id::{ChannelId, GuildId, UserId};
use thiserror::Error;

use super::messaging::{MessagingCtx, message_json};
use crate::gate::OutboundGate;
use crate::mcp::ids::Snowflake;

// ── Error type ───────────────────────────────────────────────────────────────

/// Validation errors for search parameters.
///
/// Used by [`SearchLimit`], [`SearchOffset`], and [`date_to_snowflake`] for
/// typed validation. The top-level [`search_messages`] function converts these
/// to `json!({"error": …})` strings, following the crate-wide convention
/// where all tool handlers return `Value`.
#[derive(Debug, Error)]
pub enum SearchError {
    #[error(
        "at least one search filter is required (content, author_id, mentions, has, before, after, or channel_id)"
    )]
    NoFilters,
    #[error("invalid date '{0}': expected YYYY-MM-DD")]
    InvalidDate(String),
    #[error("search limit must be 1..=25, got {0}")]
    InvalidLimit(u64),
    #[error("search offset must be 0..=9975, got {0}")]
    InvalidOffset(u64),
}

// ── Validated newtypes ──────────────────────────────────────────────────────

/// Search result limit, validated to Discord's 1..=25 range. Default: 25.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(try_from = "u64")]
pub struct SearchLimit(u8);

impl SearchLimit {
    pub fn get(self) -> u8 {
        self.0
    }
}

impl Default for SearchLimit {
    fn default() -> Self {
        Self(25)
    }
}

impl TryFrom<u64> for SearchLimit {
    type Error = SearchError;

    fn try_from(v: u64) -> Result<Self, Self::Error> {
        if !(1..=25).contains(&v) {
            return Err(SearchError::InvalidLimit(v));
        }
        Ok(Self(v as u8))
    }
}

/// Search result offset, validated to 0..=9975. Default: 0.
#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(try_from = "u64")]
pub struct SearchOffset(u16);

impl SearchOffset {
    pub fn get(self) -> u16 {
        self.0
    }
}

impl TryFrom<u64> for SearchOffset {
    type Error = SearchError;

    fn try_from(v: u64) -> Result<Self, Self::Error> {
        if v > 9975 {
            return Err(SearchError::InvalidOffset(v));
        }
        Ok(Self(v as u16))
    }
}

/// A timestamp for search bounds — accepts either a Discord snowflake ID
/// or an ISO 8601 date (YYYY-MM-DD).
#[derive(Debug, Clone)]
pub enum SearchTimestamp {
    Snowflake(u64),
    Date(chrono::NaiveDate),
}

impl SearchTimestamp {
    pub fn to_snowflake(&self) -> Result<u64, SearchError> {
        match self {
            Self::Snowflake(id) => Ok(*id),
            Self::Date(nd) => {
                let dt = nd
                    .and_hms_opt(0, 0, 0)
                    .ok_or_else(|| SearchError::InvalidDate(nd.to_string()))?;
                let ms = dt.and_utc().timestamp_millis();
                let discord_ms = ms - DISCORD_EPOCH_MS;
                if discord_ms < 0 {
                    return Err(SearchError::InvalidDate(format!(
                        "{nd}: date is before Discord epoch (2015-01-01)"
                    )));
                }
                Ok((discord_ms as u64) << 22)
            }
        }
    }
}

impl<'de> Deserialize<'de> for SearchTimestamp {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct TimestampVisitor;

        impl<'de> serde::de::Visitor<'de> for TimestampVisitor {
            type Value = SearchTimestamp;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                write!(f, "a snowflake ID (integer/string) or YYYY-MM-DD date")
            }

            fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<Self::Value, E> {
                if v == 0 {
                    return Err(E::custom("snowflake ID must be nonzero"));
                }
                Ok(SearchTimestamp::Snowflake(v))
            }

            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
                if let Ok(id) = v.parse::<u64>() {
                    if id == 0 {
                        return Err(E::custom("snowflake ID must be nonzero"));
                    }
                    return Ok(SearchTimestamp::Snowflake(id));
                }
                chrono::NaiveDate::parse_from_str(v, "%Y-%m-%d")
                    .map(SearchTimestamp::Date)
                    .map_err(|_| {
                        E::custom(format!("expected snowflake ID or YYYY-MM-DD, got '{v}'"))
                    })
            }
        }

        deserializer.deserialize_any(TimestampVisitor)
    }
}

// ── Enums ───────────────────────────────────────────────────────────────────

/// Attachment type filter for search.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchHas {
    Link,
    Embed,
    File,
    Video,
    Image,
    Sound,
    Sticker,
}

impl fmt::Display for SearchHas {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Link => f.write_str("link"),
            Self::Embed => f.write_str("embed"),
            Self::File => f.write_str("file"),
            Self::Video => f.write_str("video"),
            Self::Image => f.write_str("image"),
            Self::Sound => f.write_str("sound"),
            Self::Sticker => f.write_str("sticker"),
        }
    }
}

/// Sort field for search results.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SortBy {
    Timestamp,
    Relevance,
}

impl fmt::Display for SortBy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Timestamp => f.write_str("timestamp"),
            Self::Relevance => f.write_str("relevance"),
        }
    }
}

/// Sort direction for search results.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SortOrder {
    Asc,
    Desc,
}

impl fmt::Display for SortOrder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Asc => f.write_str("asc"),
            Self::Desc => f.write_str("desc"),
        }
    }
}

// ── ID vector deserialization ───────────────────────────────────────────────

/// Deserializes a single snowflake ID or an array of IDs from string or numeric
/// JSON values, validating each as a nonzero u64.
fn deserialize_id_vec<'de, D>(deserializer: D) -> Result<Vec<Snowflake>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de;

    struct IdVecVisitor;

    impl<'de> de::Visitor<'de> for IdVecVisitor {
        type Value = Vec<Snowflake>;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("a snowflake ID or array of snowflake IDs (string or number)")
        }

        fn visit_u64<E: de::Error>(self, v: u64) -> Result<Self::Value, E> {
            let sf = Snowflake::new(v).ok_or_else(|| E::custom("snowflake ID must be nonzero"))?;
            Ok(vec![sf])
        }

        fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
            let n: u64 = v.parse().map_err(E::custom)?;
            let sf = Snowflake::new(n).ok_or_else(|| E::custom("snowflake ID must be nonzero"))?;
            Ok(vec![sf])
        }

        fn visit_seq<A: de::SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
            let mut ids = Vec::with_capacity(seq.size_hint().unwrap_or(0));
            while let Some(elem) = seq.next_element::<Value>()? {
                let n = match &elem {
                    Value::Number(n) => n
                        .as_u64()
                        .ok_or_else(|| de::Error::custom("ID must be a positive integer"))?,
                    Value::String(s) => s
                        .parse::<u64>()
                        .map_err(|_| de::Error::custom(format!("invalid ID: {s}")))?,
                    _ => return Err(de::Error::custom("ID must be a string or number")),
                };
                let sf = Snowflake::new(n)
                    .ok_or_else(|| de::Error::custom("snowflake ID must be nonzero"))?;
                ids.push(sf);
            }
            Ok(ids)
        }

        fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(Vec::new())
        }
    }

    deserializer.deserialize_any(IdVecVisitor)
}

fn deserialize_user_ids<'de, D>(deserializer: D) -> Result<Vec<UserId>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_id_vec(deserializer).map(|v| v.into_iter().map(Snowflake::user).collect())
}

fn deserialize_channel_ids<'de, D>(deserializer: D) -> Result<Vec<ChannelId>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_id_vec(deserializer).map(|v| v.into_iter().map(Snowflake::channel).collect())
}

// ── Search parameters ───────────────────────────────────────────────────────

/// Parameters for [`search_messages`], deserialized from MCP tool arguments.
///
/// `guild_id` is parsed separately in the dispatch layer (via `parse_id`)
/// for consistency with other tools.
#[derive(Debug, Deserialize)]
pub struct SearchParams {
    #[serde(default)]
    pub content: Option<String>,
    #[serde(
        default,
        rename = "author_id",
        deserialize_with = "deserialize_user_ids"
    )]
    pub author_ids: Vec<UserId>,
    #[serde(default, deserialize_with = "deserialize_user_ids")]
    pub mentions: Vec<UserId>,
    #[serde(default)]
    pub has: Vec<SearchHas>,
    /// Snowflake ID or ISO 8601 date (YYYY-MM-DD). Converted to `max_id`.
    #[serde(default)]
    pub before: Option<SearchTimestamp>,
    /// Snowflake ID or ISO 8601 date (YYYY-MM-DD). Converted to `min_id`.
    #[serde(default)]
    pub after: Option<SearchTimestamp>,
    #[serde(
        default,
        rename = "channel_id",
        deserialize_with = "deserialize_channel_ids"
    )]
    pub channel_ids: Vec<ChannelId>,
    #[serde(default)]
    pub pinned: Option<bool>,
    #[serde(default)]
    pub sort_by: Option<SortBy>,
    #[serde(default)]
    pub sort_order: Option<SortOrder>,
    #[serde(default)]
    pub limit: Option<SearchLimit>,
    #[serde(default)]
    pub offset: Option<SearchOffset>,
}

impl SearchParams {
    /// Returns `true` if at least one search filter is set.
    fn has_any_filter(&self) -> bool {
        self.content.is_some()
            || !self.author_ids.is_empty()
            || !self.mentions.is_empty()
            || !self.has.is_empty()
            || self.before.is_some()
            || self.after.is_some()
            || !self.channel_ids.is_empty()
            || self.pinned.is_some()
    }
}

// ── Date → snowflake conversion ─────────────────────────────────────────────

/// Discord epoch: 2015-01-01T00:00:00Z in milliseconds since Unix epoch.
const DISCORD_EPOCH_MS: i64 = 1_420_070_400_000;

// ── Main search function ────────────────────────────────────────────────────

/// Search messages in a guild using Discord's search endpoint.
///
/// Results are filtered to channels permitted by the outbound gate.
/// At least one search filter must be provided.
pub async fn search_messages(ctx: &MessagingCtx, guild_id: GuildId, params: SearchParams) -> Value {
    // Invariant: at least one filter must be set.
    if !params.has_any_filter() {
        return json!({ "error": SearchError::NoFilters.to_string() });
    }

    // Build query parameters.
    let mut query: Vec<(&str, String)> = Vec::new();

    if let Some(ref content) = params.content {
        query.push(("content", content.clone()));
    }
    for id in &params.author_ids {
        query.push(("author_id", id.get().to_string()));
    }
    for id in &params.mentions {
        query.push(("mentions", id.get().to_string()));
    }
    for h in &params.has {
        query.push(("has", h.to_string()));
    }
    if let Some(ref before) = params.before {
        match before.to_snowflake() {
            Ok(sf) => query.push(("max_id", sf.to_string())),
            Err(e) => return json!({ "error": e.to_string() }),
        }
    }
    if let Some(ref after) = params.after {
        match after.to_snowflake() {
            Ok(sf) => query.push(("min_id", sf.to_string())),
            Err(e) => return json!({ "error": e.to_string() }),
        }
    }
    for id in &params.channel_ids {
        query.push(("channel_id", id.get().to_string()));
    }
    if let Some(pinned) = params.pinned {
        query.push(("pinned", pinned.to_string()));
    }
    if let Some(sort_by) = params.sort_by {
        query.push(("sort_by", sort_by.to_string()));
    }
    if let Some(sort_order) = params.sort_order {
        query.push(("sort_order", sort_order.to_string()));
    }

    let limit = params.limit.unwrap_or_default();
    query.push(("limit", limit.get().to_string()));

    let offset = params.offset.unwrap_or_default();
    if offset.get() > 0 {
        query.push(("offset", offset.get().to_string()));
    }

    // Build and send the request.
    let url = format!(
        "https://discord.com/api/v10/guilds/{}/messages/search",
        guild_id.get()
    );

    let client = reqwest::Client::new();
    let response = match client
        .get(&url)
        .header("Authorization", ctx.http.token())
        .query(&query)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => return json!({ "error": format!("HTTP request failed: {e}") }),
    };

    // Surface rate limits explicitly.
    if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("unknown");
        return json!({
            "error": format!("rate limited by Discord, retry after {retry_after}s"),
            "retry_after": retry_after
        });
    }

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return json!({ "error": format!("Discord API error {status}: {body}") });
    }

    // Parse response body.
    let text = match response.text().await {
        Ok(t) => t,
        Err(e) => return json!({ "error": format!("failed to read response: {e}") }),
    };

    let body: Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(e) => return json!({ "error": format!("failed to parse response: {e}") }),
    };

    // Discord returns [[Message, ...], ...] — nested arrays where the first
    // element of each inner array is the matching message.
    let raw_messages = match body["messages"].as_array() {
        Some(arr) => arr,
        None => {
            return json!({
                "messages": [],
                "count": 0,
                "offset": params.offset.unwrap_or_default().get(),
                "has_more": false,
            });
        }
    };

    // Filter results to channels permitted by the outbound gate.
    let state = ctx.state.read().await;
    let messages = filter_results(
        &ctx.config,
        &state.dm_channel_ids,
        &state.thread_parents,
        raw_messages,
    );
    drop(state);

    let page_size = limit.get() as usize;
    json!({
        "messages": messages,
        "count": messages.len(),
        "offset": offset.get(),
        "has_more": raw_messages.len() >= page_size,
    })
}

// ── Result filtering ────────────────────────────────────────────────────────

/// Filter search results to channels permitted by the outbound gate.
///
/// `raw_messages` is Discord's nested `[[Message, ...], ...]` format where the
/// first element of each inner array is the matching message. Returns only
/// messages from channels that pass the outbound gate check.
fn filter_results(
    config: &crate::config::LoadedConfig,
    dm_channel_ids: &HashSet<u64>,
    thread_parents: &BTreeMap<u64, Option<u64>>,
    raw_messages: &[Value],
) -> Vec<Value> {
    let mut messages: Vec<Value> = Vec::new();

    for group in raw_messages {
        let inner = match group.as_array() {
            Some(arr) if !arr.is_empty() => &arr[0],
            _ => continue,
        };

        // Extract channel_id for the outbound gate check.
        let ch_id = inner["channel_id"]
            .as_str()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);

        if ch_id == 0 {
            continue;
        }

        if !OutboundGate::check_channel_with_threads(config, ch_id, dm_channel_ids, thread_parents)
        {
            continue;
        }

        // Deserialize into serenity Message for format parity via message_json.
        match serde_json::from_value::<serenity::model::channel::Message>(inner.clone()) {
            Ok(msg) => {
                let mut msg_val = message_json(config, &msg);
                // Add channel_id — search spans multiple channels.
                if let Some(obj) = msg_val.as_object_mut() {
                    obj.insert(
                        "channel_id".to_string(),
                        json!(msg.channel_id.get().to_string()),
                    );
                }
                messages.push(msg_val);
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    channel_id = ch_id,
                    "failed to deserialize search result message, skipping"
                );
            }
        }
    }

    messages
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── SearchLimit ─────────────────────────────────────────────────────────

    #[test]
    fn search_limit_default_is_25() {
        assert_eq!(SearchLimit::default().get(), 25);
    }

    #[test]
    fn search_limit_accepts_valid_range() {
        for v in 1..=25 {
            let limit = SearchLimit::try_from(v).unwrap();
            assert_eq!(limit.get(), v as u8);
        }
    }

    #[test]
    fn search_limit_rejects_zero() {
        assert!(SearchLimit::try_from(0u64).is_err());
    }

    #[test]
    fn search_limit_rejects_over_25() {
        assert!(SearchLimit::try_from(26u64).is_err());
        assert!(SearchLimit::try_from(100u64).is_err());
    }

    #[test]
    fn search_limit_deserializes_from_json() {
        let v: SearchLimit = serde_json::from_value(json!(10)).unwrap();
        assert_eq!(v.get(), 10);
    }

    #[test]
    fn search_limit_rejects_invalid_json() {
        let result: Result<SearchLimit, _> = serde_json::from_value(json!(0));
        assert!(result.is_err());
        let result: Result<SearchLimit, _> = serde_json::from_value(json!(26));
        assert!(result.is_err());
    }

    // ── SearchOffset ────────────────────────────────────────────────────────

    #[test]
    fn search_offset_default_is_0() {
        assert_eq!(SearchOffset::default().get(), 0);
    }

    #[test]
    fn search_offset_accepts_valid_range() {
        assert_eq!(SearchOffset::try_from(0u64).unwrap().get(), 0);
        assert_eq!(SearchOffset::try_from(100u64).unwrap().get(), 100);
        assert_eq!(SearchOffset::try_from(9975u64).unwrap().get(), 9975);
    }

    #[test]
    fn search_offset_rejects_over_9975() {
        assert!(SearchOffset::try_from(9976u64).is_err());
        assert!(SearchOffset::try_from(10000u64).is_err());
    }

    #[test]
    fn search_offset_deserializes_from_json() {
        let v: SearchOffset = serde_json::from_value(json!(500)).unwrap();
        assert_eq!(v.get(), 500);
    }

    // ── date_to_snowflake ───────────────────────────────────────────────────

    #[test]
    fn search_timestamp_date_to_snowflake() {
        // 2024-01-15 00:00:00 UTC
        // Unix ms: 1705276800000
        // Discord ms: 1705276800000 - 1420070400000 = 285206400000
        // Snowflake: 285206400000 << 22
        let ts = SearchTimestamp::Date(chrono::NaiveDate::from_ymd_opt(2024, 1, 15).unwrap());
        let sf = ts.to_snowflake().unwrap();
        let expected = (1_705_276_800_000i64 - DISCORD_EPOCH_MS) as u64;
        assert_eq!(sf, expected << 22);
    }

    #[test]
    fn search_timestamp_discord_epoch() {
        let ts = SearchTimestamp::Date(chrono::NaiveDate::from_ymd_opt(2015, 1, 1).unwrap());
        let sf = ts.to_snowflake().unwrap();
        assert_eq!(sf, 0);
    }

    #[test]
    fn search_timestamp_before_epoch_is_error() {
        let ts = SearchTimestamp::Date(chrono::NaiveDate::from_ymd_opt(2014, 12, 31).unwrap());
        assert!(ts.to_snowflake().is_err());
    }

    #[test]
    fn search_timestamp_snowflake_passthrough() {
        let ts = SearchTimestamp::Snowflake(123456789);
        assert_eq!(ts.to_snowflake().unwrap(), 123456789);
    }

    #[test]
    fn search_timestamp_deserializes_date_string() {
        let ts: SearchTimestamp = serde_json::from_value(json!("2024-01-15")).unwrap();
        assert!(matches!(ts, SearchTimestamp::Date(_)));
    }

    #[test]
    fn search_timestamp_deserializes_snowflake_number() {
        let ts: SearchTimestamp = serde_json::from_value(json!(123456789)).unwrap();
        assert!(matches!(ts, SearchTimestamp::Snowflake(123456789)));
    }

    #[test]
    fn search_timestamp_deserializes_snowflake_string() {
        let ts: SearchTimestamp = serde_json::from_value(json!("123456789")).unwrap();
        assert!(matches!(ts, SearchTimestamp::Snowflake(123456789)));
    }

    #[test]
    fn search_timestamp_rejects_invalid() {
        assert!(serde_json::from_value::<SearchTimestamp>(json!("not-a-date")).is_err());
        assert!(serde_json::from_value::<SearchTimestamp>(json!(0)).is_err());
    }

    // ── SearchParams ────────────────────────────────────────────────────────

    #[test]
    fn search_params_deserializes_minimal() {
        let args = json!({ "content": "hello" });
        let params: SearchParams = serde_json::from_value(args).unwrap();
        assert_eq!(params.content.as_deref(), Some("hello"));
        assert!(params.author_ids.is_empty());
        assert!(params.mentions.is_empty());
        assert!(params.channel_ids.is_empty());
        assert!(params.has.is_empty());
        assert!(params.limit.is_none());
        assert!(params.offset.is_none());
    }

    #[test]
    fn search_params_deserializes_full() {
        let args = json!({
            "content": "hello",
            "author_id": "123",
            "mentions": "456",
            "has": ["image", "file"],
            "before": "2024-06-01",
            "after": "2024-01-01",
            "channel_id": "789",
            "sort_by": "timestamp",
            "sort_order": "desc",
            "limit": 10,
            "offset": 25
        });
        let params: SearchParams = serde_json::from_value(args).unwrap();
        assert_eq!(params.content.as_deref(), Some("hello"));
        assert_eq!(params.author_ids, vec![UserId::new(123)]);
        assert_eq!(params.mentions, vec![UserId::new(456)]);
        assert_eq!(params.has, vec![SearchHas::Image, SearchHas::File]);
        assert!(matches!(params.before, Some(SearchTimestamp::Date(_))));
        assert!(matches!(params.after, Some(SearchTimestamp::Date(_))));
        assert_eq!(params.channel_ids, vec![ChannelId::new(789)]);
        assert_eq!(params.sort_by, Some(SortBy::Timestamp));
        assert_eq!(params.sort_order, Some(SortOrder::Desc));
        assert_eq!(params.limit.unwrap().get(), 10);
        assert_eq!(params.offset.unwrap().get(), 25);
    }

    #[test]
    fn search_params_deserializes_id_arrays() {
        let args = json!({
            "content": "hello",
            "author_id": ["100", "200"],
            "mentions": ["300"],
            "channel_id": ["400", "500", "600"]
        });
        let params: SearchParams = serde_json::from_value(args).unwrap();
        assert_eq!(params.author_ids, vec![UserId::new(100), UserId::new(200)]);
        assert_eq!(params.mentions, vec![UserId::new(300)]);
        assert_eq!(
            params.channel_ids,
            vec![
                ChannelId::new(400),
                ChannelId::new(500),
                ChannelId::new(600)
            ]
        );
    }

    #[test]
    fn search_params_deserializes_numeric_ids() {
        let args = json!({
            "content": "hello",
            "author_id": 123,
            "mentions": [456, 789]
        });
        let params: SearchParams = serde_json::from_value(args).unwrap();
        assert_eq!(params.author_ids, vec![UserId::new(123)]);
        assert_eq!(params.mentions, vec![UserId::new(456), UserId::new(789)]);
    }

    #[test]
    fn search_params_rejects_zero_id() {
        let args = json!({ "author_id": "0" });
        assert!(serde_json::from_value::<SearchParams>(args).is_err());

        let args = json!({ "channel_id": 0 });
        assert!(serde_json::from_value::<SearchParams>(args).is_err());

        let args = json!({ "mentions": ["123", "0"] });
        assert!(serde_json::from_value::<SearchParams>(args).is_err());
    }

    #[test]
    fn search_params_rejects_non_numeric_id() {
        let args = json!({ "author_id": "not-a-number" });
        assert!(serde_json::from_value::<SearchParams>(args).is_err());

        let args = json!({ "channel_id": ["123", "abc"] });
        assert!(serde_json::from_value::<SearchParams>(args).is_err());
    }

    #[test]
    fn search_params_ignores_unknown_fields() {
        // guild_id is parsed separately in dispatch; serde ignores it here.
        let args = json!({
            "content": "hello",
            "guild_id": "999",
            "unknown_field": true
        });
        let params: SearchParams = serde_json::from_value(args).unwrap();
        assert_eq!(params.content.as_deref(), Some("hello"));
    }

    #[test]
    fn search_params_has_any_filter_empty() {
        let empty: SearchParams = serde_json::from_value(json!({})).unwrap();
        assert!(
            !empty.has_any_filter(),
            "params with no filters must return false"
        );
    }

    #[test]
    fn search_params_has_any_filter_content() {
        let p: SearchParams = serde_json::from_value(json!({"content": "x"})).unwrap();
        assert!(p.has_any_filter());
    }

    #[test]
    fn search_params_has_any_filter_has() {
        let p: SearchParams = serde_json::from_value(json!({"has": ["image"]})).unwrap();
        assert!(p.has_any_filter());
    }

    #[test]
    fn search_params_has_any_filter_before() {
        let p: SearchParams = serde_json::from_value(json!({"before": "2024-01-01"})).unwrap();
        assert!(p.has_any_filter());
    }

    #[test]
    fn search_params_has_any_filter_channel() {
        let p: SearchParams = serde_json::from_value(json!({"channel_id": "123"})).unwrap();
        assert!(p.has_any_filter());
    }

    #[test]
    fn search_params_has_any_filter_author_id() {
        let p: SearchParams = serde_json::from_value(json!({"author_id": "123"})).unwrap();
        assert!(p.has_any_filter());
    }

    #[test]
    fn search_params_has_any_filter_mentions() {
        let p: SearchParams = serde_json::from_value(json!({"mentions": "456"})).unwrap();
        assert!(p.has_any_filter());
    }

    #[test]
    fn search_params_has_any_filter_after() {
        let p: SearchParams = serde_json::from_value(json!({"after": "2024-06-01"})).unwrap();
        assert!(p.has_any_filter());
    }

    #[test]
    fn search_params_has_any_filter_pinned() {
        let p: SearchParams = serde_json::from_value(json!({"pinned": true})).unwrap();
        assert!(p.has_any_filter());
    }

    #[test]
    fn search_params_has_any_filter_sort_by_alone_is_not_filter() {
        let p: SearchParams = serde_json::from_value(json!({"sort_by": "timestamp"})).unwrap();
        assert!(!p.has_any_filter(), "sort_by alone is not a search filter");
    }

    #[test]
    fn search_params_has_any_filter_sort_order_alone_is_not_filter() {
        let p: SearchParams = serde_json::from_value(json!({"sort_order": "desc"})).unwrap();
        assert!(
            !p.has_any_filter(),
            "sort_order alone is not a search filter"
        );
    }

    // ── filter_results ─────────────────────────────────────────────────────

    #[test]
    fn filter_results_omits_unconfigured_channels() {
        use crate::config::{ChannelConfig, Config};

        let mut raw = Config::default();
        raw.channels.push(ChannelConfig {
            id: "42".into(),
            ..Default::default()
        });
        let config = crate::config::LoadedConfig::from_raw(raw);

        let configured_msg = json!({
            "id": "1",
            "type": 0,
            "channel_id": "42",
            "author": {
                "id": "1", "username": "test", "global_name": null,
                "avatar": null, "discriminator": "0", "public_flags": 0
            },
            "content": "hello",
            "timestamp": "2024-01-01T00:00:00.000000+00:00",
            "edited_timestamp": null,
            "tts": false,
            "mention_everyone": false,
            "mentions": [],
            "mention_roles": [],
            "attachments": [],
            "embeds": [],
            "pinned": false
        });

        let unconfigured_msg = json!({
            "id": "2",
            "type": 0,
            "channel_id": "999",
            "author": {
                "id": "1", "username": "test", "global_name": null,
                "avatar": null, "discriminator": "0", "public_flags": 0
            },
            "content": "secret",
            "timestamp": "2024-01-01T00:00:00.000000+00:00",
            "edited_timestamp": null,
            "tts": false,
            "mention_everyone": false,
            "mentions": [],
            "mention_roles": [],
            "attachments": [],
            "embeds": [],
            "pinned": false
        });

        // Discord returns [[msg], [msg]] format.
        let raw_messages = vec![json!([configured_msg]), json!([unconfigured_msg])];

        let dm_ids = HashSet::new();
        let thread_parents = BTreeMap::new();

        let results = filter_results(&config, &dm_ids, &thread_parents, &raw_messages);

        assert_eq!(
            results.len(),
            1,
            "only the configured channel's message should survive"
        );
        assert_eq!(results[0]["channel_id"], "42");
    }

    #[test]
    fn filter_results_passes_thread_of_configured_channel() {
        use crate::config::{ChannelConfig, Config};

        let mut raw = Config::default();
        raw.channels.push(ChannelConfig {
            id: "42".into(),
            ..Default::default()
        });
        let config = crate::config::LoadedConfig::from_raw(raw);

        let thread_msg = wire_search_message("100", "77");

        let raw_messages = vec![json!([thread_msg])];
        let dm_ids = HashSet::new();
        let mut thread_parents = BTreeMap::new();
        thread_parents.insert(77u64, Some(42u64));

        let results = filter_results(&config, &dm_ids, &thread_parents, &raw_messages);

        assert_eq!(results.len(), 1, "thread of configured channel should pass");
    }

    #[test]
    fn filter_results_passes_dm_channel() {
        use crate::config::Config;

        let config = crate::config::LoadedConfig::from_raw(Config::default());

        let dm_msg = wire_search_message("101", "55");

        let raw_messages = vec![json!([dm_msg])];
        let mut dm_ids = HashSet::new();
        dm_ids.insert(55u64);
        let thread_parents = BTreeMap::new();

        let results = filter_results(&config, &dm_ids, &thread_parents, &raw_messages);

        assert_eq!(results.len(), 1, "known DM channel should pass");
    }

    /// Helper: build a minimal Discord message JSON for filter tests.
    fn wire_search_message(msg_id: &str, channel_id: &str) -> Value {
        json!({
            "id": msg_id,
            "type": 0,
            "channel_id": channel_id,
            "author": {
                "id": "1", "username": "test", "global_name": null,
                "avatar": null, "discriminator": "0", "public_flags": 0
            },
            "content": "test message",
            "timestamp": "2024-01-01T00:00:00.000000+00:00",
            "edited_timestamp": null,
            "tts": false,
            "mention_everyone": false,
            "mentions": [],
            "mention_roles": [],
            "attachments": [],
            "embeds": [],
            "pinned": false
        })
    }

    // ── Enum Display ────────────────────────────────────────────────────────

    #[test]
    fn search_has_display() {
        assert_eq!(SearchHas::Link.to_string(), "link");
        assert_eq!(SearchHas::Image.to_string(), "image");
        assert_eq!(SearchHas::Sticker.to_string(), "sticker");
    }

    #[test]
    fn sort_by_display() {
        assert_eq!(SortBy::Timestamp.to_string(), "timestamp");
        assert_eq!(SortBy::Relevance.to_string(), "relevance");
    }

    #[test]
    fn sort_order_display() {
        assert_eq!(SortOrder::Asc.to_string(), "asc");
        assert_eq!(SortOrder::Desc.to_string(), "desc");
    }

    // ── Enum deserialization ────────────────────────────────────────────────

    #[test]
    fn search_has_deserializes_all_variants() {
        for (s, expected) in [
            ("link", SearchHas::Link),
            ("embed", SearchHas::Embed),
            ("file", SearchHas::File),
            ("video", SearchHas::Video),
            ("image", SearchHas::Image),
            ("sound", SearchHas::Sound),
            ("sticker", SearchHas::Sticker),
        ] {
            let v: SearchHas = serde_json::from_value(json!(s)).unwrap();
            assert_eq!(v, expected);
        }
    }

    #[test]
    fn sort_by_deserializes() {
        let v: SortBy = serde_json::from_value(json!("timestamp")).unwrap();
        assert_eq!(v, SortBy::Timestamp);
        let v: SortBy = serde_json::from_value(json!("relevance")).unwrap();
        assert_eq!(v, SortBy::Relevance);
    }

    #[test]
    fn sort_order_deserializes() {
        let v: SortOrder = serde_json::from_value(json!("asc")).unwrap();
        assert_eq!(v, SortOrder::Asc);
        let v: SortOrder = serde_json::from_value(json!("desc")).unwrap();
        assert_eq!(v, SortOrder::Desc);
    }

    mod proptests {
        use proptest::prelude::*;

        use super::*;

        proptest! {
            /// Every value in 1..=25 produces a valid SearchLimit whose get()
            /// round-trips.
            #[test]
            fn search_limit_valid_range(v in 1u64..=25) {
                let limit = SearchLimit::try_from(v).unwrap();
                prop_assert_eq!(u64::from(limit.get()), v);
            }

            /// Values outside 1..=25 are rejected.
            #[test]
            fn search_limit_invalid_rejects(v in prop_oneof![Just(0u64), 26u64..]) {
                prop_assert!(SearchLimit::try_from(v).is_err());
            }

            /// Every value in 0..=9975 produces a valid SearchOffset whose
            /// get() round-trips.
            #[test]
            fn search_offset_valid_range(v in 0u64..=9975) {
                let offset = SearchOffset::try_from(v).unwrap();
                prop_assert_eq!(u64::from(offset.get()), v);
            }

            /// Values above 9975 are rejected.
            #[test]
            fn search_offset_invalid_rejects(v in 9976u64..) {
                prop_assert!(SearchOffset::try_from(v).is_err());
            }
        }
    }
}
