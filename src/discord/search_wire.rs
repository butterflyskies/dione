//! MCP search argument parsing and response formatting.
use crate::discord::{
    ids::Snowflake,
    messaging::search::{
        SearchError, SearchErrorKind, SearchHas, SearchLimit, SearchOffset, SearchPage,
        SearchPreparationError, SearchRequest, SearchTimestamp, SortBy, SortOrder,
    },
    query_wire,
};
use serde::{Deserialize, de};
use serde_json::{Value, json};
use serenity::model::id::{ChannelId, GuildId, MessageId, UserId};
use std::fmt;
use thiserror::Error;

#[derive(Debug)]
struct InputTimestamp(SearchTimestamp);
impl<'de> Deserialize<'de> for InputTimestamp {
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
                Ok(SearchTimestamp::MessageId(MessageId::new(v)))
            }

            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
                if let Ok(id) = v.parse::<u64>() {
                    if id == 0 {
                        return Err(E::custom("snowflake ID must be nonzero"));
                    }
                    return Ok(SearchTimestamp::MessageId(MessageId::new(id)));
                }
                chrono::NaiveDate::parse_from_str(v, "%Y-%m-%d")
                    .map(SearchTimestamp::Date)
                    .map_err(|_| {
                        E::custom(format!("expected snowflake ID or YYYY-MM-DD, got '{v}'"))
                    })
            }
        }

        deserializer
            .deserialize_any(TimestampVisitor)
            .map(InputTimestamp)
    }
}

fn deserialize_id_vec<'de, D>(deserializer: D) -> Result<Vec<Snowflake>, D::Error>
where
    D: serde::Deserializer<'de>,
{
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

#[derive(Debug, Deserialize)]
struct SearchArguments {
    #[serde(default)]
    content: Option<String>,
    #[serde(
        default,
        rename = "author_id",
        deserialize_with = "deserialize_user_ids"
    )]
    author_ids: Vec<UserId>,
    #[serde(default, deserialize_with = "deserialize_user_ids")]
    mentions: Vec<UserId>,
    #[serde(default)]
    has: Vec<SearchHas>,
    /// Snowflake ID or ISO 8601 date (YYYY-MM-DD). Converted to `max_id`.
    #[serde(default)]
    before: Option<InputTimestamp>,
    /// Snowflake ID or ISO 8601 date (YYYY-MM-DD). Converted to `min_id`.
    #[serde(default)]
    after: Option<InputTimestamp>,
    #[serde(
        default,
        rename = "channel_id",
        deserialize_with = "deserialize_channel_ids"
    )]
    channel_ids: Vec<ChannelId>,
    #[serde(default)]
    pinned: Option<bool>,
    #[serde(default)]
    sort_by: Option<SortBy>,
    #[serde(default)]
    sort_order: Option<SortOrder>,
    #[serde(default)]
    limit: Option<SearchLimit>,
    #[serde(default)]
    offset: Option<SearchOffset>,
}

impl SearchArguments {
    fn into_request(self, guild: GuildId) -> Result<SearchRequest, SearchPreparationError> {
        let mut builder = SearchRequest::builder(guild)
            .author_ids(self.author_ids)
            .mentions(self.mentions)
            .has(self.has)
            .channel_ids(self.channel_ids);
        if let Some(value) = self.content {
            builder = builder.content(value);
        }
        if let Some(value) = self.before {
            builder = builder.before(value.0);
        }
        if let Some(value) = self.after {
            builder = builder.after(value.0);
        }
        if let Some(value) = self.pinned {
            builder = builder.pinned(value);
        }
        if let Some(value) = self.sort_by {
            builder = builder.sort_by(value);
        }
        if let Some(value) = self.sort_order {
            builder = builder.sort_order(value);
        }
        if let Some(value) = self.limit {
            builder = builder.limit(value);
        }
        if let Some(value) = self.offset {
            builder = builder.offset(value);
        }
        builder.build()
    }
}

/// Search input failure retaining its deserialization path or preparation cause.
#[derive(Debug, Error)]
#[error("{kind}")]
pub(crate) struct SearchInputError {
    #[source]
    pub(crate) kind: SearchInputErrorKind,
}
/// Distinguishes malformed arguments from a well-formed, rejected search request.
#[derive(Debug, Error)]
pub(crate) enum SearchInputErrorKind {
    #[error("invalid search params: {}", .0.inner())]
    Deserialize(#[source] serde_path_to_error::Error<serde_json::Error>),
    #[error("{0}")]
    Preparation(#[source] SearchPreparationError),
}

/// Parses owner arguments without losing the concrete JSON deserialization error.
pub(crate) fn parse_request(
    guild: GuildId,
    args: &Value,
) -> Result<SearchRequest, SearchInputError> {
    let arguments: SearchArguments =
        serde_path_to_error::deserialize(args).map_err(|source| SearchInputError {
            kind: SearchInputErrorKind::Deserialize(source),
        })?;
    arguments
        .into_request(guild)
        .map_err(|source| SearchInputError {
            kind: SearchInputErrorKind::Preparation(source),
        })
}

/// Projects a known preparation rejection as the existing tool error shape.
pub(crate) fn invalid_request(error: SearchPreparationError) -> Value {
    json!({"error": error.to_string()})
}

/// Projects a search page or its concrete execution failure.
pub(crate) fn result(result: Result<SearchPage, SearchError>) -> Value {
    match result {
        Ok(page) => {
            let messages: Vec<Value> = page
                .messages()
                .iter()
                .map(|hit| {
                    let mut message = query_wire::message(hit.message());
                    message["channel_id"] = json!(hit.channel().get().to_string());
                    message
                })
                .collect();
            json!({"messages":messages,"count":page.messages().len(),"offset":page.offset().get(),"has_more":page.has_more()})
        }
        Err(error) => match error.kind() {
            SearchErrorKind::Observation(source) => json!({"error": source.to_string()}),
            SearchErrorKind::Request(source) => {
                json!({"error":format!("HTTP request failed: {source}")})
            }
            SearchErrorKind::RateLimited { retry_after } => {
                let retry_after = retry_after.as_deref().unwrap_or("unknown");
                json!({"error":format!("rate limited by Discord, retry after {retry_after}s"),"retry_after":retry_after})
            }
            SearchErrorKind::Api { status, body } => {
                json!({"error":format!("Discord API error {status}: {body}")})
            }
            SearchErrorKind::Read(source) => {
                json!({"error":format!("failed to read response: {source}")})
            }
            SearchErrorKind::Parse(source) => {
                json!({"error":format!("failed to parse response: {source}")})
            }
        },
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn search_contract_parser_rejects_invalid_values_with_existing_text() {
        for (arguments, expected) in [
            (
                json!({"has":null}),
                "invalid type: null, expected a sequence",
            ),
            (json!({"before":0}), "snowflake ID must be nonzero"),
            (json!({"before":"0"}), "snowflake ID must be nonzero"),
            (
                json!({"before":"wrong"}),
                "expected snowflake ID or YYYY-MM-DD, got 'wrong'",
            ),
            (json!({"limit":0}), "search limit must be 1..=25, got 0"),
            (
                json!({"offset":9976}),
                "search offset must be 0..=9975, got 9976",
            ),
            (json!({"author_id":[true]}), "ID must be a string or number"),
        ] {
            assert_eq!(
                serde_json::from_value::<SearchArguments>(arguments)
                    .unwrap_err()
                    .to_string(),
                expected
            );
        }
        let params: SearchArguments =
            serde_json::from_value(json!({"before":1,"author_id":["2",3],"has":[]})).unwrap();
        assert_eq!(params.before.unwrap().0.to_query_param().unwrap(), "1");
        assert_eq!(params.author_ids, vec![UserId::new(2), UserId::new(3)]);
    }

    #[test]
    fn search_timestamp_deserializes_date_string() {
        let ts: InputTimestamp = serde_json::from_value(json!("2024-01-15")).unwrap();
        assert!(matches!(ts.0, SearchTimestamp::Date(_)));
    }

    #[test]
    fn search_timestamp_deserializes_snowflake_number() {
        let ts: InputTimestamp = serde_json::from_value(json!(123456789)).unwrap();
        assert!(matches!(ts.0, SearchTimestamp::MessageId(id) if id.get() == 123456789));
    }

    #[test]
    fn search_timestamp_deserializes_snowflake_string() {
        let ts: InputTimestamp = serde_json::from_value(json!("123456789")).unwrap();
        assert!(matches!(ts.0, SearchTimestamp::MessageId(id) if id.get() == 123456789));
    }

    #[test]
    fn search_timestamp_rejects_invalid() {
        assert!(serde_json::from_value::<InputTimestamp>(json!("not-a-date")).is_err());
        assert!(serde_json::from_value::<InputTimestamp>(json!(0)).is_err());
    }

    #[test]
    fn search_params_deserializes_minimal() {
        let args = json!({ "content": "hello" });
        let params: SearchArguments = serde_json::from_value(args).unwrap();
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
        let params: SearchArguments = serde_json::from_value(args).unwrap();
        assert_eq!(params.content.as_deref(), Some("hello"));
        assert_eq!(params.author_ids, vec![UserId::new(123)]);
        assert_eq!(params.mentions, vec![UserId::new(456)]);
        assert_eq!(params.has, vec![SearchHas::Image, SearchHas::File]);
        assert!(matches!(
            params.before,
            Some(InputTimestamp(SearchTimestamp::Date(_)))
        ));
        assert!(matches!(
            params.after,
            Some(InputTimestamp(SearchTimestamp::Date(_)))
        ));
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
        let params: SearchArguments = serde_json::from_value(args).unwrap();
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
        let params: SearchArguments = serde_json::from_value(args).unwrap();
        assert_eq!(params.author_ids, vec![UserId::new(123)]);
        assert_eq!(params.mentions, vec![UserId::new(456), UserId::new(789)]);
    }

    #[test]
    fn search_params_rejects_zero_id() {
        let args = json!({ "author_id": "0" });
        assert!(serde_json::from_value::<SearchArguments>(args).is_err());

        let args = json!({ "channel_id": 0 });
        assert!(serde_json::from_value::<SearchArguments>(args).is_err());

        let args = json!({ "mentions": ["123", "0"] });
        assert!(serde_json::from_value::<SearchArguments>(args).is_err());
    }

    #[test]
    fn search_params_rejects_non_numeric_id() {
        let args = json!({ "author_id": "not-a-number" });
        assert!(serde_json::from_value::<SearchArguments>(args).is_err());

        let args = json!({ "channel_id": ["123", "abc"] });
        assert!(serde_json::from_value::<SearchArguments>(args).is_err());
    }

    #[test]
    fn search_params_ignores_unknown_fields() {
        let args = json!({
            "content": "hello",
            "guild_id": "999",
            "unknown_field": true
        });
        let params: SearchArguments = serde_json::from_value(args).unwrap();
        assert_eq!(params.content.as_deref(), Some("hello"));
    }

    #[test]
    fn search_params_has_any_filter_empty() {
        let empty: SearchArguments = serde_json::from_value(json!({})).unwrap();
        assert!(
            empty.into_request(GuildId::new(7)).is_err(),
            "params with no filters must return false"
        );
    }

    #[test]
    fn search_params_has_any_filter_content() {
        let p: SearchArguments = serde_json::from_value(json!({"content": "x"})).unwrap();
        assert!(p.into_request(GuildId::new(7)).is_ok());
    }

    #[test]
    fn search_params_has_any_filter_has() {
        let p: SearchArguments = serde_json::from_value(json!({"has": ["image"]})).unwrap();
        assert!(p.into_request(GuildId::new(7)).is_ok());
    }

    #[test]
    fn search_params_has_any_filter_before() {
        let p: SearchArguments = serde_json::from_value(json!({"before": "2024-01-01"})).unwrap();
        assert!(p.into_request(GuildId::new(7)).is_ok());
    }

    #[test]
    fn search_params_has_any_filter_channel() {
        let p: SearchArguments = serde_json::from_value(json!({"channel_id": "123"})).unwrap();
        assert!(p.into_request(GuildId::new(7)).is_ok());
    }

    #[test]
    fn search_params_has_any_filter_author_id() {
        let p: SearchArguments = serde_json::from_value(json!({"author_id": "123"})).unwrap();
        assert!(p.into_request(GuildId::new(7)).is_ok());
    }

    #[test]
    fn search_params_has_any_filter_mentions() {
        let p: SearchArguments = serde_json::from_value(json!({"mentions": "456"})).unwrap();
        assert!(p.into_request(GuildId::new(7)).is_ok());
    }

    #[test]
    fn search_params_has_any_filter_after() {
        let p: SearchArguments = serde_json::from_value(json!({"after": "2024-06-01"})).unwrap();
        assert!(p.into_request(GuildId::new(7)).is_ok());
    }

    #[test]
    fn search_params_has_any_filter_pinned() {
        let p: SearchArguments = serde_json::from_value(json!({"pinned": true})).unwrap();
        assert!(p.into_request(GuildId::new(7)).is_ok());
    }

    #[test]
    fn search_params_has_any_filter_sort_by_alone_is_not_filter() {
        let p: SearchArguments = serde_json::from_value(json!({"sort_by": "timestamp"})).unwrap();
        assert!(
            p.into_request(GuildId::new(7)).is_err(),
            "sort_by alone is not a search filter"
        );
    }

    #[test]
    fn search_params_has_any_filter_sort_order_alone_is_not_filter() {
        let p: SearchArguments = serde_json::from_value(json!({"sort_order": "desc"})).unwrap();
        assert!(
            p.into_request(GuildId::new(7)).is_err(),
            "sort_order alone is not a search filter"
        );
    }
}
