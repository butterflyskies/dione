use std::collections::{HashMap, HashSet};
use std::fs::{self, File};

use camino::{Utf8Path, Utf8PathBuf};
use regex::RegexSet;
use serde::Deserialize;
use thiserror::Error;

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
    pub access: AccessConfig,
    pub channels: Vec<ChannelConfig>,
    pub mentions: MentionConfig,
    pub delivery: DeliveryConfig,
    pub access_requests: AccessRequestsConfig,
    pub voice: VoiceConfig,
}

/// Access control configuration.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct AccessConfig {
    pub dm_policy: DmPolicy,
    pub allow_from: Vec<String>,
    pub admins: Vec<String>,
}

impl Default for AccessConfig {
    fn default() -> Self {
        Self {
            dm_policy: DmPolicy::Queue,
            allow_from: Vec::new(),
            admins: Vec::new(),
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
}

impl Default for ChannelConfig {
    fn default() -> Self {
        Self {
            id: String::new(),
            require_mention: true,
            allow_from: Vec::new(),
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
}

impl Default for DeliveryConfig {
    fn default() -> Self {
        Self {
            ack_reaction: "👀".to_string(),
            reply_to_mode: ReplyToMode::First,
            text_chunk_limit: 2000,
            chunk_mode: ChunkMode::Paragraph,
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
}

/// Pre-parsed per-channel access policy.
#[derive(Debug, Clone)]
pub struct ChannelPolicy {
    pub require_mention: bool,
    pub allow_from: HashSet<u64>,
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
                    },
                ))
            })
            .collect();
        let mention_patterns = compile_mention_patterns(&raw);
        Self {
            raw,
            allowed_ids,
            admin_ids,
            channel_policies,
            mention_patterns,
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

/// Loads configuration from `{state_dir}/config.toml`.
///
/// On missing file, returns defaults. On parse error, renames the corrupt file
/// to `.corrupt-{timestamp}` and returns defaults.
///
/// Returns a `LoadedConfig` with pre-parsed ID sets and compiled regexes.
pub fn load_config(state_dir: &Utf8Path) -> LoadedConfig {
    let config_path = state_dir.join("config.toml");
    let raw = match try_load_config(&config_path) {
        Ok(cfg) => cfg,
        Err(ConfigError::NotFound { .. }) => {
            tracing::debug!(path = %config_path, "config file not found, using defaults");
            Config::default()
        }
        Err(ConfigError::Parse(e)) => {
            tracing::error!(path = %config_path, error = %e, "config parse error, renaming to .corrupt");
            rename_corrupt(&config_path);
            Config::default()
        }
        Err(ConfigError::Io(e)) => {
            tracing::warn!(path = %config_path, error = %e, "failed to read config, using defaults");
            Config::default()
        }
    };
    LoadedConfig::from_raw(raw)
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

fn try_load_config(config_path: &Utf8Path) -> Result<Config, ConfigError> {
    let std_path = config_path.as_std_path();
    if !std_path.exists() {
        return Err(ConfigError::NotFound {
            path: config_path.to_owned(),
        });
    }

    let file = File::open(std_path)?;
    // Best-effort lock — if another process is writing, proceed without locking.
    let _lock_guard = file.try_lock().ok();

    let contents = fs::read_to_string(std_path)?;
    let config: Config = toml::from_str(&contents)?;
    Ok(config)
}

fn rename_corrupt(config_path: &Utf8Path) {
    let ts = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");
    let corrupt_name = format!(".corrupt-{ts}");
    let corrupt_path = config_path
        .parent()
        .unwrap_or(Utf8Path::new("."))
        .join(corrupt_name);
    if let Err(e) = fs::rename(config_path.as_std_path(), corrupt_path.as_std_path()) {
        tracing::warn!(error = %e, "failed to rename corrupt config file");
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn temp_state_dir() -> (TempDir, Utf8PathBuf) {
        let dir = TempDir::new().unwrap();
        let path = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        (dir, path)
    }

    #[test]
    fn test_missing_config_returns_defaults() {
        let (_dir, state_dir) = temp_state_dir();
        let cfg = load_config(&state_dir);
        assert_eq!(cfg.access.dm_policy, DmPolicy::Queue);
        assert!(cfg.access.allow_from.is_empty());
        assert_eq!(cfg.delivery.text_chunk_limit, 2000);
    }

    #[test]
    fn test_corrupt_config_renames_and_returns_defaults() {
        let (_dir, state_dir) = temp_state_dir();
        let config_path = state_dir.join("config.toml");
        fs::write(config_path.as_std_path(), b"this is not valid toml {{{{").unwrap();

        let cfg = load_config(&state_dir);
        // Corrupt file should have been renamed away.
        assert!(
            !config_path.as_std_path().exists(),
            "config.toml should be renamed"
        );
        // Should have a .corrupt-* file.
        let entries: Vec<_> = fs::read_dir(state_dir.as_std_path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with(".corrupt-"))
            .collect();
        assert_eq!(entries.len(), 1, "expected one .corrupt-* file");
        // Should return defaults.
        assert_eq!(cfg.access.dm_policy, DmPolicy::Queue);
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
        let cfg = load_config(&state_dir);

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
        let cfg = load_config(&state_dir);
        assert!(cfg.access.allow_from.is_empty());
    }

    // ── LoadedConfig method tests ─────────────────────────────────────────────

    fn make_loaded() -> LoadedConfig {
        let raw = Config {
            access: AccessConfig {
                dm_policy: DmPolicy::Queue,
                allow_from: vec!["111".to_string(), "222".to_string()],
                admins: vec!["111".to_string()],
            },
            channels: vec![ChannelConfig {
                id: "500".to_string(),
                require_mention: true,
                allow_from: vec!["333".to_string()],
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
            },
            channels: vec![ChannelConfig {
                id: "not-numeric".to_string(), // invalid channel ID
                require_mention: false,
                allow_from: vec![],
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

    // TC-62: Empty allow_from + admins → functional (everything gated).
    #[test]
    fn test_empty_allow_from_and_admins_functional() {
        let raw = Config {
            access: AccessConfig {
                dm_policy: DmPolicy::Queue,
                allow_from: vec![],
                admins: vec![],
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
