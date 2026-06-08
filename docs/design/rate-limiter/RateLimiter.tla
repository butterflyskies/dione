------------------------------ MODULE RateLimiter ------------------------------
\* Token-bucket rate limiter for bot-to-bot message gating.
\*
\* Models the state machine:
\*   Idle -> Active(remaining) -> Exhausted -> Cooldown(expires) -> Idle
\*                 |                                                  ^
\*                 +--- (window expires without exhaustion) ----------+
\*
\* Each (sender, channel) pair gets an independent bucket.
\* Safety properties are checked as invariants.
\* Liveness properties require weak fairness on Tick, WindowExpires,
\* and CooldownExpires.
\*
\* NOTE: Liveness holds only when MaxTime is large enough for all cooldowns
\* and windows to complete. The bounded clock is a modeling artifact for
\* finite state space -- the real system has unbounded time.

EXTENDS Integers, FiniteSets, Sequences

CONSTANTS
    Senders,        \* Set of sender identifiers
    Channels,       \* Set of channel identifiers
    MaxTokens,      \* Maximum tokens per bucket (positive integer)
    WindowLen,      \* Window duration in ticks
    CooldownLen,    \* Cooldown duration in ticks
    MaxTime         \* Upper bound on model time for finite state space

ASSUME MaxTokens \in Nat /\ MaxTokens > 0
ASSUME WindowLen \in Nat /\ WindowLen > 0
ASSUME CooldownLen \in Nat /\ CooldownLen > 0
ASSUME MaxTime \in Nat /\ MaxTime > 0

VARIABLES
    buckets,        \* [Senders x Channels] -> bucket record
    clock,          \* Global logical clock (0..MaxTime)
    delivered,      \* [Senders x Channels] -> count of delivered messages per window
    history         \* [Senders x Channels] -> sequence of events (for property checking)

\* Helper: the set of all (sender, channel) keys
Keys == Senders \X Channels

\* Bucket states
BucketStates == {"Idle", "Active", "Exhausted", "Cooldown"}

\* Type invariant -- intentionally does NOT constrain delivered to 0..MaxTokens,
\* so that DeliveredBound can catch violations independently.
TypeOK ==
    /\ clock \in 0..MaxTime
    /\ \A k \in Keys :
        /\ buckets[k].state \in BucketStates
        /\ buckets[k].remaining \in 0..MaxTokens
        /\ buckets[k].cooldown_expires \in 0..MaxTime
        /\ buckets[k].window_expires \in 0..MaxTime
        /\ delivered[k] \in Nat

vars == <<buckets, clock, delivered, history>>

-----------------------------------------------------------------------------
\* Initial state: all buckets idle, clock at 0, no deliveries

Init ==
    /\ buckets = [k \in Keys |->
        [state            |-> "Idle",
         remaining        |-> MaxTokens,
         cooldown_expires |-> 0,
         window_expires   |-> 0]]
    /\ clock = 0
    /\ delivered = [k \in Keys |-> 0]
    /\ history = [k \in Keys |-> << >>]

-----------------------------------------------------------------------------
\* Action: SendMessage(sender, channel)
\*
\* A sender sends a message in a channel. The rate limiter decides whether
\* to allow or limit the message based on bucket state.

ConsumeToken(s, c) ==
    LET k == <<s, c>>
        b == buckets[k]
    IN
    \* Precondition: bucket is Idle or Active with tokens remaining
    /\ b.state \in {"Idle", "Active"}
    /\ b.remaining > 0
    /\ LET newRemaining == b.remaining - 1
           \* Start the window timer on first message (Idle -> Active)
           newWindowExpires == IF b.state = "Idle"
                               THEN clock + WindowLen
                               ELSE b.window_expires
       IN
       buckets' = [buckets EXCEPT ![k] =
            [state            |-> IF newRemaining > 0
                                   THEN "Active"
                                   ELSE "Exhausted",
             remaining        |-> newRemaining,
             cooldown_expires |-> IF newRemaining = 0
                                   THEN clock + CooldownLen
                                   ELSE b.cooldown_expires,
             window_expires   |-> newWindowExpires]]
    /\ delivered' = [delivered EXCEPT ![k] = delivered[k] + 1]
    /\ history' = [history EXCEPT ![k] = Append(history[k], "Allowed")]
    /\ UNCHANGED clock

DropMessage(s, c) ==
    LET k == <<s, c>>
        b == buckets[k]
    IN
    \* Precondition: bucket is Exhausted or in Cooldown
    /\ b.state \in {"Exhausted", "Cooldown"}
    /\ history' = [history EXCEPT ![k] = Append(history[k], "Limited")]
    /\ UNCHANGED <<buckets, clock, delivered>>

SendMessage(s, c) ==
    \/ ConsumeToken(s, c)
    \/ DropMessage(s, c)

\* Action: Tick
\*
\* Advance the global clock by 1.

Tick ==
    /\ clock < MaxTime
    /\ clock' = clock + 1
    /\ UNCHANGED <<buckets, delivered, history>>

\* Action: WindowExpires
\*
\* When an Active bucket's window timer expires without exhausting tokens,
\* reset to Idle with full tokens. This models the spec's "window" concept:
\* a partially-consumed bucket resets when the time window elapses.

WindowExpires(s, c) ==
    LET k == <<s, c>>
        b == buckets[k]
    IN
    /\ b.state = "Active"
    /\ clock >= b.window_expires
    /\ buckets' = [buckets EXCEPT ![k] =
        [state            |-> "Idle",
         remaining        |-> MaxTokens,
         cooldown_expires |-> 0,
         window_expires   |-> 0]]
    /\ delivered' = [delivered EXCEPT ![k] = 0]
    /\ UNCHANGED <<clock, history>>

\* Action: CooldownExpires
\*
\* When a bucket is in Exhausted or Cooldown state and the clock has reached
\* or passed its cooldown_expires time, reset the bucket to Idle with full
\* tokens.

CooldownExpires(s, c) ==
    LET k == <<s, c>>
        b == buckets[k]
    IN
    /\ b.state \in {"Exhausted", "Cooldown"}
    /\ clock >= b.cooldown_expires
    /\ buckets' = [buckets EXCEPT ![k] =
        [state            |-> "Idle",
         remaining        |-> MaxTokens,
         cooldown_expires |-> 0,
         window_expires   |-> 0]]
    /\ delivered' = [delivered EXCEPT ![k] = 0]
    /\ UNCHANGED <<clock, history>>

-----------------------------------------------------------------------------
\* Next-state relation

Next ==
    \/ \E s \in Senders, c \in Channels : SendMessage(s, c)
    \/ Tick
    \/ \E s \in Senders, c \in Channels : WindowExpires(s, c)
    \/ \E s \in Senders, c \in Channels : CooldownExpires(s, c)

\* Fairness: Tick, WindowExpires, and CooldownExpires must eventually happen
\* (weak fairness) so that liveness properties hold.

Fairness ==
    /\ WF_vars(Tick)
    /\ \A s \in Senders, c \in Channels : WF_vars(WindowExpires(s, c))
    /\ \A s \in Senders, c \in Channels : WF_vars(CooldownExpires(s, c))

Spec == Init /\ [][Next]_vars /\ Fairness

-----------------------------------------------------------------------------
\* SAFETY PROPERTIES (invariants)

\* Property 1: Budget never negative
BudgetNonNegative ==
    \A k \in Keys : buckets[k].remaining >= 0

\* Property 2: No delivery after exhaustion -- if a bucket is Exhausted or
\* in Cooldown, no Allowed decision is possible. Checked as an invariant:
\* the delivered count can only increase when state is Idle or Active.
\* Equivalently: remaining = 0 implies state is Exhausted or Cooldown,
\* and in those states no delivery occurs.
NoDeliveryAfterExhaustion ==
    \A k \in Keys :
        buckets[k].state \in {"Exhausted", "Cooldown"} =>
            buckets[k].remaining = 0

\* Property 4: Isolation -- sender A's messages never modify sender B's bucket.
\* Structural: SendMessage(s, c) only modifies buckets[<<s, c>>].
\* Verified by inspection. Not expressible as a state invariant without
\* auxiliary history variables tracking per-action modifications.

\* Property 5: Monotonic decrement -- remaining decreases by exactly 1 per
\* Allowed decision. Structural: ConsumeToken sets remaining to
\* b.remaining - 1. Verified by inspection.

\* Property 6: Delivered messages in a window never exceed MaxTokens.
\* This is independently checkable because TypeOK does NOT constrain
\* delivered to 0..MaxTokens.
DeliveredBound ==
    \A k \in Keys : delivered[k] <= MaxTokens

\* Combined safety invariant
SafetyInvariant ==
    /\ TypeOK
    /\ BudgetNonNegative
    /\ NoDeliveryAfterExhaustion
    /\ DeliveredBound

-----------------------------------------------------------------------------
\* LIVENESS PROPERTIES (temporal)
\*
\* NOTE: These properties hold under the assumption that MaxTime is large
\* enough for all cooldowns and windows to complete. In the real system,
\* time is unbounded, so these always hold. In the model, they hold when
\* MaxTime >= CooldownLen + WindowLen + some slack.

\* Property 3: Eventual refill -- if a bucket enters Exhausted, it eventually
\* returns to Idle (requires fairness on Tick and CooldownExpires).

EventualRefill ==
    \A s \in Senders, c \in Channels :
        LET k == <<s, c>>
        IN buckets[k].state = "Exhausted" ~> buckets[k].state = "Idle"

\* Every Cooldown eventually resolves
CooldownResolves ==
    \A s \in Senders, c \in Channels :
        LET k == <<s, c>>
        IN buckets[k].state = "Cooldown" ~> buckets[k].state = "Idle"

\* Active buckets with expired windows eventually reset
WindowResets ==
    \A s \in Senders, c \in Channels :
        LET k == <<s, c>>
        IN (buckets[k].state = "Active" /\ clock >= buckets[k].window_expires)
            ~> buckets[k].state = "Idle"

=============================================================================
\* Modification History
\* Revised 2026-06-08 -- ariadne review:
\*   - Added WindowLen, CooldownLen constants (cooldown was implicitly 1 tick)
\*   - Added window_expires field and WindowExpires action for partial-consumption reset
\*   - Merged duplicate Idle/Active cases into ConsumeToken
\*   - Added explicit NoDeliveryAfterExhaustion safety invariant (was comment-only)
\*   - Decoupled TypeOK from DeliveredBound (TypeOK no longer constrains delivered to 0..MaxTokens)
\*   - Simplified Exhausted->Cooldown: CooldownExpires now handles both states
\*   - Added history variable and DropMessage action for observability
\*   - Added WindowResets liveness property
\*   - Documented MaxTime liveness caveat
\* Created 2026-06-03
