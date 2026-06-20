use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::Read as _;
use std::sync::Arc;

use arc_swap::ArcSwap;
use camino::{Utf8Path, Utf8PathBuf};
use regex::RegexSet;
use serde::Deserialize;
use thiserror::Error;

use crate::timestamp::Timestamp;

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
    pub fn from_raw(raw: Config) -> Self {
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
        Self {
            raw,
            allowed_ids,
            admin_ids,
            channel_policies,
            mention_patterns,
            tz,
            rate_limit_runtime,
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
    let config_path = state_dir.join("config.toml");
    let mut config_error = None;
    let raw = match try_load_config(&config_path) {
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

    #[test]
    fn test_missing_config_generates_and_returns_defaults() {
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
}
