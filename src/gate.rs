use crate::config::{DmPolicy, LoadedConfig};
use camino::Utf8Path;
use regex::RegexSet;
use std::fs;

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
    ///
    /// When `guild_id` is `Some`, checks the guild mute store first — a muted
    /// guild drops all push delivery regardless of channel or sender policy.
    ///
    /// This is the direct-human gate. Verified app actions use the separate
    /// linear `VerifiedActionGate`; accepting an origin/proof surrogate here
    /// would recreate the bypass that boundary is designed to prevent.
    pub(crate) fn check_guild(
        config: &LoadedConfig,
        channel_id: u64,
        sender_id: u64,
        is_mentioned: bool,
        guild_id: Option<u64>,
    ) -> GateDecision {
        if let Some(gid) = guild_id
            && let Some(store) = crate::mute_store::global()
            && store.is_guild_muted(gid)
        {
            tracing::debug!(
                guild_id = gid,
                channel_id,
                "guild message dropped: guild muted"
            );
            return GateDecision::Drop;
        }

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

        // When any identity filter is active, a direct user must be explicitly
        // present in `allow_from`. Provider-specific selectors cannot authorize
        // a direct transport.
        if policy.has_identity_filter() && !policy.allow_from.contains(&sender_id) {
            tracing::debug!(
                channel_id,
                sender_id,
                "guild message dropped: sender not in identity lists (direct transport)"
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

/// Typed evidence that a guild message was directed to the construct.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MentionKind {
    /// Discord carried an explicit mention of the construct's bot user.
    DirectMention,
    /// The message replied to one authored by the construct's bot user.
    ReplyToConstruct,
    /// A configured mention pattern matched the message content.
    ConfiguredPattern,
}

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
        Self::classify(
            bot_user_id,
            message_mentions,
            message_content,
            referenced_author_id,
            mention_patterns,
        )
        .is_some()
    }

    /// Returns the first typed reason that makes a message directed.
    pub fn classify(
        bot_user_id: u64,
        message_mentions: &[u64],
        message_content: &str,
        referenced_author_id: Option<u64>,
        mention_patterns: Option<&RegexSet>,
    ) -> Option<MentionKind> {
        // 1. Direct @mention.
        if message_mentions.contains(&bot_user_id) {
            return Some(MentionKind::DirectMention);
        }

        // 2. Reply to a message the bot sent.
        if let Some(author_id) = referenced_author_id
            && author_id == bot_user_id
        {
            return Some(MentionKind::ReplyToConstruct);
        }

        // 3. Regex pattern match (pre-compiled set).
        if let Some(set) = mention_patterns
            && set.is_match(message_content)
        {
            return Some(MentionKind::ConfiguredPattern);
        }

        None
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
    use crate::{
        config::{AccessConfig, ChannelConfig, Config, DmPolicy, MentionConfig},
        principal_policy::{Admission, Attention, LegacyGuildPolicyInput, LegacyPolicyTranslation},
    };
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
                ..Default::default()
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

    #[test]
    fn legacy_dm_translation_matches_the_runtime_gate() {
        for dm_policy in [DmPolicy::Queue, DmPolicy::Drop, DmPolicy::Disabled] {
            for sender_is_allowed in [false, true] {
                let mut raw = base_config();
                raw.access.dm_policy = dm_policy;
                raw.access.allow_from = sender_is_allowed
                    .then(|| "999".to_owned())
                    .into_iter()
                    .collect();
                let config = loaded(raw);
                let legacy = InboundGate::check_dm(&config, 999);
                let typed = LegacyPolicyTranslation::dm(dm_policy, sender_is_allowed);
                let translated = match typed.admission {
                    Admission::Admit => GateDecision::Deliver,
                    Admission::Request => GateDecision::Queue,
                    Admission::Reject => GateDecision::Drop,
                };

                assert_eq!(translated, legacy);
                assert_eq!(typed.attention, Attention::Normal);
            }
        }
    }

    // ── Guild gate tests ──────────────────────────────────────────────────────

    #[test]
    fn test_guild_opted_in_with_mention_delivers() {
        let config = loaded(base_config());
        assert_eq!(
            InboundGate::check_guild(&config, 500, 999, true, None),
            GateDecision::Deliver
        );
    }

    #[test]
    fn test_guild_not_opted_drops() {
        let config = loaded(base_config());
        assert_eq!(
            InboundGate::check_guild(&config, 9999, 100, true, None),
            GateDecision::Drop
        );
    }

    #[test]
    fn test_guild_no_mention_in_require_mention_drops() {
        let config = loaded(base_config());
        assert_eq!(
            InboundGate::check_guild(&config, 500, 100, false, None),
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
            InboundGate::check_guild(&config, 500, 200, false, None),
            GateDecision::Deliver
        );
        // Non-allowed user drops.
        assert_eq!(
            InboundGate::check_guild(&config, 500, 999, false, None),
            GateDecision::Drop
        );
    }

    // Direct transport cannot satisfy provider-specific identity selectors.
    #[test]
    fn test_pk_only_filter_rejects_direct_user() {
        let mut raw = base_config();
        raw.channels[0].require_mention = false;
        raw.channels[0].allow_pk_systems = vec!["a0000001-0000-0000-0000-000000000001".to_string()];
        let config = loaded(raw);

        assert_eq!(
            InboundGate::check_guild(&config, 500, 999, false, None),
            GateDecision::Drop
        );
    }

    #[test]
    fn test_pk_filter_and_explicit_direct_user_admit() {
        let mut raw = base_config();
        raw.channels[0].require_mention = false;
        raw.channels[0].allow_from = vec!["999".to_string()];
        raw.channels[0].allow_pk_systems = vec!["a0000001-0000-0000-0000-000000000001".to_string()];
        let config = loaded(raw);

        assert_eq!(
            InboundGate::check_guild(&config, 500, 999, false, None),
            GateDecision::Deliver
        );
    }

    #[test]
    fn test_invalid_pk_only_filter_still_rejects_direct_user() {
        let mut raw = base_config();
        raw.channels[0].require_mention = false;
        raw.channels[0].allow_pk_systems = vec!["invalid".to_string()];
        let config = loaded(raw);

        assert_eq!(
            InboundGate::check_guild(&config, 500, 999, false, None),
            GateDecision::Drop
        );
    }

    #[test]
    fn legacy_guild_translation_matches_the_runtime_gate() {
        for channel_is_configured in [false, true] {
            for require_mention in [false, true] {
                for identity_filter_is_active in [false, true] {
                    for sender_is_allowed in [false, true] {
                        for mentioned in [false, true] {
                            let mut raw = base_config();
                            raw.channels.clear();
                            if channel_is_configured {
                                raw.channels.push(ChannelConfig {
                                    id: "500".to_owned(),
                                    require_mention,
                                    allow_from: if identity_filter_is_active && sender_is_allowed {
                                        vec!["999".to_owned()]
                                    } else if identity_filter_is_active {
                                        vec!["200".to_owned()]
                                    } else {
                                        Vec::new()
                                    },
                                    ..Default::default()
                                });
                            }
                            let config = loaded(raw);
                            let legacy =
                                InboundGate::check_guild(&config, 500, 999, mentioned, None);
                            let typed = LegacyPolicyTranslation::guild(LegacyGuildPolicyInput {
                                channel_is_configured,
                                require_mention,
                                identity_filter_is_active,
                                sender_is_allowed,
                            });
                            let attention_allows = match typed.attention {
                                Attention::Normal => true,
                                Attention::MentionOnly => mentioned,
                                Attention::Quiet => false,
                            };
                            let translated =
                                if typed.admission == Admission::Admit && attention_allows {
                                    GateDecision::Deliver
                                } else {
                                    GateDecision::Drop
                                };

                            assert_eq!(
                                translated, legacy,
                                "configured={channel_is_configured}, mention_required={require_mention}, identity_filter={identity_filter_is_active}, sender_allowed={sender_is_allowed}, mentioned={mentioned}"
                            );
                        }
                    }
                }
            }
        }
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
        assert_eq!(
            MentionDetector::classify(42, &[42], "hello", None, None),
            Some(MentionKind::DirectMention)
        );
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
        assert_eq!(
            MentionDetector::classify(42, &[], "what did you say?", Some(42), None),
            Some(MentionKind::ReplyToConstruct)
        );
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
        assert_eq!(
            MentionDetector::classify(
                42,
                &[],
                "hey Dione, how are you?",
                None,
                config.mention_patterns.as_ref(),
            ),
            Some(MentionKind::ConfiguredPattern)
        );
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

    // ── Direct channel policy precedence over thread map ───────────────────

    // Channel 500 is both directly configured AND in the thread map.
    // The direct channel_policy check short-circuits before consulting
    // the thread map — this test documents that intent.
    #[test]
    fn test_outbound_direct_channel_policy_takes_precedence_over_thread_map() {
        let config = loaded(base_config()); // channel 500 is in config
        let threads = thread_map(&[(500, Some(9999))]); // thread map says parent is 9999 (not configured)
        assert!(
            OutboundGate::check_channel_with_threads(&config, 500, &dm_ids(&[]), &threads),
            "directly configured channel must pass even when thread map maps it elsewhere"
        );
    }

    // ── check_channel delegates with empty thread map ────────────────────

    // check_channel (convenience wrapper) passes an empty thread map, so a
    // thread channel ID that would be allowed via thread_parents is rejected.
    #[test]
    fn test_outbound_check_channel_does_not_use_thread_map() {
        let config = loaded(base_config()); // channel 500 is in config
        // Thread 700 → parent 500 would pass check_channel_with_threads.
        assert!(
            OutboundGate::check_channel_with_threads(
                &config,
                700,
                &dm_ids(&[]),
                &thread_map(&[(700, Some(500))])
            ),
            "sanity: thread 700 with parent 500 should pass check_channel_with_threads"
        );
        // But check_channel (which passes empty map) should reject it.
        assert!(
            !OutboundGate::check_channel(&config, 700, &dm_ids(&[])),
            "check_channel must not consult thread parents — it passes an empty map"
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
