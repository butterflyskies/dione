# Rate Limiter -- Design Specification

**Status:** Draft

## Problem

Two AI agents sharing a Discord server will ping-pong indefinitely without a
circuit breaker, burning tokens. Enforcement must be server-side -- bots cannot
be trusted to self-limit, and each dione instance runs independently with no
shared state. The rate limiter must be substrate-agnostic in its core types so
it can eventually support platforms beyond Discord.

## Solution

Token bucket rate limiter per (sender, channel) pair, keyed by participant
identity and channel reference. Each bucket tracks remaining tokens within a
time window. When tokens are exhausted, the bucket enters a cooldown period
before refilling.

## State Machine

```
Idle --> Active(remaining: N) --> Exhausted --> Cooldown(expires: T) --> Idle
```

- **Idle:** No messages seen for this (sender, channel) pair, or bucket has
  been fully reset after cooldown. All messages are allowed; first message
  transitions to Active.
- **Active(remaining: N):** Messages are being consumed. Each allowed message
  decrements `remaining` by exactly 1. When `remaining` reaches 0, transitions
  to Exhausted.
- **Exhausted:** Token budget fully consumed. All messages are rate-limited
  according to the configured overflow policy (drop or buffer). Transitions to
  Cooldown when the window expires.
- **Cooldown(expires: T):** Waiting for the cooldown period to elapse. Messages
  remain rate-limited. When `expires` is reached, bucket resets and transitions
  back to Idle.

## Types (Rust)

```rust
enum OverflowPolicy {
    Drop,
    Buffer,
}

struct ScopeConfig {
    tokens: u32,
    window: Duration,
    cooldown: Duration,
    notify: bool,
    overflow: OverflowPolicy,
}

struct RateLimitConfig {
    enabled: bool,
    bot: ScopeConfig,
    human: Option<ScopeConfig>,
    channels: HashMap<ChannelRef, ScopeConfig>,
}

enum RateLimitDecision {
    Allowed {
        remaining: u32,
        window_resets: Instant,
    },
    Limited {
        retry_after: Duration,
        policy: RateLimitPolicy,
    },
}

struct RateLimitPolicy {
    notify: bool,
    overflow: OverflowPolicy,
}

// Substrate-agnostic identifiers
struct ParticipantId(String);
struct ChannelRef(String);

struct RateLimitKey {
    sender: ParticipantId,
    channel: ChannelRef,
}

struct Participant {
    id: ParticipantId,
    is_bot: bool,
}

struct TokenBucket {
    remaining: u32,
    window_start: Instant,
    config: ScopeConfig,
}

struct RateLimiter {
    config: RateLimitConfig,
    buckets: HashMap<RateLimitKey, TokenBucket>,
}
```

## Bot Detection

Use Discord's `author.bot` field to distinguish bots from humans. This avoids
maintaining a manual bot list and handles new bots automatically. The
`Participant.is_bot` field is populated from this during Discord event parsing,
before the rate limiter is consulted.

## Integration Point

The rate limiter sits in the message delivery path:

```
Discord gateway event
  --> event parsing (extract Participant, ChannelRef)
  --> rate limiter check
  --> MCP channel forwarding
```

Enforcement is upstream of the bot's context window -- prompt injection in
message content cannot bypass the rate limiter because the decision is made
before the message reaches the MCP transport.

## Policy Decisions

### Allowed

Inject metadata into the forwarded message:

- `remaining`: tokens left in this window
- `window_resets`: when the current window expires

This lets the persona layer make informed decisions about pacing.

### Limited + Drop

1. Send exactly one notification to the channel on the first `Limited` decision
   for this bucket (if `notify` is true).
2. Silently drop all subsequent messages until the bucket refills.

### Limited + Buffer

1. Queue incoming messages in memory.
2. On bucket refill, deliver queued messages in bulk (oldest first).
3. Buffer size should be bounded to prevent memory exhaustion (future: make
   configurable).

## Behavioral Norms (Persona Layer)

These are not enforced in code -- they are conventions for the persona layer
that complement the rate limiter:

- **R-07:** Don't defer to another bot as a substitute for reasoning. The rate
  limiter prevents runaway ping-pong, but the persona should still engage
  substantively rather than delegating.
- **R-08:** When both bots can respond to the same human message, one defers.
  This is a social norm, not a rate limit -- but the rate limiter provides
  backpressure that makes this norm easier to follow.

## Configuration (TOML)

```toml
[rate_limit]
enabled = true

[rate_limit.bot]
tokens = 5
window_seconds = 7200
cooldown_seconds = 7200
policy = "notify_and_drop"

[rate_limit.human]
tokens = 30
window_seconds = 3600
policy = "buffer"
```

### Config Resolution

1. Check `channels[channel_ref]` for a per-channel override.
2. Fall back to `bot` or `human` scope based on `Participant.is_bot`.
3. If no matching scope config exists, the message passes through with no limit.

## Properties

| # | Kind | Property | Description |
|---|------|----------|-------------|
| 1 | Safety | Budget non-negative | `remaining >= 0` at all times |
| 2 | Safety | No delivery after exhaustion | No `Allowed` decision while bucket is in Exhausted or Cooldown state |
| 3 | Liveness | Eventual refill | After entering Exhausted, the bucket eventually returns to Idle |
| 4 | Safety | Isolation | Sender A's messages never modify sender B's bucket |
| 5 | Safety | Monotonic decrement | `remaining` decreases by exactly 1 per `Allowed` decision |
| 6 | Safety | Config precedence | Per-channel config overrides global scope config |
| 7 | Safety | No policy = no limit | Unconfigured sender classes pass through unconditionally |

## Test Plan

| Test | Type | Property |
|------|------|----------|
| Consume N+1 messages, Nth allowed, N+1th limited | Unit | Budget enforcement |
| Wait past window+cooldown, bucket refills | Unit | Liveness |
| Sender without matching config always gets Allowed | Unit | No policy = no limit |
| Per-channel config overrides global config | Unit | Config precedence |
| Buffer overflow queues messages, delivers on refill | Integration | Buffer policy |
| Drop overflow silently discards | Integration | Drop policy |
| notify=true sends exactly one notification on first Limited | Integration | Notification |
| proptest: arbitrary event sequences maintain remaining >= 0 | Property | Safety |
| proptest: Allowed count per window never exceeds tokens | Property | Budget ceiling |

## Future Scope

- **P2P dione instance coordination:** Shared rate limit state across multiple
  dione instances (requires a coordination protocol -- out of scope for v1).
- **Content-aware filtering:** Rate limiting based on message content patterns
  (e.g., detecting repetitive exchanges).
- **Per-user rate limiting:** Extending the human scope to per-user buckets on a
  shared substrate.

## Threat Model Placeholder

Alignment considerations for bot-to-bot communication are deferred. The key
structural property is that prompt injection cannot bypass the rate limiter
since enforcement is upstream of the bot's context window -- the rate limit
decision is made on message metadata (sender identity, channel, timestamps)
before content reaches the MCP transport.
