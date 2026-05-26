use std::fs;

use camino::Utf8Path;
use regex::RegexSet;

use crate::config::{DmPolicy, LoadedConfig};

// ── Decision type ─────────────────────────────────────────────────────────────

/// The gate's verdict for an inbound message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateDecision {
    /// Forward the message to Claude via MCP.
    Deliver,
    /// Add to the access request queue and notify admin.
    Queue,
    /// Silently discard.
    Drop,
}

// ── Inbound gate ──────────────────────────────────────────────────────────────

/// Checks inbound messages against the access policy.
pub struct InboundGate;

impl InboundGate {
    /// Decides what to do with an inbound DM. O(1) allowlist check.
    pub fn check_dm(config: &LoadedConfig, sender_id: u64) -> GateDecision {
        if config.access.dm_policy == DmPolicy::Disabled {
            tracing::debug!(sender_id, "DM dropped: dm_policy=disabled");
            return GateDecision::Drop;
        }
        if config.is_allowed(sender_id) {
            return GateDecision::Deliver;
        }
        match config.access.dm_policy {
            DmPolicy::Queue => {
                tracing::debug!(sender_id, "DM queued: unknown sender");
                GateDecision::Queue
            }
            _ => {
                tracing::debug!(sender_id, "DM dropped: unknown sender, dm_policy=drop");
                GateDecision::Drop
            }
        }
    }

    /// Decides what to do with an inbound guild message. O(1) channel + sender lookup.
    pub fn check_guild(
        config: &LoadedConfig,
        channel_id: u64,
        sender_id: u64,
        is_mentioned: bool,
    ) -> GateDecision {
        let Some(policy) = config.channel_policy(channel_id) else {
            tracing::debug!(channel_id, "guild message dropped: channel not opted in");
            return GateDecision::Drop;
        };

        if policy.require_mention && !is_mentioned {
            tracing::debug!(
                channel_id,
                sender_id,
                "guild message dropped: mention required"
            );
            return GateDecision::Drop;
        }

        if !policy.allow_from.is_empty() && !policy.allow_from.contains(&sender_id) {
            tracing::debug!(
                channel_id,
                sender_id,
                "guild message dropped: sender not in channel allow_from"
            );
            return GateDecision::Drop;
        }

        GateDecision::Deliver
    }

    /// Decides what to do with a passive guild event (edit, delete) that lacks
    /// mention context. Enforces channel opt-in and `allow_from` but not
    /// `require_mention`.
    pub fn check_guild_passive(
        config: &LoadedConfig,
        channel_id: u64,
        sender_id: u64,
    ) -> GateDecision {
        let Some(policy) = config.channel_policy(channel_id) else {
            tracing::debug!(channel_id, "guild event dropped: channel not opted in");
            return GateDecision::Drop;
        };

        if !policy.allow_from.is_empty() && !policy.allow_from.contains(&sender_id) {
            tracing::debug!(
                channel_id,
                sender_id,
                "guild event dropped: sender not in channel allow_from"
            );
            return GateDecision::Drop;
        }

        GateDecision::Deliver
    }
}

// ── Outbound gate ─────────────────────────────────────────────────────────────

/// Checks outbound tool-call targets against the access policy.
pub struct OutboundGate;

impl OutboundGate {
    /// Returns `true` if the bot may send to `channel_id`.
    ///
    /// Convenience wrapper that passes an empty thread-parent cache.
    pub fn check_channel(
        config: &LoadedConfig,
        channel_id: u64,
        dm_channel_ids: &std::collections::HashSet<u64>,
    ) -> bool {
        Self::check_channel_with_threads(
            config,
            channel_id,
            dm_channel_ids,
            &std::collections::BTreeMap::new(),
        )
    }

    /// Like [`check_channel`](Self::check_channel) but also checks thread parent mappings.
    pub fn check_channel_with_threads(
        config: &LoadedConfig,
        channel_id: u64,
        dm_channel_ids: &std::collections::HashSet<u64>,
        thread_parents: &std::collections::BTreeMap<u64, Option<u64>>,
    ) -> bool {
        // O(1) check: is this an established DM channel?
        if dm_channel_ids.contains(&channel_id) {
            return true;
        }

        // O(log n) check: is this an opted-in guild channel?
        if config.channel_policy(channel_id).is_some() {
            return true;
        }

        // O(log n) check: is this a thread whose parent is opted in?
        if let Some(Some(parent_id)) = thread_parents.get(&channel_id) {
            return config.channel_policy(*parent_id).is_some();
        }

        false
    }

    /// Returns `true` if `path` may be sent as a file attachment.
    ///
    /// Files inside `state_dir` are rejected except those under `state_dir/inbox/`.
    /// Uses `fs::canonicalize` to resolve symlinks.
    pub fn check_file_send(path: &Utf8Path, state_dir: &Utf8Path) -> bool {
        // Canonicalize both paths to resolve symlinks.
        let canon_path = match fs::canonicalize(path.as_std_path()) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(path = %path, error = %e, "file send rejected: cannot canonicalize");
                return false;
            }
        };

        let canon_state_dir = match fs::canonicalize(state_dir.as_std_path()) {
            Ok(p) => p,
            Err(_) => {
                // State dir may not exist yet — if it can't be canonicalized,
                // treat its prefix check as non-applicable and allow.
                return true;
            }
        };

        // Allow inbox/ subdirectory.
        let inbox = canon_state_dir.join("inbox");
        if canon_path.starts_with(&inbox) {
            return true;
        }

        // Reject anything else inside the state directory.
        if canon_path.starts_with(&canon_state_dir) {
            tracing::warn!(
                path = %path,
                state_dir = %state_dir,
                "file send rejected: path is inside state dir"
            );
            return false;
        }

        // Path is outside the state directory — allowed.
        true
    }
}

/// Detects whether a message constitutes a mention of the bot.
pub struct MentionDetector;

impl MentionDetector {
    /// Returns `true` if the message counts as a bot mention.
    ///
    /// Conditions (any one is sufficient):
    /// 1. Discord `@mention` of the bot's user ID.
    /// 2. Reply to a message the bot sent (author ID matches bot).
    /// 3. Message content matches any compiled regex pattern.
    pub fn is_mentioned(
        bot_user_id: u64,
        message_mentions: &[u64],
        message_content: &str,
        referenced_author_id: Option<u64>,
        mention_patterns: Option<&RegexSet>,
    ) -> bool {
        // 1. Direct @mention.
        if message_mentions.contains(&bot_user_id) {
            return true;
        }

        // 2. Reply to a message the bot sent.
        if let Some(author_id) = referenced_author_id
            && author_id == bot_user_id
        {
            return true;
        }

        // 3. Regex pattern match (pre-compiled set).
        if let Some(set) = mention_patterns
            && set.is_match(message_content)
        {
            return true;
        }

        false
    }
}

// ── Utility ───────────────────────────────────────────────────────────────────

/// Sanitizes an attachment filename, stripping dangerous characters and
/// extracting only the final filename component to prevent path traversal.
///
/// Returns `"attachment"` if the result would be empty.
pub fn sanitize_filename(name: &str) -> String {
    // Strip characters dangerous in filenames or shell contexts.
    let filtered: String = name
        .chars()
        .filter(|&c| !matches!(c, '[' | ']' | '\r' | '\n' | ';'))
        .collect();

    // Extract only the filename component to prevent path traversal via `..` or `/`.
    let filename = Utf8Path::new(&filtered)
        .file_name()
        .unwrap_or("attachment")
        .to_string();

    if filename.is_empty() {
        "attachment".to_string()
    } else {
        filename
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AccessConfig, ChannelConfig, Config, DmPolicy, MentionConfig};
    use std::collections::HashSet;

    fn base_config() -> Config {
        Config {
            access: AccessConfig {
                dm_policy: DmPolicy::Queue,
                allow_from: vec!["100".to_string()],
                admins: vec!["100".to_string()],
                admin_only_mutations: false,
            },
            channels: vec![ChannelConfig {
                id: "500".to_string(),
                require_mention: true,
                allow_from: vec![],
            }],
            ..Default::default()
        }
    }

    fn loaded(config: Config) -> LoadedConfig {
        LoadedConfig::from_raw(config)
    }

    // ── DM gate tests ─────────────────────────────────────────────────────────

    #[test]
    fn test_dm_allowed_user_delivers() {
        let config = loaded(base_config());
        assert_eq!(InboundGate::check_dm(&config, 100), GateDecision::Deliver);
    }

    #[test]
    fn test_dm_unknown_user_queues() {
        let config = loaded(base_config());
        assert_eq!(InboundGate::check_dm(&config, 999), GateDecision::Queue);
    }

    #[test]
    fn test_dm_policy_disabled_drops() {
        let mut raw = base_config();
        raw.access.dm_policy = DmPolicy::Disabled;
        let config = loaded(raw);
        // Even allowed users are dropped when disabled.
        assert_eq!(InboundGate::check_dm(&config, 100), GateDecision::Drop);
        assert_eq!(InboundGate::check_dm(&config, 999), GateDecision::Drop);
    }

    #[test]
    fn test_dm_policy_drop_drops_unknown() {
        let mut raw = base_config();
        raw.access.dm_policy = DmPolicy::Drop;
        let config = loaded(raw);
        assert_eq!(InboundGate::check_dm(&config, 999), GateDecision::Drop);
        // Allowed user still delivers.
        assert_eq!(InboundGate::check_dm(&config, 100), GateDecision::Deliver);
    }

    // ── Guild gate tests ──────────────────────────────────────────────────────

    #[test]
    fn test_guild_opted_in_with_mention_delivers() {
        let config = loaded(base_config());
        assert_eq!(
            InboundGate::check_guild(&config, 500, 999, true),
            GateDecision::Deliver
        );
    }

    #[test]
    fn test_guild_not_opted_drops() {
        let config = loaded(base_config());
        assert_eq!(
            InboundGate::check_guild(&config, 9999, 100, true),
            GateDecision::Drop
        );
    }

    #[test]
    fn test_guild_no_mention_in_require_mention_drops() {
        let config = loaded(base_config());
        assert_eq!(
            InboundGate::check_guild(&config, 500, 100, false),
            GateDecision::Drop
        );
    }

    #[test]
    fn test_guild_per_channel_allow_from() {
        let mut raw = base_config();
        raw.channels[0].require_mention = false;
        raw.channels[0].allow_from = vec!["200".to_string()];
        let config = loaded(raw);

        // Allowed user delivers.
        assert_eq!(
            InboundGate::check_guild(&config, 500, 200, false),
            GateDecision::Deliver
        );
        // Non-allowed user drops.
        assert_eq!(
            InboundGate::check_guild(&config, 500, 999, false),
            GateDecision::Drop
        );
    }

    // ── Passive guild gate tests ────────────────────────────────────────────────

    #[test]
    fn test_guild_passive_delivers_in_require_mention_channel() {
        let config = loaded(base_config());
        assert_eq!(
            InboundGate::check_guild_passive(&config, 500, 999),
            GateDecision::Deliver
        );
    }

    #[test]
    fn test_guild_passive_delivers_without_require_mention() {
        let mut raw = base_config();
        raw.channels[0].require_mention = false;
        let config = loaded(raw);
        assert_eq!(
            InboundGate::check_guild_passive(&config, 500, 999),
            GateDecision::Deliver
        );
    }

    #[test]
    fn test_guild_passive_drops_unknown_channel() {
        let config = loaded(base_config());
        assert_eq!(
            InboundGate::check_guild_passive(&config, 9999, 100),
            GateDecision::Drop
        );
    }

    #[test]
    fn test_guild_passive_enforces_allow_from() {
        let mut raw = base_config();
        raw.channels[0].require_mention = true;
        raw.channels[0].allow_from = vec!["200".to_string()];
        let config = loaded(raw);

        assert_eq!(
            InboundGate::check_guild_passive(&config, 500, 200),
            GateDecision::Deliver
        );
        assert_eq!(
            InboundGate::check_guild_passive(&config, 500, 999),
            GateDecision::Drop
        );
    }

    // ── Outbound gate tests ───────────────────────────────────────────────────

    fn dm_ids(ids: &[u64]) -> HashSet<u64> {
        ids.iter().copied().collect()
    }

    #[test]
    fn test_outbound_allowed_dm() {
        let config = loaded(base_config());
        assert!(OutboundGate::check_channel(&config, 600, &dm_ids(&[600])));
    }

    #[test]
    fn test_outbound_rejected_channel() {
        let config = loaded(base_config());
        assert!(!OutboundGate::check_channel(&config, 9999, &dm_ids(&[])));
    }

    // ── File send tests ───────────────────────────────────────────────────────

    #[test]
    fn test_file_send_inbox_allowed() {
        let dir = tempfile::TempDir::new().unwrap();
        let state_dir = Utf8Path::from_path(dir.path()).unwrap();
        let inbox = dir.path().join("inbox");
        std::fs::create_dir_all(&inbox).unwrap();
        let file = inbox.join("attachment.png");
        std::fs::write(&file, b"fake image").unwrap();

        let file_path = Utf8Path::from_path(&file).unwrap();
        assert!(OutboundGate::check_file_send(file_path, state_dir));
    }

    #[test]
    fn test_file_send_state_dir_rejected() {
        let dir = tempfile::TempDir::new().unwrap();
        let state_dir = Utf8Path::from_path(dir.path()).unwrap();
        let config = dir.path().join("config.toml");
        std::fs::write(&config, b"[access]").unwrap();

        let file_path = Utf8Path::from_path(&config).unwrap();
        assert!(!OutboundGate::check_file_send(file_path, state_dir));
    }

    #[test]
    fn test_file_send_outside_allowed() {
        let state_dir_tmp = tempfile::TempDir::new().unwrap();
        let state_dir = Utf8Path::from_path(state_dir_tmp.path()).unwrap();

        let other_tmp = tempfile::TempDir::new().unwrap();
        let file = other_tmp.path().join("report.md");
        std::fs::write(&file, b"some content").unwrap();

        let file_path = Utf8Path::from_path(&file).unwrap();
        assert!(OutboundGate::check_file_send(file_path, state_dir));
    }

    // ── Mention detection tests ───────────────────────────────────────────────

    #[test]
    fn test_mention_at_mention() {
        assert!(MentionDetector::is_mentioned(
            42,    // bot_user_id
            &[42], // message_mentions includes bot
            "hello",
            None,
            None,
        ));
    }

    #[test]
    fn test_mention_reply_to_bot() {
        assert!(MentionDetector::is_mentioned(
            42,
            &[],
            "what did you say?",
            Some(42),
            None,
        ));
    }

    #[test]
    fn test_mention_regex_pattern() {
        let mut raw = base_config();
        raw.mentions = MentionConfig {
            patterns: vec!["(?i)\\bdione\\b".to_string()],
        };
        let config = loaded(raw);
        assert!(MentionDetector::is_mentioned(
            42,
            &[],
            "hey Dione, how are you?",
            None,
            config.mention_patterns.as_ref(),
        ));
        assert!(!MentionDetector::is_mentioned(
            42,
            &[],
            "hello there",
            None,
            config.mention_patterns.as_ref(),
        ));
    }

    #[test]
    fn test_mention_invalid_regex_no_crash() {
        let mut raw = base_config();
        raw.mentions = MentionConfig {
            patterns: vec!["[invalid".to_string()],
        };
        let config = loaded(raw);
        // Invalid patterns produce None — must not panic.
        let result =
            MentionDetector::is_mentioned(42, &[], "hello", None, config.mention_patterns.as_ref());
        assert!(!result);
    }

    // ── Outbound gate additional edge cases ──────────────────────────────────

    // TC-10: Outbound to a known DM channel (in dm_channel_map) → allowed.
    #[test]
    fn test_outbound_allows_dm_channel_in_map() {
        let config = loaded(base_config());
        assert!(
            OutboundGate::check_channel(&config, 800, &dm_ids(&[800])),
            "DM channel present in dm_channel_ids must be allowed"
        );
    }

    // TC-11: Outbound to a channel that is NOT in the DM set AND NOT in config → reject.
    #[test]
    fn test_outbound_rejects_channel_not_in_map_nor_config() {
        let config = loaded(base_config());
        assert!(
            !OutboundGate::check_channel(&config, 9999, &dm_ids(&[600])),
            "channel absent from both dm_channel_ids and config must be rejected"
        );
    }

    // TC-11a: Opted-in guild channel appears in config → outbound allowed.
    #[test]
    fn test_outbound_allows_opted_in_guild_channel() {
        let config = loaded(base_config()); // channel 500 is in config
        assert!(
            OutboundGate::check_channel(&config, 500, &dm_ids(&[])),
            "opted-in guild channel from config must be allowed for outbound"
        );
    }

    // TC-41: Symlink whose real path is inside state dir → rejected.
    #[cfg(unix)]
    #[test]
    fn test_file_send_symlink_into_state_dir_rejected() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::TempDir::new().unwrap();
        let state_dir = Utf8Path::from_path(dir.path()).unwrap();

        // Create a real file inside the state dir.
        let real_file = dir.path().join("secret.key");
        std::fs::write(&real_file, b"private key data").unwrap();

        // Create a symlink outside the state dir that points INTO the state dir.
        let link_dir = tempfile::TempDir::new().unwrap();
        let symlink_path = link_dir.path().join("innocent_link.txt");
        symlink(&real_file, &symlink_path).expect("symlink creation must succeed");

        let file_path = Utf8Path::from_path(&symlink_path).unwrap();
        assert!(
            !OutboundGate::check_file_send(file_path, state_dir),
            "symlink whose canonical path is inside state_dir must be rejected"
        );
    }

    // ── Thread-aware outbound gate tests ────────────────────────────────────

    fn thread_map(entries: &[(u64, Option<u64>)]) -> std::collections::BTreeMap<u64, Option<u64>> {
        entries.iter().copied().collect()
    }

    // Thread whose parent is allowed → allowed.
    #[test]
    fn test_outbound_thread_parent_allowed() {
        let config = loaded(base_config()); // channel 500 is in config
        let threads = thread_map(&[(700, Some(500))]);
        assert!(
            OutboundGate::check_channel_with_threads(&config, 700, &dm_ids(&[]), &threads),
            "thread whose parent channel is opted in must be allowed"
        );
    }

    // Thread whose parent is not allowed → rejected.
    #[test]
    fn test_outbound_thread_parent_not_allowed() {
        let config = loaded(base_config());
        let threads = thread_map(&[(700, Some(9999))]);
        assert!(
            !OutboundGate::check_channel_with_threads(&config, 700, &dm_ids(&[]), &threads),
            "thread whose parent channel is not opted in must be rejected"
        );
    }

    // Thread not in map → rejected.
    #[test]
    fn test_outbound_thread_not_in_map() {
        let config = loaded(base_config());
        let threads = thread_map(&[]);
        assert!(
            !OutboundGate::check_channel_with_threads(&config, 700, &dm_ids(&[]), &threads),
            "thread not in cache must be rejected"
        );
    }

    // Negatively cached channel (None) → rejected.
    #[test]
    fn test_outbound_thread_negatively_cached() {
        let config = loaded(base_config());
        let threads = thread_map(&[(700, None)]);
        assert!(
            !OutboundGate::check_channel_with_threads(&config, 700, &dm_ids(&[]), &threads),
            "negatively cached channel (not a thread) must be rejected"
        );
    }

    // ── Sanitize filename tests ───────────────────────────────────────────────

    #[test]
    fn test_sanitize_filename() {
        assert_eq!(sanitize_filename("normal.txt"), "normal.txt");
        assert_eq!(sanitize_filename("[evil].txt"), "evil.txt");
        assert_eq!(sanitize_filename("file\r\n.txt"), "file.txt");
        assert_eq!(sanitize_filename("file;rm -rf.txt"), "filerm -rf.txt");
        assert_eq!(sanitize_filename("a[b]c\rd;e\nf"), "abcdef");
        // Path traversal prevention.
        assert_eq!(sanitize_filename("../../etc/passwd"), "passwd");
        assert_eq!(sanitize_filename("/absolute/path/file.txt"), "file.txt");
        assert_eq!(sanitize_filename("sub/dir/file.txt"), "file.txt");
        // Empty result falls back to "attachment".
        assert_eq!(sanitize_filename(""), "attachment");
        assert_eq!(sanitize_filename("/"), "attachment");
    }
}
