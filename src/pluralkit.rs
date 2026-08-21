//! PluralKit identity resolution for proxy webhook messages.
//!
//! When a message arrives through a PluralKit webhook, the Discord author ID
//! belongs to the webhook bot — not the human who sent it. This module queries
//! the PluralKit API to resolve the actual sender's identity: their Discord
//! user ID, PK system UUID, and PK member UUID.
//!
//! # Design
//!
//! - **Cache resolved identity facts** (sender, system UUID, member UUID),
//!   never allow/deny verdicts.
//! - **Concurrency limit**: PK allows 10 req/s. We use a semaphore for bounded
//!   concurrency (not rate limiting) and retry on 429. True paced/token-bucket
//!   rate limiting is tracked in Forgejo issue #330.
//! - **Fail closed** on restricted channels: if PK returns 404, times out,
//!   or exhausts retries, the message is dropped.
//! - **Data minimization**: cache only the authorization projection (IDs),
//!   not names, pronouns, or avatars.

use crate::discord::verified_action::{EventBinding as VerifiedEventBinding, EventContext};
use serenity::model::id::{ChannelId, GuildId, MessageId, UserId};
use std::{
    collections::{HashMap, VecDeque},
    num::NonZeroU64,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::{RwLock, Semaphore};
use uuid::Uuid;

// ── Types ───────────────────────────────────────────────────────────────────

/// A validated PluralKit UUID.
///
/// Validates that the string is a well-formed UUID (8-4-4-4-12 hex format).
/// Prevents raw arbitrary strings from being used as identity claims.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct PkUuid(String);

impl PkUuid {
    /// Parse and validate a PluralKit UUID string.
    ///
    /// Accepts only the standard UUID format: 8-4-4-4-12 hex with dashes.
    /// PK short IDs (5-char alphanumeric) are NOT accepted because the PK API
    /// returns full UUIDs — a configured short ID would never match the
    /// resolver's output, silently failing to authorize.
    pub(crate) fn parse(s: impl Into<String>) -> Result<Self, &'static str> {
        let s = s.into();
        if s.is_empty() {
            return Err("PK UUID must be non-empty");
        }
        let is_uuid = s.len() == 36
            && s.chars().enumerate().all(|(i, c)| {
                if i == 8 || i == 13 || i == 18 || i == 23 {
                    c == '-'
                } else {
                    c.is_ascii_hexdigit()
                }
            });
        if !is_uuid {
            return Err("PK identifier must be a full UUID (8-4-4-4-12 hex); \
                 short IDs are not accepted — use the full UUID from PK's API");
        }
        // Canonicalize to lowercase for consistent matching.
        Ok(Self(s.to_ascii_lowercase()))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for PkUuid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Error from PK API resolution.
#[derive(Debug)]
pub enum PkResolveError {
    /// PK returned 404 — message is not a PK-proxied message.
    NotFound,
    /// PK returned 429 and retries were exhausted.
    RateLimited,
    /// HTTP or network error.
    HttpError(String),
    /// Timeout waiting for PK response.
    Timeout,
    /// Event binding mismatch — a specific coordinate doesn't match.
    BindingMismatch {
        dimension: &'static str,
        expected: u64,
        actual: u64,
    },
    /// Resolver concurrency semaphore was closed (shutdown in progress).
    SemaphoreClosed,
    /// PK API returned a response that could not be decoded or validated.
    InvalidResponse(String),
}

impl std::fmt::Display for PkResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound => write!(f, "PK API returned 404"),
            Self::RateLimited => write!(f, "PK API rate limited (429 exhausted)"),
            Self::HttpError(msg) => write!(f, "PK API HTTP error: {msg}"),
            Self::Timeout => write!(f, "PK API request timed out"),
            Self::BindingMismatch {
                dimension,
                expected,
                actual,
            } => write!(
                f,
                "PK response {dimension} binding mismatch: expected {expected}, got {actual}"
            ),
            Self::SemaphoreClosed => write!(f, "PK resolver semaphore closed"),
            Self::InvalidResponse(msg) => write!(f, "PK API invalid response: {msg}"),
        }
    }
}

impl std::error::Error for PkResolveError {}

// ── PK API response types ───────────────────────────────────────────────────

/// Subset of PK `/v2/messages/{id}` response — only the fields we need.
#[derive(Debug, serde::Deserialize)]
struct PkMessageResponse {
    /// Discord message ID of the proxied message.
    #[serde(deserialize_with = "deserialize_snowflake")]
    id: u64,
    /// Discord user snowflake of the human who triggered the proxy.
    #[serde(deserialize_with = "deserialize_snowflake")]
    sender: u64,
    /// The Discord channel the proxied message was sent in.
    #[serde(deserialize_with = "deserialize_snowflake")]
    channel: u64,
    /// The Discord guild the proxied message was sent in.
    #[serde(deserialize_with = "deserialize_snowflake")]
    guild: u64,
    system: Option<PkSystemRef>,
    member: Option<PkMemberRef>,
}

#[derive(Debug, serde::Deserialize)]
struct PkSystemRef {
    uuid: String,
}

#[derive(Debug, serde::Deserialize)]
struct PkMemberRef {
    uuid: String,
}

fn deserialize_snowflake<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize;
    let s = String::deserialize(deserializer)?;
    s.parse::<u64>().map_err(serde::de::Error::custom)
}

// ── PK Client ───────────────────────────────────────────────────────────────

/// Configuration for the PK resolver.
#[derive(Debug, Clone)]
pub struct PkResolverConfig {
    /// Test-only origin override. Production code has no configurable provider
    /// origin and always uses `OFFICIAL_PK_ORIGIN`.
    #[cfg(test)]
    test_origin: Option<String>,
    /// Per-request timeout.
    pub request_timeout: Duration,
    /// Maximum concurrent PK API requests.
    pub max_concurrent: usize,
    /// Maximum retries on 429.
    pub max_retries: u32,
    /// Cache TTL for resolved identities.
    pub cache_ttl: Duration,
}

impl Default for PkResolverConfig {
    fn default() -> Self {
        Self {
            #[cfg(test)]
            test_origin: None,
            request_timeout: Duration::from_secs(5),
            max_concurrent: 5,
            max_retries: 3,
            cache_ttl: Duration::from_secs(300), // 5 minutes
        }
    }
}

/// Typed identity facts returned to the verified-action boundary.
///
/// These are provider facts only: they contain no transport proof, policy
/// decision, or independently supplied event coordinates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum VerifiedPkFacts {
    /// The official application acted as itself and represented no principal.
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "the accepted boundary reserves AppOnly, but PK currently has no authoritative successful app-only response"
        )
    )]
    AppOnly,
    /// The official application represented a Discord principal.
    Represented {
        discord_user_id: UserId,
        system_id: Option<Uuid>,
        member_id: Option<Uuid>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct VerifiedLookupKey {
    message_id: MessageId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VerifiedBindingContext {
    Guild(GuildId),
    DirectMessage,
}

trait VerifiedBindingView {
    fn message_id(&self) -> MessageId;
    fn channel_id(&self) -> ChannelId;
    fn context(&self) -> VerifiedBindingContext;
}

impl VerifiedBindingView for VerifiedEventBinding {
    fn message_id(&self) -> MessageId {
        self.message_id()
    }

    fn channel_id(&self) -> ChannelId {
        self.channel_id()
    }

    fn context(&self) -> VerifiedBindingContext {
        match self.context() {
            EventContext::Guild(guild_id) => VerifiedBindingContext::Guild(*guild_id),
            EventContext::DirectMessage => VerifiedBindingContext::DirectMessage,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum VerifiedResponseContext {
    Guild(GuildId),
    DirectMessage,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct VerifiedResponseBinding {
    channel_id: ChannelId,
    context: VerifiedResponseContext,
}

#[derive(Debug, Clone)]
struct VerifiedCachedFacts {
    facts: VerifiedPkFacts,
    response_binding: VerifiedResponseBinding,
    cached_at: Instant,
}

/// PluralKit-specific fact cache. Provider identity is implicit in the cache
/// owner, while the typed message ID is the exact provider lookup key.
struct VerifiedFactsCache {
    entries: HashMap<VerifiedLookupKey, VerifiedCachedFacts>,
    order: VecDeque<VerifiedLookupKey>,
    capacity: usize,
}

impl VerifiedFactsCache {
    fn new(capacity: usize) -> Self {
        Self {
            entries: HashMap::new(),
            order: VecDeque::new(),
            capacity,
        }
    }

    fn get(
        &mut self,
        key: VerifiedLookupKey,
        binding: &impl VerifiedBindingView,
        ttl: Duration,
        now: Instant,
    ) -> Result<Option<VerifiedPkFacts>, PkResolveError> {
        self.remove_expired(ttl, now);
        let Some(cached) = self.entries.get(&key) else {
            return Ok(None);
        };
        verify_response_binding(binding, &cached.response_binding)?;
        Ok(Some(cached.facts.clone()))
    }

    fn insert(
        &mut self,
        key: VerifiedLookupKey,
        facts: VerifiedPkFacts,
        response_binding: VerifiedResponseBinding,
        now: Instant,
    ) {
        self.order.retain(|queued| *queued != key);
        self.order.push_back(key);
        self.entries.insert(
            key,
            VerifiedCachedFacts {
                facts,
                response_binding,
                cached_at: now,
            },
        );
        while self.entries.len() > self.capacity {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            self.entries.remove(&oldest);
        }
    }

    fn remove_expired(&mut self, ttl: Duration, now: Instant) {
        self.entries
            .retain(|_, entry| now.saturating_duration_since(entry.cached_at) < ttl);
        self.order.retain(|key| self.entries.contains_key(key));
    }
}

/// PluralKit identity resolver with caching and concurrency limiting.
pub struct PkResolver {
    client: reqwest::Client,
    config: PkResolverConfig,
    /// Bounded concurrency limiter — caps simultaneous in-flight PK requests.
    /// Note: this is a concurrency limit, not a rate limit. PK's 10 req/s
    /// quota requires the separate paced/token-bucket limiter tracked in
    /// Forgejo issue #330.
    semaphore: Arc<Semaphore>,
    /// Facts-only cache used exclusively by the verified-action boundary.
    verified_facts_cache: RwLock<VerifiedFactsCache>,
}

impl Default for PkResolver {
    fn default() -> Self {
        Self::with_defaults()
    }
}

/// Maximum cache entries before pruning.
const CACHE_CAP: usize = 500;

const OFFICIAL_PK_ORIGIN: &str = "https://api.pluralkit.me";
const MAX_VERIFIED_RESPONSE_BYTES: usize = 64 * 1024;

/// Maximum Retry-After delay we'll honour from PK (seconds).
const MAX_RETRY_AFTER_SECS: f64 = 30.0;

fn map_reqwest_error(error: reqwest::Error) -> PkResolveError {
    if error.is_timeout() {
        PkResolveError::Timeout
    } else {
        PkResolveError::HttpError(error.to_string())
    }
}

fn retry_delay(response: &reqwest::Response, attempt: u32) -> Duration {
    response
        .headers()
        .get("retry-after")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<f64>().ok())
        .filter(|value| value.is_finite() && *value >= 0.0)
        .map(|value| Duration::from_secs_f64(value.min(MAX_RETRY_AFTER_SECS)))
        .unwrap_or_else(|| {
            let exponent = 2u64.checked_pow(attempt).unwrap_or(u64::MAX);
            Duration::from_millis(100u64.saturating_mul(exponent).min(30_000))
        })
}

fn verified_facts_from_response(
    binding: &impl VerifiedBindingView,
    response: PkMessageResponse,
) -> Result<(VerifiedPkFacts, VerifiedResponseBinding), PkResolveError> {
    if response.id != binding.message_id().get() {
        return Err(PkResolveError::BindingMismatch {
            dimension: "message_id",
            expected: binding.message_id().get(),
            actual: response.id,
        });
    }
    if response.channel != binding.channel_id().get() {
        return Err(PkResolveError::BindingMismatch {
            dimension: "channel_id",
            expected: binding.channel_id().get(),
            actual: response.channel,
        });
    }

    let response_context = match binding.context() {
        VerifiedBindingContext::Guild(guild_id) if response.guild == guild_id.get() => {
            VerifiedResponseContext::Guild(guild_id)
        }
        VerifiedBindingContext::DirectMessage if response.guild == 0 => {
            VerifiedResponseContext::DirectMessage
        }
        VerifiedBindingContext::Guild(guild_id) => {
            return Err(PkResolveError::BindingMismatch {
                dimension: "guild_id",
                expected: guild_id.get(),
                actual: response.guild,
            });
        }
        VerifiedBindingContext::DirectMessage => {
            return Err(PkResolveError::BindingMismatch {
                dimension: "guild_id",
                expected: 0,
                actual: response.guild,
            });
        }
    };

    let discord_user_id = NonZeroU64::new(response.sender)
        .map(UserId::from)
        .ok_or_else(|| {
            PkResolveError::InvalidResponse("sender snowflake must be nonzero".to_string())
        })?;
    let system_id = response
        .system
        .map(|system| Uuid::parse_str(&system.uuid))
        .transpose()
        .map_err(|error| PkResolveError::InvalidResponse(format!("system UUID: {error}")))?;
    let member_id = response
        .member
        .map(|member| Uuid::parse_str(&member.uuid))
        .transpose()
        .map_err(|error| PkResolveError::InvalidResponse(format!("member UUID: {error}")))?;

    Ok((
        VerifiedPkFacts::Represented {
            discord_user_id,
            system_id,
            member_id,
        },
        VerifiedResponseBinding {
            channel_id: binding.channel_id(),
            context: response_context,
        },
    ))
}

fn verify_response_binding(
    binding: &impl VerifiedBindingView,
    response_binding: &VerifiedResponseBinding,
) -> Result<(), PkResolveError> {
    if response_binding.channel_id != binding.channel_id() {
        return Err(PkResolveError::BindingMismatch {
            dimension: "channel_id",
            expected: binding.channel_id().get(),
            actual: response_binding.channel_id.get(),
        });
    }
    match (binding.context(), &response_binding.context) {
        (VerifiedBindingContext::Guild(expected), VerifiedResponseContext::Guild(actual))
            if expected == *actual =>
        {
            Ok(())
        }
        (VerifiedBindingContext::DirectMessage, VerifiedResponseContext::DirectMessage) => Ok(()),
        (VerifiedBindingContext::Guild(expected), VerifiedResponseContext::Guild(actual)) => {
            Err(PkResolveError::BindingMismatch {
                dimension: "guild_id",
                expected: expected.get(),
                actual: actual.get(),
            })
        }
        (VerifiedBindingContext::Guild(expected), VerifiedResponseContext::DirectMessage) => {
            Err(PkResolveError::BindingMismatch {
                dimension: "guild_id",
                expected: expected.get(),
                actual: 0,
            })
        }
        (VerifiedBindingContext::DirectMessage, VerifiedResponseContext::Guild(actual)) => {
            Err(PkResolveError::BindingMismatch {
                dimension: "guild_id",
                expected: 0,
                actual: actual.get(),
            })
        }
    }
}

impl PkResolver {
    /// Create a new resolver with the given config.
    ///
    /// Returns `Err` if the config is invalid (zero concurrency, unbounded
    /// retries, or timeout/TTL out of range).
    pub fn new(config: PkResolverConfig) -> Result<Self, String> {
        if config.max_concurrent == 0 {
            return Err("max_concurrent must be > 0".into());
        }
        if config.max_retries > 10 {
            return Err("max_retries must be <= 10".into());
        }
        if config.request_timeout > Duration::from_secs(60) {
            return Err("request_timeout must be <= 60s".into());
        }
        if config.cache_ttl > Duration::from_secs(3600) {
            return Err("cache_ttl must be <= 1 hour".into());
        }

        let client = reqwest::Client::builder()
            .timeout(config.request_timeout)
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(
                "dione/0.33 (Discord MCP gateway; +https://github.com/butterflyskies/dione)",
            )
            .build()
            .expect("reqwest client build should not fail");

        let semaphore = Arc::new(Semaphore::new(config.max_concurrent));

        Ok(Self {
            client,
            config,
            semaphore,
            verified_facts_cache: RwLock::new(VerifiedFactsCache::new(CACHE_CAP)),
        })
    }

    /// Create a resolver with default config (always succeeds).
    pub fn with_defaults() -> Self {
        Self::new(PkResolverConfig::default()).expect("default PkResolverConfig is valid")
    }

    /// Resolve provider facts for an already verified, immutably bound action.
    ///
    /// One outer timeout covers cache access, semaphore acquisition, every
    /// retry and backoff, response-body reads, and JSON parsing. Identical
    /// concurrent cache misses may issue duplicate requests, bounded by the
    /// resolver semaphore; only complete validated facts are published.
    pub(crate) async fn resolve_verified_facts(
        &self,
        binding: &VerifiedEventBinding,
    ) -> Result<VerifiedPkFacts, PkResolveError> {
        self.resolve_verified_facts_for(binding).await
    }

    async fn resolve_verified_facts_for(
        &self,
        binding: &impl VerifiedBindingView,
    ) -> Result<VerifiedPkFacts, PkResolveError> {
        tokio::time::timeout(
            self.config.request_timeout,
            self.resolve_verified_facts_inner(binding),
        )
        .await
        .map_err(|_| PkResolveError::Timeout)?
    }

    async fn resolve_verified_facts_inner(
        &self,
        binding: &impl VerifiedBindingView,
    ) -> Result<VerifiedPkFacts, PkResolveError> {
        let key = VerifiedLookupKey {
            message_id: binding.message_id(),
        };
        if let Some(facts) = self.verified_facts_cache.write().await.get(
            key,
            binding,
            self.config.cache_ttl,
            Instant::now(),
        )? {
            return Ok(facts);
        }

        let _permit = self
            .semaphore
            .acquire()
            .await
            .map_err(|_| PkResolveError::SemaphoreClosed)?;

        // A caller ahead of us may have published complete facts while this
        // action waited for a permit. Rechecking here bounds duplicate calls to
        // the requests that actually acquired permits before publication.
        if let Some(facts) = self.verified_facts_cache.write().await.get(
            key,
            binding,
            self.config.cache_ttl,
            Instant::now(),
        )? {
            return Ok(facts);
        }

        let response = self.query_verified_pk_api(key.message_id).await?;
        let (facts, response_binding) = verified_facts_from_response(binding, response)?;

        self.verified_facts_cache.write().await.insert(
            key,
            facts.clone(),
            response_binding,
            Instant::now(),
        );
        Ok(facts)
    }

    async fn query_verified_pk_api(
        &self,
        message_id: MessageId,
    ) -> Result<PkMessageResponse, PkResolveError> {
        let url = format!(
            "{}/v2/messages/{}",
            self.verified_origin(),
            message_id.get()
        );

        for attempt in 0..=self.config.max_retries {
            let response = self
                .client
                .get(&url)
                .send()
                .await
                .map_err(map_reqwest_error)?;
            let status = response.status();
            if status.is_success() {
                let mut response = response;
                let mut body = Vec::new();
                while let Some(chunk) = response.chunk().await.map_err(map_reqwest_error)? {
                    if body.len().saturating_add(chunk.len()) > MAX_VERIFIED_RESPONSE_BYTES {
                        return Err(PkResolveError::InvalidResponse(
                            "response body exceeds 64 KiB".to_string(),
                        ));
                    }
                    body.extend_from_slice(&chunk);
                }
                return serde_json::from_slice(&body)
                    .map_err(|error| PkResolveError::InvalidResponse(error.to_string()));
            }
            if status.as_u16() == 404 {
                return Err(PkResolveError::NotFound);
            }
            if status.as_u16() != 429 {
                return Err(PkResolveError::HttpError(format!(
                    "unexpected status {status}"
                )));
            }
            if attempt == self.config.max_retries {
                return Err(PkResolveError::RateLimited);
            }
            tokio::time::sleep(retry_delay(&response, attempt)).await;
        }

        Err(PkResolveError::RateLimited)
    }

    fn verified_origin(&self) -> &str {
        #[cfg(test)]
        {
            self.config
                .test_origin
                .as_deref()
                .unwrap_or(OFFICIAL_PK_ORIGIN)
        }
        #[cfg(not(test))]
        {
            OFFICIAL_PK_ORIGIN
        }
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::{
        collections::HashSet,
        sync::atomic::{AtomicUsize, Ordering},
    };

    // ── PkResolverConfig defaults ───────────────────────────────────────────

    #[test]
    fn test_default_config() {
        let config = PkResolverConfig::default();
        assert!(config.test_origin.is_none());
        assert_eq!(config.request_timeout, Duration::from_secs(5));
        assert_eq!(config.max_concurrent, 5);
        assert_eq!(config.max_retries, 3);
        assert_eq!(config.cache_ttl, Duration::from_secs(300));
    }

    // ── Mock PK API tests using a local HTTP server ─────────────────────────

    fn verified_test_resolver(
        base_url: &str,
        request_timeout: Duration,
        max_concurrent: usize,
    ) -> PkResolver {
        PkResolver::new(PkResolverConfig {
            test_origin: Some(base_url.to_string()),
            request_timeout,
            max_concurrent,
            max_retries: 2,
            cache_ttl: Duration::from_secs(60),
        })
        .expect("verified test config is valid")
    }

    #[derive(Debug, Clone, Copy)]
    struct TestVerifiedBinding {
        message_id: MessageId,
        channel_id: ChannelId,
        context: VerifiedBindingContext,
    }

    impl VerifiedBindingView for TestVerifiedBinding {
        fn message_id(&self) -> MessageId {
            self.message_id
        }

        fn channel_id(&self) -> ChannelId {
            self.channel_id
        }

        fn context(&self) -> VerifiedBindingContext {
            self.context
        }
    }

    fn guild_binding(message_id: u64) -> TestVerifiedBinding {
        TestVerifiedBinding {
            message_id: MessageId::new(message_id),
            channel_id: ChannelId::new(500),
            context: VerifiedBindingContext::Guild(GuildId::new(1)),
        }
    }

    fn represented_facts(sender: u64) -> VerifiedPkFacts {
        VerifiedPkFacts::Represented {
            discord_user_id: UserId::new(sender),
            system_id: Some(Uuid::from_u128(1)),
            member_id: Some(Uuid::from_u128(2)),
        }
    }

    fn response_binding(binding: TestVerifiedBinding) -> VerifiedResponseBinding {
        VerifiedResponseBinding {
            channel_id: binding.channel_id,
            context: match binding.context {
                VerifiedBindingContext::Guild(guild_id) => VerifiedResponseContext::Guild(guild_id),
                VerifiedBindingContext::DirectMessage => VerifiedResponseContext::DirectMessage,
            },
        }
    }

    /// Simulated PK API JSON response.
    fn pk_message_json(
        message_id: u64,
        sender: u64,
        channel: u64,
        guild: u64,
        system_uuid: &str,
        member_uuid: &str,
    ) -> String {
        serde_json::json!({
            "id": message_id.to_string(),
            "sender": sender.to_string(),
            "channel": channel.to_string(),
            "guild": guild.to_string(),
            "system": { "uuid": system_uuid },
            "member": { "uuid": member_uuid }
        })
        .to_string()
    }

    #[test]
    fn verified_origin_is_fixed_to_official_https_endpoint() {
        assert_eq!(OFFICIAL_PK_ORIGIN, "https://api.pluralkit.me");
        assert_eq!(PkResolver::default().verified_origin(), OFFICIAL_PK_ORIGIN);
        assert!(OFFICIAL_PK_ORIGIN.starts_with("https://"));
    }

    #[tokio::test]
    async fn verified_resolution_returns_typed_facts_and_rejects_cached_binding_mismatch() {
        let server = MockPkServer::start().await;
        server
            .enqueue(
                200,
                pk_message_json(
                    5100,
                    42,
                    500,
                    1,
                    "a0000001-0000-0000-0000-000000000001",
                    "b0000001-0000-0000-0000-000000000001",
                ),
            )
            .await;
        let resolver = verified_test_resolver(&server.base_url(), Duration::from_secs(1), 2);
        let binding = guild_binding(5100);

        let facts = resolver
            .resolve_verified_facts_for(&binding)
            .await
            .expect("represented facts");
        assert!(matches!(
            facts,
            VerifiedPkFacts::Represented {
                discord_user_id,
                system_id: Some(_),
                member_id: Some(_),
            } if discord_user_id == UserId::new(42)
        ));

        let changed_channel = TestVerifiedBinding {
            channel_id: ChannelId::new(501),
            ..binding
        };
        assert!(matches!(
            resolver.resolve_verified_facts_for(&changed_channel).await,
            Err(PkResolveError::BindingMismatch {
                dimension: "channel_id",
                ..
            })
        ));
        assert_eq!(server.request_count(), 1);
    }

    #[tokio::test]
    async fn verified_resolution_does_not_treat_404_as_app_only() {
        let server = MockPkServer::start().await;
        server.enqueue(404, "{}".to_string()).await;
        let resolver = verified_test_resolver(&server.base_url(), Duration::from_secs(1), 1);

        assert!(matches!(
            resolver
                .resolve_verified_facts_for(&guild_binding(5101))
                .await,
            Err(PkResolveError::NotFound)
        ));
    }

    #[tokio::test]
    async fn verified_resolution_exhausts_429_retries_as_rate_limited() {
        let server = MockPkServer::start().await;
        for _ in 0..=2 {
            server
                .enqueue_with_headers(
                    429,
                    "{}".to_string(),
                    vec![("Retry-After".to_string(), "0".to_string())],
                )
                .await;
        }
        let resolver = verified_test_resolver(&server.base_url(), Duration::from_secs(1), 1);

        assert!(matches!(
            resolver
                .resolve_verified_facts_for(&guild_binding(5107))
                .await,
            Err(PkResolveError::RateLimited)
        ));
        assert_eq!(server.request_count(), 3);
    }

    #[test]
    fn verified_response_rejects_each_mismatched_binding_coordinate() {
        let binding = guild_binding(5108);
        let cases = [
            (5109, 500, 1, "message_id"),
            (5108, 501, 1, "channel_id"),
            (5108, 500, 2, "guild_id"),
        ];

        for (message_id, channel_id, guild_id, expected_dimension) in cases {
            let response = PkMessageResponse {
                id: message_id,
                sender: 42,
                channel: channel_id,
                guild: guild_id,
                system: None,
                member: None,
            };
            assert!(matches!(
                verified_facts_from_response(&binding, response),
                Err(PkResolveError::BindingMismatch { dimension, .. })
                    if dimension == expected_dimension
            ));
        }
    }

    #[test]
    fn verified_response_enforces_nonzero_sender_and_dm_context() {
        let direct_message = TestVerifiedBinding {
            message_id: MessageId::new(5102),
            channel_id: ChannelId::new(500),
            context: VerifiedBindingContext::DirectMessage,
        };
        let response = PkMessageResponse {
            id: 5102,
            sender: 42,
            channel: 500,
            guild: 0,
            system: None,
            member: None,
        };
        assert!(matches!(
            verified_facts_from_response(&direct_message, response),
            Ok((
                VerifiedPkFacts::Represented { .. },
                VerifiedResponseBinding {
                    context: VerifiedResponseContext::DirectMessage,
                    ..
                }
            ))
        ));

        let zero_sender = PkMessageResponse {
            id: 5102,
            sender: 0,
            channel: 500,
            guild: 0,
            system: None,
            member: None,
        };
        assert!(matches!(
            verified_facts_from_response(&direct_message, zero_sender),
            Err(PkResolveError::InvalidResponse(_))
        ));

        let guild_response_for_dm = PkMessageResponse {
            id: 5102,
            sender: 42,
            channel: 500,
            guild: 1,
            system: None,
            member: None,
        };
        assert!(matches!(
            verified_facts_from_response(&direct_message, guild_response_for_dm),
            Err(PkResolveError::BindingMismatch {
                dimension: "guild_id",
                expected: 0,
                actual: 1,
            })
        ));
    }

    #[tokio::test]
    async fn verified_total_deadline_covers_semaphore_body_and_retry_waits() {
        let semaphore_server = MockPkServer::start().await;
        let semaphore_resolver =
            verified_test_resolver(&semaphore_server.base_url(), Duration::from_millis(30), 1);
        let _held_permit = semaphore_resolver
            .semaphore
            .acquire()
            .await
            .expect("open semaphore");
        let started = Instant::now();
        assert!(matches!(
            semaphore_resolver
                .resolve_verified_facts_for(&guild_binding(5103))
                .await,
            Err(PkResolveError::Timeout)
        ));
        assert!(started.elapsed() < Duration::from_millis(250));

        let body_server = MockPkServer::start().await;
        body_server
            .enqueue_delayed(
                200,
                pk_message_json(
                    5104,
                    42,
                    500,
                    1,
                    "a0000001-0000-0000-0000-000000000001",
                    "b0000001-0000-0000-0000-000000000001",
                ),
                Duration::from_millis(200),
            )
            .await;
        let body_resolver =
            verified_test_resolver(&body_server.base_url(), Duration::from_millis(30), 1);
        assert!(matches!(
            body_resolver
                .resolve_verified_facts_for(&guild_binding(5104))
                .await,
            Err(PkResolveError::Timeout)
        ));

        let retry_server = MockPkServer::start().await;
        retry_server
            .enqueue_with_headers(
                429,
                "{}".to_string(),
                vec![("Retry-After".to_string(), "1".to_string())],
            )
            .await;
        let retry_resolver =
            verified_test_resolver(&retry_server.base_url(), Duration::from_millis(30), 1);
        assert!(matches!(
            retry_resolver
                .resolve_verified_facts_for(&guild_binding(5105))
                .await,
            Err(PkResolveError::Timeout)
        ));
    }

    #[tokio::test]
    async fn verified_invalid_body_is_unavailable_not_app_only() {
        let server = MockPkServer::start().await;
        server.enqueue(200, "not JSON".to_string()).await;
        let resolver = verified_test_resolver(&server.base_url(), Duration::from_secs(1), 1);
        assert!(matches!(
            resolver
                .resolve_verified_facts_for(&guild_binding(5106))
                .await,
            Err(PkResolveError::InvalidResponse(_))
        ));
    }

    #[test]
    fn verified_cache_expiry_is_physical_and_does_not_erase_lifecycle_admission() {
        use crate::{
            discord::verified_action::{
                LifecycleContext, LifecycleProvenance, test_lifecycle_facts,
            },
            ingress_ledger::{IngressLedger, TransitionResult},
        };
        use serenity::model::{Timestamp, id::WebhookId};

        let mut cache = VerifiedFactsCache::new(2);
        let now = Instant::now();
        let first = guild_binding(5200);
        let second = guild_binding(5201);
        let third = guild_binding(5202);
        for (binding, sender) in [(first, 40), (second, 41), (first, 42), (third, 43)] {
            let facts = if binding.message_id == second.message_id {
                VerifiedPkFacts::AppOnly
            } else {
                represented_facts(sender)
            };
            cache.insert(
                VerifiedLookupKey {
                    message_id: binding.message_id,
                },
                facts,
                response_binding(binding),
                now,
            );
        }

        assert_eq!(cache.entries.len(), 2);
        assert_eq!(cache.order.len(), 2);
        assert!(cache.entries.contains_key(&VerifiedLookupKey {
            message_id: first.message_id,
        }));
        assert!(!cache.entries.contains_key(&VerifiedLookupKey {
            message_id: second.message_id,
        }));

        let ledger = IngressLedger::new();
        let lifecycle_facts = test_lifecycle_facts(
            first.message_id,
            first.channel_id,
            GuildId::new(1),
            WebhookId::new(900),
            LifecycleProvenance::Represented {
                discord_user_id: UserId::new(42),
                system_id: Some(Uuid::from_u128(1)),
                member_id: Some(Uuid::from_u128(2)),
            },
        );
        assert!(matches!(
            ledger.admit_verified_create(
                lifecycle_facts,
                first.message_id,
                first.channel_id,
                LifecycleContext::Guild(GuildId::new(1)),
                WebhookId::new(900),
                UserId::new(901),
                None,
                "original",
                Timestamp::from_unix_timestamp(1).unwrap(),
            ),
            TransitionResult::Admitted(_)
        ));

        cache.remove_expired(Duration::from_secs(1), now + Duration::from_secs(2));
        assert!(cache.entries.is_empty());
        assert!(cache.order.is_empty());
        assert!(
            cache
                .get(
                    VerifiedLookupKey {
                        message_id: first.message_id,
                    },
                    &first,
                    Duration::from_secs(1),
                    now + Duration::from_secs(2),
                )
                .expect("cache lookup")
                .is_none()
        );

        assert!(matches!(
            ledger.transition_passive_edit(
                first.message_id,
                first.channel_id,
                LifecycleContext::Guild(GuildId::new(1)),
                UserId::new(901),
                "edited after identity cache expiry",
                Timestamp::from_unix_timestamp(2).unwrap(),
                |_| true,
            ),
            TransitionResult::Admitted(_)
        ));
    }

    #[tokio::test]
    async fn verified_concurrent_identical_misses_issue_bounded_duplicate_calls() {
        let server = MockPkServer::start().await;
        let body = pk_message_json(
            5300,
            42,
            500,
            1,
            "a0000001-0000-0000-0000-000000000001",
            "b0000001-0000-0000-0000-000000000001",
        );
        server
            .enqueue_delayed(200, body.clone(), Duration::from_millis(60))
            .await;
        server
            .enqueue_delayed(200, body, Duration::from_millis(60))
            .await;
        let resolver = verified_test_resolver(&server.base_url(), Duration::from_secs(1), 2);
        let binding = guild_binding(5300);

        let (first, second) = tokio::join!(
            resolver.resolve_verified_facts_for(&binding),
            resolver.resolve_verified_facts_for(&binding),
        );
        assert_eq!(first.expect("first facts"), second.expect("second facts"));
        assert_eq!(server.request_count(), 2);
        let cache = resolver.verified_facts_cache.read().await;
        assert_eq!(cache.entries.len(), 1);
        assert_eq!(cache.order.len(), 1);
    }

    #[tokio::test]
    async fn verified_queued_identical_misses_recheck_cache_after_one_request() {
        let server = MockPkServer::start().await;
        server
            .enqueue_delayed(
                200,
                pk_message_json(
                    5301,
                    42,
                    500,
                    1,
                    "a0000001-0000-0000-0000-000000000001",
                    "b0000001-0000-0000-0000-000000000001",
                ),
                Duration::from_millis(60),
            )
            .await;
        let resolver = Arc::new(verified_test_resolver(
            &server.base_url(),
            Duration::from_secs(1),
            1,
        ));
        let binding = guild_binding(5301);
        let mut tasks = Vec::new();
        for _ in 0..8 {
            let resolver = resolver.clone();
            tasks.push(tokio::spawn(async move {
                resolver.resolve_verified_facts_for(&binding).await
            }));
        }
        for task in tasks {
            assert!(task.await.expect("resolution task").is_ok());
        }

        assert_eq!(server.request_count(), 1);
        let cache = resolver.verified_facts_cache.read().await;
        assert_eq!(cache.entries.len(), 1);
        assert_eq!(cache.order.len(), 1);
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(32))]

        #[test]
        fn concurrent_mixed_bindings_never_publish_cross_bound_principals(
            first_id in 1u64..u64::MAX,
            id_delta in 1u64..10_000,
            first_sender in 1u64..u64::MAX,
            sender_delta in 1u64..10_000,
        ) {
            let second_id = first_id.checked_add(id_delta).unwrap_or(id_delta);
            let second_sender = first_sender.checked_add(sender_delta).unwrap_or(sender_delta);
            prop_assume!(first_id != second_id);
            prop_assume!(first_sender != second_sender);

            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("test runtime");
            let (crossed_first, crossed_second, crossed_cache_len, first, second, cached) =
                runtime.block_on(async {
                let server = MockPkServer::start().await;
                server
                    .enqueue_for(
                        first_id,
                        200,
                        pk_message_json(
                            second_id,
                            second_sender,
                            500,
                            1,
                            "a0000001-0000-0000-0000-000000000001",
                            "b0000001-0000-0000-0000-000000000001",
                        ),
                    )
                    .await;
                server
                    .enqueue_for(
                        second_id,
                        200,
                        pk_message_json(
                            first_id,
                            first_sender,
                            500,
                            1,
                            "a0000001-0000-0000-0000-000000000001",
                            "b0000001-0000-0000-0000-000000000001",
                        ),
                    )
                    .await;
                let resolver = verified_test_resolver(
                    &server.base_url(),
                    Duration::from_secs(1),
                    2,
                );
                let first_binding = guild_binding(first_id);
                let second_binding = guild_binding(second_id);
                let (crossed_first, crossed_second) = tokio::join!(
                    resolver.resolve_verified_facts_for(&first_binding),
                    resolver.resolve_verified_facts_for(&second_binding),
                );
                let crossed_cache_len = resolver.verified_facts_cache.read().await.entries.len();

                server
                    .enqueue_for(
                        first_id,
                        200,
                        pk_message_json(
                            first_id,
                            first_sender,
                            500,
                            1,
                            "a0000001-0000-0000-0000-000000000001",
                            "b0000001-0000-0000-0000-000000000001",
                        ),
                    )
                    .await;
                server
                    .enqueue_for(
                        second_id,
                        200,
                        pk_message_json(
                            second_id,
                            second_sender,
                            500,
                            1,
                            "a0000001-0000-0000-0000-000000000001",
                            "b0000001-0000-0000-0000-000000000001",
                        ),
                    )
                    .await;
                let (first, second) = tokio::join!(
                    resolver.resolve_verified_facts_for(&first_binding),
                    resolver.resolve_verified_facts_for(&second_binding),
                );
                let cache = resolver.verified_facts_cache.read().await;
                let cached = cache
                    .entries
                    .iter()
                    .map(|(key, value)| (key.message_id.get(), value.facts.clone()))
                    .collect::<Vec<_>>();
                (
                    crossed_first,
                    crossed_second,
                    crossed_cache_len,
                    first,
                    second,
                    cached,
                )
            });

            let expected_facts = |sender| VerifiedPkFacts::Represented {
                discord_user_id: UserId::new(sender),
                system_id: Some(
                    Uuid::parse_str("a0000001-0000-0000-0000-000000000001")
                        .expect("valid system UUID"),
                ),
                member_id: Some(
                    Uuid::parse_str("b0000001-0000-0000-0000-000000000001")
                        .expect("valid member UUID"),
                ),
            };
            let first_cross_rejected =
                matches!(crossed_first, Err(PkResolveError::BindingMismatch { .. }));
            let second_cross_rejected =
                matches!(crossed_second, Err(PkResolveError::BindingMismatch { .. }));
            prop_assert!(first_cross_rejected);
            prop_assert!(second_cross_rejected);
            prop_assert_eq!(crossed_cache_len, 0);
            prop_assert!(matches!(first, Ok(ref facts) if *facts == expected_facts(first_sender)));
            prop_assert!(matches!(second, Ok(ref facts) if *facts == expected_facts(second_sender)));
            prop_assert_eq!(cached.len(), 2);
            for (message_id, facts) in cached {
                if message_id == first_id {
                    prop_assert_eq!(facts, expected_facts(first_sender));
                } else if message_id == second_id {
                    prop_assert_eq!(facts, expected_facts(second_sender));
                } else {
                    prop_assert!(false, "unexpected cache key {message_id}");
                }
            }
        }
    }

    proptest! {
        #[test]
        fn verified_cache_fifo_and_map_remain_bijective_under_refreshes(
            message_ids in proptest::collection::vec(1u64..50, 1..120),
            capacity in 1usize..8,
        ) {
            let mut cache = VerifiedFactsCache::new(capacity);
            let now = Instant::now();
            for (index, message_id) in message_ids.into_iter().enumerate() {
                let binding = guild_binding(message_id);
                cache.insert(
                    VerifiedLookupKey { message_id: binding.message_id },
                    represented_facts((index as u64 % 100) + 1),
                    response_binding(binding),
                    now,
                );

                let queued: HashSet<_> = cache.order.iter().copied().collect();
                prop_assert_eq!(queued.len(), cache.order.len());
                prop_assert_eq!(queued.len(), cache.entries.len());
                prop_assert!(queued.iter().all(|key| cache.entries.contains_key(key)));
                prop_assert!(cache.entries.len() <= capacity);
            }
        }
    }

    // ── Config validation tests ──────────────────────────────────────────────

    #[test]
    fn test_config_zero_concurrent_rejected() {
        let config = PkResolverConfig {
            max_concurrent: 0,
            ..PkResolverConfig::default()
        };
        assert!(PkResolver::new(config).is_err());
    }

    #[test]
    fn test_config_excessive_retries_rejected() {
        let config = PkResolverConfig {
            max_retries: 11,
            ..PkResolverConfig::default()
        };
        assert!(PkResolver::new(config).is_err());
    }

    #[test]
    fn test_pk_uuid_empty_rejected() {
        assert!(PkUuid::parse("").is_err());
    }

    #[test]
    fn test_pk_uuid_invalid_format_rejected() {
        assert!(PkUuid::parse("not-a-uuid").is_err());
        assert!(PkUuid::parse("sys-1").is_err());
    }

    #[test]
    fn test_pk_uuid_short_id_rejected() {
        // Short IDs cannot match resolver output (which returns full UUIDs),
        // so they must be rejected to prevent silent authorization failures.
        assert!(PkUuid::parse("abcde").is_err());
    }

    #[test]
    fn test_pk_uuid_valid_uuid_accepted() {
        assert!(PkUuid::parse("12345678-1234-1234-1234-123456789abc").is_ok());
    }

    #[test]
    fn test_pk_uuid_uppercase_canonicalized() {
        let uuid = PkUuid::parse("12345678-ABCD-1234-1234-123456789ABC").unwrap();
        assert_eq!(
            uuid.as_str(),
            "12345678-abcd-1234-1234-123456789abc",
            "uppercase UUID must be canonicalized to lowercase"
        );
    }

    // ── Mock HTTP server ────────────────────────────────────────────────────

    /// A queued mock response with optional extra headers.
    struct MockResponse {
        request_message_id: Option<u64>,
        status: u16,
        body: String,
        extra_headers: Vec<(String, String)>,
        delay: Duration,
    }

    /// Simple mock PK API server for deterministic tests.
    struct MockPkServer {
        addr: std::net::SocketAddr,
        responses: Arc<tokio::sync::Mutex<Vec<MockResponse>>>,
        request_count: Arc<AtomicUsize>,
        _handle: tokio::task::JoinHandle<()>,
    }

    impl MockPkServer {
        async fn start() -> Self {
            let responses: Arc<tokio::sync::Mutex<Vec<MockResponse>>> =
                Arc::new(tokio::sync::Mutex::new(Vec::new()));
            let request_count = Arc::new(AtomicUsize::new(0));

            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind mock server");
            let addr = listener.local_addr().expect("get local addr");

            let resp_clone = responses.clone();
            let count_clone = request_count.clone();
            let handle = tokio::spawn(async move {
                loop {
                    let Ok((stream, _)) = listener.accept().await else {
                        break;
                    };
                    let responses = resp_clone.clone();
                    let request_count = count_clone.clone();
                    tokio::spawn(async move {
                        handle_connection(stream, responses, request_count).await;
                    });
                }
            });

            Self {
                addr,
                responses,
                request_count,
                _handle: handle,
            }
        }

        fn base_url(&self) -> String {
            format!("http://{}", self.addr)
        }

        async fn enqueue(&self, status: u16, body: String) {
            self.responses.lock().await.push(MockResponse {
                request_message_id: None,
                status,
                body,
                extra_headers: vec![],
                delay: Duration::ZERO,
            });
        }

        async fn enqueue_delayed(&self, status: u16, body: String, delay: Duration) {
            self.responses.lock().await.push(MockResponse {
                request_message_id: None,
                status,
                body,
                extra_headers: vec![],
                delay,
            });
        }

        fn request_count(&self) -> usize {
            self.request_count.load(Ordering::SeqCst)
        }

        async fn enqueue_for(&self, request_message_id: u64, status: u16, body: String) {
            self.responses.lock().await.push(MockResponse {
                request_message_id: Some(request_message_id),
                status,
                body,
                extra_headers: vec![],
                delay: Duration::ZERO,
            });
        }

        async fn enqueue_with_headers(
            &self,
            status: u16,
            body: String,
            headers: Vec<(String, String)>,
        ) {
            self.responses.lock().await.push(MockResponse {
                request_message_id: None,
                status,
                body,
                extra_headers: headers,
                delay: Duration::ZERO,
            });
        }
    }

    /// Minimal HTTP/1.1 server handler for mock responses.
    async fn handle_connection(
        stream: tokio::net::TcpStream,
        responses: Arc<tokio::sync::Mutex<Vec<MockResponse>>>,
        request_count: Arc<AtomicUsize>,
    ) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let mut stream = stream;
        request_count.fetch_add(1, Ordering::SeqCst);
        let mut buf = [0u8; 4096];
        // Read the request (we don't parse it, just consume it).
        let bytes_read = stream.read(&mut buf).await.unwrap_or_default();
        let request = String::from_utf8_lossy(&buf[..bytes_read]);

        let mock = {
            let mut queue = responses.lock().await;
            if queue.is_empty() {
                MockResponse {
                    request_message_id: None,
                    status: 500,
                    body: "no response enqueued".to_string(),
                    extra_headers: vec![],
                    delay: Duration::ZERO,
                }
            } else {
                let index = queue
                    .iter()
                    .position(|response| {
                        response.request_message_id.is_some_and(|message_id| {
                            request.starts_with(&format!("GET /v2/messages/{message_id} "))
                        })
                    })
                    .unwrap_or(0);
                queue.remove(index)
            }
        };

        tokio::time::sleep(mock.delay).await;

        let status_text = match mock.status {
            200 => "OK",
            404 => "Not Found",
            429 => "Too Many Requests",
            _ => "Error",
        };

        let mut extra = String::new();
        for (k, v) in &mock.extra_headers {
            extra.push_str(&format!("{k}: {v}\r\n"));
        }

        let response = format!(
            "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{}Connection: close\r\n\r\n{}",
            mock.status,
            status_text,
            mock.body.len(),
            extra,
            mock.body,
        );

        let _ = stream.write_all(response.as_bytes()).await;
        let _ = stream.flush().await;
    }
}
