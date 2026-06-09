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

/// A sender class groups participants by category for policy purposes.
/// Classes may map to Discord guild roles, bot/human distinction, or
/// any other substrate-specific grouping.
struct SenderClass(String);

struct RateLimitConfig {
    enabled: bool,
    /// Global default applied when no class or individual override matches.
    default: ScopeConfig,
    /// Per-class overrides. Classes are arbitrary labels (e.g. "bot",
    /// "human", or a Discord guild role name like "moderator").
    classes: HashMap<SenderClass, ScopeConfig>,
    /// Per-individual overrides, keyed by ParticipantId. Highest priority.
    individuals: HashMap<ParticipantId, ScopeConfig>,
    /// Per-channel overrides, applied after sender resolution.
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
    /// Classes this participant belongs to (e.g. "bot", "moderator").
    /// Populated during event parsing from substrate-specific data
    /// (Discord roles, bot flag, etc.). A participant may belong to
    /// multiple classes; the first matching class in config order wins.
    classes: Vec<SenderClass>,
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

## Sender Classification

Participants are assigned to one or more `SenderClass` values during event
parsing, before the rate limiter is consulted. Classification is
substrate-specific -- the core rate limiter only sees the resulting class
labels. See "Discord Integration" below for how Discord populates classes.

## Discord Integration

The core types are substrate-agnostic, but dione's first (and currently only)
substrate is Discord. This section defines how the abstract types map to
Discord's identity model.

### Type Mapping

| Abstract Type | Discord Equivalent | Notes |
|---|---|---|
| `ParticipantId` | Discord user ID (snowflake) | Stored as string. Unique across all guilds. |
| `ChannelRef` | Discord channel ID (snowflake) | Stored as string. Unique across all guilds. |
| `SenderClass` | Derived from bot flag + guild roles | See below. |

### Sender Classification from Discord

During event parsing, dione populates `Participant.classes` from Discord data:

1. **Bot flag:** If `message.author.bot` is true, the participant gets the
   `"bot"` class. Otherwise, `"human"`.
2. **Guild roles:** For each guild role the member holds, the role name is
   added as a class (e.g. `"moderator"`, `"admin"`). Role names are
   lowercased for matching.

This means a config entry like `[rate_limit.class.moderator]` will match any
Discord user with the "Moderator" role in the guild where the message was sent.

### Guild Role Queries

Dione already maintains a Discord gateway connection and caches guild member
data. Role membership is available from the `GuildMemberUpdate` and
`MessageCreate` events (the latter includes a partial member object with
roles). No additional API calls are needed for classification -- the data
arrives with each message event.

### Multi-Guild Considerations

A participant may have different roles in different guilds. Since the rate
limiter keys on `(ParticipantId, ChannelRef)` and channels are guild-scoped,
the class list is populated from the guild where the message originated. A
user who is "moderator" in guild A but not in guild B will only get the
moderator class for messages in guild A's channels.

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

## Configuration (TOML)

```toml
[rate_limit]
enabled = true

# Global default -- applies to any sender without a class or individual match.
[rate_limit.default]
tokens = 30
window_seconds = 3600
overflow = "buffer"
notify = false

# Class overrides -- keyed by class name (e.g. "bot", "moderator", or a
# Discord guild role). Matched against the participant's class list.
[rate_limit.class.bot]
tokens = 5
window_seconds = 7200
cooldown_seconds = 7200
overflow = "drop"
notify = true

# Individual overrides -- keyed by ParticipantId. Highest priority.
[rate_limit.individual."bot:vesper"]
tokens = 10
window_seconds = 7200
cooldown_seconds = 3600
overflow = "drop"
notify = true

# Per-channel overrides -- applied after sender resolution.
# [rate_limit.channel."1234567890"]
# tokens = 3
# window_seconds = 3600
# overflow = "drop"
# notify = true
```

### Config Resolution

Resolution follows a **default -> class -> individual** hierarchy, with
per-channel overrides applied as a final layer:

1. Start with `default` as the base policy.
2. If the participant belongs to any class in `classes`, use the first matching
   `class.<name>` override. Class matching checks the participant's class list
   in order against configured classes.
3. If `individual.<participant_id>` exists, use it instead (highest priority).
4. If `channel.<channel_ref>` exists, it overrides the sender-resolved config
   for that specific channel.
5. If rate limiting is disabled (`enabled = false`), all messages pass through.

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
| 9 | Safety | Config precedence | Resolution follows default -> class -> individual -> channel hierarchy | Unit test (not in TLA+ -- config is outside the state machine) |
| 10 | Safety | No policy = no limit | Unconfigured sender classes pass through unconditionally | Unit test (not in TLA+ -- config is outside the state machine) |

## Test Plan

| Test | Type | Property |
|------|------|----------|
| Consume N+1 messages, Nth allowed, N+1th limited | Unit | Budget enforcement |
| Wait past window+cooldown, bucket refills | Unit | Liveness |
| Partial consumption, wait past window, bucket resets to Idle | Unit | Window reset |
| Sender without matching config always gets Allowed | Unit | No policy = no limit |
| Individual override takes precedence over class and default | Unit | Config precedence |
| Class override takes precedence over default | Unit | Config precedence |
| Per-channel config overrides sender-resolved config | Unit | Config precedence |
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

## Threat Model Placeholder

Alignment considerations for bot-to-bot communication are deferred. The key
structural property is that prompt injection cannot bypass the rate limiter
since enforcement is upstream of the bot's context window -- the rate limit
decision is made on message metadata (sender identity, channel, timestamps)
before content reaches the MCP transport.
