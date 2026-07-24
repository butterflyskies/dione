use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::Read as _;
use std::sync::Arc;

use arc_swap::ArcSwap;
use camino::{Utf8Path, Utf8PathBuf};
use regex::RegexSet;
use serde::{Deserialize, Deserializer, de::Error as _};
use serenity::model::id::UserId;
use thiserror::Error;

use crate::contradictionary::{Contradictionary, ContradictionaryConfig, load_sidecar_entries};
use crate::pre_send::ConstructId;
use crate::timestamp::Timestamp;

/// Default maximum size of one GAIE attachment (25 MiB).
pub const DEFAULT_ARCHIVE_MAX_ATTACHMENT_BYTES: u64 = 25 * 1024 * 1024;
/// Default cumulative GAIE attachment download budget for one backfill run (250 MiB).
pub const DEFAULT_ARCHIVE_MAX_RUN_DOWNLOAD_BYTES: u64 = 250 * 1024 * 1024;

// ── Error type ───────────────────────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("config file not found at {path}")]
    NotFound { path: Utf8PathBuf },
    #[error("config parse error: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

// ── Config types ─────────────────────────────────────────────────────────────

/// Top-level configuration loaded from `config.toml`.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct Config {
    /// Optional Discord bot token. `DISCORD_BOT_TOKEN` env var takes precedence.
    pub token: Option<String>,
    /// IANA timezone name for timestamps in channel events (e.g. "America/Los_Angeles").
    /// Defaults to UTC.
    pub timezone: Option<String>,
    pub access: AccessConfig,
    pub channels: Vec<ChannelConfig>,
    pub mentions: MentionConfig,
    pub delivery: DeliveryConfig,
    pub access_requests: AccessRequestsConfig,
    pub voice: VoiceConfig,
    pub rate_limit: RateLimitTomlConfig,
    pub contradictionary: ContradictionaryConfig,
    pub pre_send: PreSendConfig,
    /// Inbound memory-bell shadow evaluation.
    pub bell_rings: BellRingsConfig,
    /// Restart-only, one-shot GAIE archive configuration.
    pub archive: ArchiveConfig,
}

/// Configuration for the opt-in GAIE one-shot archive commands.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct ArchiveConfig {
    /// Enables archive commands. The daemon never starts an archive job.
    pub enabled: bool,
    /// The sole parent channel admitted to the archive.
    pub channel_id: String,
    /// The guild which owns the parent channel.
    pub guild_id: String,
    /// The filesystem-safe corpus identifier.
    pub corpus_id: String,
    /// The local directory containing archive artifacts.
    pub data_dir: Utf8PathBuf,
    /// Permits successful completion when Discord coverage is incomplete.
    pub allow_partial: bool,
    /// Maximum admitted size of one attachment.
    pub max_attachment_bytes: u64,
    /// Maximum cumulative attachment download bytes admitted during one backfill run.
    pub max_run_download_bytes: u64,
}

impl Default for ArchiveConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            channel_id: String::new(),
            guild_id: String::new(),
            corpus_id: String::new(),
            data_dir: Utf8PathBuf::new(),
            allow_partial: false,
            max_attachment_bytes: DEFAULT_ARCHIVE_MAX_ATTACHMENT_BYTES,
            max_run_download_bytes: DEFAULT_ARCHIVE_MAX_RUN_DOWNLOAD_BYTES,
        }
    }
}

impl ArchiveConfig {
    /// Validates the archive gate and its relationship to configured channels.
    pub fn validate(&self, channels: &[ChannelConfig]) -> Result<(), String> {
        if !self.enabled {
            return Err("archive is disabled; set archive.enabled = true".to_owned());
        }
        let channel_id = self
            .channel_id
            .parse::<u64>()
            .map_err(|_| "archive.channel_id must be a nonzero Discord ID".to_owned())?;
        if channel_id == 0 {
            return Err("archive.channel_id must be a nonzero Discord ID".to_owned());
        }
        let guild_id = self
            .guild_id
            .parse::<u64>()
            .map_err(|_| "archive.guild_id must be a nonzero Discord ID".to_owned())?;
        if guild_id == 0 {
            return Err("archive.guild_id must be a nonzero Discord ID".to_owned());
        }
        crate::gaie::CorpusId::parse(&self.corpus_id).map_err(|error| error.to_string())?;
        if self.data_dir.as_str().is_empty()
            || !self.data_dir.is_absolute()
            || self
                .data_dir
                .components()
                .any(|component| component.as_str() == "..")
        {
            return Err("archive.data_dir must be an absolute path without `..`".to_owned());
        }
        let occurrences = channels
            .iter()
            .filter(|channel| channel.id == self.channel_id)
            .count();
        if occurrences != 1 {
            return Err(format!(
                "archive.channel_id must appear exactly once in [[channels]]; found {occurrences}"
            ));
        }
        if self.max_attachment_bytes == 0 {
            return Err("archive.max_attachment_bytes must be nonzero".to_owned());
        }
        if self.max_run_download_bytes == 0 {
            return Err("archive.max_run_download_bytes must be nonzero".to_owned());
        }
        if self.max_run_download_bytes < self.max_attachment_bytes {
            return Err(
                "archive.max_run_download_bytes must be at least archive.max_attachment_bytes"
                    .to_owned(),
            );
        }
        Ok(())
    }
}

/// Whether bell evaluation results are injected into delivery metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BellMode {
    /// Evaluate and log, but do not alter delivery.
    #[default]
    Shadow,
    /// Evaluate and inject `bells` into notification metadata.
    Live,
}

/// Inbound memory-bell configuration.
#[derive(Debug, Clone, PartialEq)]
pub struct BellRingsConfig {
    /// Enables evaluation.
    pub enabled: bool,
    /// Shadow (log only) or live (inject into metadata).
    pub mode: BellMode,
    /// The single memory-mcp provider. A singular field makes multi-provider
    /// fan-out unrepresentable in the first slice.
    pub provider: Option<BellProviderConfig>,
    /// Largest admitted cosine distance.
    pub max_semantic_distance: f64,
    /// Maximum candidates requested and bells retained.
    pub max_bells: usize,
    /// Total evaluation deadline in milliseconds.
    pub deadline_ms: u64,
}

#[derive(Deserialize)]
#[serde(default)]
struct BellRingsConfigWire {
    enabled: bool,
    mode: BellMode,
    provider: Option<BellProviderConfig>,
    #[serde(deserialize_with = "deserialize_max_semantic_distance")]
    max_semantic_distance: f64,
    #[serde(deserialize_with = "deserialize_max_bells")]
    max_bells: usize,
    #[serde(deserialize_with = "deserialize_bell_deadline")]
    deadline_ms: u64,
}

impl Default for BellRingsConfigWire {
    fn default() -> Self {
        let defaults = BellRingsConfig::default();
        Self {
            enabled: defaults.enabled,
            mode: defaults.mode,
            provider: defaults.provider,
            max_semantic_distance: defaults.max_semantic_distance,
            max_bells: defaults.max_bells,
            deadline_ms: defaults.deadline_ms,
        }
    }
}

impl<'de> Deserialize<'de> for BellRingsConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = BellRingsConfigWire::deserialize(deserializer)?;
        if wire.enabled && wire.provider.is_none() {
            return Err(D::Error::custom(
                "enabled bell_rings requires exactly one provider",
            ));
        }
        Ok(Self {
            enabled: wire.enabled,
            mode: wire.mode,
            provider: wire.provider,
            max_semantic_distance: wire.max_semantic_distance,
            max_bells: wire.max_bells,
            deadline_ms: wire.deadline_ms,
        })
    }
}

impl Default for BellRingsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            mode: BellMode::Shadow,
            provider: None,
            max_semantic_distance: 0.3,
            max_bells: 3,
            deadline_ms: 300,
        }
    }
}

/// One memory-mcp endpoint and one explicit allowed scope.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BellProviderConfig {
    /// Streamable HTTP MCP endpoint.
    pub url: BellProviderUrl,
    /// Explicit recall scope. `all` and empty scopes are rejected while parsing.
    pub scope: BellScope,
}

/// A validated HTTP(S) memory-mcp endpoint without embedded credentials.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BellProviderUrl(String);

impl BellProviderUrl {
    pub fn parse(value: impl Into<String>) -> Result<Self, String> {
        let value = value.into();
        let parsed = value
            .parse::<reqwest::Url>()
            .map_err(|_| "bell_rings provider url must be a valid HTTP(S) URL".to_owned())?;
        if !matches!(parsed.scheme(), "http" | "https")
            || !parsed.username().is_empty()
            || parsed.password().is_some()
        {
            return Err(
                "bell_rings provider url must be HTTP(S) without embedded credentials".to_owned(),
            );
        }
        Ok(Self(parsed.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for BellProviderUrl {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::parse(String::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

/// A validated memory scope which cannot represent the cross-scope `all` value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BellScope(String);

impl BellScope {
    pub fn parse(value: impl Into<String>) -> Result<Self, String> {
        let value = value.into();
        let trimmed = value.trim();
        if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("all") {
            Err("bell_rings provider scope must be explicit and cannot be `all`".to_owned())
        } else {
            Ok(Self(trimmed.to_owned()))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for BellScope {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::parse(String::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

fn deserialize_max_semantic_distance<'de, D>(deserializer: D) -> Result<f64, D::Error>
where
    D: Deserializer<'de>,
{
    let value = f64::deserialize(deserializer)?;
    (value.is_finite() && (0.0..=2.0).contains(&value))
        .then_some(value)
        .ok_or_else(|| D::Error::custom("max_semantic_distance must be finite and between 0 and 2"))
}

fn deserialize_max_bells<'de, D>(deserializer: D) -> Result<usize, D::Error>
where
    D: Deserializer<'de>,
{
    let value = usize::deserialize(deserializer)?;
    (1..=100)
        .contains(&value)
        .then_some(value)
        .ok_or_else(|| D::Error::custom("max_bells must be between 1 and 100"))
}

fn deserialize_bell_deadline<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: Deserializer<'de>,
{
    let value = u64::deserialize(deserializer)?;
    (1..=300)
        .contains(&value)
        .then_some(value)
        .ok_or_else(|| D::Error::custom("deadline_ms must be between 1 and 300"))
}

/// Pre-send hook lifecycle configuration.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct PreSendConfig {
    /// Disables the pipeline entirely when false. The enabled default is
    /// always Observe mode; enforcement is intentionally not configurable yet.
    pub enabled: bool,
    /// Stable construct identity included in hook context and bypass audits.
    pub construct_id: String,
    /// Discord bot user ID when known before gateway readiness.
    #[serde(default, deserialize_with = "deserialize_optional_user_id")]
    pub author_id: Option<UserId>,
}

fn deserialize_optional_user_id<'de, D>(deserializer: D) -> Result<Option<UserId>, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum UserIdValue {
        String(String),
        Integer(u64),
    }

    Option::<UserIdValue>::deserialize(deserializer)?
        .map(|value| {
            let raw = match value {
                UserIdValue::String(value) => value.parse::<u64>().map_err(D::Error::custom)?,
                UserIdValue::Integer(value) => value,
            };
            crate::mcp::ids::Snowflake::new(raw)
                .map(crate::mcp::ids::Snowflake::user)
                .ok_or_else(|| D::Error::custom("Discord user ID must be nonzero"))
        })
        .transpose()
}

impl Default for PreSendConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            construct_id: "dione".to_owned(),
            author_id: None,
        }
    }
}

/// Access control configuration.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct AccessConfig {
    pub dm_policy: DmPolicy,
    pub allow_from: Vec<String>,
    pub admins: Vec<String>,
    #[serde(default)]
    pub admin_only_mutations: bool,
}

impl Default for AccessConfig {
    fn default() -> Self {
        Self {
            dm_policy: DmPolicy::Queue,
            allow_from: Vec::new(),
            admins: Vec::new(),
            admin_only_mutations: false,
        }
    }
}

/// How to handle DMs from users not in `allow_from`.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum DmPolicy {
    /// Queue the message for admin review.
    #[default]
    Queue,
    /// Silently drop the message.
    Drop,
    /// Drop all DMs, including from `allow_from`.
    Disabled,
}

/// Per-channel guild configuration.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ChannelConfig {
    pub id: String,
    pub require_mention: bool,
    pub allow_from: Vec<String>,
    /// Per-channel coalescing delay for channel events (milliseconds).
    /// When > 0, channel events (messages, edits, deletes, reactions) are
    /// buffered and flushed after this delay. Non-channel events (traces,
    /// permission responses, config errors) pass through immediately.
    /// If `None`, inherits from `[delivery] delivery_delay_ms`.
    pub delivery_delay_ms: Option<u64>,
}

impl Default for ChannelConfig {
    fn default() -> Self {
        Self {
            id: String::new(),
            require_mention: true,
            allow_from: Vec::new(),
            delivery_delay_ms: None,
        }
    }
}

/// Mention detection configuration.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct MentionConfig {
    pub patterns: Vec<String>,
}

/// Message delivery configuration.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct DeliveryConfig {
    pub ack_reaction: String,
    pub reply_to_mode: ReplyToMode,
    pub text_chunk_limit: usize,
    pub chunk_mode: ChunkMode,
    /// Global default coalescing delay for channel events (milliseconds).
    /// Per-channel `delivery_delay_ms` overrides this. Default: 0 (no buffering).
    pub delivery_delay_ms: u64,
}

impl Default for DeliveryConfig {
    fn default() -> Self {
        Self {
            ack_reaction: "👀".to_string(),
            reply_to_mode: ReplyToMode::First,
            text_chunk_limit: 2000,
            chunk_mode: ChunkMode::Paragraph,
            delivery_delay_ms: 0,
        }
    }
}

/// Which messages to thread chunked replies to.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ReplyToMode {
    /// Reply to the first message in a chunk sequence.
    #[default]
    First,
    /// Reply to every previous chunk.
    All,
    /// Do not thread.
    Off,
}

/// How to split long messages.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ChunkMode {
    /// Split at `\n\n`, then `\n`, then space, then hard cut.
    #[default]
    Paragraph,
    /// Hard cut at the limit.
    Length,
}

/// Access request queue configuration.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct AccessRequestsConfig {
    pub expiry_seconds: u64,
    pub max_pending: usize,
    pub notify_cooldown_seconds: u64,
}

impl Default for AccessRequestsConfig {
    fn default() -> Self {
        Self {
            expiry_seconds: 86400,
            max_pending: 50,
            notify_cooldown_seconds: 60,
        }
    }
}

/// Voice feature configuration.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct VoiceConfig {
    pub enabled: bool,
}

// ── Rate limit config (TOML representation) ────────────────────────────────

/// TOML-level rate limit configuration. Deserialized from `[rate_limit]`.
///
/// Maps to the runtime [`crate::rate_limiter::RateLimitConfig`] via
/// [`RateLimitTomlConfig::into_runtime`].
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct RateLimitTomlConfig {
    pub enabled: bool,
    /// Default tokens per window.
    pub max_tokens: Option<u32>,
    /// Default window duration in seconds.
    pub window_secs: Option<u64>,
    /// Default cooldown duration in seconds.
    pub cooldown_secs: Option<u64>,
    /// Default overflow policy: "drop" or "buffer".
    pub overflow: Option<String>,
}

impl RateLimitTomlConfig {
    /// Convert to the runtime rate limit config.
    pub fn into_runtime(self) -> crate::rate_limiter::RateLimitConfig {
        use crate::rate_limiter::{OverflowPolicy, RateLimitConfig, ScopeConfig};
        use std::time::Duration;

        let overflow = match self.overflow.as_deref() {
            Some("buffer") => OverflowPolicy::Buffer,
            Some("drop") | None => OverflowPolicy::Drop { notify: true },
            Some(other) => {
                tracing::warn!(
                    value = other,
                    "unrecognized rate_limit.overflow value, defaulting to \"drop\""
                );
                OverflowPolicy::Drop { notify: true }
            }
        };

        let window_secs = self.window_secs.unwrap_or(3600);
        let cooldown_secs = self.cooldown_secs.unwrap_or(3600);

        if self.enabled && window_secs == 0 {
            tracing::warn!("rate_limit.window_secs is 0, rate limiting is effectively disabled");
        }
        if self.enabled && cooldown_secs == 0 {
            tracing::warn!("rate_limit.cooldown_secs is 0, cooldown is effectively disabled");
        }

        let default = ScopeConfig {
            max_tokens: self.max_tokens.unwrap_or(20),
            window: Duration::from_secs(window_secs),
            cooldown: Duration::from_secs(cooldown_secs),
            overflow,
        };

        RateLimitConfig {
            enabled: self.enabled,
            default,
            classes: Vec::new(),
            individuals: std::collections::HashMap::new(),
            channels: std::collections::HashMap::new(),
        }
    }
}

// ── Loaded config (pre-parsed for hot-path performance) ──────────────────────

/// Config with pre-computed O(1) lookup structures.
/// Created once per config load; used for all gate checks.
#[derive(Debug, Clone)]
pub struct LoadedConfig {
    pub raw: Config,
    /// Parsed user IDs from `access.allow_from` for O(1) membership test.
    pub allowed_ids: HashSet<u64>,
    /// Parsed admin IDs for O(1) membership test and iteration.
    pub admin_ids: HashSet<u64>,
    /// Per-channel parsed policies. O(1) lookup by channel ID.
    pub channel_policies: HashMap<u64, ChannelPolicy>,
    /// Pre-compiled mention regex patterns.
    pub mention_patterns: Option<RegexSet>,
    /// Parsed timezone for timestamp conversion. None = UTC.
    pub tz: Option<chrono_tz::Tz>,
    /// Pre-computed runtime rate limit config (avoids per-event `into_runtime()`).
    rate_limit_runtime: crate::rate_limiter::RateLimitConfig,
    /// Pre-built Aho-Corasick concordance for outbound text scanning.
    pub contradictionary: Option<Arc<Contradictionary>>,
    /// Validated configured Discord identity for outbound hook context.
    pub pre_send_author_id: Option<UserId>,
    /// Validated construct identity for outbound hook context.
    pub pre_send_construct_id: ConstructId,
}

/// Pre-parsed per-channel access policy.
#[derive(Debug, Clone)]
pub struct ChannelPolicy {
    pub require_mention: bool,
    pub allow_from: HashSet<u64>,
    /// Per-channel coalescing delay (milliseconds). 0 = immediate.
    pub delivery_delay_ms: u64,
}

impl std::ops::Deref for LoadedConfig {
    type Target = Config;
    fn deref(&self) -> &Config {
        &self.raw
    }
}

impl LoadedConfig {
    /// Build from raw Config, parsing IDs and compiling regexes.
    pub fn from_raw(mut raw: Config) -> Self {
        let allowed_ids = parse_id_set(&raw.access.allow_from);
        let admin_ids = parse_id_set(&raw.access.admins);
        let channel_policies = raw
            .channels
            .iter()
            .filter_map(|ch| {
                let id = ch.id.parse::<u64>().ok()?;
                Some((
                    id,
                    ChannelPolicy {
                        require_mention: ch.require_mention,
                        allow_from: parse_id_set(&ch.allow_from),
                        delivery_delay_ms: ch
                            .delivery_delay_ms
                            .unwrap_or(raw.delivery.delivery_delay_ms),
                    },
                ))
            })
            .collect();
        let mention_patterns = compile_mention_patterns(&raw);
        let tz = raw
            .timezone
            .as_deref()
            .and_then(|s| match s.parse::<chrono_tz::Tz>() {
                Ok(tz) => Some(tz),
                Err(_) => {
                    tracing::warn!(timezone = s, "invalid IANA timezone, falling back to UTC");
                    None
                }
            });
        let rate_limit_runtime = raw.rate_limit.clone().into_runtime();
        let contradictionary =
            if raw.contradictionary.enabled && !raw.contradictionary.entries.is_empty() {
                Some(Arc::new(Contradictionary::new(
                    raw.contradictionary.entries.clone(),
                )))
            } else {
                None
            };
        let pre_send_author_id = raw.pre_send.author_id;
        let pre_send_construct_id = match ConstructId::parse(raw.pre_send.construct_id.clone()) {
            Ok(construct_id) => construct_id,
            Err(error) => {
                tracing::warn!(
                    %error,
                    "invalid pre_send.construct_id; disabling pre-send pipeline"
                );
                raw.pre_send.enabled = false;
                ConstructId::default()
            }
        };
        Self {
            raw,
            allowed_ids,
            admin_ids,
            channel_policies,
            mention_patterns,
            tz,
            rate_limit_runtime,
            contradictionary,
            pre_send_author_id,
            pre_send_construct_id,
        }
    }

    /// O(1) check if a user is in the allowlist.
    pub fn is_allowed(&self, user_id: u64) -> bool {
        self.allowed_ids.contains(&user_id)
    }

    /// O(1) check if a user is an admin.
    pub fn is_admin(&self, user_id: u64) -> bool {
        self.admin_ids.contains(&user_id)
    }

    /// O(1) channel policy lookup.
    pub fn channel_policy(&self, channel_id: u64) -> Option<&ChannelPolicy> {
        self.channel_policies.get(&channel_id)
    }

    /// Returns the pre-computed runtime rate limit config (avoids per-event allocation).
    pub fn rate_limit_runtime(&self) -> &crate::rate_limiter::RateLimitConfig {
        &self.rate_limit_runtime
    }

    /// Returns the delivery delay (ms) for a channel.
    ///
    /// Resolution order: per-channel override → global `[delivery]` default → 0.
    pub fn delivery_delay_ms(&self, channel_id: u64) -> u64 {
        self.channel_policies
            .get(&channel_id)
            .map(|p| p.delivery_delay_ms)
            .unwrap_or(self.raw.delivery.delivery_delay_ms)
    }

    /// Convert a `chrono::DateTime<Utc>` to a [`Timestamp`] in the configured timezone.
    pub fn localize_utc(&self, utc: &chrono::DateTime<chrono::Utc>) -> Timestamp {
        let dt = match self.tz {
            Some(tz) => utc.with_timezone(&tz).fixed_offset(),
            None => utc.fixed_offset(),
        };
        Timestamp(dt)
    }

    /// Convert an RFC3339 timestamp string to a [`Timestamp`] in the configured timezone.
    ///
    /// If parsing fails, falls back to the current UTC time (localized to the
    /// configured timezone). Callers should ensure input is valid — the fallback
    /// exists only as a safety net.
    pub fn localize_rfc3339(&self, rfc3339: &str) -> Timestamp {
        let dt = match chrono::DateTime::parse_from_rfc3339(rfc3339) {
            Ok(dt) => dt,
            Err(_) => {
                tracing::warn!(
                    input = rfc3339,
                    "failed to parse RFC3339 timestamp; falling back to current UTC"
                );
                chrono::Utc::now().fixed_offset()
            }
        };
        let localized = match self.tz {
            Some(tz) => dt.with_timezone(&tz).fixed_offset(),
            None => dt,
        };
        Timestamp(localized)
    }
}

fn parse_id_set(ids: &[String]) -> HashSet<u64> {
    ids.iter().filter_map(|s| s.parse::<u64>().ok()).collect()
}

fn compile_mention_patterns(config: &Config) -> Option<RegexSet> {
    if config.mentions.patterns.is_empty() {
        return None;
    }
    let patterns: Vec<&str> = config
        .mentions
        .patterns
        .iter()
        .map(String::as_str)
        .collect();
    match RegexSet::new(&patterns) {
        Ok(set) => Some(set),
        Err(_) => {
            // Try individually to find and skip bad patterns.
            let valid: Vec<&str> = config
                .mentions
                .patterns
                .iter()
                .filter(|p| regex::Regex::new(p).is_ok())
                .map(String::as_str)
                .collect();
            if valid.is_empty() {
                None
            } else {
                RegexSet::new(&valid).ok()
            }
        }
    }
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Returns the state directory path.
///
/// Uses `DIONE_STATE_DIR` env var if set; otherwise defaults to
/// `~/.claude/channels/dione/`.
pub fn state_dir() -> Utf8PathBuf {
    if let Ok(dir) = std::env::var("DIONE_STATE_DIR") {
        return Utf8PathBuf::from(dir);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    Utf8PathBuf::from(home).join(".claude/channels/dione")
}

static CONFIG_PATH_OVERRIDE: std::sync::OnceLock<Utf8PathBuf> = std::sync::OnceLock::new();

/// Set a custom config file path, overriding the default `state_dir/config.toml`.
///
/// Must be called before [`reload_config`]. Can only be set once; subsequent
/// calls are no-ops.
pub fn set_config_path(path: Utf8PathBuf) {
    CONFIG_PATH_OVERRIDE.set(path).ok();
}

/// Returns the config file path.
///
/// If [`set_config_path`] was called, returns the override path.
/// Otherwise returns `state_dir/config.toml`.
pub fn config_path(state_dir: &Utf8Path) -> Utf8PathBuf {
    CONFIG_PATH_OVERRIDE
        .get()
        .cloned()
        .unwrap_or_else(|| state_dir.join("config.toml"))
}

static LAST_VALID_CONFIG: std::sync::LazyLock<ArcSwap<LoadedConfig>> =
    std::sync::LazyLock::new(|| ArcSwap::from_pointee(LoadedConfig::from_raw(Config::default())));

/// Returns the current config from the in-memory cache.
///
/// Returns an `Arc<LoadedConfig>` loaded from the ArcSwap without cloning
/// the inner config. Callers that need ownership can `Arc::clone()`.
///
/// If the cache has not been populated by [`reload_config`] yet, returns
/// defaults. In practice, `reload_config` is called at startup before any
/// reader.
pub fn load_config(_state_dir: &Utf8Path) -> Arc<LoadedConfig> {
    LAST_VALID_CONFIG.load_full()
}

/// Reads config from disk, updates the in-memory cache, and returns the result.
///
/// Called by the file watcher on changes, by [`ConfigStore::save`] after writes,
/// and as a fallback when the cache is empty.
pub fn reload_config(state_dir: &Utf8Path) -> (LoadedConfig, Option<String>) {
    let config_path = config_path(state_dir);
    let mut config_error = None;
    let mut raw = match try_load_config(&config_path) {
        Ok(cfg) => cfg,
        Err(ConfigError::NotFound { .. }) => {
            let defaults = Config::default();
            write_default_config(&config_path);
            tracing::info!(path = %config_path, "config file not found, generated default config");
            defaults
        }
        Err(ConfigError::Parse(e)) => {
            let error_msg = format!("config parse error: {e}");
            let cached = (**LAST_VALID_CONFIG.load()).clone();
            tracing::warn!(
                path = %config_path,
                error = %e,
                "config parse error, continuing with last valid config"
            );
            return (cached, Some(error_msg));
        }
        Err(ConfigError::Io(e)) => {
            let error_msg = format!("config IO error: {e}");
            tracing::warn!(path = %config_path, error = %e, "failed to read config, using defaults");
            config_error = Some(error_msg);
            Config::default()
        }
    };
    // ── Load contradictionary sidecar ──────────────────────────────────────
    if raw.contradictionary.enabled {
        let config_dir = config_path.parent().unwrap_or_else(|| Utf8Path::new("."));
        let sidecar = if raw.contradictionary.sidecar_path.is_empty() {
            config_dir.join("contradictionary.toml")
        } else {
            let p = Utf8PathBuf::from(&raw.contradictionary.sidecar_path);
            if p.is_absolute() {
                p
            } else {
                config_dir.join(p)
            }
        };
        match load_sidecar_entries(sidecar.as_std_path()) {
            Ok(entries) if !entries.is_empty() => {
                tracing::info!(
                    path = %sidecar,
                    count = entries.len(),
                    "loaded contradictionary sidecar entries"
                );
                raw.contradictionary.entries.extend(entries);
            }
            Ok(_) => {} // file missing or empty — no-op
            Err(e) => {
                // Fail closed. `load_sidecar_entries` parses the sidecar as one
                // unit, so a single malformed entry yields zero entries — and
                // installing that would silently disable the whole gate. Keep
                // the last valid config instead, matching the fallback used for
                // a corrupt `config.toml` above.
                let cached = (**LAST_VALID_CONFIG.load()).clone();
                tracing::warn!(
                    path = %sidecar,
                    error = %e,
                    "contradictionary sidecar parse error, continuing with last valid config"
                );
                return (cached, Some(config_error.unwrap_or(e)));
            }
        }
    }

    let loaded = LoadedConfig::from_raw(raw);
    store_loaded_config(&loaded);
    (loaded, config_error)
}

/// Update the ArcSwap cache directly (no disk read).
///
/// Call this after writing a new config to disk (e.g. from `ConfigStore::save`)
/// to keep the in-memory cache consistent without a redundant re-read.
pub fn store_loaded_config(loaded: &LoadedConfig) {
    LAST_VALID_CONFIG.store(Arc::new(loaded.clone()));
}

/// Resolves the Discord bot token.
///
/// `DISCORD_BOT_TOKEN` env var takes precedence over `config.token`.
pub fn resolve_token(config: &Config) -> Option<String> {
    std::env::var("DISCORD_BOT_TOKEN")
        .ok()
        .or_else(|| config.token.clone())
}

// ── Internal helpers ─────────────────────────────────────────────────────────

fn write_default_config(config_path: &Utf8Path) {
    const TEMPLATE: &str = include_str!("config_template.toml");
    if let Err(e) = std::fs::write(config_path.as_std_path(), TEMPLATE) {
        tracing::warn!(path = %config_path, error = %e, "failed to write default config");
    }
}

fn try_load_config(config_path: &Utf8Path) -> Result<Config, ConfigError> {
    let mut file = match File::open(config_path.as_std_path()) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(ConfigError::NotFound {
                path: config_path.to_owned(),
            });
        }
        Err(e) => return Err(ConfigError::Io(e)),
    };

    // Best-effort shared lock — if another process holds an exclusive lock, skip.
    let _ = file.try_lock();

    let mut contents = String::new();
    file.read_to_string(&mut contents)?;
    let config: Config = toml::from_str(&contents)?;
    Ok(config)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn temp_state_dir() -> (TempDir, Utf8PathBuf) {
        let dir = TempDir::new().unwrap();
        let path = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        (dir, path)
    }

    /// Serialises tests that call [`reload_config`].
    ///
    /// `LAST_VALID_CONFIG` is a process-global `ArcSwap`. Under a thread-parallel
    /// runner (`cargo test`) any test calling `reload_config` clobbers it for
    /// every other test, so assertions that span two reloads — priming the cache
    /// then observing the fallback — are otherwise racy.
    static CONFIG_CACHE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Takes the config-cache lock, ignoring poisoning from an unrelated panic.
    fn config_cache_guard() -> std::sync::MutexGuard<'static, ()> {
        CONFIG_CACHE_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn test_archive_config_is_disabled_by_default() {
        assert!(!Config::default().archive.enabled);
    }

    #[test]
    fn test_archive_config_requires_exactly_one_allowlisted_channel() {
        let archive = ArchiveConfig {
            enabled: true,
            channel_id: "42".to_owned(),
            guild_id: "7".to_owned(),
            corpus_id: "fixture-v1".to_owned(),
            data_dir: Utf8PathBuf::from("/tmp/gaie-fixture"),
            allow_partial: false,
            ..ArchiveConfig::default()
        };
        assert!(archive.validate(&[]).is_err());
        assert!(
            archive
                .validate(&[ChannelConfig {
                    id: "42".to_owned(),
                    ..ChannelConfig::default()
                }])
                .is_ok()
        );
        assert!(
            archive
                .validate(&[
                    ChannelConfig {
                        id: "42".to_owned(),
                        ..ChannelConfig::default()
                    },
                    ChannelConfig {
                        id: "42".to_owned(),
                        ..ChannelConfig::default()
                    },
                ])
                .is_err()
        );
    }

    #[test]
    fn test_archive_config_rejects_path_like_corpus_and_relative_data_dir() {
        let mut archive = ArchiveConfig {
            enabled: true,
            channel_id: "42".into(),
            guild_id: "7".into(),
            corpus_id: "../escape".into(),
            data_dir: Utf8PathBuf::from("relative"),
            allow_partial: false,
            ..ArchiveConfig::default()
        };
        let channels = [ChannelConfig {
            id: "42".into(),
            ..ChannelConfig::default()
        }];
        assert!(archive.validate(&channels).is_err());
        archive.corpus_id = "safe".into();
        assert!(archive.validate(&channels).is_err());
    }

    #[test]
    fn test_archive_attachment_limits_have_safe_defaults() {
        let archive = ArchiveConfig::default();
        let deserialized: ArchiveConfig = toml::from_str("").unwrap();

        assert_eq!(
            archive.max_attachment_bytes,
            DEFAULT_ARCHIVE_MAX_ATTACHMENT_BYTES
        );
        assert_eq!(
            archive.max_run_download_bytes,
            DEFAULT_ARCHIVE_MAX_RUN_DOWNLOAD_BYTES
        );
        assert!(archive.max_run_download_bytes >= archive.max_attachment_bytes);
        assert_eq!(deserialized, archive);
    }

    #[test]
    fn test_archive_attachment_limits_must_be_nonzero_and_ordered() {
        let channels = [ChannelConfig {
            id: "42".into(),
            ..ChannelConfig::default()
        }];
        let mut archive = ArchiveConfig {
            enabled: true,
            channel_id: "42".into(),
            guild_id: "7".into(),
            corpus_id: "safe".into(),
            data_dir: Utf8PathBuf::from("/tmp/gaie-fixture"),
            ..ArchiveConfig::default()
        };

        archive.max_attachment_bytes = 0;
        assert_eq!(
            archive.validate(&channels).unwrap_err(),
            "archive.max_attachment_bytes must be nonzero"
        );

        archive.max_attachment_bytes = 10;
        archive.max_run_download_bytes = 0;
        assert_eq!(
            archive.validate(&channels).unwrap_err(),
            "archive.max_run_download_bytes must be nonzero"
        );

        archive.max_run_download_bytes = 9;
        assert_eq!(
            archive.validate(&channels).unwrap_err(),
            "archive.max_run_download_bytes must be at least archive.max_attachment_bytes"
        );

        archive.max_run_download_bytes = 10;
        assert!(archive.validate(&channels).is_ok());
    }

    #[test]
    fn test_missing_config_generates_and_returns_defaults() {
        let _cache = config_cache_guard();
        let (_dir, state_dir) = temp_state_dir();
        let config_path = state_dir.join("config.toml");

        let cfg = reload_config(&state_dir).0;
        assert_eq!(cfg.access.dm_policy, DmPolicy::Queue);
        assert!(cfg.access.allow_from.is_empty());
        assert_eq!(cfg.delivery.text_chunk_limit, 2000);

        // Should have generated a default config file.
        assert!(
            config_path.as_std_path().exists(),
            "default config.toml should have been generated"
        );
        let contents = fs::read_to_string(config_path.as_std_path()).unwrap();
        assert!(contents.contains("dm_policy"));
    }

    #[test]
    fn test_corrupt_config_keeps_file_and_falls_back() {
        let _cache = config_cache_guard();
        let (_dir, state_dir) = temp_state_dir();
        let config_path = state_dir.join("config.toml");

        // Load a valid config with dm_policy = "drop" to prime the cache.
        fs::write(
            config_path.as_std_path(),
            b"[access]\ndm_policy = \"drop\"\n",
        )
        .unwrap();
        let before = reload_config(&state_dir).0;
        assert_eq!(before.access.dm_policy, DmPolicy::Drop);

        // Now write a corrupt config — should fall back and report an error.
        fs::write(config_path.as_std_path(), b"not valid toml {{{{").unwrap();
        let (after, error) = reload_config(&state_dir);

        // Must report a parse error.
        assert!(
            error.is_some(),
            "corrupt config must produce an error message"
        );
        assert!(
            error.unwrap().contains("config parse error"),
            "error should describe parse failure"
        );

        // The returned config must be usable (either last-valid or defaults).
        // NOTE: We can't assert the exact fallback because LAST_VALID_CONFIG
        // is process-global and may be overwritten by parallel tests. What
        // matters is that we get *some* valid config and the file is left
        // intact for the user to fix.
        let _ = after;

        assert!(
            config_path.as_std_path().exists(),
            "config.toml should still exist (not deleted or overwritten)"
        );
        let contents = fs::read_to_string(config_path.as_std_path()).unwrap();
        assert_eq!(
            contents, "not valid toml {{{{",
            "corrupt file must be left untouched"
        );
    }

    #[test]
    fn test_valid_config_parses() {
        let _cache = config_cache_guard();
        let (_dir, state_dir) = temp_state_dir();
        let config_path = state_dir.join("config.toml");
        let toml = r#"
token = "my-token"

[access]
dm_policy = "drop"
allow_from = ["111", "222"]
admins = ["111"]

[[channels]]
id = "999"
require_mention = false
allow_from = []

[delivery]
text_chunk_limit = 1000
chunk_mode = "length"
ack_reaction = "✅"
reply_to_mode = "all"

[access_requests]
expiry_seconds = 3600
max_pending = 10
notify_cooldown_seconds = 30

[voice]
enabled = true
"#;
        fs::write(config_path.as_std_path(), toml.as_bytes()).unwrap();
        let cfg = reload_config(&state_dir).0;

        assert_eq!(cfg.token.as_deref(), Some("my-token"));
        assert_eq!(cfg.access.dm_policy, DmPolicy::Drop);
        assert_eq!(cfg.access.allow_from, ["111", "222"]);
        assert_eq!(cfg.access.admins, ["111"]);
        assert_eq!(cfg.channels.len(), 1);
        assert_eq!(cfg.channels[0].id, "999");
        assert!(!cfg.channels[0].require_mention);
        assert_eq!(cfg.delivery.text_chunk_limit, 1000);
        assert_eq!(cfg.delivery.chunk_mode, ChunkMode::Length);
        assert_eq!(cfg.delivery.ack_reaction, "✅");
        assert_eq!(cfg.delivery.reply_to_mode, ReplyToMode::All);
        assert_eq!(cfg.access_requests.expiry_seconds, 3600);
        assert_eq!(cfg.access_requests.max_pending, 10);
        assert_eq!(cfg.access_requests.notify_cooldown_seconds, 30);
        assert!(cfg.voice.enabled);
    }

    #[test]
    fn test_env_var_overrides_token() {
        let cfg = Config {
            token: Some("config-token".to_string()),
            ..Default::default()
        };
        // With env var set.
        // SAFETY: test-only; no concurrent threads modify this env var.
        unsafe { std::env::set_var("DISCORD_BOT_TOKEN", "env-token") };
        let resolved = resolve_token(&cfg);
        // SAFETY: same.
        unsafe { std::env::remove_var("DISCORD_BOT_TOKEN") };
        assert_eq!(resolved.as_deref(), Some("env-token"));

        // Without env var.
        let resolved = resolve_token(&cfg);
        assert_eq!(resolved.as_deref(), Some("config-token"));
    }

    #[test]
    fn test_empty_allow_from_is_valid() {
        let _cache = config_cache_guard();
        let (_dir, state_dir) = temp_state_dir();
        let config_path = state_dir.join("config.toml");
        fs::write(config_path.as_std_path(), b"[access]\nallow_from = []\n").unwrap();
        let cfg = reload_config(&state_dir).0;
        assert!(cfg.access.allow_from.is_empty());
    }

    // ── LoadedConfig method tests ─────────────────────────────────────────────

    fn make_loaded() -> LoadedConfig {
        let raw = Config {
            access: AccessConfig {
                dm_policy: DmPolicy::Queue,
                allow_from: vec!["111".to_string(), "222".to_string()],
                admins: vec!["111".to_string()],
                admin_only_mutations: false,
            },
            channels: vec![ChannelConfig {
                id: "500".to_string(),
                require_mention: true,
                allow_from: vec!["333".to_string()],
                ..Default::default()
            }],
            ..Default::default()
        };
        LoadedConfig::from_raw(raw)
    }

    // TC-62 variant: is_allowed uses O(1) HashSet — allowed user returns true.
    #[test]
    fn test_loaded_config_is_allowed_true_for_allowlisted_user() {
        let cfg = make_loaded();
        assert!(
            cfg.is_allowed(111),
            "user 111 is in allow_from, must return true"
        );
        assert!(
            cfg.is_allowed(222),
            "user 222 is in allow_from, must return true"
        );
    }

    // TC-62 variant: is_allowed returns false for unknown user.
    #[test]
    fn test_loaded_config_is_allowed_false_for_unknown_user() {
        let cfg = make_loaded();
        assert!(!cfg.is_allowed(9999), "unknown user must not be allowed");
    }

    // is_admin returns true for configured admin.
    #[test]
    fn test_loaded_config_is_admin_true() {
        let cfg = make_loaded();
        assert!(cfg.is_admin(111), "user 111 is admin, must return true");
    }

    // is_admin returns false for non-admin allowlisted user.
    #[test]
    fn test_loaded_config_is_admin_false_for_non_admin() {
        let cfg = make_loaded();
        // user 222 is in allow_from but NOT in admins.
        assert!(
            !cfg.is_admin(222),
            "allowlisted non-admin must not be admin"
        );
        assert!(!cfg.is_admin(9999), "unknown user must not be admin");
    }

    // channel_policy returns correct policy for a known channel.
    #[test]
    fn test_loaded_config_channel_policy_known_channel() {
        let cfg = make_loaded();
        let policy = cfg
            .channel_policy(500)
            .expect("channel 500 must have a policy");
        assert!(policy.require_mention, "channel 500 must require_mention");
        assert!(
            policy.allow_from.contains(&333),
            "channel 500 allow_from must contain user 333"
        );
    }

    // channel_policy returns None for unknown channel.
    #[test]
    fn test_loaded_config_channel_policy_unknown_channel() {
        let cfg = make_loaded();
        assert!(
            cfg.channel_policy(9999).is_none(),
            "unknown channel must return None from channel_policy"
        );
    }

    // Invalid (non-numeric) ID strings in config are silently skipped.
    #[test]
    fn test_loaded_config_invalid_ids_skipped() {
        let raw = Config {
            access: AccessConfig {
                dm_policy: DmPolicy::Queue,
                allow_from: vec![
                    "111".to_string(),
                    "not-a-number".to_string(),
                    "".to_string(),
                    "999".to_string(),
                ],
                admins: vec!["bad-admin-id".to_string()],
                admin_only_mutations: false,
            },
            channels: vec![ChannelConfig {
                id: "not-numeric".to_string(),
                require_mention: false,
                allow_from: vec![],
                ..Default::default()
            }],
            ..Default::default()
        };
        let cfg = LoadedConfig::from_raw(raw);

        // Only valid IDs must be present.
        assert!(cfg.is_allowed(111), "valid ID 111 must be allowed");
        assert!(cfg.is_allowed(999), "valid ID 999 must be allowed");
        assert!(
            !cfg.is_allowed(0),
            "ID 0 must not be in allow_from (was not listed)"
        );

        // Invalid admin ID is skipped — admins set must be empty.
        assert!(
            !cfg.is_admin(111),
            "111 is not an admin (admin list had no valid IDs)"
        );

        // Channel with non-numeric ID is silently skipped.
        assert!(
            cfg.channel_policies.is_empty(),
            "channel with non-numeric ID must be silently dropped"
        );
    }

    // ── delivery_delay_ms tests ────────────────────────────────────────────

    #[test]
    fn test_delivery_delay_ms_from_channel_config() {
        let raw = Config {
            channels: vec![ChannelConfig {
                id: "700".to_string(),
                delivery_delay_ms: Some(500),
                ..Default::default()
            }],
            ..Default::default()
        };
        let cfg = LoadedConfig::from_raw(raw);
        assert_eq!(cfg.delivery_delay_ms(700), 500);
        assert_eq!(cfg.delivery_delay_ms(999), 0, "unknown channel returns 0");
    }

    #[test]
    fn test_delivery_delay_ms_defaults_to_zero() {
        let raw = Config {
            channels: vec![ChannelConfig {
                id: "800".to_string(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let cfg = LoadedConfig::from_raw(raw);
        assert_eq!(
            cfg.delivery_delay_ms(800),
            0,
            "default delivery_delay_ms should be 0"
        );
    }

    // ── global delivery_delay_ms tests ────────────────────────────────────

    #[test]
    fn test_global_delivery_delay_applies_when_channel_has_no_override() {
        let raw = Config {
            delivery: DeliveryConfig {
                delivery_delay_ms: 850,
                ..Default::default()
            },
            channels: vec![ChannelConfig {
                id: "100".to_string(),
                // No per-channel delivery_delay_ms (None) — inherits global.
                ..Default::default()
            }],
            ..Default::default()
        };
        let cfg = LoadedConfig::from_raw(raw);
        assert_eq!(
            cfg.delivery_delay_ms(100),
            850,
            "configured channel without override should inherit global"
        );
    }

    #[test]
    fn test_per_channel_override_takes_precedence_over_global() {
        let raw = Config {
            delivery: DeliveryConfig {
                delivery_delay_ms: 850,
                ..Default::default()
            },
            channels: vec![ChannelConfig {
                id: "100".to_string(),
                delivery_delay_ms: Some(200),
                ..Default::default()
            }],
            ..Default::default()
        };
        let cfg = LoadedConfig::from_raw(raw);
        assert_eq!(
            cfg.delivery_delay_ms(100),
            200,
            "per-channel override should take precedence over global"
        );
    }

    #[test]
    fn test_per_channel_explicit_zero_overrides_nonzero_global() {
        let raw = Config {
            delivery: DeliveryConfig {
                delivery_delay_ms: 850,
                ..Default::default()
            },
            channels: vec![ChannelConfig {
                id: "100".to_string(),
                delivery_delay_ms: Some(0),
                ..Default::default()
            }],
            ..Default::default()
        };
        let cfg = LoadedConfig::from_raw(raw);
        assert_eq!(
            cfg.delivery_delay_ms(100),
            0,
            "explicit per-channel 0 should override non-zero global"
        );
    }

    #[test]
    fn test_unconfigured_channel_inherits_global_default() {
        let raw = Config {
            delivery: DeliveryConfig {
                delivery_delay_ms: 750,
                ..Default::default()
            },
            // No channels configured.
            ..Default::default()
        };
        let cfg = LoadedConfig::from_raw(raw);
        assert_eq!(
            cfg.delivery_delay_ms(999),
            750,
            "unconfigured channel should inherit global default"
        );
    }

    #[test]
    fn test_global_delivery_delay_ms_toml_parses() {
        let _cache = config_cache_guard();
        let (_dir, state_dir) = temp_state_dir();
        let config_path = state_dir.join("config.toml");
        let toml = r#"
[delivery]
delivery_delay_ms = 850

[[channels]]
id = "100"
require_mention = false

[[channels]]
id = "200"
require_mention = false
delivery_delay_ms = 300
"#;
        fs::write(config_path.as_std_path(), toml.as_bytes()).unwrap();
        let cfg = reload_config(&state_dir).0;

        assert_eq!(cfg.delivery.delivery_delay_ms, 850);
        assert_eq!(
            cfg.delivery_delay_ms(100),
            850,
            "channel without override inherits global"
        );
        assert_eq!(
            cfg.delivery_delay_ms(200),
            300,
            "channel with override uses its own value"
        );
        assert_eq!(
            cfg.delivery_delay_ms(999),
            850,
            "unconfigured channel inherits global"
        );
    }

    // ── rate_limit config tests ──────────────────────────────────────────────

    #[test]
    fn test_rate_limit_toml_parses() {
        let _cache = config_cache_guard();
        let (_dir, state_dir) = temp_state_dir();
        let config_path = state_dir.join("config.toml");
        let toml = r#"
[rate_limit]
enabled = true
max_tokens = 10
window_secs = 60
cooldown_secs = 30
overflow = "buffer"
"#;
        fs::write(config_path.as_std_path(), toml.as_bytes()).unwrap();
        let cfg = reload_config(&state_dir).0;
        assert!(cfg.rate_limit.enabled);
        assert_eq!(cfg.rate_limit.max_tokens, Some(10));
        assert_eq!(cfg.rate_limit.window_secs, Some(60));
        assert_eq!(cfg.rate_limit.cooldown_secs, Some(30));
        assert_eq!(cfg.rate_limit.overflow.as_deref(), Some("buffer"));
    }

    #[test]
    fn test_rate_limit_toml_defaults() {
        let cfg = Config::default();
        assert!(!cfg.rate_limit.enabled);
        assert_eq!(cfg.rate_limit.max_tokens, None);
    }

    #[test]
    fn test_rate_limit_toml_into_runtime() {
        use crate::rate_limiter::OverflowPolicy;
        let toml_cfg = RateLimitTomlConfig {
            enabled: true,
            max_tokens: Some(5),
            window_secs: Some(120),
            cooldown_secs: Some(60),
            overflow: Some("buffer".to_string()),
        };
        let rt = toml_cfg.into_runtime();
        assert!(rt.enabled);
        assert_eq!(rt.default.max_tokens, 5);
        assert_eq!(rt.default.window, std::time::Duration::from_secs(120));
        assert_eq!(rt.default.cooldown, std::time::Duration::from_secs(60));
        assert_eq!(rt.default.overflow, OverflowPolicy::Buffer);
    }

    #[test]
    fn test_rate_limit_toml_into_runtime_defaults() {
        use crate::rate_limiter::OverflowPolicy;
        let toml_cfg = RateLimitTomlConfig::default();
        let rt = toml_cfg.into_runtime();
        assert!(!rt.enabled);
        assert_eq!(rt.default.max_tokens, 20);
        assert!(matches!(
            rt.default.overflow,
            OverflowPolicy::Drop { notify: true }
        ));
    }

    #[test]
    fn test_delivery_delay_ms_toml_parses() {
        let _cache = config_cache_guard();
        let (_dir, state_dir) = temp_state_dir();
        let config_path = state_dir.join("config.toml");
        let toml = r#"
[[channels]]
id = "123"
require_mention = false
delivery_delay_ms = 750
"#;
        fs::write(config_path.as_std_path(), toml.as_bytes()).unwrap();
        let cfg = reload_config(&state_dir).0;
        assert_eq!(cfg.channels[0].delivery_delay_ms, Some(750));
        assert_eq!(cfg.delivery_delay_ms(123), 750);
    }

    // TC-62: Empty allow_from + admins → functional (everything gated).
    #[test]
    fn test_empty_allow_from_and_admins_functional() {
        let raw = Config {
            access: AccessConfig {
                dm_policy: DmPolicy::Queue,
                allow_from: vec![],
                admins: vec![],
                admin_only_mutations: false,
            },
            ..Default::default()
        };
        let cfg = LoadedConfig::from_raw(raw);
        // No user is allowed.
        assert!(
            !cfg.is_allowed(42),
            "no user may be allowed when allow_from is empty"
        );
        // No user is admin.
        assert!(
            !cfg.is_admin(42),
            "no user may be admin when admins is empty"
        );
    }

    // ── contradictionary sidecar tests ──────────────────────────────────────

    #[test]
    fn test_contradictionary_sidecar_loads_toml() {
        let _cache = config_cache_guard();
        let (_dir, state_dir) = temp_state_dir();
        let config_path = state_dir.join("config.toml");
        let sidecar_path = state_dir.join("contradictionary.toml");

        let toml = r#"
[contradictionary]
enabled = true
"#;
        fs::write(config_path.as_std_path(), toml.as_bytes()).unwrap();
        fs::write(
            sidecar_path.as_std_path(),
            r#"
[[entry]]
pattern = "load-bearing"
action = "warn"
reason = "substrate tell"

[[entry]]
pattern = "synergy"
action = "block"
"#,
        )
        .unwrap();

        let cfg = reload_config(&state_dir).0;
        assert!(
            cfg.contradictionary.is_some(),
            "contradictionary must be built"
        );
        let c = cfg.contradictionary.as_ref().unwrap();
        let hits = c.check("load-bearing synergy");
        assert_eq!(hits.len(), 2);
        assert!(c.has_block(&hits));
    }

    #[test]
    fn test_contradictionary_sidecar_missing_is_ok() {
        let _cache = config_cache_guard();
        let (_dir, state_dir) = temp_state_dir();
        let config_path = state_dir.join("config.toml");

        let toml = r#"
[contradictionary]
enabled = true
"#;
        fs::write(config_path.as_std_path(), toml.as_bytes()).unwrap();
        // No sidecar file — should not error.
        let (cfg, error) = reload_config(&state_dir);
        assert!(error.is_none(), "missing sidecar must not produce an error");
        assert!(
            cfg.contradictionary.is_none(),
            "no entries means no concordance"
        );
    }

    #[test]
    fn test_contradictionary_empty_sidecar_is_ok() {
        let _cache = config_cache_guard();
        let (_dir, state_dir) = temp_state_dir();
        let config_path = state_dir.join("config.toml");
        let sidecar_path = state_dir.join("contradictionary.toml");

        let toml = r#"
[contradictionary]
enabled = true
"#;
        fs::write(config_path.as_std_path(), toml.as_bytes()).unwrap();
        // Present but empty — distinct from missing, and still a clean no-op.
        fs::write(sidecar_path.as_std_path(), b"").unwrap();

        let (cfg, error) = reload_config(&state_dir);
        assert!(error.is_none(), "empty sidecar must not produce an error");
        assert!(
            cfg.contradictionary.is_none(),
            "no entries means no concordance"
        );
    }

    #[test]
    fn test_corrupt_sidecar_preserves_previously_loaded_entries() {
        let _cache = config_cache_guard();
        let (_dir, state_dir) = temp_state_dir();
        let config_path = state_dir.join("config.toml");
        let sidecar_path = state_dir.join("contradictionary.toml");

        let toml = r#"
[contradictionary]
enabled = true
"#;
        fs::write(config_path.as_std_path(), toml.as_bytes()).unwrap();

        // Prime the cache with a working gate.
        fs::write(
            sidecar_path.as_std_path(),
            br#"
[[entry]]
pattern = "canary-phrase"
action = "block"
"#,
        )
        .unwrap();
        let primed = reload_config(&state_dir).0;
        let gate = primed
            .contradictionary
            .as_ref()
            .expect("primed contradictionary must be built");
        assert_eq!(gate.check("canary-phrase").len(), 1);

        // Corrupt the sidecar. One bad entry fails the whole file's parse.
        fs::write(
            sidecar_path.as_std_path(),
            b"[[entry]]\npattern = \"unclosed",
        )
        .unwrap();
        let (after, error) = reload_config(&state_dir);

        assert!(
            error.is_some(),
            "corrupt sidecar must report an error to the caller"
        );

        // The gate must NOT silently disappear. Failing to parse the wordlist
        // is not a licence to run with no wordlist at all.
        let gate = after.contradictionary.as_ref().expect(
            "corrupt sidecar must not leave the contradictionary absent — \
             a gate whose job is failing closed must not fail open",
        );
        assert_eq!(
            gate.check("canary-phrase").len(),
            1,
            "previously loaded entries must survive a corrupt reload"
        );

        // And the cache itself must still hold the working gate, so readers
        // that never see the error still get a live gate.
        let cached = load_config(&state_dir);
        let cached_gate = cached
            .contradictionary
            .as_ref()
            .expect("cache must retain the last valid contradictionary");
        assert_eq!(cached_gate.check("canary-phrase").len(), 1);
    }

    #[test]
    fn test_contradictionary_inline_and_sidecar_merged() {
        let _cache = config_cache_guard();
        let (_dir, state_dir) = temp_state_dir();
        let config_path = state_dir.join("config.toml");
        let sidecar_path = state_dir.join("contradictionary.toml");

        let toml = r#"
[contradictionary]
enabled = true

[[contradictionary.entries]]
pattern = "inline-word"
action = "log"
"#;
        fs::write(config_path.as_std_path(), toml.as_bytes()).unwrap();
        fs::write(
            sidecar_path.as_std_path(),
            r#"
[[entry]]
pattern = "sidecar-word"
action = "warn"
"#,
        )
        .unwrap();

        let cfg = reload_config(&state_dir).0;
        let c = cfg
            .contradictionary
            .as_ref()
            .expect("contradictionary must be built");
        // Both inline and sidecar entries should be present.
        let hits_inline = c.check("inline-word");
        let hits_sidecar = c.check("sidecar-word");
        assert_eq!(hits_inline.len(), 1);
        assert_eq!(hits_sidecar.len(), 1);
    }

    #[test]
    fn test_contradictionary_custom_sidecar_path() {
        let _cache = config_cache_guard();
        let (_dir, state_dir) = temp_state_dir();
        let config_path = state_dir.join("config.toml");
        let custom_dir = state_dir.join("custom");
        fs::create_dir_all(custom_dir.as_std_path()).unwrap();
        let custom_sidecar = custom_dir.join("words.toml");

        let toml = r#"
[contradictionary]
enabled = true
sidecar_path = "custom/words.toml"
"#;
        fs::write(config_path.as_std_path(), toml.as_bytes()).unwrap();
        fs::write(
            custom_sidecar.as_std_path(),
            r#"
[[entry]]
pattern = "custom-word"
action = "celebrate"
"#,
        )
        .unwrap();

        let cfg = reload_config(&state_dir).0;
        let c = cfg
            .contradictionary
            .as_ref()
            .expect("contradictionary must be built");
        let hits = c.check("custom-word");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].action, crate::contradictionary::Action::Celebrate);
    }

    #[test]
    fn pre_send_author_id_deserializes_string_and_integer_snowflakes() {
        for value in ["\"42\"", "42"] {
            let raw: Config = toml::from_str(&format!("[pre_send]\nauthor_id = {value}"))
                .expect("valid author id config");
            assert_eq!(raw.pre_send.author_id, Some(UserId::new(42)));
            assert_eq!(
                LoadedConfig::from_raw(raw).pre_send_author_id,
                Some(UserId::new(42))
            );
        }
    }

    #[test]
    fn pre_send_author_id_rejects_zero_and_nonnumeric_at_serde_boundary() {
        for invalid in ["0", "not-a-snowflake"] {
            let source = format!("[pre_send]\nauthor_id = \"{invalid}\"");
            assert!(toml::from_str::<Config>(&source).is_err());
        }
    }

    #[test]
    fn pre_send_construct_id_is_validated_once_when_config_is_loaded() {
        let mut raw = Config::default();
        raw.pre_send.construct_id = "syne".to_owned();
        assert_eq!(
            LoadedConfig::from_raw(raw).pre_send_construct_id.as_str(),
            "syne"
        );
    }

    #[test]
    fn invalid_pre_send_construct_id_disables_pipeline() {
        let mut raw = Config::default();
        raw.pre_send.construct_id = "Not Valid".to_owned();
        let loaded = LoadedConfig::from_raw(raw);
        assert!(!loaded.pre_send.enabled);
    }
}
