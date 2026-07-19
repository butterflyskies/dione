//! Generic pre-send hook plumbing for outbound messages.

use serenity::model::id::{ChannelId, GuildId, MessageId, UserId};
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, OnceLock, RwLock};

/// Whether hook dispositions are observed or enforced.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PipelineMode {
    /// Record findings without changing, redirecting, or stopping output.
    #[default]
    Observe,
    /// Apply hook rewrites, redirects, and halts.
    Enforce,
}

/// The Discord destination kind for an outbound message.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelType {
    Public,
    DirectMessage,
    Thread,
}

/// The typed destination known before an outbound send reaches Discord.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutboundDestination {
    Channel(ChannelId),
    DmRecipient(UserId),
}

/// A validated, stable hook identifier.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct HookName(String);

impl HookName {
    pub fn parse(value: impl Into<String>) -> Result<Self, PipelineError> {
        let value = value.into();
        if valid_identifier(&value) {
            Ok(Self(value))
        } else {
            Err(PipelineError::InvalidHookName(value))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for HookName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// A validated construct identifier used in hook attribution.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ConstructId(String);

impl ConstructId {
    pub fn parse(value: impl Into<String>) -> Result<Self, String> {
        let value = value.into();
        valid_identifier(&value)
            .then_some(Self(value.clone()))
            .ok_or_else(|| format!("invalid construct id `{value}`"))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for ConstructId {
    fn default() -> Self {
        Self("dione".to_owned())
    }
}

/// Current-message context available to a pre-send hook.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookContext {
    text: String,
    author_id: Option<UserId>,
    destination: OutboundDestination,
    channel_type: ChannelType,
    guild_id: Option<GuildId>,
    reply_to_message_id: Option<MessageId>,
    construct_id: ConstructId,
    metadata: HashMap<String, String>,
}

impl HookContext {
    pub fn new(
        text: impl Into<String>,
        destination: OutboundDestination,
        channel_type: ChannelType,
        construct_id: ConstructId,
    ) -> Self {
        Self {
            text: text.into(),
            author_id: None,
            destination,
            channel_type,
            guild_id: None,
            reply_to_message_id: None,
            construct_id,
            metadata: HashMap::new(),
        }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn destination(&self) -> OutboundDestination {
        self.destination
    }

    pub fn channel_id(&self) -> Option<ChannelId> {
        match self.destination {
            OutboundDestination::Channel(channel_id) => Some(channel_id),
            OutboundDestination::DmRecipient(_) => None,
        }
    }

    pub fn dm_recipient_id(&self) -> Option<UserId> {
        match self.destination {
            OutboundDestination::DmRecipient(user_id) => Some(user_id),
            OutboundDestination::Channel(_) => None,
        }
    }

    pub fn channel_type(&self) -> ChannelType {
        self.channel_type
    }

    pub fn author_id(&self) -> Option<UserId> {
        self.author_id
    }

    pub fn guild_id(&self) -> Option<GuildId> {
        self.guild_id
    }

    pub fn reply_to_message_id(&self) -> Option<MessageId> {
        self.reply_to_message_id
    }

    pub fn construct_id(&self) -> &str {
        self.construct_id.as_str()
    }

    pub fn metadata(&self, key: &str) -> Option<&str> {
        self.metadata.get(key).map(String::as_str)
    }

    pub fn with_reply_to(mut self, message_id: Option<MessageId>) -> Self {
        self.reply_to_message_id = message_id;
        self
    }

    pub fn with_metadata(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.metadata.insert(key.into(), value.into());
        self
    }

    pub fn with_author_id(mut self, author_id: Option<UserId>) -> Self {
        self.author_id = author_id;
        self
    }

    pub fn with_guild_id(mut self, guild_id: Option<GuildId>) -> Self {
        self.guild_id = guild_id;
        self
    }
}

/// One observation produced by a pre-send hook.
#[derive(Debug, Clone, PartialEq)]
pub struct Assessment {
    hook_name: Option<HookName>,
    category: String,
    confidence: f32,
    detail: String,
}

impl Assessment {
    pub fn new(category: impl Into<String>, confidence: f32, detail: impl Into<String>) -> Self {
        let confidence = if confidence.is_finite() {
            confidence.clamp(0.0, 1.0)
        } else {
            0.0
        };
        Self {
            hook_name: None,
            category: category.into(),
            confidence,
            detail: detail.into(),
        }
    }

    pub fn hook_name(&self) -> Option<&HookName> {
        self.hook_name.as_ref()
    }

    pub fn category(&self) -> &str {
        &self.category
    }

    pub fn confidence(&self) -> f32 {
        self.confidence
    }

    pub fn detail(&self) -> &str {
        &self.detail
    }

    fn attribute_to(&mut self, hook_name: &HookName) {
        self.hook_name = Some(hook_name.clone());
    }
}

/// Assessments routed back to the construct for self-correction.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ConstructFeedback(Vec<Assessment>);

impl ConstructFeedback {
    pub fn new(assessments: Vec<Assessment>) -> Self {
        Self(assessments)
    }

    pub fn as_slice(&self) -> &[Assessment] {
        &self.0
    }

    fn append_attributed(&mut self, hook_name: &HookName, source: &Self) {
        self.0
            .extend(source.0.iter().cloned().map(|mut assessment| {
                assessment.attribute_to(hook_name);
                assessment
            }));
    }
}

/// Assessments routed to durable or operator-facing audit storage.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AuditTrail(Vec<Assessment>);

impl AuditTrail {
    pub fn new(assessments: Vec<Assessment>) -> Self {
        Self(assessments)
    }

    pub fn as_slice(&self) -> &[Assessment] {
        &self.0
    }

    fn append_attributed(&mut self, hook_name: &HookName, source: &Self) {
        self.0
            .extend(source.0.iter().cloned().map(|mut assessment| {
                assessment.attribute_to(hook_name);
                assessment
            }));
    }

    fn push_attributed(&mut self, hook_name: &HookName, mut assessment: Assessment) {
        assessment.attribute_to(hook_name);
        self.0.push(assessment);
    }
}

/// Assessment-free disposition returned by one hook.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub enum HookDecision {
    Continue,
    Rewrite { text: String },
    Halt { reason: String },
    Redirect { channel_id: ChannelId },
}

impl HookDecision {
    fn strictness(&self) -> u8 {
        match self {
            Self::Continue => 0,
            Self::Rewrite { .. } => 1,
            Self::Redirect { .. } => 2,
            Self::Halt { .. } => 3,
        }
    }
}

/// One hook's decision plus its two assessment streams.
#[derive(Debug, Clone, PartialEq)]
pub struct HookOutput {
    decision: HookDecision,
    to_construct: ConstructFeedback,
    to_audit: AuditTrail,
}

impl HookOutput {
    pub fn new(
        decision: HookDecision,
        to_construct: ConstructFeedback,
        to_audit: AuditTrail,
    ) -> Self {
        Self {
            decision,
            to_construct,
            to_audit,
        }
    }

    fn decision(&self) -> &HookDecision {
        &self.decision
    }
    fn routed(&self) -> (&ConstructFeedback, &AuditTrail) {
        (&self.to_construct, &self.to_audit)
    }
}

/// A synchronous, immutable pre-send hook.
pub trait PreSendHook: Send + Sync {
    fn name(&self) -> HookName;
    fn execute(&self, context: &HookContext) -> HookOutput;
}

/// A failure writing one assessment stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SinkError(String);

impl SinkError {
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for SinkError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Receives accumulated construct-facing feedback once per pipeline run.
pub trait FeedbackSink: Send + Sync {
    fn record(&self, feedback: &ConstructFeedback, context: &HookContext) -> Result<(), SinkError>;
}

/// Receives accumulated audit assessments once per pipeline run.
pub trait AuditSink: Send + Sync {
    fn record(&self, trail: &AuditTrail, context: &HookContext) -> Result<(), SinkError>;
}

/// What to do when either routing sink fails or panics.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SinkFailurePolicy {
    /// Preserve the send and report degraded routing in the outcome.
    #[default]
    Degrade,
    /// Stop the send when assessment delivery cannot be guaranteed.
    FailClosed,
}

/// A sink failure captured without losing the other routing stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SinkFailure {
    sink: SinkKind,
    detail: String,
}

impl SinkFailure {
    pub fn sink(&self) -> SinkKind {
        self.sink
    }

    pub fn detail(&self) -> &str {
        &self.detail
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SinkKind {
    Feedback,
    Audit,
}

impl SinkKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Feedback => "feedback",
            Self::Audit => "audit",
        }
    }
}

/// Pipeline construction or fail-closed execution error.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PipelineError {
    InvalidHookName(String),
    DuplicateHookName(String),
    UnknownHookName(HookName),
    SinkFailures(Vec<SinkFailure>),
    HookPanicked(String),
}

impl fmt::Display for PipelineError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidHookName(name) => write!(formatter, "invalid pre-send hook name `{name}`"),
            Self::DuplicateHookName(name) => {
                write!(formatter, "duplicate pre-send hook name `{name}`")
            }
            Self::UnknownHookName(name) => write!(formatter, "unknown pre-send hook name `{name}`"),
            Self::SinkFailures(failures) => write!(
                formatter,
                "pre-send assessment routing failed for {} sink(s)",
                failures.len()
            ),
            Self::HookPanicked(name) => write!(formatter, "pre-send hook `{name}` panicked"),
        }
    }
}

impl std::error::Error for PipelineError {}

/// Names individual hooks bypassed for one send.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NoRly {
    hook_names: HashSet<HookName>,
}

impl NoRly {
    fn new(hook_names: impl IntoIterator<Item = HookName>) -> Self {
        Self {
            hook_names: hook_names.into_iter().collect(),
        }
    }

    fn bypasses(&self, hook_name: &HookName) -> bool {
        self.hook_names.contains(hook_name)
    }
}

/// Final disposition of one pre-send pipeline run.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub struct PipelineOutcome {
    decision: HookDecision,
    to_construct: ConstructFeedback,
    to_audit: AuditTrail,
    final_text: Option<String>,
    sink_failures: Vec<SinkFailure>,
}

impl PipelineOutcome {
    pub fn decision(&self) -> &HookDecision {
        &self.decision
    }
    pub fn to_construct(&self) -> &ConstructFeedback {
        &self.to_construct
    }
    pub fn to_audit(&self) -> &AuditTrail {
        &self.to_audit
    }
    pub fn final_text(&self) -> Option<&str> {
        self.final_text.as_deref()
    }
    pub fn sink_failures(&self) -> &[SinkFailure] {
        &self.sink_failures
    }
}

struct NullFeedbackSink;

impl FeedbackSink for NullFeedbackSink {
    fn record(
        &self,
        _feedback: &ConstructFeedback,
        _context: &HookContext,
    ) -> Result<(), SinkError> {
        Ok(())
    }
}

struct NullAuditSink;

impl AuditSink for NullAuditSink {
    fn record(&self, _trail: &AuditTrail, _context: &HookContext) -> Result<(), SinkError> {
        Ok(())
    }
}

struct TracingFeedbackSink;

impl FeedbackSink for TracingFeedbackSink {
    fn record(&self, feedback: &ConstructFeedback, context: &HookContext) -> Result<(), SinkError> {
        for assessment in feedback.as_slice() {
            tracing::info!(
                hook = assessment
                    .hook_name()
                    .map(HookName::as_str)
                    .unwrap_or("unattributed"),
                category = assessment.category(),
                confidence = assessment.confidence(),
                construct = context.construct_id(),
                detail = assessment.detail(),
                "pre-send construct feedback"
            );
        }
        Ok(())
    }
}

struct TracingAuditSink;

impl AuditSink for TracingAuditSink {
    fn record(&self, trail: &AuditTrail, context: &HookContext) -> Result<(), SinkError> {
        for assessment in trail.as_slice() {
            tracing::info!(
                hook = assessment
                    .hook_name()
                    .map(HookName::as_str)
                    .unwrap_or("unattributed"),
                category = assessment.category(),
                confidence = assessment.confidence(),
                construct = context.construct_id(),
                channel = context.channel_id().map(ChannelId::get),
                dm_recipient = context.dm_recipient_id().map(UserId::get),
                detail = assessment.detail(),
                "pre-send audit assessment"
            );
        }
        Ok(())
    }
}

static INSTALLED_PIPELINE: OnceLock<RwLock<Option<Arc<PreSendPipeline>>>> = OnceLock::new();

/// Builds the production Observe pipeline and supplies the hook registration seam.
pub fn observe_pipeline(
    hooks: Vec<Box<dyn PreSendHook>>,
) -> Result<Arc<PreSendPipeline>, PipelineError> {
    PreSendPipeline::new(hooks).map(|pipeline| {
        Arc::new(pipeline.with_sinks(
            Box::new(TracingFeedbackSink),
            Box::new(TracingAuditSink),
            SinkFailurePolicy::Degrade,
        ))
    })
}

/// Applies lifecycle configuration while preserving Observe as the only
/// production mode. `None` is returned only for explicit disablement.
pub fn configured_pipeline(
    enabled: bool,
    hooks: Vec<Box<dyn PreSendHook>>,
) -> Result<Option<Arc<PreSendPipeline>>, PipelineError> {
    if enabled {
        observe_pipeline(hooks).map(Some)
    } else {
        Ok(None)
    }
}

/// Installs or explicitly disables the process-wide outbound pipeline.
pub fn install_pipeline(pipeline: Option<Arc<PreSendPipeline>>) {
    *INSTALLED_PIPELINE
        .get_or_init(|| RwLock::new(None))
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = pipeline;
}

pub(crate) fn installed_pipeline() -> Option<Arc<PreSendPipeline>> {
    INSTALLED_PIPELINE
        .get_or_init(|| RwLock::new(None))
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

/// Sequential pre-send hook runner with separate feedback and audit sinks.
pub struct PreSendPipeline {
    mode: PipelineMode,
    hooks: Vec<Box<dyn PreSendHook>>,
    feedback_sink: Box<dyn FeedbackSink>,
    audit_sink: Box<dyn AuditSink>,
    sink_failure_policy: SinkFailurePolicy,
}

impl PreSendPipeline {
    /// Creates a log-only pipeline. Enforcement must be enabled explicitly.
    pub fn new(hooks: Vec<Box<dyn PreSendHook>>) -> Result<Self, PipelineError> {
        Self::validate_hooks(&hooks)?;
        Ok(Self {
            mode: PipelineMode::Observe,
            hooks,
            feedback_sink: Box::new(NullFeedbackSink),
            audit_sink: Box::new(NullAuditSink),
            sink_failure_policy: SinkFailurePolicy::Degrade,
        })
    }

    pub fn with_mode(mut self, mode: PipelineMode) -> Self {
        self.mode = mode;
        self
    }

    pub fn with_sinks(
        mut self,
        feedback_sink: Box<dyn FeedbackSink>,
        audit_sink: Box<dyn AuditSink>,
        failure_policy: SinkFailurePolicy,
    ) -> Self {
        self.feedback_sink = feedback_sink;
        self.audit_sink = audit_sink;
        self.sink_failure_policy = failure_policy;
        self
    }

    fn validate_hooks(hooks: &[Box<dyn PreSendHook>]) -> Result<(), PipelineError> {
        let mut names = HashSet::with_capacity(hooks.len());
        for hook in hooks {
            let name = hook.name();
            if !valid_identifier(name.as_str()) {
                return Err(PipelineError::InvalidHookName(name.to_string()));
            }
            if !names.insert(name.clone()) {
                return Err(PipelineError::DuplicateHookName(name.to_string()));
            }
        }
        Ok(())
    }

    pub fn no_rly(&self, names: &[HookName]) -> Result<NoRly, PipelineError> {
        for name in names {
            if !self.hooks.iter().any(|hook| hook.name() == *name) {
                return Err(PipelineError::UnknownHookName(name.clone()));
            }
        }
        Ok(NoRly::new(names.iter().cloned()))
    }

    pub fn run(
        &self,
        context: &HookContext,
        no_rly: &NoRly,
    ) -> Result<PipelineOutcome, PipelineError> {
        for hook_name in &no_rly.hook_names {
            if !self.hooks.iter().any(|hook| hook.name() == *hook_name) {
                return Err(PipelineError::UnknownHookName(hook_name.clone()));
            }
        }
        let original_text = context.text.clone();
        let mut working_context = context.clone();
        let mut to_construct = ConstructFeedback::default();
        let mut to_audit = AuditTrail::default();
        let mut strictest = None;

        for hook in &self.hooks {
            let hook_name = hook.name();
            if no_rly.bypasses(&hook_name) {
                to_audit.push_attributed(
                    &hook_name,
                    Assessment::new(
                        "no-rly-bypass",
                        1.0,
                        format!("hook bypassed for construct `{}`", context.construct_id()),
                    ),
                );
                continue;
            }

            let result = catch_unwind(AssertUnwindSafe(|| hook.execute(&working_context)));
            let result = match result {
                Ok(result) => result,
                Err(_) if self.mode == PipelineMode::Observe => HookOutput::new(
                    HookDecision::Continue,
                    ConstructFeedback::default(),
                    AuditTrail::new(vec![Assessment::new(
                        "internal-error",
                        1.0,
                        "hook panicked during execution",
                    )]),
                ),
                Err(_) => return Err(PipelineError::HookPanicked(hook_name.to_string())),
            };
            let (construct_assessments, audit_assessments) = result.routed();
            if self.mode == PipelineMode::Enforce {
                to_construct.append_attributed(&hook_name, construct_assessments);
            }
            to_audit.append_attributed(&hook_name, audit_assessments);

            if self.mode == PipelineMode::Observe {
                continue;
            }

            if let HookDecision::Rewrite { text } = result.decision() {
                working_context.text.clone_from(text);
            }
            if strictest.as_ref().is_none_or(|current: &HookDecision| {
                result.decision().strictness() >= current.strictness()
            }) {
                strictest = Some(result.decision().clone());
            }
            if matches!(result.decision(), HookDecision::Halt { .. }) {
                break;
            }
        }

        let decision = if self.mode == PipelineMode::Observe {
            HookDecision::Continue
        } else {
            strictest.unwrap_or(HookDecision::Continue)
        };
        let final_text = if self.mode == PipelineMode::Observe {
            Some(original_text)
        } else if matches!(decision, HookDecision::Halt { .. }) {
            None
        } else {
            Some(working_context.text.clone())
        };
        let sink_context = HookContext {
            text: final_text.clone().unwrap_or(working_context.text),
            ..context.clone()
        };
        let sink_failures = self.route_to_sinks(&to_construct, &to_audit, &sink_context);
        if self.sink_failure_policy == SinkFailurePolicy::FailClosed && !sink_failures.is_empty() {
            return Err(PipelineError::SinkFailures(sink_failures));
        }

        Ok(PipelineOutcome {
            decision,
            to_construct,
            to_audit,
            final_text,
            sink_failures,
        })
    }

    fn route_to_sinks(
        &self,
        feedback: &ConstructFeedback,
        audit: &AuditTrail,
        context: &HookContext,
    ) -> Vec<SinkFailure> {
        let mut failures = Vec::new();
        capture_sink_failure(SinkKind::Feedback, &mut failures, || {
            self.feedback_sink.record(feedback, context)
        });
        capture_sink_failure(SinkKind::Audit, &mut failures, || {
            self.audit_sink.record(audit, context)
        });
        failures
    }
}

fn valid_identifier(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('-')
        && !name.ends_with('-')
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

fn capture_sink_failure(
    sink: SinkKind,
    failures: &mut Vec<SinkFailure>,
    record: impl FnOnce() -> Result<(), SinkError>,
) {
    match catch_unwind(AssertUnwindSafe(record)) {
        Ok(Ok(())) => {}
        Ok(Err(error)) => failures.push(SinkFailure {
            sink,
            detail: error.to_string(),
        }),
        Err(_) => failures.push(SinkFailure {
            sink,
            detail: "sink panicked".to_owned(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    struct FailingSink;

    impl FeedbackSink for FailingSink {
        fn record(
            &self,
            _feedback: &ConstructFeedback,
            _context: &HookContext,
        ) -> Result<(), SinkError> {
            Err(SinkError::new("feedback unavailable"))
        }
    }

    struct PanickingAudit;

    impl AuditSink for PanickingAudit {
        fn record(&self, _trail: &AuditTrail, _context: &HookContext) -> Result<(), SinkError> {
            panic!("audit panic")
        }
    }

    struct TestHook {
        name: HookName,
        result: HookOutput,
    }

    impl PreSendHook for TestHook {
        fn name(&self) -> HookName {
            self.name.clone()
        }

        fn execute(&self, _context: &HookContext) -> HookOutput {
            self.result.clone()
        }
    }

    struct ObservingHook(Arc<Mutex<Vec<String>>>);

    impl PreSendHook for ObservingHook {
        fn name(&self) -> HookName {
            HookName::parse("observer").expect("valid test hook name")
        }

        fn execute(&self, context: &HookContext) -> HookOutput {
            self.0
                .lock()
                .expect("observation lock")
                .push(context.text().to_owned());
            output(HookDecision::Continue)
        }
    }

    struct PanickingHook;

    impl PreSendHook for PanickingHook {
        fn name(&self) -> HookName {
            HookName::parse("panicking-hook").expect("valid test hook name")
        }

        fn execute(&self, _context: &HookContext) -> HookOutput {
            panic!("hook panic")
        }
    }

    fn context() -> HookContext {
        HookContext::new(
            "original",
            OutboundDestination::Channel(ChannelId::new(2)),
            ChannelType::Public,
            ConstructId::parse("test").expect("valid construct id"),
        )
    }

    fn assessment(detail: &str) -> Assessment {
        Assessment::new("test", 0.5, detail)
    }

    fn output(decision: HookDecision) -> HookOutput {
        HookOutput::new(
            decision,
            ConstructFeedback::default(),
            AuditTrail::default(),
        )
    }

    fn assessed(
        decision: HookDecision,
        to_construct: ConstructFeedback,
        to_audit: AuditTrail,
    ) -> HookOutput {
        HookOutput::new(decision, to_construct, to_audit)
    }

    fn pipeline(hooks: Vec<Box<dyn PreSendHook>>, mode: PipelineMode) -> PreSendPipeline {
        PreSendPipeline::new(hooks)
            .expect("valid pipeline")
            .with_mode(mode)
    }

    #[test]
    fn observe_is_default_and_preserves_original_text() {
        let pipeline = PreSendPipeline::new(vec![Box::new(TestHook {
            name: HookName::parse("halt").unwrap(),
            result: assessed(
                HookDecision::Halt {
                    reason: "stop".to_owned(),
                },
                ConstructFeedback::new(vec![assessment("feedback")]),
                AuditTrail::new(vec![assessment("audit")]),
            ),
        })])
        .expect("valid pipeline");

        let outcome = pipeline.run(&context(), &NoRly::default()).expect("run");

        assert_eq!(outcome.final_text.as_deref(), Some("original"));
        assert!(matches!(outcome.decision, HookDecision::Continue));
        assert!(outcome.to_construct.as_slice().is_empty());
        assert_eq!(outcome.to_audit.as_slice()[0].detail(), "audit");
    }

    #[test]
    fn hook_panic_observes_with_audit_but_enforcement_fails_closed() {
        let observe = pipeline(vec![Box::new(PanickingHook)], PipelineMode::Observe);
        let observed = observe
            .run(&context(), &NoRly::default())
            .expect("observe run");
        assert_eq!(observed.final_text.as_deref(), Some("original"));
        assert_eq!(observed.to_audit.as_slice()[0].category(), "internal-error");

        let enforce = pipeline(vec![Box::new(PanickingHook)], PipelineMode::Enforce);
        assert!(matches!(
            enforce.run(&context(), &NoRly::default()),
            Err(PipelineError::HookPanicked(name)) if name == "panicking-hook"
        ));
    }

    #[test]
    fn enforce_applies_rewrite_and_exposes_it_to_next_hook() {
        let observed = Arc::new(Mutex::new(Vec::new()));
        let pipeline = pipeline(
            vec![
                Box::new(TestHook {
                    name: HookName::parse("rewriter").unwrap(),
                    result: output(HookDecision::Rewrite {
                        text: "rewritten".to_owned(),
                    }),
                }),
                Box::new(ObservingHook(Arc::clone(&observed))),
            ],
            PipelineMode::Enforce,
        );

        let outcome = pipeline.run(&context(), &NoRly::default()).expect("run");

        assert_eq!(outcome.final_text.as_deref(), Some("rewritten"));
        assert_eq!(*observed.lock().expect("lock"), vec!["rewritten"]);
    }

    #[test]
    fn multiple_equal_rewrites_keep_latest_verdict_and_text() {
        let rewrite = |name: &'static str, text: &str| {
            Box::new(TestHook {
                name: HookName::parse(name).unwrap(),
                result: output(HookDecision::Rewrite {
                    text: text.to_owned(),
                }),
            }) as Box<dyn PreSendHook>
        };
        let pipeline = pipeline(
            vec![rewrite("first", "one"), rewrite("second", "two")],
            PipelineMode::Enforce,
        );

        let outcome = pipeline.run(&context(), &NoRly::default()).expect("run");

        assert_eq!(outcome.final_text.as_deref(), Some("two"));
        assert!(matches!(outcome.decision, HookDecision::Rewrite { ref text } if text == "two"));
    }

    #[test]
    fn immutable_context_prevents_untyped_side_mutation() {
        fn assert_signature<H: PreSendHook>(hook: &H, context: &HookContext) {
            let _ = hook.execute(context);
        }
        let hook = TestHook {
            name: HookName::parse("immutable").unwrap(),
            result: output(HookDecision::Continue),
        };
        let context = context();

        assert_signature(&hook, &context);
        assert_eq!(context.text(), "original");
    }

    #[test]
    fn targeted_no_rly_bypass_is_audited_and_other_hook_runs() {
        let observed = Arc::new(Mutex::new(Vec::new()));
        let pipeline = pipeline(
            vec![
                Box::new(TestHook {
                    name: HookName::parse("bypassed").unwrap(),
                    result: output(HookDecision::Halt {
                        reason: "stop".to_owned(),
                    }),
                }),
                Box::new(ObservingHook(Arc::clone(&observed))),
            ],
            PipelineMode::Enforce,
        );

        let outcome = pipeline
            .run(
                &context(),
                &NoRly::new([HookName::parse("bypassed").unwrap()]),
            )
            .expect("run");

        assert_eq!(*observed.lock().expect("lock"), vec!["original"]);
        assert_eq!(
            outcome.to_audit.as_slice()[0].hook_name().unwrap().as_str(),
            "bypassed"
        );
        assert_eq!(outcome.to_audit.as_slice()[0].category(), "no-rly-bypass");
        assert!(outcome.to_audit.as_slice()[0].detail().contains("test"));
    }

    #[test]
    fn hook_attribution_is_stamped_by_pipeline() {
        let pipeline = pipeline(
            vec![Box::new(TestHook {
                name: HookName::parse("real-hook").unwrap(),
                result: assessed(
                    HookDecision::Continue,
                    ConstructFeedback::default(),
                    AuditTrail::new(vec![assessment("finding")]),
                ),
            })],
            PipelineMode::Observe,
        );

        let outcome = pipeline.run(&context(), &NoRly::default()).expect("run");

        assert_eq!(
            outcome.to_audit.as_slice()[0].hook_name().unwrap().as_str(),
            "real-hook"
        );
    }

    #[test]
    fn assessments_are_attributed_once_and_not_duplicated_into_decision() {
        let pipeline = pipeline(
            vec![Box::new(TestHook {
                name: HookName::parse("one-source").unwrap(),
                result: assessed(
                    HookDecision::Rewrite {
                        text: "rewritten".to_owned(),
                    },
                    ConstructFeedback::new(vec![assessment("construct")]),
                    AuditTrail::new(vec![assessment("audit")]),
                ),
            })],
            PipelineMode::Enforce,
        );

        let outcome = pipeline.run(&context(), &NoRly::default()).unwrap();

        assert!(
            matches!(outcome.decision(), HookDecision::Rewrite { text } if text == "rewritten")
        );
        assert_eq!(outcome.to_construct().as_slice().len(), 1);
        assert_eq!(outcome.to_audit().as_slice().len(), 1);
        assert_eq!(
            outcome.to_construct().as_slice()[0]
                .hook_name()
                .unwrap()
                .as_str(),
            "one-source"
        );
        assert_eq!(
            outcome.to_audit().as_slice()[0]
                .hook_name()
                .unwrap()
                .as_str(),
            "one-source"
        );
    }

    #[test]
    fn confidence_normalizes_non_finite_values() {
        assert_eq!(assessment("finite").confidence(), 0.5);
        assert_eq!(Assessment::new("test", f32::NAN, "nan").confidence(), 0.0);
        assert_eq!(
            Assessment::new("test", f32::INFINITY, "infinite").confidence(),
            0.0
        );
    }

    #[test]
    fn duplicate_and_invalid_hook_names_are_rejected() {
        let empty = || output(HookDecision::Continue);
        assert!(matches!(
            PreSendPipeline::new(vec![
                Box::new(TestHook { name: HookName::parse("same").unwrap(), result: empty() }),
                Box::new(TestHook { name: HookName::parse("same").unwrap(), result: empty() }),
            ]),
            Err(PipelineError::DuplicateHookName(name)) if name == "same"
        ));
        assert!(matches!(
            PreSendPipeline::new(vec![Box::new(TestHook {
                name: HookName("Not Valid".to_owned()),
                result: empty(),
            })]),
            Err(PipelineError::InvalidHookName(name)) if name == "Not Valid"
        ));
    }

    #[test]
    fn hook_name_boundary_accepts_only_canonical_names() {
        assert_eq!(HookName::parse("tier-1").unwrap().as_str(), "tier-1");
        for invalid in ["", "-leading", "trailing-", "UPPER", "under_score"] {
            assert!(matches!(
                HookName::parse(invalid),
                Err(PipelineError::InvalidHookName(_))
            ));
        }
    }

    #[test]
    fn unknown_hook_override_is_rejected_before_audit() {
        let pipeline = PreSendPipeline::new(vec![Box::new(TestHook {
            name: HookName::parse("known").unwrap(),
            result: output(HookDecision::Continue),
        })])
        .unwrap();
        let unknown = HookName::parse("unknown").unwrap();

        assert_eq!(
            pipeline.no_rly(std::slice::from_ref(&unknown)),
            Err(PipelineError::UnknownHookName(unknown))
        );
    }

    #[test]
    fn run_revalidates_bypass_set_against_registry() {
        let pipeline = PreSendPipeline::new(Vec::new()).unwrap();
        let unknown = HookName::parse("unknown").unwrap();
        let bypass = NoRly::new([unknown.clone()]);

        assert_eq!(
            pipeline.run(&context(), &bypass),
            Err(PipelineError::UnknownHookName(unknown))
        );
    }

    #[test]
    fn construct_id_rejects_empty_and_noncanonical_values() {
        assert_eq!(ConstructId::parse("syne").unwrap().as_str(), "syne");
        for invalid in ["", "Syne", "syne_2", "-syne", "syne-"] {
            assert!(ConstructId::parse(invalid).is_err());
        }
    }

    #[test]
    fn outcome_accessors_preserve_consistent_observe_invariants() {
        let outcome = PreSendPipeline::new(Vec::new())
            .unwrap()
            .run(&context(), &NoRly::default())
            .unwrap();

        assert!(matches!(outcome.decision(), HookDecision::Continue));
        assert_eq!(outcome.final_text(), Some("original"));
        assert!(outcome.to_construct().as_slice().is_empty());
        assert!(outcome.to_audit().as_slice().is_empty());
        assert!(outcome.sink_failures().is_empty());
    }

    #[test]
    fn sink_kind_strings_are_canonical() {
        assert_eq!(SinkKind::Feedback.as_str(), "feedback");
        assert_eq!(SinkKind::Audit.as_str(), "audit");
    }

    #[test]
    fn sink_errors_and_panics_degrade_explicitly() {
        let pipeline = PreSendPipeline::new(Vec::new())
            .expect("empty pipeline")
            .with_sinks(
                Box::new(FailingSink),
                Box::new(PanickingAudit),
                SinkFailurePolicy::Degrade,
            );

        let outcome = pipeline.run(&context(), &NoRly::default()).expect("run");

        assert_eq!(outcome.sink_failures.len(), 2);
        assert_eq!(outcome.sink_failures[0].sink(), SinkKind::Feedback);
        assert!(outcome.sink_failures[0].detail().contains("unavailable"));
        assert_eq!(outcome.sink_failures[1].detail(), "sink panicked");
    }

    #[test]
    fn fail_closed_sink_policy_returns_error() {
        let pipeline = PreSendPipeline::new(Vec::new())
            .expect("empty pipeline")
            .with_sinks(
                Box::new(FailingSink),
                Box::new(NullAuditSink),
                SinkFailurePolicy::FailClosed,
            );

        assert!(matches!(
            pipeline.run(&context(), &NoRly::default()),
            Err(PipelineError::SinkFailures(failures)) if failures.len() == 1
        ));
    }

    #[test]
    fn empty_pipeline_sends_original_text() {
        let pipeline = PreSendPipeline::new(Vec::new()).expect("empty pipeline");

        let outcome = pipeline.run(&context(), &NoRly::default()).expect("run");

        assert_eq!(outcome.final_text.as_deref(), Some("original"));
        assert!(outcome.to_audit.as_slice().is_empty());
    }

    #[test]
    fn production_lifecycle_defaults_to_observe_and_none_means_disabled() {
        let enabled = configured_pipeline(true, Vec::new())
            .expect("configured pipeline")
            .expect("enabled pipeline");
        let outcome = enabled.run(&context(), &NoRly::default()).expect("run");
        assert!(matches!(outcome.decision, HookDecision::Continue));

        assert!(
            configured_pipeline(false, Vec::new())
                .expect("disabled configuration")
                .is_none()
        );
    }
}
