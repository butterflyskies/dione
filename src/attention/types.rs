//! Source-bound values shared by admission, provider evaluation and local learning.

use serde::{Deserialize, Serialize};
use serenity::model::id::{ChannelId, MessageId, UserId};
use sha2::{Digest, Sha256};

macro_rules! identity {
    ($name:ident, $doc:literal) => {
        #[doc = $doc]
        #[derive(
            Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            /// Borrows the wire identifier; possession does not establish membership or authority.
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl From<String> for $name {
            fn from(value: String) -> Self {
                Self(value)
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self(value.to_owned())
            }
        }

        impl From<$name> for String {
            fn from(value: $name) -> Self {
                value.0
            }
        }

        impl std::borrow::Borrow<str> for $name {
            fn borrow(&self) -> &str {
                self.as_str()
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str(self.as_str())
            }
        }
    };
}

identity!(
    RecordId,
    "Identity of one source-bound attention decision, not proof of eligibility."
);
identity!(
    RecipientId,
    "Recipient or attributed actor identity; current authorization is checked separately."
);
identity!(
    EvaluationId,
    "Identity of a predeclared held-out evaluation, distinct from a model artifact."
);
identity!(
    ArtifactDigest,
    "Content-addressed model artifact identity; lookup and integrity checks establish validity."
);
identity!(
    IncarnationId,
    "Process-lifetime identity that prevents stale work from joining a new controller."
);

/// Version of the question rubric bound into decisions and learned artifacts.
pub const RUBRIC_VERSION: &str = "attention-v1";
/// Version and order of the four calibration features.
pub const FEATURE_VERSION: &str = "wanted-prompt-participation-change-v1";
/// Explicit default provider model identity; not a floating model alias.
pub const DEFAULT_MODEL: &str = "jev-1.13.0";

/// Finite probability; invalid provider/config values cannot enter policy arithmetic.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(try_from = "f64", into = "f64")]
pub struct Probability(f64);

impl Probability {
    /// Returns the validated probability in the inclusive range zero to one.
    pub fn get(self) -> f64 {
        self.0
    }
}

impl TryFrom<f64> for Probability {
    type Error = &'static str;
    fn try_from(value: f64) -> Result<Self, Self::Error> {
        if value.is_finite() && (0.0..=1.0).contains(&value) {
            Ok(Self(value))
        } else {
            Err("probability must be finite and between zero and one")
        }
    }
}

impl From<Probability> for f64 {
    fn from(value: Probability) -> Self {
        value.0
    }
}

/// Discord channel/message identity, independent of content version or access rights.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct SourceKey {
    pub channel_id: ChannelId,
    pub message_id: MessageId,
}

/// Discord's verified author class for a non-webhook message.
///
/// This is captured at ingress instead of being inferred later from an effective
/// user ID, because bot allowlist authority applies only to direct transport.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DirectAuthorKind {
    /// An ordinary Discord user account.
    Human,
    /// A Discord bot account whose admission requires the current bot allowlist.
    Bot,
}

impl DirectAuthorKind {
    /// Converts Discord's authenticated author flag into a typed direct author class.
    pub fn from_bot_flag(is_bot: bool) -> Self {
        if is_bot { Self::Bot } else { Self::Human }
    }
}

/// Transport-aware author evidence persisted with a source version.
///
/// Verified webhook transport remains distinct from direct human and bot
/// authors even when downstream display identity is a represented Discord user.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceAuthorKind {
    /// Legacy or otherwise unavailable author-class evidence; never treated as human proof.
    #[default]
    Unknown,
    /// A directly authored Discord message from a human account.
    DirectHuman,
    /// A directly authored Discord message from a bot account.
    DirectBot,
    /// A webhook message admitted through verified webhook provenance.
    VerifiedWebhook,
}

impl From<DirectAuthorKind> for SourceAuthorKind {
    fn from(value: DirectAuthorKind) -> Self {
        match value {
            DirectAuthorKind::Human => Self::DirectHuman,
            DirectAuthorKind::Bot => Self::DirectBot,
        }
    }
}

/// Content-version evidence with attributed transport provenance; not an authorization grant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceVersion {
    pub key: SourceKey,
    pub author_id: UserId,
    /// Verified author/transport class used by delayed authority checks.
    #[serde(default)]
    pub author_kind: SourceAuthorKind,
    /// Stable conversation group supplied by the source resolver, not a semantic guess.
    pub conversation: String,
    pub content_hash: String,
    pub observed_at_ms: u64,
}

impl SourceVersion {
    /// Checks exact UTF-8 content bytes against this version's stored digest.
    pub fn matches_text(&self, text: &str) -> bool {
        self.content_hash == content_hash(text)
    }
}

/// Hashes exact UTF-8 content bytes as lowercase SHA-256 hexadecimal.
pub fn content_hash(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.as_bytes()))
}

/// Complete model, rubric, feature and recipient-brief identity required for reuse.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Compatibility {
    pub model: String,
    pub rubric: String,
    pub features: String,
    pub brief_version: String,
}

/// Independent wantedness, timeliness, participation and incremental-change probabilities.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Scores {
    pub wanted: Probability,
    pub prompt: Probability,
    pub participation: Probability,
    pub change: Probability,
}

impl Scores {
    /// Returns features in the order identified by [`FEATURE_VERSION`].
    pub fn values(&self) -> [f64; 4] {
        [
            self.wanted.get(),
            self.prompt.get(),
            self.participation.get(),
            self.change.get(),
        ]
    }
}

/// Bounded provider timing and usage metadata, without source text or credentials.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderTelemetry {
    pub elapsed_ms: u64,
    pub attempts: u32,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
}

/// Reusable provider scores and context sufficiency, not an admission decision.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RawJudgment {
    pub model: String,
    pub scores: Scores,
    pub context_sufficient: Probability,
    #[serde(default)]
    pub telemetry: Option<ProviderTelemetry>,
}

/// Policy outcome kept separate from source authorization and delivery state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Admission {
    Ordinary,
    Prompt,
    NextTurn,
    RetrievalOnly,
    Unknown,
}

/// Durable delivery lifecycle; an uncertain receipt is not permission to replay.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryState {
    Observed,
    Held,
    Deferred,
    Admitted,
    Dispatched,
    ReceiptUncertain,
    Invalidated,
}

/// Recipient-local decision tied to exact sources, compatibility and controller lifetime.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecisionRecord {
    pub id: RecordId,
    pub recipient: RecipientId,
    pub sources: Vec<SourceVersion>,
    pub compatibility: Compatibility,
    pub config_generation: u64,
    pub incarnation: IncarnationId,
    pub created_at_ms: u64,
    pub expires_at_ms: u64,
    pub judgment: Option<RawJudgment>,
    #[serde(default)]
    pub policy_digest: Option<ArtifactDigest>,
    pub hypothetical: Admission,
    pub actual: Admission,
    pub delivery: DeliveryState,
    pub selection_probability: Option<Probability>,
}

/// Explicit recipient assessment; absence and uncertainty are not negative labels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FeedbackLabel {
    WantedPromptly,
    WantedLater,
    NotNeeded,
    Unsure,
}

/// Actor-attributed assessment bound to the exact source versions reviewed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Feedback {
    pub record_id: RecordId,
    pub annotator: RecipientId,
    pub label: FeedbackLabel,
    pub assessed_at_ms: u64,
    pub source_versions: Vec<SourceVersion>,
}
