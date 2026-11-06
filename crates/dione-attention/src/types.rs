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

pub use super::record_id::{RecordId, RecordIdError, RecordIdErrorKind};
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
    /// Discord channel containing the observed message.
    pub channel_id: ChannelId,
    /// Discord message identity within the channel.
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
    /// Exact channel and message identity for this observation.
    pub key: SourceKey,
    /// Observed Discord author identity; current authority is checked separately.
    pub author_id: UserId,
    /// Verified author/transport class used by delayed authority checks.
    #[serde(default)]
    pub author_kind: SourceAuthorKind,
    /// Stable conversation group supplied by the source resolver, not a semantic guess.
    pub conversation: String,
    /// Lowercase SHA-256 digest of the exact UTF-8 source text.
    pub content_hash: String,
    /// Unix time in milliseconds when the source version was observed.
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
    /// Exact provider model identity used to produce the scores.
    pub model: String,
    /// Question-rubric version bound into the decision.
    pub rubric: String,
    /// Calibration feature version and ordering bound into the decision.
    pub features: String,
    /// Digest binding the recipient brief text, expiry, and export grant.
    pub brief_version: String,
}

/// Independent wantedness, timeliness, participation and incremental-change probabilities.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Scores {
    /// Probability that the trigger is useful to the recipient.
    pub wanted: Probability,
    /// Probability that the trigger warrants attention at the next safe opportunity.
    pub prompt: Probability,
    /// Probability that the conversation invites the recipient to participate.
    pub participation: Probability,
    /// Probability that the trigger updates relevant facts or work state.
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
    /// Total elapsed judgment time in milliseconds, including retries.
    pub elapsed_ms: u64,
    /// Number of provider request attempts made for this judgment.
    pub attempts: u32,
    /// Provider-reported input token count when supplied.
    pub input_tokens: Option<u64>,
    /// Provider-reported output token count when supplied.
    pub output_tokens: Option<u64>,
}

/// Reusable provider scores and context sufficiency, not an admission decision.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RawJudgment {
    /// Provider model identity returned with this judgment.
    pub model: String,
    /// Independent relevance, timeliness, participation, and change probabilities.
    pub scores: Scores,
    /// Probability that the authorized evidence is sufficient to judge the other dimensions.
    pub context_sufficient: Probability,
    #[serde(default)]
    /// Optional bounded timing and usage metadata from the provider boundary.
    pub telemetry: Option<ProviderTelemetry>,
}

/// Policy outcome kept separate from source authorization and delivery state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Admission {
    /// Preserve ordinary delivery without attention-specific scheduling.
    Ordinary,
    /// Schedule the trigger at the next supported safe idle opportunity.
    Prompt,
    /// Deliver the trigger at the recipient's next supported safe turn.
    NextTurn,
    /// Retain the trigger for explicit retrieval without scheduling delivery.
    RetrievalOnly,
    /// No usable attention decision is available.
    Unknown,
}

/// Durable delivery lifecycle; an uncertain receipt is not permission to replay.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryState {
    /// The source is recorded without an attention-managed dispatch.
    Observed,
    /// The source is retained while pending attention work completes.
    Held,
    /// The trigger is retained for retrieval without scheduled dispatch.
    Deferred,
    /// The trigger is eligible for attention-managed dispatch.
    Admitted,
    /// The transport acknowledged dispatch of the admitted trigger.
    Dispatched,
    /// The transport outcome is uncertain and does not authorize replay.
    ReceiptUncertain,
    /// The retained source evidence no longer permits delivery.
    Invalidated,
}

/// Recipient-local decision tied to exact sources, compatibility and controller lifetime.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecisionRecord {
    /// Durable identity of this source-bound decision.
    pub id: RecordId,
    /// Recipient whose local attention store owns the decision.
    pub recipient: RecipientId,
    /// Exact source versions on which the decision depends.
    pub sources: Vec<SourceVersion>,
    /// Model, rubric, feature, and brief identities required to reuse the decision.
    pub compatibility: Compatibility,
    /// Application configuration generation captured for the decision.
    pub config_generation: u64,
    /// Controller process lifetime that created the decision.
    pub incarnation: IncarnationId,
    /// Unix time in milliseconds when the record was created.
    pub created_at_ms: u64,
    /// Unix time in milliseconds after which the record is expired.
    pub expires_at_ms: u64,
    /// Provider scores when evaluation completed successfully.
    pub judgment: Option<RawJudgment>,
    #[serde(default)]
    /// Selected learned artifact used for this decision, when present.
    pub policy_digest: Option<ArtifactDigest>,
    /// Policy outcome calculated independently of the active delivery mode.
    pub hypothetical: Admission,
    /// Delivery policy outcome applied under the active mode.
    pub actual: Admission,
    /// Durable state of attention-managed transport handling.
    pub delivery: DeliveryState,
    /// Recorded review-selection probability when sampled for feedback.
    pub selection_probability: Option<Probability>,
}

/// Explicit recipient assessment; absence and uncertainty are not negative labels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FeedbackLabel {
    /// The recipient wanted the source at the next safe opportunity.
    WantedPromptly,
    /// The recipient wanted the source without a prompt interruption.
    WantedLater,
    /// The recipient did not need the source.
    NotNeeded,
    /// The recipient supplied no definite wantedness or timeliness target.
    Unsure,
}

/// Actor-attributed assessment bound to the exact source versions reviewed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Feedback {
    /// Decision whose exact source versions were assessed.
    pub record_id: RecordId,
    /// Actor identity attributed to this assessment.
    pub annotator: RecipientId,
    /// Explicit assessment supplied by the actor.
    pub label: FeedbackLabel,
    /// Unix time in milliseconds when the assessment was made.
    pub assessed_at_ms: u64,
    /// Exact source versions reviewed when the assessment was supplied.
    pub source_versions: Vec<SourceVersion>,
}
