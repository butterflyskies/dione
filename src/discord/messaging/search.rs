//! Guild search requests, Discord execution, and authorized message observations.
use crate::{
    config::LoadedConfig,
    discord::{
        ids::Snowflake,
        messaging::{
            MessagingCtx,
            queries::{MessageView, ObservationError},
        },
    },
    gate::OutboundGate,
};
use serde::Deserialize;
use serde_json::Value;
use serenity::model::{
    channel::Message,
    id::{ChannelId, GuildId, MessageId, UserId},
};
use std::{
    collections::{BTreeMap, HashSet},
    fmt,
};
use thiserror::Error;

/// A rejected search input, before HTTP execution.
#[derive(Debug, Error)]
#[error("{kind}")]
pub(crate) struct SearchPreparationError {
    #[source]
    kind: SearchPreparationErrorKind,
}

/// Exhaustive reasons a search request cannot be prepared.
#[derive(Debug, Error)]
pub(crate) enum SearchPreparationErrorKind {
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

impl From<SearchPreparationErrorKind> for SearchPreparationError {
    fn from(kind: SearchPreparationErrorKind) -> Self {
        Self { kind }
    }
}

/// Search result limit, validated to Discord's 1..=25 range. Default: 25.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(try_from = "u64")]
pub(crate) struct SearchLimit(u8);

impl SearchLimit {
    pub(crate) fn get(self) -> u8 {
        self.0
    }
}

impl Default for SearchLimit {
    fn default() -> Self {
        Self(25)
    }
}

impl TryFrom<u64> for SearchLimit {
    type Error = SearchPreparationError;

    fn try_from(v: u64) -> Result<Self, Self::Error> {
        if !(1..=25).contains(&v) {
            return Err(SearchPreparationErrorKind::InvalidLimit(v).into());
        }
        u8::try_from(v)
            .map(Self)
            .map_err(|_| SearchPreparationErrorKind::InvalidLimit(v).into())
    }
}

/// Search result offset, validated to 0..=9975. Default: 0.
#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(try_from = "u64")]
pub(crate) struct SearchOffset(u16);

impl SearchOffset {
    pub(crate) fn get(self) -> u16 {
        self.0
    }
}

impl TryFrom<u64> for SearchOffset {
    type Error = SearchPreparationError;

    fn try_from(v: u64) -> Result<Self, Self::Error> {
        if v > 9975 {
            return Err(SearchPreparationErrorKind::InvalidOffset(v).into());
        }
        u16::try_from(v)
            .map(Self)
            .map_err(|_| SearchPreparationErrorKind::InvalidOffset(v).into())
    }
}

/// A message identity or calendar date used as a search bound.
#[derive(Debug, Clone)]
pub(crate) enum SearchTimestamp {
    MessageId(MessageId),
    Date(chrono::NaiveDate),
}

impl SearchTimestamp {
    /// Date bounds preserve epoch zero and the existing wrapping snowflake conversion.
    pub(crate) fn to_query_param(&self) -> Result<String, SearchPreparationError> {
        match self {
            Self::MessageId(id) => Ok(id.to_string()),
            Self::Date(nd) => {
                let dt = nd
                    .and_hms_opt(0, 0, 0)
                    .ok_or_else(|| SearchPreparationErrorKind::InvalidDate(nd.to_string()))?;
                let ms = dt.and_utc().timestamp_millis();
                let discord_ms = ms - DISCORD_EPOCH_MS;
                let discord_ms = u64::try_from(discord_ms).map_err(|_| {
                    SearchPreparationErrorKind::InvalidDate(format!(
                        "{nd}: date is before Discord epoch (2015-01-01)"
                    ))
                })?;
                Ok(discord_ms.wrapping_shl(22).to_string())
            }
        }
    }
}

/// Attachment type filter for search.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SearchHas {
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
pub(crate) enum SortBy {
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
pub(crate) enum SortOrder {
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

const DISCORD_EPOCH_MS: i64 = 1_420_070_400_000;

/// Typed search choices awaiting filter and calendar-bound validation.
#[derive(Debug)]
pub(crate) struct SearchRequestBuilder {
    guild: GuildId,
    content: Option<String>,
    author_ids: Vec<UserId>,
    mentions: Vec<UserId>,
    has: Vec<SearchHas>,
    before: Option<SearchTimestamp>,
    after: Option<SearchTimestamp>,
    channel_ids: Vec<ChannelId>,
    pinned: Option<bool>,
    sort_by: Option<SortBy>,
    sort_order: Option<SortOrder>,
    limit: Option<SearchLimit>,
    offset: Option<SearchOffset>,
}

impl SearchRequestBuilder {
    pub(crate) fn content(mut self, value: String) -> Self {
        self.content = Some(value);
        self
    }
    pub(crate) fn author_ids(mut self, value: Vec<UserId>) -> Self {
        self.author_ids = value;
        self
    }
    pub(crate) fn mentions(mut self, value: Vec<UserId>) -> Self {
        self.mentions = value;
        self
    }
    pub(crate) fn has(mut self, value: Vec<SearchHas>) -> Self {
        self.has = value;
        self
    }
    pub(crate) fn before(mut self, value: SearchTimestamp) -> Self {
        self.before = Some(value);
        self
    }
    pub(crate) fn after(mut self, value: SearchTimestamp) -> Self {
        self.after = Some(value);
        self
    }
    pub(crate) fn channel_ids(mut self, value: Vec<ChannelId>) -> Self {
        self.channel_ids = value;
        self
    }
    pub(crate) fn pinned(mut self, value: bool) -> Self {
        self.pinned = Some(value);
        self
    }
    pub(crate) fn sort_by(mut self, value: SortBy) -> Self {
        self.sort_by = Some(value);
        self
    }
    pub(crate) fn sort_order(mut self, value: SortOrder) -> Self {
        self.sort_order = Some(value);
        self
    }
    pub(crate) fn limit(mut self, value: SearchLimit) -> Self {
        self.limit = Some(value);
        self
    }
    pub(crate) fn offset(mut self, value: SearchOffset) -> Self {
        self.offset = Some(value);
        self
    }
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

    pub(crate) fn build(self) -> Result<SearchRequest, SearchPreparationError> {
        if !self.has_any_filter() {
            return Err(SearchPreparationErrorKind::NoFilters.into());
        }
        let mut query: Vec<(&'static str, String)> = Vec::new();

        if let Some(content) = self.content {
            query.push(("content", content));
        }
        for id in &self.author_ids {
            query.push(("author_id", id.get().to_string()));
        }
        for id in &self.mentions {
            query.push(("mentions", id.get().to_string()));
        }
        for h in &self.has {
            query.push(("has", h.to_string()));
        }
        if let Some(ref before) = self.before {
            query.push(("max_id", before.to_query_param()?));
        }
        if let Some(ref after) = self.after {
            query.push(("min_id", after.to_query_param()?));
        }
        for id in &self.channel_ids {
            query.push(("channel_id", id.get().to_string()));
        }
        if let Some(pinned) = self.pinned {
            query.push(("pinned", pinned.to_string()));
        }
        if let Some(sort_by) = self.sort_by {
            query.push(("sort_by", sort_by.to_string()));
        }
        if let Some(sort_order) = self.sort_order {
            query.push(("sort_order", sort_order.to_string()));
        }

        let limit = self.limit.unwrap_or_default();
        query.push(("limit", limit.get().to_string()));

        let offset = self.offset.unwrap_or_default();
        if offset.get() > 0 {
            query.push(("offset", offset.get().to_string()));
        }

        Ok(SearchRequest {
            guild: self.guild,
            query,
            limit,
            offset,
        })
    }
}

/// An executable guild search with a filter and prepared bounds.
#[derive(Debug)]
pub(crate) struct SearchRequest {
    guild: GuildId,
    query: Vec<(&'static str, String)>,
    limit: SearchLimit,
    offset: SearchOffset,
}

impl SearchRequest {
    pub(crate) fn builder(guild: GuildId) -> SearchRequestBuilder {
        SearchRequestBuilder {
            guild,
            content: Default::default(),
            author_ids: Default::default(),
            mentions: Default::default(),
            has: Default::default(),
            before: Default::default(),
            after: Default::default(),
            channel_ids: Default::default(),
            pinned: Default::default(),
            sort_by: Default::default(),
            sort_order: Default::default(),
            limit: Default::default(),
            offset: Default::default(),
        }
    }
}

/// An authorized message observation and its originating channel.
#[derive(Debug)]
pub(crate) struct SearchHit {
    channel: ChannelId,
    message: MessageView,
}

impl SearchHit {
    fn from_discord(config: &LoadedConfig, message: &Message) -> Result<Self, ObservationError> {
        Ok(Self {
            channel: message.channel_id,
            message: MessageView::from_discord(
                config,
                message,
                crate::core::context::ObservationOrigin::Search,
            )?,
        })
    }
    pub(crate) fn channel(&self) -> ChannelId {
        self.channel
    }
    pub(crate) fn message(&self) -> &MessageView {
        &self.message
    }
}

/// Ordered visible hits and continuation derived from the unfiltered Discord page.
#[derive(Debug)]
pub(crate) struct SearchPage {
    messages: Vec<SearchHit>,
    offset: SearchOffset,
    has_more: bool,
}

impl SearchPage {
    fn new(
        messages: Vec<SearchHit>,
        offset: SearchOffset,
        raw_count: usize,
        limit: SearchLimit,
    ) -> Self {
        Self {
            messages,
            offset,
            has_more: raw_count >= usize::from(limit.get()),
        }
    }
    pub(crate) fn messages(&self) -> &[SearchHit] {
        &self.messages
    }
    pub(crate) fn offset(&self) -> SearchOffset {
        self.offset
    }
    pub(crate) fn has_more(&self) -> bool {
        self.has_more
    }
}

/// An execution failure retaining its guild and underlying cause.
#[derive(Debug, Error)]
#[error("search in guild {guild} failed: {kind}")]
pub(crate) struct SearchError {
    guild: GuildId,
    #[source]
    kind: SearchErrorKind,
}

impl SearchError {
    pub(crate) fn kind(&self) -> &SearchErrorKind {
        &self.kind
    }
}

/// Transport, status, and decoding failures from an attempted search.
#[derive(Debug, Error)]
pub(crate) enum SearchErrorKind {
    #[error(transparent)]
    Observation(#[from] ObservationError),
    #[error("http request failed: {0}")]
    Request(#[source] reqwest::Error),
    #[error("rate limited by Discord")]
    RateLimited { retry_after: Option<String> },
    #[error("discord API error {status}: {body}")]
    Api {
        status: reqwest::StatusCode,
        body: String,
    },
    #[error("failed to read response: {0}")]
    Read(#[source] reqwest::Error),
    #[error("failed to parse response: {0}")]
    Parse(#[source] serde_json::Error),
}

/// Discord search endpoint; production always uses the fixed Discord API base.
pub(crate) struct SearchTransport {
    api_base: String,
}
impl Default for SearchTransport {
    fn default() -> Self {
        Self {
            api_base: "https://discord.com/api/v10".to_owned(),
        }
    }
}
impl SearchTransport {
    #[cfg(test)]
    /// Uses a local HTTP fixture endpoint.
    pub(crate) fn local(api_base: String) -> Self {
        Self { api_base }
    }

    /// Executes a prepared search and filters observations through current policy.
    pub(crate) async fn search_messages(
        &self,
        ctx: &MessagingCtx,
        request: SearchRequest,
    ) -> Result<SearchPage, SearchError> {
        let url = format!(
            "{}/guilds/{}/messages/search",
            self.api_base,
            request.guild.get()
        );
        let http = reqwest::Client::builder()
            .use_rustls_tls()
            .build()
            .map_err(|source| SearchError {
                guild: request.guild,
                kind: SearchErrorKind::Request(source),
            })?
            .get(&url)
            .header("Authorization", ctx.http.token());
        execute_search(ctx, request, http).await
    }
}

async fn execute_search(
    ctx: &MessagingCtx,
    request: SearchRequest,
    http: reqwest::RequestBuilder,
) -> Result<SearchPage, SearchError> {
    let error = |kind| SearchError {
        guild: request.guild,
        kind,
    };
    let response = http
        .query(&request.query)
        .send()
        .await
        .map_err(|source| error(SearchErrorKind::Request(source)))?;
    if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        return Err(error(SearchErrorKind::RateLimited { retry_after }));
    }
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(error(SearchErrorKind::Api { status, body }));
    }
    let text = response
        .text()
        .await
        .map_err(|source| error(SearchErrorKind::Read(source)))?;
    let body: Value =
        serde_json::from_str(&text).map_err(|source| error(SearchErrorKind::Parse(source)))?;
    let Some(raw_messages) = body["messages"].as_array() else {
        return Ok(SearchPage::new(
            Vec::new(),
            request.offset,
            0,
            request.limit,
        ));
    };
    let state = ctx.state.read().await;
    let messages = filter_results(
        &ctx.config,
        &state.dm_channel_ids,
        &state.thread_parents,
        raw_messages,
    )
    .map_err(|source| error(SearchErrorKind::Observation(source)))?;
    drop(state);
    Ok(SearchPage::new(
        messages,
        request.offset,
        raw_messages.len(),
        request.limit,
    ))
}
fn filter_results(
    config: &LoadedConfig,
    dm_channel_ids: &HashSet<u64>,
    thread_parents: &BTreeMap<u64, Option<u64>>,
    raw_messages: &[Value],
) -> Result<Vec<SearchHit>, ObservationError> {
    let mut messages: Vec<SearchHit> = Vec::new();

    for group in raw_messages {
        let inner = match group.as_array() {
            Some(arr) if !arr.is_empty() => &arr[0],
            _ => continue,
        };

        let Some(channel) = inner["channel_id"]
            .as_str()
            .and_then(|s| s.parse::<u64>().ok())
            .and_then(Snowflake::new)
            .map(Snowflake::channel)
        else {
            continue;
        };

        if !OutboundGate::check_channel_with_threads(
            config,
            channel.get(),
            dm_channel_ids,
            thread_parents,
        ) {
            continue;
        }

        match serde_json::from_value::<Message>(inner.clone()) {
            Ok(msg) => {
                messages.push(SearchHit::from_discord(config, &msg)?);
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    channel_id = channel.get(),
                    "failed to deserialize search result message, skipping"
                );
            }
        }
    }

    Ok(messages)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::{ChannelConfig, Config},
        discord::search_wire,
        ingress_ledger::IngressLedger,
        no_rly::consent::ConsentGate,
        state::new_state,
    };
    use serde_json::json;
    use std::{sync::Arc, time::Duration};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        sync::oneshot,
        task::JoinHandle,
    };

    struct SearchHttpFixture {
        request: reqwest::RequestBuilder,
        received: oneshot::Receiver<String>,
        respond: oneshot::Sender<Vec<u8>>,
        server: JoinHandle<()>,
    }

    impl SearchHttpFixture {
        async fn new() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let (captured, received) = oneshot::channel();
            let (respond, response) = oneshot::channel::<Vec<u8>>();
            let server = tokio::spawn(async move {
                tokio::time::timeout(Duration::from_secs(10), async move {
                    let (mut stream, _) = listener.accept().await.unwrap();
                    let mut request = Vec::new();
                    let mut buffer = [0; 1024];
                    while !request.windows(4).any(|part| part == b"\r\n\r\n") {
                        let read = stream.read(&mut buffer).await.unwrap();
                        assert_ne!(read, 0, "request ended before its headers");
                        request.extend_from_slice(&buffer[..read]);
                    }
                    captured.send(String::from_utf8(request).unwrap()).unwrap();
                    stream.write_all(&response.await.unwrap()).await.unwrap();
                    stream.shutdown().await.unwrap();
                })
                .await
                .expect("local search exchange timed out");
            });
            Self {
                request: reqwest::Client::new()
                    .get(format!("http://{address}/guilds/7/messages/search"))
                    .header("Authorization", "Bot fake"),
                received,
                respond,
                server,
            }
        }
    }

    fn search_context() -> MessagingCtx {
        let mut config = Config::default();
        config.channels.push(ChannelConfig {
            id: "42".into(),
            ..Default::default()
        });
        MessagingCtx::new(
            Arc::new(serenity::http::Http::new("fake")),
            new_state(),
            Arc::new(LoadedConfig::from_raw(config)),
            "/tmp".into(),
            Arc::new(ConsentGate::new(camino::Utf8Path::new("/tmp"))),
            Arc::new(IngressLedger::new()),
        )
    }

    fn http_response(status: &str, headers: &[u8], body: &str) -> Vec<u8> {
        let mut response = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n",
            body.len()
        )
        .into_bytes();
        response.extend_from_slice(headers);
        response.extend_from_slice(b"\r\n");
        response.extend_from_slice(body.as_bytes());
        response
    }

    async fn search_exchange(arguments: Value, response: Vec<u8>) -> (Value, String) {
        let fixture = SearchHttpFixture::new().await;
        fixture.respond.send(response).unwrap();
        let result = execute_search(
            &search_context(),
            search_wire::parse_request(GuildId::new(7), &arguments).unwrap(),
            fixture.request,
        )
        .await;
        let received = fixture.received.await.unwrap();
        fixture.server.await.unwrap();
        (search_wire::result(result), received)
    }

    fn filtered_wire(
        config: &LoadedConfig,
        dm: &HashSet<u64>,
        threads: &BTreeMap<u64, Option<u64>>,
        raw: &[Value],
    ) -> Vec<Value> {
        let page = SearchPage::new(
            filter_results(config, dm, threads, raw).unwrap(),
            SearchOffset::default(),
            raw.len(),
            SearchLimit::default(),
        );
        search_wire::result(Ok(page))["messages"]
            .as_array()
            .unwrap()
            .clone()
    }
    #[test]
    fn search_observation_conversion_error_is_visible_instead_of_a_hit() {
        let mut message: Message = serde_json::from_value(wire_search_message("7", "42")).unwrap();
        let config = LoadedConfig::from_raw(Config::default());
        assert_eq!(
            SearchHit::from_discord(&config, &message)
                .unwrap()
                .message()
                .id(),
            MessageId::new(7)
        );
        message.timestamp =
            serenity::model::Timestamp::from_unix_timestamp(-62_167_219_201).unwrap();
        let source = SearchHit::from_discord(&config, &message).unwrap_err();
        let error = SearchError {
            guild: GuildId::new(7),
            kind: SearchErrorKind::Observation(source),
        };
        assert_eq!(
            search_wire::result(Err(error)),
            json!({"error":"invalid Discord observation for channel 42, message 7: timestamp cannot be formatted as RFC 3339"})
        );
    }

    #[tokio::test]
    async fn search_contract_raw_pagination_order_duplicates_and_first_member() {
        let mut numeric_channel = wire_search_message("40", "42");
        numeric_channel["channel_id"] = json!(42);
        let response = json!({"total_results": 900, "messages": [
            [wire_search_message("30", "42"), wire_search_message("31", "42")],
            [wire_search_message("20", "99")],
            [],
            {"channel_id":"42"},
            [{"channel_id":"42", "id":"invalid"}],
            [numeric_channel],
            [wire_search_message("10", "42")],
            [wire_search_message("30", "42")]
        ]});
        let (actual, request) = search_exchange(
            json!({"content":"x", "limit":8, "offset":16}),
            http_response("200 OK", b"", &response.to_string()),
        )
        .await;
        assert_eq!(
            actual,
            json!({"messages":[
            {"id":"30","author":"test","author_id":"1","content":"test message",
             "timestamp":"2024-01-01T00:00:00+00:00","attachments":[],"channel_id":"42"},
            {"id":"10","author":"test","author_id":"1","content":"test message",
             "timestamp":"2024-01-01T00:00:00+00:00","attachments":[],"channel_id":"42"},
            {"id":"30","author":"test","author_id":"1","content":"test message",
             "timestamp":"2024-01-01T00:00:00+00:00","attachments":[],"channel_id":"42"}
        ],"count":3,"offset":16,"has_more":true})
        );
        assert_eq!(
            request.lines().next(),
            Some("GET /guilds/7/messages/search?content=x&limit=8&offset=16 HTTP/1.1")
        );
    }

    #[tokio::test]
    async fn search_contract_missing_and_nonarray_messages_are_empty_pages() {
        for body in ["{}", r#"{"messages":null}"#, r#"{"messages":{}}"#] {
            let (actual, _) = search_exchange(
                json!({"content":"x", "offset":9}),
                http_response("200 OK", b"", body),
            )
            .await;
            assert_eq!(
                actual,
                json!({"messages":[],"count":0,"offset":9,"has_more":false})
            );
        }
        let (actual, _) = search_exchange(
            json!({"pinned":false}),
            http_response(
                "200 OK",
                b"",
                &json!({"messages":[[wire_search_message("1", "42")]]}).to_string(),
            ),
        )
        .await;
        assert_eq!(
            actual,
            json!({"messages":[
            {"id":"1","author":"test","author_id":"1","content":"test message",
             "timestamp":"2024-01-01T00:00:00+00:00","attachments":[],"channel_id":"42"}
        ],"count":1,"offset":0,"has_more":false})
        );
    }

    #[tokio::test]
    async fn search_contract_reads_live_dm_and_thread_permissions_after_response() {
        let ctx = search_context();
        {
            let mut state = ctx.state.write().await;
            state.dm_channel_ids.insert(66);
            state.thread_parents.insert(88, Some(42));
        }
        let fixture = SearchHttpFixture::new().await;
        let search = execute_search(
            &ctx,
            search_wire::parse_request(GuildId::new(7), &json!({"content":"x"})).unwrap(),
            fixture.request,
        );
        let change_permissions = async {
            let request = fixture.received.await.unwrap();
            assert_eq!(
                request.lines().next(),
                Some("GET /guilds/7/messages/search?content=x&limit=25 HTTP/1.1")
            );
            {
                let mut state = ctx.state.write().await;
                state.dm_channel_ids.remove(&66);
                state.thread_parents.remove(&88);
                state.dm_channel_ids.insert(55);
                state.thread_parents.insert(77, Some(42));
            }
            fixture
                .respond
                .send(http_response(
                    "200 OK",
                    b"",
                    &json!({"messages":[
                        [wire_search_message("1", "55")], [wire_search_message("2", "77")],
                        [wire_search_message("3", "66")], [wire_search_message("4", "88")]
                    ]})
                    .to_string(),
                ))
                .unwrap();
        };
        let (actual, ()) = tokio::join!(search, change_permissions);
        let actual = search_wire::result(actual);
        fixture.server.await.unwrap();
        assert_eq!(
            actual,
            json!({"messages":[
            {"id":"1","author":"test","author_id":"1","content":"test message",
             "timestamp":"2024-01-01T00:00:00+00:00","attachments":[],"channel_id":"55"},
            {"id":"2","author":"test","author_id":"1","content":"test message",
             "timestamp":"2024-01-01T00:00:00+00:00","attachments":[],"channel_id":"77"}
        ],"count":2,"offset":0,"has_more":false})
        );
    }

    #[tokio::test]
    async fn search_contract_rate_limit_preserves_raw_header_or_unknown() {
        for (header, expected) in [
            (
                b"Retry-After: next Tuesday\r\n".as_slice(),
                json!({"error":"rate limited by Discord, retry after next Tuesdays","retry_after":"next Tuesday"}),
            ),
            (
                b"".as_slice(),
                json!({"error":"rate limited by Discord, retry after unknowns","retry_after":"unknown"}),
            ),
            (
                b"Retry-After: \xff\r\n".as_slice(),
                json!({"error":"rate limited by Discord, retry after unknowns","retry_after":"unknown"}),
            ),
        ] {
            let (actual, _) = search_exchange(
                json!({"content":"x"}),
                http_response("429 Too Many Requests", header, "not JSON"),
            )
            .await;
            assert_eq!(actual, expected);
        }
    }

    #[tokio::test]
    async fn search_contract_status_body_and_json_failures() {
        for (response, expected) in [
            (http_response("403 Forbidden", b"", "raw denied body"), json!({"error":"Discord API error 403 Forbidden: raw denied body"})),
            (b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 100\r\nConnection: close\r\n\r\nshort".to_vec(), json!({"error":"Discord API error 500 Internal Server Error: "})),
            (http_response("200 OK", b"", "{"), json!({"error":"failed to parse response: EOF while parsing an object at line 1 column 1"})),
        ] {
            let (actual, _) = search_exchange(json!({"content":"x"}), response).await;
            assert_eq!(actual, expected);
        }
    }

    #[tokio::test]
    async fn search_contract_body_read_and_transport_error_prefixes() {
        let (actual, _) = search_exchange(
            json!({"content":"x"}),
            b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\nConnection: close\r\n\r\nshort".to_vec(),
        )
        .await;
        assert_eq!(actual.as_object().unwrap().len(), 1);
        assert!(
            actual["error"]
                .as_str()
                .unwrap()
                .starts_with("failed to read response: "),
            "{actual}"
        );

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let actual = execute_search(
            &search_context(),
            search_wire::parse_request(GuildId::new(7), &json!({"content":"x"})).unwrap(),
            reqwest::Client::new().get(format!("http://{address}/search")),
        )
        .await;
        let actual = search_wire::result(actual);
        assert_eq!(actual.as_object().unwrap().len(), 1);
        assert!(
            actual["error"]
                .as_str()
                .unwrap()
                .starts_with("HTTP request failed: "),
            "{actual}"
        );
    }

    #[tokio::test]
    async fn search_contract_query_preserves_order_repeats_and_empty_filters() {
        let (actual, request) = search_exchange(
            json!({
                "content":"", "author_id":["3",2,"3"], "mentions":["5","5"],
                "has":["image","link","image"], "before":"2015-01-01", "after":"1",
                "channel_id":["42","42"], "pinned":false, "sort_by":"timestamp",
                "sort_order":"asc", "limit":2, "offset":4, "ignored":true
            }),
            http_response("200 OK", b"", "{}"),
        )
        .await;
        assert_eq!(
            request.lines().next(),
            Some(
                "GET /guilds/7/messages/search?content=&author_id=3&author_id=2&author_id=3&mentions=5&mentions=5&has=image&has=link&has=image&max_id=0&min_id=1&channel_id=42&channel_id=42&pinned=false&sort_by=timestamp&sort_order=asc&limit=2&offset=4 HTTP/1.1"
            )
        );
        assert!(
            request
                .lines()
                .any(|line| line.eq_ignore_ascii_case("authorization: Bot fake"))
        );
        assert_eq!(
            actual,
            json!({"messages":[],"count":0,"offset":4,"has_more":false})
        );
        let (actual, request) = search_exchange(json!({"content":"", "author_id":null, "mentions":null, "channel_id":null, "limit":null,"offset":null}), http_response("200 OK", b"", "{}")).await;
        assert_eq!(
            request.lines().next(),
            Some("GET /guilds/7/messages/search?content=&limit=25 HTTP/1.1")
        );
        assert_eq!(
            actual,
            json!({"messages":[],"count":0,"offset":0,"has_more":false})
        );
    }

    #[tokio::test]
    async fn search_contract_future_date_keeps_wrapping_query_bound() {
        let (actual, request) = search_exchange(
            json!({"before":"2500-01-01", "after":"2015-01-01", "offset":0}),
            http_response("200 OK", b"", "{}"),
        )
        .await;
        assert_eq!(
            request.lines().next(),
            Some(
                "GET /guilds/7/messages/search?max_id=8854241454852145152&min_id=0&limit=25 HTTP/1.1"
            )
        );
        assert_eq!(
            actual,
            json!({"messages":[],"count":0,"offset":0,"has_more":false})
        );
    }

    #[test]
    fn search_limit_default_is_25() {
        assert_eq!(SearchLimit::default().get(), 25);
    }

    #[test]
    fn search_limit_accepts_valid_range() {
        for v in 1..=25 {
            let limit = SearchLimit::try_from(v).unwrap();
            assert_eq!(limit.get(), u8::try_from(v).unwrap());
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

    #[test]
    fn search_timestamp_date_to_snowflake() {
        let ts = SearchTimestamp::Date(chrono::NaiveDate::from_ymd_opt(2024, 1, 15).unwrap());
        let param = ts.to_query_param().unwrap();
        assert_eq!(param, "1196242344345600000");
    }

    #[test]
    fn search_timestamp_discord_epoch() {
        let ts = SearchTimestamp::Date(chrono::NaiveDate::from_ymd_opt(2015, 1, 1).unwrap());
        let param = ts.to_query_param().unwrap();
        assert_eq!(param, "0");
    }

    #[test]
    fn search_timestamp_before_epoch_is_error() {
        let ts = SearchTimestamp::Date(chrono::NaiveDate::from_ymd_opt(2014, 12, 31).unwrap());
        assert!(ts.to_query_param().is_err());
    }

    #[test]
    fn search_timestamp_message_id_passthrough() {
        let ts = SearchTimestamp::MessageId(MessageId::new(123456789));
        assert_eq!(ts.to_query_param().unwrap(), "123456789");
    }

    #[test]
    fn filter_results_omits_unconfigured_channels() {
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
        let raw_messages = vec![json!([configured_msg]), json!([unconfigured_msg])];

        let dm_ids = HashSet::new();
        let thread_parents = BTreeMap::new();

        let results = filtered_wire(&config, &dm_ids, &thread_parents, &raw_messages);

        assert_eq!(
            results.len(),
            1,
            "only the configured channel's message should survive"
        );
        assert_eq!(results[0]["channel_id"], "42");
    }

    #[test]
    fn filter_results_passes_thread_of_configured_channel() {
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

        let results = filtered_wire(&config, &dm_ids, &thread_parents, &raw_messages);

        assert_eq!(results.len(), 1, "thread of configured channel should pass");
    }

    #[test]
    fn filter_results_passes_dm_channel() {
        let config = crate::config::LoadedConfig::from_raw(Config::default());

        let dm_msg = wire_search_message("101", "55");

        let raw_messages = vec![json!([dm_msg])];
        let mut dm_ids = HashSet::new();
        dm_ids.insert(55u64);
        let thread_parents = BTreeMap::new();

        let results = filtered_wire(&config, &dm_ids, &thread_parents, &raw_messages);

        assert_eq!(results.len(), 1, "known DM channel should pass");
    }

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

    #[test]
    fn query_contract_filter_results_preserves_complete_payloads_and_transport_order() {
        let config = crate::config::LoadedConfig::from_raw(crate::config::Config::default());
        let raw_messages = vec![
            json!([
                wire_search_message("30", "42"),
                wire_search_message("31", "42")
            ]),
            json!([wire_search_message("20", "99")]),
            json!([]),
            json!({"channel_id":"42"}),
            json!([{"channel_id":"42","id":"invalid"}]),
            json!([wire_search_message("10", "42")]),
        ];
        let results = filtered_wire(
            &config,
            &HashSet::from([42]),
            &BTreeMap::new(),
            &raw_messages,
        );
        assert_eq!(
            results,
            vec![
                json!({"id":"30","author":"test","author_id":"1","content":"test message",
                   "timestamp":"2024-01-01T00:00:00+00:00","attachments":[],"channel_id":"42"}),
                json!({"id":"10","author":"test","author_id":"1","content":"test message",
                   "timestamp":"2024-01-01T00:00:00+00:00","attachments":[],"channel_id":"42"})
            ]
        );
    }

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

    #[test]
    fn filter_results_produces_distinct_messages() {
        let mut raw = Config::default();
        raw.channels.push(ChannelConfig {
            id: "42".into(),
            ..Default::default()
        });
        let config = crate::config::LoadedConfig::from_raw(raw);

        fn make_msg(id: &str) -> Value {
            json!({
                "id": id,
                "type": 0,
                "channel_id": "42",
                "author": {
                    "id": "1", "username": "test", "global_name": null,
                    "avatar": null, "discriminator": "0", "public_flags": 0
                },
                "content": format!("message {id}"),
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
        let page1_raw = vec![json!([make_msg("100")]), json!([make_msg("101")])];
        let page2_raw = vec![json!([make_msg("200")]), json!([make_msg("201")])];

        let dm_ids = HashSet::new();
        let thread_parents = BTreeMap::new();

        let page1 = filtered_wire(&config, &dm_ids, &thread_parents, &page1_raw);
        let page2 = filtered_wire(&config, &dm_ids, &thread_parents, &page2_raw);

        assert_eq!(page1.len(), 2);
        assert_eq!(page2.len(), 2);

        let page1_ids: Vec<&str> = page1.iter().map(|m| m["id"].as_str().unwrap()).collect();
        let page2_ids: Vec<&str> = page2.iter().map(|m| m["id"].as_str().unwrap()).collect();
        for id in &page2_ids {
            assert!(
                !page1_ids.contains(id),
                "page 2 should contain different messages than page 1"
            );
        }
    }

    mod proptests {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            #[test]
            fn search_limit_valid_range(v in 1u64..=25) {
                let limit = SearchLimit::try_from(v).unwrap();
                prop_assert_eq!(u64::from(limit.get()), v);
            }

            #[test]
            fn search_limit_invalid_rejects(v in prop_oneof![Just(0u64), 26u64..]) {
                prop_assert!(SearchLimit::try_from(v).is_err());
            }

            #[test]
            fn search_offset_valid_range(v in 0u64..=9975) {
                let offset = SearchOffset::try_from(v).unwrap();
                prop_assert_eq!(u64::from(offset.get()), v);
            }

            #[test]
            fn search_offset_invalid_rejects(v in 9976u64..) {
                prop_assert!(SearchOffset::try_from(v).is_err());
            }
        }
    }
}
