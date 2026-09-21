//! Attention controls are independent of Discord access and provider eligibility.

use super::types::{
    Compatibility, DEFAULT_MODEL, FEATURE_VERSION, RUBRIC_VERSION, RecipientId, content_hash,
};
use serde::{Deserialize, Serialize};
use serenity::model::id::ChannelId;
use std::collections::BTreeMap;

/// Delivery policy; enabling attention never grants source access or provider export.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttentionMode {
    #[default]
    Off,
    Log,
    On,
}

/// Which health transitions may produce a cooldown-limited operator notice.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoticeMode {
    Off,
    Failures,
    #[default]
    FailuresAndRecovery,
}

/// Exact-channel overrides, with export permission independent of delivery mode.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RoomAttention {
    pub mode: Option<AttentionMode>,
    pub provider_eligible: bool,
    pub direct: bool,
}

/// Recipient-supplied context with an explicit export grant and expiry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttentionBrief {
    pub text: String,
    pub expires_at_ms: u64,
    pub provider_eligible: bool,
}

impl AttentionBrief {
    /// Hashes the text, expiry, and export grant to bind learned artifact compatibility.
    pub fn version(&self) -> String {
        content_hash(&format!(
            "{}:{}:{}",
            self.expires_at_ms, self.provider_eligible, self.text
        ))
    }

    /// Requires nonempty, unexpired context explicitly eligible for provider export.
    pub fn usable(&self, now_ms: u64) -> bool {
        self.provider_eligible && self.expires_at_ms > now_ms && !self.text.trim().is_empty()
    }
}

/// Operator-owned notice destination, independent of recipient attention controls.
/// Set in the top-level seat configuration, not through the attention configure operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecipientNoticeRoute {
    /// Recipient whose health information may be sent to this destination.
    pub recipient: RecipientId,
    /// Explicitly authorized audience for that recipient's notices.
    pub channel: ChannelId,
}

/// Seat-owned controls and resource limits, validated before publication or use.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AttentionConfig {
    /// One recipient per authenticated Dione seat. Records never pool recipients.
    pub recipient: RecipientId,
    pub mode: AttentionMode,
    pub emergency_off: bool,
    /// Exact room grants: parent-room permission never implicitly exports a thread.
    pub rooms: BTreeMap<ChannelId, RoomAttention>,
    pub brief: Option<AttentionBrief>,
    pub model: String,
    /// Environment variable NAME, never an inline credential value.
    pub api_key_env: String,
    pub request_timeout_ms: u64,
    pub outage_retry_ms: u64,
    pub max_segment_bytes: usize,
    pub max_antecedents: usize,
    pub max_in_flight: usize,
    pub retention_ms: u64,
    pub revalidate_ms: u64,
    pub max_records: usize,
    pub notices: NoticeMode,
    pub notice_cooldown_ms: u64,
    pub notice_channel: Option<ChannelId>,
}

impl Default for AttentionConfig {
    fn default() -> Self {
        Self {
            recipient: "default".into(),
            mode: AttentionMode::Off,
            emergency_off: false,
            rooms: BTreeMap::new(),
            brief: None,
            model: DEFAULT_MODEL.into(),
            api_key_env: "TYPESAFE_API_KEY".into(),
            request_timeout_ms: 3_000,
            outage_retry_ms: 30_000,
            max_segment_bytes: 16_384,
            max_antecedents: 8,
            max_in_flight: 8,
            retention_ms: 7 * 24 * 60 * 60 * 1_000,
            revalidate_ms: 60_000,
            max_records: 10_000,
            notices: NoticeMode::FailuresAndRecovery,
            notice_cooldown_ms: 60_000,
            notice_channel: None,
        }
    }
}

impl AttentionConfig {
    /// Invalid controls cannot grant provider access or silently become intentional off.
    pub fn validate(&self) -> Result<(), String> {
        if self.recipient.as_str().is_empty()
            || self.recipient.as_str().len() > 128
            || !self
                .recipient
                .as_str()
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "_-.:".contains(c))
        {
            return Err("attention recipient must be a nonempty bounded identifier".into());
        }
        if self.model.trim().is_empty() || self.model.len() > 128 {
            return Err("attention model must be an explicit bounded model identity".into());
        }
        if self.api_key_env.is_empty()
            || self.api_key_env.len() > 128
            || !self
                .api_key_env
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            return Err("attention api_key_env must be an environment variable name".into());
        }
        if !(1..=60_000).contains(&self.request_timeout_ms)
            || !(1..=3_600_000).contains(&self.outage_retry_ms)
            || !(256..=65_536).contains(&self.max_segment_bytes)
            || self.max_antecedents > 32
            || !(1..=64).contains(&self.max_in_flight)
            || !(1..=30 * 24 * 60 * 60 * 1_000).contains(&self.retention_ms)
            || !(1..=24 * 60 * 60 * 1_000).contains(&self.revalidate_ms)
            || !(1..=100_000).contains(&self.max_records)
            || self.notice_cooldown_ms > 24 * 60 * 60 * 1_000
        {
            return Err("attention resource and lifecycle bounds are invalid".into());
        }
        if self
            .brief
            .as_ref()
            .is_some_and(|brief| brief.text.len() > 16_384)
        {
            return Err("attention brief exceeds 16384 bytes".into());
        }
        Ok(())
    }

    /// Resolves a notice only against the independent operator-owned route binding.
    pub fn notice_destination(
        &self,
        route: Option<&RecipientNoticeRoute>,
    ) -> Result<Option<ChannelId>, &'static str> {
        if self.notices == NoticeMode::Off {
            return Ok(None);
        }
        let Some(channel) = self.notice_channel else {
            return Ok(None);
        };
        match route {
            Some(route) if route.recipient == self.recipient && route.channel == channel => {
                Ok(Some(channel))
            }
            _ => Err("attention notice route is not authorized for this recipient"),
        }
    }

    /// Applies emergency-off first, then an exact-room override, then the seat default.
    pub fn effective_mode(&self, channel: ChannelId) -> AttentionMode {
        if self.emergency_off {
            return AttentionMode::Off;
        }
        self.rooms
            .get(&channel)
            .and_then(|room| room.mode)
            .unwrap_or(self.mode)
    }

    /// Checks only the exact-room export grant; source access must also be revalidated.
    pub fn provider_eligible(&self, channel: ChannelId) -> bool {
        self.rooms
            .get(&channel)
            .is_some_and(|room| room.provider_eligible)
    }

    /// Whether the room routes directly without attention filtering, independently of export.
    pub fn direct_room(&self, channel: ChannelId) -> bool {
        self.rooms.get(&channel).is_some_and(|room| room.direct)
    }

    /// Binds artifacts to the selected model, compiled rubric/features, and current brief.
    pub fn compatibility(&self) -> Compatibility {
        Compatibility {
            model: self.model.clone(),
            rubric: RUBRIC_VERSION.into(),
            features: FEATURE_VERSION.into(),
            brief_version: self
                .brief
                .as_ref()
                .map(AttentionBrief::version)
                .unwrap_or_default(),
        }
    }
}

/// Rejects invalid persisted controls rather than treating them as intentional off.
pub fn deserialize_config<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<AttentionConfig, D::Error> {
    use serde::de::Error;
    let config = AttentionConfig::deserialize(deserializer)?;
    config.validate().map_err(D::Error::custom)?;
    Ok(config)
}
