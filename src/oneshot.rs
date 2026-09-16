//! One-shot, outbound-only send (`dione-send`, #426).
//!
//! The library half of the `dione-send` binary: a preflight that runs
//! entirely in-process against a [`LoadedConfig`] and a Discord REST
//! client, then at most one send. Nothing here initializes the gateway,
//! the MCP server, the config watcher, the mute store, the ingress ledger,
//! or the no_rly consent gate, and nothing here writes to disk — a bounce
//! *refuses* instead of holding, because nothing would ever release a held
//! ticket in a process that exits immediately.
//!
//! Preflight order (each step fails closed; `--dry-run` stops after it):
//!
//! 1. token from the *named* source only ([`TokenSource`]);
//! 2. `GET /users/@me` — the bot user id must equal `--expect-identity`;
//! 3. outbound target gate against `[[channels]]` with empty gateway state;
//! 4. chunk count under `delivery.{text_chunk_limit,chunk_mode}`;
//! 5. pre-send pipeline (when enabled) and the contradictionary judge.
//!
//! The binary prints exactly one JSON object ([`Outcome::to_json`]) on
//! stdout and maps [`Outcome::exit_code`] to the process status.

use crate::{
    config::{ChunkMode, ConfigError, LoadedConfig, ReadOnlyConfigError},
    discord::chunk_preserving_fences,
    gate::OutboundGate,
    no_rly::{OutboundJudge as _, Verdict},
    pre_send::{
        ChannelType, HookContext, HookDecision, NoRly, OutboundDestination, observe_pipeline,
    },
};
use camino::Utf8Path;
use serde::Serialize;
use serenity::{
    builder::CreateMessage,
    http::{Http, HttpBuilder, HttpError},
    model::{
        channel::Nonce,
        id::{ChannelId, UserId},
    },
};
use std::{
    collections::{BTreeMap, HashSet},
    fmt,
};

/// Where the bot token is read from. Ambient `DISCORD_BOT_TOKEN` never
/// wins by default: the environment is consulted only when named.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenSource {
    /// `token = "..."` in the config file.
    Config,
    /// The named environment variable.
    Env(String),
}

impl TokenSource {
    /// Parses `config` or `env:<VAR>`.
    pub fn parse(value: &str) -> Result<Self, String> {
        if value == "config" {
            return Ok(Self::Config);
        }
        if let Some(var) = value.strip_prefix("env:") {
            if var.is_empty() || !var.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
                return Err(format!(
                    "--token-source env:<VAR> needs a non-empty [A-Za-z0-9_] variable name, got {value:?}"
                ));
            }
            return Ok(Self::Env(var.to_string()));
        }
        Err(format!(
            "--token-source must be `config` or `env:<VAR>`, got {value:?}"
        ))
    }
}

impl fmt::Display for TokenSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config => f.write_str("config"),
            Self::Env(var) => write!(f, "env:{var}"),
        }
    }
}

/// What the caller asked for, already validated at the argv level.
#[derive(Debug, Clone)]
pub struct SendRequest {
    pub channel_id: u64,
    pub expect_identity: u64,
    pub message: String,
    pub token_source: TokenSource,
    pub allow_multi_chunk: bool,
    pub dry_run: bool,
    /// Caller-stable idempotency key (at most [`NONCE_MAX_LEN`] characters).
    /// Sent as `nonce` + `enforce_nonce: true` as defense-in-depth for a
    /// prompt manual replay only; it never makes an ambiguous failure
    /// retryable (see [`retryable_for`]).
    pub nonce: Option<String>,
}

/// Discord's limit on a message nonce, in characters.
pub const NONCE_MAX_LEN: usize = 25;

/// Refusal slugs, exactly as the contract names them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    /// Bad argv or unreadable stdin; exit 1, stderr carries the human text.
    Usage,
    ConfigInvalid,
    NoToken,
    IdentityMismatch,
    NotPermittedTarget,
    WouldChunk,
    ContradictionaryBounce,
    SendFailed,
}

impl Reason {
    pub const ALL: [Reason; 8] = [
        Reason::Usage,
        Reason::ConfigInvalid,
        Reason::NoToken,
        Reason::IdentityMismatch,
        Reason::NotPermittedTarget,
        Reason::WouldChunk,
        Reason::ContradictionaryBounce,
        Reason::SendFailed,
    ];

    pub fn slug(self) -> &'static str {
        match self {
            Self::Usage => "usage",
            Self::ConfigInvalid => "config_invalid",
            Self::NoToken => "no_token",
            Self::IdentityMismatch => "identity_mismatch",
            Self::NotPermittedTarget => "not_permitted_target",
            Self::WouldChunk => "would_chunk",
            Self::ContradictionaryBounce => "contradictionary_bounce",
            Self::SendFailed => "send_failed",
        }
    }

    /// Process exit status: 1 config/usage/IO, 2 preflight refusal,
    /// 3 Discord REST failure.
    pub fn exit_code(self) -> i32 {
        match self {
            Self::Usage | Self::ConfigInvalid => 1,
            Self::NoToken
            | Self::IdentityMismatch
            | Self::NotPermittedTarget
            | Self::WouldChunk
            | Self::ContradictionaryBounce => 2,
            Self::SendFailed => 3,
        }
    }

    /// Whether a refusal with this reason may ever be retried. Only
    /// `send_failed` can, and only for transient transport failures — the
    /// caller decides that per failure via [`Refusal::send_failed`].
    pub fn may_retry(self) -> bool {
        matches!(self, Self::SendFailed)
    }
}

/// One refusal: slug, sanitized human detail, and whether a retry could help.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub reason: Reason,
    pub detail: String,
    pub retryable: bool,
    /// True only for a POST failure that cannot prove no message was
    /// created (5xx, timeout, lost response). The caller must reconcile
    /// before resending instead of dropping or retrying blindly.
    pub delivery_ambiguous: bool,
}

impl Refusal {
    pub fn exit_code(&self) -> i32 {
        self.reason.exit_code()
    }

    /// A deterministic refusal — never retryable.
    pub fn new(reason: Reason, detail: impl Into<String>) -> Self {
        Self {
            reason,
            detail: detail.into(),
            retryable: false,
            delivery_ambiguous: false,
        }
    }

    /// A `send_failed` refusal; `retryable` is decided by the failure class.
    pub fn send_failed(detail: impl Into<String>, retryable: bool) -> Self {
        Self {
            reason: Reason::SendFailed,
            detail: detail.into(),
            retryable,
            delivery_ambiguous: false,
        }
    }

    /// A `send_failed` whose delivery is ambiguous: never retryable, and
    /// flagged so the caller routes to reconciliation.
    pub fn send_ambiguous(detail: impl Into<String>) -> Self {
        Self {
            reason: Reason::SendFailed,
            detail: format!(
                "{}; delivery ambiguous; reconcile before resending",
                detail.into()
            ),
            retryable: false,
            delivery_ambiguous: true,
        }
    }
}

/// The verified bot identity the send was bound to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub bot_user_id: u64,
    pub username: String,
    pub token_source: TokenSource,
}

/// Terminal result of one invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Sent {
        channel_id: u64,
        message_ids: Vec<u64>,
        identity: Identity,
    },
    /// `--dry-run` cleared every preflight step.
    PreflightOk {
        channel_id: u64,
        chunks: usize,
        identity: Identity,
    },
    Refused(Refusal),
}

impl From<Refusal> for Outcome {
    fn from(refusal: Refusal) -> Self {
        Self::Refused(refusal)
    }
}

impl Outcome {
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Sent { .. } | Self::PreflightOk { .. } => 0,
            Self::Refused(refusal) => refusal.reason.exit_code(),
        }
    }

    /// The single stdout JSON object. Snowflakes are strings; `retryable`
    /// is present on every shape.
    pub fn to_json(&self) -> serde_json::Value {
        match self {
            Self::Sent {
                channel_id,
                message_ids,
                identity,
            } => serde_json::json!({
                "ok": true,
                "status": "sent",
                "retryable": false,
                "delivery_ambiguous": false,
                "channel_id": channel_id.to_string(),
                "message_ids": message_ids.iter().map(ToString::to_string).collect::<Vec<_>>(),
                "identity": identity_json(identity),
            }),
            Self::PreflightOk {
                channel_id,
                chunks,
                identity,
            } => serde_json::json!({
                "ok": true,
                "status": "preflight_ok",
                "retryable": false,
                "delivery_ambiguous": false,
                "channel_id": channel_id.to_string(),
                "chunks": chunks,
                "identity": identity_json(identity),
            }),
            Self::Refused(refusal) => serde_json::json!({
                "ok": false,
                "status": "refused",
                "reason": refusal.reason.slug(),
                "detail": refusal.detail,
                "retryable": refusal.retryable,
                "delivery_ambiguous": refusal.delivery_ambiguous,
            }),
        }
    }
}

fn identity_json(identity: &Identity) -> serde_json::Value {
    serde_json::json!({
        "bot_user_id": identity.bot_user_id.to_string(),
        "username": identity.username,
        "token_source": identity.token_source.to_string(),
    })
}

// ── Config (step 0) ─────────────────────────────────────────────────────────

/// Loads the config read-only for a one-shot send, mapping every failure to
/// a `config_invalid` refusal whose detail is **redacted**: it names the
/// path, the error kind, and a line/column, but never a source excerpt or
/// message text that could echo config values (a malformed `token = "..."`
/// line would otherwise reach stdout verbatim).
pub fn load_config_for_send(config_path: &Utf8Path) -> Result<LoadedConfig, Refusal> {
    crate::config::load_config_readonly(config_path).map_err(|error| {
        Refusal::new(
            Reason::ConfigInvalid,
            redacted_config_error(config_path, &error),
        )
    })
}

/// The stable, value-free diagnostic for a config load failure.
pub fn redacted_config_error(config_path: &Utf8Path, error: &ReadOnlyConfigError) -> String {
    match error {
        ReadOnlyConfigError::Config(ConfigError::NotFound { path }) => {
            format!("config file not found at {path}")
        }
        ReadOnlyConfigError::Config(ConfigError::Parse(parse)) => {
            let location = parse
                .span()
                .and_then(|span| line_col_at(config_path, span.start))
                .map(|(line, col)| format!(" at line {line}, column {col}"))
                .unwrap_or_default();
            format!("{config_path}: TOML parse error{location} (source excerpt withheld)")
        }
        ReadOnlyConfigError::Config(ConfigError::Io(io)) => {
            format!("{config_path}: I/O error reading config ({:?})", io.kind())
        }
        ReadOnlyConfigError::Sidecar(_) => format!(
            "{config_path}: contradictionary sidecar failed to load (details withheld; see stderr)"
        ),
        ReadOnlyConfigError::Generation(_) => {
            format!("{config_path}: configuration generation counter exhausted")
        }
    }
}

/// 1-based line/column of byte `offset` in the file at `path`, computed
/// locally so no file content leaves this function.
fn line_col_at(path: &Utf8Path, offset: usize) -> Option<(usize, usize)> {
    let bytes = std::fs::read(path.as_std_path()).ok()?;
    let offset = offset.min(bytes.len());
    let line = bytes[..offset].iter().filter(|b| **b == b'\n').count() + 1;
    let line_start = bytes[..offset]
        .iter()
        .rposition(|b| *b == b'\n')
        .map_or(0, |i| i + 1);
    Some((line, offset - line_start + 1))
}

// ── Preflight steps ─────────────────────────────────────────────────────────

/// Step 1: the token from the named source only. `env` abstracts the
/// process environment so the choice is testable without mutating it.
pub fn select_token(
    config: &LoadedConfig,
    source: &TokenSource,
    env: impl Fn(&str) -> Option<String>,
) -> Result<String, Refusal> {
    let token = match source {
        TokenSource::Config => config.raw.token.clone(),
        TokenSource::Env(var) => env(var),
    };
    match token.map(|t| t.trim().to_string()) {
        Some(token) if !token.is_empty() => Ok(token),
        _ => Err(Refusal::new(
            Reason::NoToken,
            format!(
                "no bot token at token source `{source}` (ambient DISCORD_BOT_TOKEN is never consulted unless named)"
            ),
        )),
    }
}

/// Step 2 (pure half): bind the send to the numeric bot user id only.
pub fn check_identity(
    current: (u64, String),
    expected: u64,
    source: &TokenSource,
) -> Result<Identity, Refusal> {
    let (bot_user_id, username) = current;
    if bot_user_id != expected {
        return Err(Refusal::new(
            Reason::IdentityMismatch,
            format!(
                "token at `{source}` authenticates as bot user {bot_user_id}, expected {expected}"
            ),
        ));
    }
    Ok(Identity {
        bot_user_id,
        username,
        token_source: source.clone(),
    })
}

/// Step 2 (REST half): `GET /users/@me` through `http`, then [`check_identity`].
pub async fn verify_identity(
    http: &Http,
    expected: u64,
    source: &TokenSource,
    token: &str,
) -> Result<Identity, Refusal> {
    match http.get_current_user().await {
        Ok(user) => check_identity((user.id.get(), user.name.clone()), expected, source),
        Err(error) => {
            let class = classify_rest_error(&error, token);
            if class.status == Some(401) {
                return Err(Refusal::new(
                    Reason::NoToken,
                    format!(
                        "Discord rejected the token at `{source}` (HTTP 401) during identity lookup"
                    ),
                ));
            }
            Err(Refusal::send_failed(
                format!("identity lookup GET /users/@me failed: {}", class.detail),
                retryable_for(Stage::Identity, class.class, false),
            ))
        }
    }
}

/// Step 3: the config-only outbound gate. DM channels and threads of
/// allowed parents need gateway caches that a one-shot never populates, so
/// they are refused here by construction, and the detail says so.
pub fn check_target(config: &LoadedConfig, channel_id: u64) -> Result<(), Refusal> {
    let permitted = OutboundGate::check_channel_with_threads(
        config,
        channel_id,
        &HashSet::new(),
        &BTreeMap::new(),
    );
    if permitted {
        return Ok(());
    }
    Err(Refusal::new(
        Reason::NotPermittedTarget,
        format!(
            "channel {channel_id} is not in [[channels]]; DM and thread-of-allowed-parent rules need gateway state and are unavailable in one-shot mode"
        ),
    ))
}

/// Discord's inline message content limit in characters.
pub const DISCORD_MESSAGE_LIMIT: usize = 2000;

/// Step 4: chunk exactly as `deliver_reply` would, refusing more than one
/// chunk unless the caller opted in.
pub fn chunk_message(
    config: &LoadedConfig,
    text: &str,
    allow_multi_chunk: bool,
) -> Result<Vec<String>, Refusal> {
    if text.trim().is_empty() {
        return Err(Refusal::new(
            Reason::WouldChunk,
            "message is empty; nothing to send",
        ));
    }
    let limit = config.delivery.text_chunk_limit;
    let (effective_limit, effective_mode) = if limit == 0 {
        (2000, ChunkMode::Paragraph)
    } else {
        (limit, config.delivery.chunk_mode)
    };
    if effective_limit > DISCORD_MESSAGE_LIMIT {
        return Err(Refusal::new(
            Reason::ConfigInvalid,
            format!(
                "delivery.text_chunk_limit={effective_limit} exceeds Discord's {DISCORD_MESSAGE_LIMIT}-character message limit; a chunk that size would be rejected at send time"
            ),
        ));
    }
    let chunks = chunk_preserving_fences(text, effective_limit, effective_mode).map_err(|e| {
        Refusal::new(
            Reason::WouldChunk,
            format!("message cannot be delivered inline: {e}"),
        )
    })?;
    if chunks.is_empty() {
        return Err(Refusal::new(
            Reason::WouldChunk,
            "message is empty after chunking; nothing to send",
        ));
    }
    if chunks.len() > 1 && !allow_multi_chunk {
        return Err(Refusal::new(
            Reason::WouldChunk,
            format!(
                "message would be delivered as {} chunks under text_chunk_limit={effective_limit} chunk_mode={effective_mode:?}; pass --allow-multi-chunk to accept",
                chunks.len()
            ),
        ));
    }
    Ok(chunks.into_iter().map(|c| c.rendered).collect())
}

/// Step 5: the pre-send pipeline (in-process, observe mode, when enabled)
/// and the contradictionary judge. Returns the text to send — the pipeline
/// may rewrite it — or a refusal. A `Bounce` refuses outright: there is no
/// consent gate, no journal, and no held ticket in one-shot mode.
pub fn judge_message(
    config: &LoadedConfig,
    channel_id: u64,
    identity: &Identity,
    text: &str,
) -> Result<String, Refusal> {
    let mut final_text = text.to_string();
    if config.raw.pre_send.enabled {
        let pipeline = observe_pipeline(Vec::new()).map_err(|e| {
            Refusal::new(
                Reason::ConfigInvalid,
                format!("pre-send pipeline could not be built: {e}"),
            )
        })?;
        let context = HookContext::new(
            text,
            OutboundDestination::Channel(ChannelId::new(channel_id)),
            ChannelType::Public,
            config.pre_send_construct_id.clone(),
        )
        .with_author_id(Some(UserId::new(identity.bot_user_id)))
        .with_metadata("outbound_surface", "oneshot");
        let outcome = pipeline.run(&context, &NoRly::default()).map_err(|e| {
            Refusal::new(
                Reason::ConfigInvalid,
                format!("pre-send pipeline failed: {e}"),
            )
        })?;
        for failure in outcome.sink_failures() {
            tracing::warn!(
                sink = failure.sink().as_str(),
                error = failure.detail(),
                "pre-send assessment sink degraded"
            );
        }
        match outcome.decision() {
            HookDecision::Halt { reason } => {
                return Err(Refusal::new(
                    Reason::ContradictionaryBounce,
                    format!("pre-send pipeline halted the message: {reason}"),
                ));
            }
            HookDecision::Redirect { channel_id: target } => {
                return Err(Refusal::new(
                    Reason::NotPermittedTarget,
                    format!(
                        "pre-send pipeline redirected to channel {target}; redirects are unavailable in one-shot mode"
                    ),
                ));
            }
            HookDecision::Continue | HookDecision::Rewrite { .. } => {}
        }
        if let Some(rewritten) = outcome.final_text() {
            final_text = rewritten.to_string();
        }
    }
    if let Some(judge) = config.contradictionary.as_ref()
        && let Verdict::Bounce(reason) = judge.judge(&final_text)
    {
        return Err(Refusal::new(
            Reason::ContradictionaryBounce,
            format!(
                "contradictionary blocked the message: {reason} (no hold ticket is issued in one-shot mode; revise and resend)"
            ),
        ));
    }
    Ok(final_text)
}

/// Per-chunk nonces derived from the caller's key: the key itself for a
/// single chunk, `<key>-<index>` for each chunk of a multi-chunk send so
/// Discord's `enforce_nonce` deduplicates every chunk independently.
/// Refuses (`usage`) when a derived nonce exceeds [`NONCE_MAX_LEN`].
pub fn chunk_nonces(
    nonce: Option<&str>,
    chunk_count: usize,
) -> Result<Vec<Option<String>>, Refusal> {
    let Some(nonce) = nonce else {
        return Ok(vec![None; chunk_count]);
    };
    if nonce.is_empty() {
        return Err(Refusal::new(Reason::Usage, "--nonce must not be empty"));
    }
    let derived: Vec<String> = if chunk_count <= 1 {
        vec![nonce.to_string()]
    } else {
        (0..chunk_count).map(|i| format!("{nonce}-{i}")).collect()
    };
    if let Some(too_long) = derived.iter().find(|n| n.chars().count() > NONCE_MAX_LEN) {
        return Err(Refusal::new(
            Reason::Usage,
            format!(
                "nonce {:?} is {} characters; Discord allows at most {NONCE_MAX_LEN} (multi-chunk sends append `-<index>`)",
                too_long,
                too_long.chars().count()
            ),
        ));
    }
    Ok(derived.into_iter().map(Some).collect())
}

/// The production REST client. Serenity's internal ratelimiter is
/// **disabled** on purpose: with it enabled a 429 sleeps for `retry-after`
/// and retries inside the process, so the promised
/// `send_failed`/`retryable:true` would never surface and a caller's
/// timeout could kill the process with no JSON receipt. The one-shot never
/// retries internally; the caller owns retry. The API base is always
/// Discord's: no endpoint override exists in production bytes.
pub fn build_http(token: &str) -> Http {
    HttpBuilder::new(token).ratelimiter_disabled(true).build()
}

/// Test-seam variant of [`build_http`] whose `api_base` replaces the
/// Discord API base URL (a local mock). Exists only with the
/// `oneshot-test-seam` cargo feature.
#[cfg(feature = "oneshot-test-seam")]
pub fn build_http_with_api_base(token: &str, api_base: &str) -> Http {
    HttpBuilder::new(token)
        .ratelimiter_disabled(true)
        .proxy(api_base)
        .build()
}

// ── Driver ──────────────────────────────────────────────────────────────────

/// Runs preflight and, unless `dry_run`, the send, against Discord.
pub async fn run(config: &LoadedConfig, request: &SendRequest) -> Outcome {
    match run_inner(config, request, build_http).await {
        Ok(outcome) => outcome,
        Err(refusal) => Outcome::Refused(refusal),
    }
}

/// [`run`] against a replacement API base (the test suite's mock Discord).
/// Exists only with the `oneshot-test-seam` cargo feature.
#[cfg(feature = "oneshot-test-seam")]
pub async fn run_with_api_base(
    config: &LoadedConfig,
    request: &SendRequest,
    api_base: &str,
) -> Outcome {
    match run_inner(config, request, |token| {
        build_http_with_api_base(token, api_base)
    })
    .await
    {
        Ok(outcome) => outcome,
        Err(refusal) => Outcome::Refused(refusal),
    }
}

async fn run_inner(
    config: &LoadedConfig,
    request: &SendRequest,
    build_http: impl FnOnce(&str) -> Http,
) -> Result<Outcome, Refusal> {
    let token = select_token(config, &request.token_source, |var| std::env::var(var).ok())?;
    let http = build_http(&token);
    let identity = verify_identity(
        &http,
        request.expect_identity,
        &request.token_source,
        &token,
    )
    .await?;
    check_target(config, request.channel_id)?;
    let chunks = chunk_message(config, &request.message, request.allow_multi_chunk)?;
    let final_text = judge_message(config, request.channel_id, &identity, &request.message)?;
    // A rewriting hook may change the chunk count; re-apply the same rule.
    let chunks = if final_text == request.message {
        chunks
    } else {
        chunk_message(config, &final_text, request.allow_multi_chunk)?
    };
    let nonces = chunk_nonces(request.nonce.as_deref(), chunks.len())?;

    if request.dry_run {
        return Ok(Outcome::PreflightOk {
            channel_id: request.channel_id,
            chunks: chunks.len(),
            identity,
        });
    }

    let channel = ChannelId::new(request.channel_id);
    let mut message_ids = Vec::with_capacity(chunks.len());
    for (index, (chunk, nonce)) in chunks.iter().zip(&nonces).enumerate() {
        let mut builder = CreateMessage::new().content(chunk);
        if let Some(nonce) = nonce {
            builder = builder
                .nonce(Nonce::String(nonce.clone()))
                .enforce_nonce(true);
        }
        match channel.send_message(&http, builder).await {
            Ok(message) => message_ids.push(message.id.get()),
            Err(error) => {
                let class = classify_rest_error(&error, &token);
                if message_ids.is_empty() {
                    let detail = format!(
                        "POST /channels/{}/messages failed on chunk 1 of {}: {}",
                        request.channel_id,
                        chunks.len(),
                        class.detail
                    );
                    if class.class.post_is_ambiguous() {
                        return Err(Refusal::send_ambiguous(detail));
                    }
                    return Err(Refusal::send_failed(
                        detail,
                        retryable_for(Stage::Post, class.class, nonce.is_some()),
                    ));
                }
                // A partial delivery must not be retried blindly — the
                // caller would double-post the chunks that already landed.
                let partial = Refusal::send_failed(
                    format!(
                        "POST /channels/{}/messages failed on chunk {} of {} after {} delivered (message_ids {:?}): {}",
                        request.channel_id,
                        index + 1,
                        chunks.len(),
                        message_ids.len(),
                        message_ids
                            .iter()
                            .map(ToString::to_string)
                            .collect::<Vec<_>>(),
                        class.detail
                    ),
                    false,
                );
                if class.class.post_is_ambiguous() {
                    return Err(Refusal::send_ambiguous(partial.detail));
                }
                return Err(partial);
            }
        }
    }
    Ok(Outcome::Sent {
        channel_id: request.channel_id,
        message_ids,
        identity,
    })
}

// ── REST failure classification ─────────────────────────────────────────────

/// What kind of failure a REST call produced, independent of when.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureClass {
    /// HTTP 429: Discord created nothing.
    RateLimited,
    /// HTTP 5xx: the server may have committed the message before failing.
    ServerError,
    /// Any other 4xx: deterministic.
    ClientError,
    /// The connection was never established: the request was never written.
    ConnectFailed,
    /// The request may have been sent and the response lost.
    Timeout,
    /// Other transport failure after the request may have been written.
    Transport,
    /// Non-HTTP client error (URL, header, decode...): deterministic.
    Other,
}

impl FailureClass {
    /// True when, for a POST, this class cannot prove no message was created.
    pub fn post_is_ambiguous(self) -> bool {
        matches!(self, Self::ServerError | Self::Timeout | Self::Transport)
    }
}

/// Which call failed. Identity lookup is a GET — nothing was created.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Identity,
    Post,
}

/// The retry table. On POST, only failures that provably created nothing
/// (429, connection never established) are retryable; ambiguous failures
/// (5xx, timeout, lost response) are never retryable, **even with a
/// nonce**: Discord's `enforce_nonce` deduplicates only within an
/// undocumented few-minute window, and a caller that persists retryable
/// work across later runs would double-post past it. The `has_nonce`
/// dimension is kept so the table test proves it changes no row. Identity
/// lookup is a GET, so its transport/5xx failures stay retryable. 4xx
/// (other than 429) and non-HTTP client errors are never retryable.
pub fn retryable_for(stage: Stage, class: FailureClass, _has_nonce: bool) -> bool {
    match class {
        FailureClass::RateLimited | FailureClass::ConnectFailed => true,
        FailureClass::ClientError | FailureClass::Other => false,
        FailureClass::ServerError | FailureClass::Timeout | FailureClass::Transport => {
            match stage {
                Stage::Identity => true,
                Stage::Post => false,
            }
        }
    }
}

/// A sanitized view of one serenity REST failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestFailure {
    pub status: Option<u16>,
    pub class: FailureClass,
    pub detail: String,
}

/// HTTP status to failure class.
pub fn classify_status(status: u16) -> FailureClass {
    match status {
        429 => FailureClass::RateLimited,
        500..=599 => FailureClass::ServerError,
        _ => FailureClass::ClientError,
    }
}

/// Classifies a serenity error. The token never reaches the detail.
pub fn classify_rest_error(error: &serenity::Error, token: &str) -> RestFailure {
    let (status, class, detail) = match error {
        serenity::Error::Http(HttpError::UnsuccessfulRequest(response)) => {
            let status = response.status_code.as_u16();
            (
                Some(status),
                classify_status(status),
                format!(
                    "Discord HTTP {status} on {} {}: {} (code {})",
                    response.method, response.url, response.error.message, response.error.code
                ),
            )
        }
        serenity::Error::Http(HttpError::Request(request_error)) => {
            let class = if request_error.is_connect() {
                FailureClass::ConnectFailed
            } else if request_error.is_timeout() {
                FailureClass::Timeout
            } else {
                FailureClass::Transport
            };
            (
                None,
                class,
                format!("transport error ({class:?}): {request_error}"),
            )
        }
        serenity::Error::Http(other) => (
            None,
            FailureClass::Other,
            format!("HTTP client error: {other}"),
        ),
        other => (
            None,
            FailureClass::Other,
            format!("Discord client error: {other}"),
        ),
    };
    RestFailure {
        status,
        class,
        detail: scrub_token(&detail, token),
    }
}

/// Belt and braces: no formatted error may carry the token.
pub fn scrub_token(detail: &str, token: &str) -> String {
    if token.is_empty() {
        return detail.to_string();
    }
    detail.replace(token, "[redacted]")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ChannelConfig, Config};
    use crate::contradictionary::{Action, Entry, MatchMode};

    fn config_with(channel: &str, mutate: impl FnOnce(&mut Config)) -> LoadedConfig {
        let mut raw = Config::default();
        raw.channels.push(ChannelConfig {
            id: channel.into(),
            ..Default::default()
        });
        mutate(&mut raw);
        LoadedConfig::try_from_raw(raw).expect("test configuration generation")
    }

    fn identity() -> Identity {
        Identity {
            bot_user_id: 7,
            username: "trundle".into(),
            token_source: TokenSource::Config,
        }
    }

    #[test]
    fn token_source_parses_config_and_env_only() {
        assert_eq!(TokenSource::parse("config"), Ok(TokenSource::Config));
        assert_eq!(
            TokenSource::parse("env:TRUNDLE_TOKEN"),
            Ok(TokenSource::Env("TRUNDLE_TOKEN".into()))
        );
        assert!(TokenSource::parse("env:").is_err());
        assert!(TokenSource::parse("env:BAD-NAME").is_err());
        assert!(TokenSource::parse("environment").is_err());
        assert!(TokenSource::parse("").is_err());
    }

    #[test]
    fn config_source_ignores_ambient_env() {
        let config = config_with("42", |raw| raw.token = Some("config-token".into()));
        let env = |var: &str| (var == "DISCORD_BOT_TOKEN").then(|| "ambient".to_string());
        assert_eq!(
            select_token(&config, &TokenSource::Config, env).unwrap(),
            "config-token"
        );
    }

    #[test]
    fn config_source_without_token_refuses_no_token_even_if_env_set() {
        let config = config_with("42", |_| {});
        let env = |_: &str| Some("leaked-env-token".to_string());
        let refusal = select_token(&config, &TokenSource::Config, env).unwrap_err();
        assert_eq!(refusal.reason, Reason::NoToken);
        assert!(!refusal.retryable);
        assert!(!refusal.detail.contains("leaked-env-token"));
    }

    #[test]
    fn env_source_reads_only_the_named_variable() {
        let config = config_with("42", |raw| raw.token = Some("config-token".into()));
        let env = |var: &str| match var {
            "DISCORD_BOT_TOKEN" => Some("ambient".to_string()),
            "TRUNDLE_TOKEN" => Some("named".to_string()),
            _ => None,
        };
        assert_eq!(
            select_token(&config, &TokenSource::Env("TRUNDLE_TOKEN".into()), env).unwrap(),
            "named"
        );
        let missing = select_token(&config, &TokenSource::Env("OTHER".into()), env).unwrap_err();
        assert_eq!(missing.reason, Reason::NoToken);
        assert!(missing.detail.contains("env:OTHER"));
    }

    #[test]
    fn blank_token_is_no_token() {
        let config = config_with("42", |raw| raw.token = Some("   ".into()));
        let refusal = select_token(&config, &TokenSource::Config, |_| None).unwrap_err();
        assert_eq!(refusal.reason, Reason::NoToken);
    }

    #[test]
    fn identity_binds_to_numeric_id_not_username() {
        let ok = check_identity((7, "anything".into()), 7, &TokenSource::Config).unwrap();
        assert_eq!(ok.bot_user_id, 7);
        assert_eq!(ok.username, "anything");
        let bad = check_identity((8, "trundle".into()), 7, &TokenSource::Config).unwrap_err();
        assert_eq!(bad.reason, Reason::IdentityMismatch);
        assert!(!bad.retryable);
        assert!(bad.detail.contains("bot user 8, expected 7"));
    }

    #[test]
    fn target_gate_accepts_configured_channel_only() {
        let config = config_with("42", |_| {});
        assert!(check_target(&config, 42).is_ok());
        let refusal = check_target(&config, 43).unwrap_err();
        assert_eq!(refusal.reason, Reason::NotPermittedTarget);
        assert!(refusal.detail.contains("unavailable in one-shot mode"));
        assert!(refusal.detail.contains("DM"));
        assert!(refusal.detail.contains("thread"));
    }

    #[test]
    fn single_chunk_passes_multi_chunk_refuses_unless_allowed() {
        let config = config_with("42", |raw| {
            raw.delivery.text_chunk_limit = 10;
            raw.delivery.chunk_mode = ChunkMode::Length;
        });
        assert_eq!(
            chunk_message(&config, "short", false).unwrap(),
            vec!["short"]
        );
        let long = "0123456789abcdef";
        let refusal = chunk_message(&config, long, false).unwrap_err();
        assert_eq!(refusal.reason, Reason::WouldChunk);
        assert!(refusal.detail.contains("2 chunks"));
        assert!(refusal.detail.contains("--allow-multi-chunk"));
        assert_eq!(chunk_message(&config, long, true).unwrap().len(), 2);
    }

    #[test]
    fn empty_message_refuses_would_chunk() {
        let config = config_with("42", |_| {});
        assert_eq!(
            chunk_message(&config, "", false).unwrap_err().reason,
            Reason::WouldChunk
        );
    }

    #[test]
    fn contradictionary_bounce_refuses() {
        let config = config_with("42", |raw| {
            raw.contradictionary.enabled = true;
            raw.contradictionary.entries = vec![Entry {
                pattern: "straightforward".into(),
                action: Action::Block,
                match_mode: MatchMode::Word,
                reason: Some("nothing ever is".into()),
            }];
        });
        let refusal =
            judge_message(&config, 42, &identity(), "this is straightforward").unwrap_err();
        assert_eq!(refusal.reason, Reason::ContradictionaryBounce);
        assert!(!refusal.retryable);
        assert!(refusal.detail.contains("straightforward"));
        assert_eq!(
            judge_message(&config, 42, &identity(), "this is subtle").unwrap(),
            "this is subtle"
        );
    }

    #[test]
    fn pre_send_observe_pipeline_passes_text_through() {
        let config = config_with("42", |raw| raw.pre_send.enabled = true);
        assert_eq!(
            judge_message(&config, 42, &identity(), "hello").unwrap(),
            "hello"
        );
    }

    #[test]
    fn retryable_table_covers_every_reason_and_status() {
        // Deterministic refusals: never retryable, whatever the detail.
        for reason in Reason::ALL {
            let refusal = Refusal::new(reason, "x");
            assert!(
                !refusal.retryable,
                "{} defaulted to retryable",
                reason.slug()
            );
            let json = Outcome::Refused(refusal).to_json();
            assert_eq!(json["retryable"], serde_json::json!(false));
            assert_eq!(json["delivery_ambiguous"], serde_json::json!(false));
            assert_eq!(json["reason"], serde_json::json!(reason.slug()));
            assert_eq!(reason.may_retry(), reason == Reason::SendFailed);
        }
        // Exit codes per contract.
        assert_eq!(Reason::ConfigInvalid.exit_code(), 1);
        for reason in [
            Reason::NoToken,
            Reason::IdentityMismatch,
            Reason::NotPermittedTarget,
            Reason::WouldChunk,
            Reason::ContradictionaryBounce,
        ] {
            assert_eq!(reason.exit_code(), 2, "{}", reason.slug());
        }
        assert_eq!(Reason::SendFailed.exit_code(), 3);
        // Success shapes carry retryable:false too.
        let sent = Outcome::Sent {
            channel_id: 1,
            message_ids: vec![2],
            identity: identity(),
        };
        assert_eq!(sent.to_json()["retryable"], serde_json::json!(false));
        assert_eq!(
            sent.to_json()["delivery_ambiguous"],
            serde_json::json!(false)
        );
        let ok = Outcome::PreflightOk {
            channel_id: 1,
            chunks: 1,
            identity: identity(),
        };
        assert_eq!(ok.to_json()["retryable"], serde_json::json!(false));
        assert_eq!(ok.to_json()["status"], serde_json::json!("preflight_ok"));
        assert_eq!(ok.to_json()["delivery_ambiguous"], serde_json::json!(false));
        // send_failed: {nonce, no-nonce} x {class} x {stage}. Every row is
        // explicit so nothing defaults into a retry loop or a double-post.
        use FailureClass::*;
        let rows: &[(Stage, FailureClass, bool, bool)] = &[
            // stage, class, has_nonce, retryable
            (Stage::Post, RateLimited, false, true),
            (Stage::Post, RateLimited, true, true),
            (Stage::Post, ServerError, false, false),
            (Stage::Post, ServerError, true, false),
            (Stage::Post, ConnectFailed, false, true),
            (Stage::Post, ConnectFailed, true, true),
            (Stage::Post, Timeout, false, false),
            (Stage::Post, Timeout, true, false),
            (Stage::Post, Transport, false, false),
            (Stage::Post, Transport, true, false),
            (Stage::Post, ClientError, false, false),
            (Stage::Post, ClientError, true, false),
            (Stage::Post, Other, false, false),
            (Stage::Post, Other, true, false),
            (Stage::Identity, ServerError, false, true),
            (Stage::Identity, ConnectFailed, false, true),
            (Stage::Identity, Timeout, false, true),
            (Stage::Identity, Transport, false, true),
            (Stage::Identity, RateLimited, false, true),
            (Stage::Identity, ClientError, false, false),
            (Stage::Identity, Other, false, false),
        ];
        for (stage, class, has_nonce, retryable) in rows {
            assert_eq!(
                retryable_for(*stage, *class, *has_nonce),
                *retryable,
                "{stage:?} {class:?} nonce={has_nonce}"
            );
            // The nonce never changes a row.
            assert_eq!(
                retryable_for(*stage, *class, !*has_nonce),
                *retryable,
                "{stage:?} {class:?} nonce flipped"
            );
            let ambiguous = *stage == Stage::Post && class.post_is_ambiguous();
            let refusal = if ambiguous {
                Refusal::send_ambiguous(format!("{class:?}"))
            } else {
                Refusal::send_failed(format!("{class:?}"), *retryable)
            };
            assert_eq!(refusal.reason, Reason::SendFailed);
            assert_eq!(refusal.retryable, *retryable);
            assert_eq!(refusal.delivery_ambiguous, ambiguous);
            let json = Outcome::Refused(refusal).to_json();
            assert_eq!(json["retryable"], serde_json::json!(*retryable));
            assert_eq!(json["delivery_ambiguous"], serde_json::json!(ambiguous));
            if ambiguous {
                assert!(
                    json["detail"]
                        .as_str()
                        .unwrap()
                        .contains("delivery ambiguous; reconcile before resending")
                );
            }
        }
        // HTTP statuses map to classes.
        for (status, class) in [
            (400, ClientError),
            (401, ClientError),
            (403, ClientError),
            (404, ClientError),
            (429, RateLimited),
            (500, ServerError),
            (502, ServerError),
            (503, ServerError),
        ] {
            assert_eq!(classify_status(status), class, "HTTP {status}");
        }
        let other = classify_rest_error(
            &serenity::Error::Other("connection refused (secret-token-value)"),
            "secret-token-value",
        );
        assert_eq!(other.class, Other);
        assert_eq!(other.status, None);
    }

    #[test]
    fn chunk_nonces_derive_per_chunk_and_bound_length() {
        assert_eq!(chunk_nonces(None, 2).unwrap(), vec![None, None]);
        assert_eq!(chunk_nonces(Some("k"), 1).unwrap(), vec![Some("k".into())]);
        assert_eq!(
            chunk_nonces(Some("k"), 2).unwrap(),
            vec![Some("k-0".into()), Some("k-1".into())]
        );
        let max = "x".repeat(NONCE_MAX_LEN);
        assert!(chunk_nonces(Some(&max), 1).is_ok());
        let refusal = chunk_nonces(Some(&max), 2).unwrap_err();
        assert_eq!(refusal.reason, Reason::Usage);
        assert_eq!(refusal.exit_code(), 1);
        assert_eq!(chunk_nonces(Some(""), 1).unwrap_err().reason, Reason::Usage);
        assert_eq!(
            chunk_nonces(Some(&"y".repeat(26)), 1).unwrap_err().reason,
            Reason::Usage
        );
    }

    #[test]
    fn chunk_limit_above_discord_max_is_config_invalid() {
        let config = config_with("42", |raw| raw.delivery.text_chunk_limit = 4000);
        let refusal = chunk_message(&config, "hello", false).unwrap_err();
        assert_eq!(refusal.reason, Reason::ConfigInvalid);
        assert!(refusal.detail.contains("4000"));
        let ok = config_with("42", |raw| raw.delivery.text_chunk_limit = 2000);
        assert!(chunk_message(&ok, "hello", false).is_ok());
    }

    #[test]
    fn rest_error_detail_never_carries_the_token() {
        let failure = classify_rest_error(
            &serenity::Error::Other("rejected secret-token-value"),
            "secret-token-value",
        );
        assert!(!failure.detail.contains("secret-token-value"));
        assert!(failure.detail.contains("[redacted]"));
        assert_eq!(scrub_token("plain", ""), "plain");
    }

    #[test]
    fn malformed_config_detail_never_echoes_token_bytes() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = camino::Utf8PathBuf::from_path_buf(dir.path().join("config.toml")).unwrap();
        std::fs::write(
            &path,
            "token = \"SECRETTOKENBYTES\n[[channels]]\nid = \"1\"\n",
        )
        .unwrap();
        let refusal = load_config_for_send(&path).unwrap_err();
        assert_eq!(refusal.reason, Reason::ConfigInvalid);
        assert!(!refusal.retryable);
        let json = Outcome::Refused(refusal.clone()).to_json().to_string();
        assert!(!json.contains("SECRETTOKENBYTES"), "{json}");
        assert!(json.contains("config_invalid"));
        assert!(json.contains("\"retryable\":false"));
        assert!(refusal.detail.contains("line 1"), "{}", refusal.detail);
        assert!(refusal.detail.contains("source excerpt withheld"));
        assert_eq!(refusal.exit_code(), 1);

        let missing = load_config_for_send(&path.with_file_name("none.toml")).unwrap_err();
        assert!(missing.detail.contains("not found"));
        // Reporting a missing file wrote nothing.
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn json_shapes_use_string_snowflakes() {
        let sent = Outcome::Sent {
            channel_id: 1549533156150087812,
            message_ids: vec![1549533248667910167],
            identity: Identity {
                bot_user_id: 1549533370038362205,
                username: "trundle".into(),
                token_source: TokenSource::Env("TRUNDLE_TOKEN".into()),
            },
        };
        let json = sent.to_json();
        assert_eq!(json["ok"], serde_json::json!(true));
        assert_eq!(json["status"], serde_json::json!("sent"));
        assert_eq!(json["channel_id"], serde_json::json!("1549533156150087812"));
        assert_eq!(
            json["message_ids"],
            serde_json::json!(["1549533248667910167"])
        );
        assert_eq!(
            json["identity"]["bot_user_id"],
            serde_json::json!("1549533370038362205")
        );
        assert_eq!(
            json["identity"]["token_source"],
            serde_json::json!("env:TRUNDLE_TOKEN")
        );
        assert_eq!(sent.exit_code(), 0);
    }
}
