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
                                                  (tokens        (enter         (cooldown
                                                   exhausted)     cooldown)      expires)
Idle --> Active(remaining: N, window_expires: T) --> Exhausted --> Cooldown(expires: T) --> Idle
  ^                    |                                                                    ^
  |                    +--- (window expires without exhaustion) ----------------------------+
  |                                                                                        |
  +----------------------------------------------------------------------------------------+
```

- **Idle:** No messages seen for this (sender, channel) pair, or bucket has
  been fully reset after cooldown or window expiry. All messages are allowed;
  first message transitions to Active and starts the window timer.
- **Active(remaining: N, window_expires: T):** Messages are being consumed.
  Each allowed message decrements `remaining` by exactly 1. When `remaining`
  reaches 0, transitions to Exhausted. If the window timer expires before
  tokens are exhausted, the bucket resets directly to Idle with full tokens
  (no cooldown needed -- the sender stayed within budget).
- **Exhausted:** Token budget fully consumed. All messages are rate-limited
  according to the configured overflow policy (drop or buffer). Transitions to
  Cooldown on the next tick (the cooldown timer was set when the last token
  was consumed).
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

enum BucketState {
    Idle,
    Active { window_expires: Instant },
    Exhausted,
    Cooldown { expires: Instant },
}

struct TokenBucket {
    state: BucketState,
    remaining: u32,
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
overflow = "drop"
notify = true

[rate_limit.human]
tokens = 30
window_seconds = 3600
overflow = "buffer"
notify = false
```

### Config Resolution

1. Check `channels[channel_ref]` for a per-channel override.
2. Fall back to `bot` or `human` scope based on `Participant.is_bot`.
3. If no matching scope config exists, the message passes through with no limit.

## Properties

| # | Kind | Property | Description | Verified by |
|---|------|----------|-------------|-------------|
| 1 | Safety | Budget non-negative | `remaining >= 0` at all times | TLA+ invariant `BudgetNonNegative` |
| 2 | Safety | No delivery after exhaustion | No `Allowed` decision while bucket is in Exhausted or Cooldown state | TLA+ invariant `NoDeliveryAfterExhaustion` |
| 3 | Liveness | Eventual refill | After entering Exhausted, the bucket eventually returns to Idle | TLA+ temporal `EventualRefill` |
| 4 | Safety | Isolation | Sender A's messages never modify sender B's bucket | Structural (TLA+ `ConsumeToken` only writes `buckets[<<s, c>>]`) |
| 5 | Safety | Monotonic decrement | `remaining` decreases by exactly 1 per `Allowed` decision | Structural (TLA+ `ConsumeToken` sets `remaining - 1`) |
| 6 | Safety | Delivered bound | Delivered messages per window never exceed `tokens` | TLA+ invariant `DeliveredBound` |
| 7 | Liveness | Window reset | Active buckets with expired windows eventually reset to Idle | TLA+ temporal `WindowResets` |
| 8 | Liveness | Cooldown resolves | Buckets in Cooldown eventually return to Idle | TLA+ temporal `CooldownResolves` |
| 9 | Safety | Config precedence | Per-channel config overrides global scope config | Unit test (not in TLA+ -- config is outside the state machine) |
| 10 | Safety | No policy = no limit | Unconfigured sender classes pass through unconditionally | Unit test (not in TLA+ -- config is outside the state machine) |

## Test Plan

| Test | Type | Property |
|------|------|----------|
| Consume N+1 messages, Nth allowed, N+1th limited | Unit | Budget enforcement |
| Wait past window+cooldown, bucket refills | Unit | Liveness |
| Partial consumption, wait past window, bucket resets to Idle | Unit | Window reset |
| Sender without matching config always gets Allowed | Unit | No policy = no limit |
| Per-channel config overrides global config | Unit | Config precedence |
| Two senders in same channel get independent buckets | Unit | Isolation |
| Buffer overflow queues messages, delivers on refill | Integration | Buffer policy |
| Drop overflow silently discards | Integration | Drop policy |
| notify=true sends exactly one notification on first Limited | Integration | Notification |
| proptest: arbitrary event sequences maintain remaining >= 0 | Property | Safety |
| proptest: Allowed count per window never exceeds tokens | Property | Budget ceiling |

## Model Checking

The TLA+ model can be checked with TLC using the provided configuration files:

```
java -jar tla2tools.jar -config RateLimiter.cfg RateLimiter.tla
```

### Suggested Parameters

| Constant | Value | Rationale |
|----------|-------|-----------|
| Senders | {s1, s2} | Two senders exercises isolation; more explodes state space |
| Channels | {c1} | One channel is sufficient; senders x channels is the scaling factor |
| MaxTokens | 2 | Smallest value that exercises Active->Active->Exhausted path |
| WindowLen | 3 | Long enough for partial consumption + window expiry |
| CooldownLen | 2 | Long enough to see Exhausted->Cooldown->Idle |
| MaxTime | 8 | Must be >= WindowLen + CooldownLen + slack for liveness |

The `StateView` definition in the TLA+ model excludes the `history` variable
from state comparison. Without this VIEW, the append-only `history` sequences
make the state space infinite. The `.cfg` file references `VIEW StateView` to
enable this projection.

### Files

| File | Purpose |
|------|---------|
| `RateLimiter.tla` | Main specification module |
| `MC.tla` | Model-checking constants (extends RateLimiter) |
| `RateLimiter.cfg` | TLC configuration: constants, spec, view, invariants, properties |

## Relationship to Delivery Buffer (#61)

The delivery buffer (#61) and rate limiter solve different problems at
different layers:

- **Delivery buffer:** Coalescing window that batches incoming messages so a
  slower bot sees a faster bot's response before deciding to reply. Reduces
  crosstalk by giving bots shared context. Operates on all messages, not just
  bot messages.
- **Rate limiter:** Token-budget throttle that hard-caps how many messages a
  sender can deliver per time window. Prevents runaway ping-pong by cutting
  off the feedback loop entirely.

They are complementary. The delivery buffer sits upstream of the rate limiter
in the pipeline:

```
Discord gateway event
  --> event parsing (extract Participant, ChannelRef)
  --> delivery buffer (coalesce within delay window)
  --> rate limiter check
  --> MCP channel forwarding
```

A coalesced batch from the delivery buffer counts as one rate-limit check per
message in the batch, not one check per batch. The rate limiter does not need
to be aware of the delivery buffer.

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
