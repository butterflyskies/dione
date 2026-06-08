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
\*
\* NOTE: Variables are indexed as [sender][channel] (nested functions)
\* rather than [<<sender, channel>>] (tuple-keyed functions) because TLC's
\* temporal property checker crashes with ArrayIndexOutOfBoundsException
\* on tuple-keyed function access inside ~> formulas.

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
    buckets,        \* [Senders][Channels] -> bucket record
    clock,          \* Global logical clock (0..MaxTime)
    delivered,      \* [Senders][Channels] -> count of delivered messages per window
    history         \* [Senders][Channels] -> sequence of events (for property checking)

\* Bucket states
BucketStates == {"Idle", "Active", "Exhausted", "Cooldown"}

\* Type invariant -- intentionally does NOT constrain delivered to 0..MaxTokens,
\* so that DeliveredBound can catch violations independently.
TypeOK ==
    /\ clock \in 0..MaxTime
    /\ \A s \in Senders, c \in Channels :
        /\ buckets[s][c].state \in BucketStates
        /\ buckets[s][c].remaining \in 0..MaxTokens
        /\ buckets[s][c].cooldown_expires \in 0..(MaxTime + CooldownLen)
        /\ buckets[s][c].window_expires \in 0..(MaxTime + WindowLen)
        /\ delivered[s][c] \in Nat

vars == <<buckets, clock, delivered, history>>

-----------------------------------------------------------------------------
\* Initial state: all buckets idle, clock at 0, no deliveries

Init ==
    /\ buckets = [s \in Senders |-> [c \in Channels |->
        [state            |-> "Idle",
         remaining        |-> MaxTokens,
         cooldown_expires |-> 0,
         window_expires   |-> 0]]]
    /\ clock = 0
    /\ delivered = [s \in Senders |-> [c \in Channels |-> 0]]
    /\ history = [s \in Senders |-> [c \in Channels |-> << >>]]

-----------------------------------------------------------------------------
\* Action: SendMessage(sender, channel)
\*
\* A sender sends a message in a channel. The rate limiter decides whether
\* to allow or limit the message based on bucket state.

ConsumeToken(s, c) ==
    LET b == buckets[s][c]
    IN
    \* Precondition: bucket is Idle or Active with tokens remaining
    /\ b.state \in {"Idle", "Active"}
    /\ b.remaining > 0
    \* Model-checking guard: ensure enough time remains for the full
    \* lifecycle (window + cooldown) so liveness properties hold.
    \* The real system has unbounded time; this guards only the model.
    /\ clock + WindowLen + CooldownLen <= MaxTime
    /\ LET newRemaining == b.remaining - 1
           \* Start the window timer on first message (Idle -> Active)
           newWindowExpires == IF b.state = "Idle"
                               THEN clock + WindowLen
                               ELSE b.window_expires
       IN
       buckets' = [buckets EXCEPT ![s][c] =
            [state            |-> IF newRemaining > 0
                                   THEN "Active"
                                   ELSE "Exhausted",
             remaining        |-> newRemaining,
             cooldown_expires |-> IF newRemaining = 0
                                   THEN clock + CooldownLen
                                   ELSE b.cooldown_expires,
             window_expires   |-> newWindowExpires]]
    /\ delivered' = [delivered EXCEPT ![s][c] = delivered[s][c] + 1]
    /\ history' = [history EXCEPT ![s][c] = Append(history[s][c], "Allowed")]
    /\ UNCHANGED clock

DropMessage(s, c) ==
    LET b == buckets[s][c]
    IN
    \* Precondition: bucket is Exhausted or in Cooldown
    /\ b.state \in {"Exhausted", "Cooldown"}
    /\ history' = [history EXCEPT ![s][c] = Append(history[s][c], "Limited")]
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

\* Action: EnterCooldown
\*
\* After a bucket enters Exhausted, transition to Cooldown on the next tick.
\* The cooldown_expires timer was already set in ConsumeToken; this action
\* just changes the state label so the bucket is visibly in Cooldown while
\* waiting for the timer to expire. Without this, Cooldown is unreachable
\* and CooldownResolves is vacuously true.

EnterCooldown(s, c) ==
    LET b == buckets[s][c]
    IN
    /\ b.state = "Exhausted"
    /\ buckets' = [buckets EXCEPT ![s][c].state = "Cooldown"]
    /\ UNCHANGED <<clock, delivered, history>>

\* Action: WindowExpires
\*
\* When an Active bucket's window timer expires without exhausting tokens,
\* reset to Idle with full tokens. This models the spec's "window" concept:
\* a partially-consumed bucket resets when the time window elapses.

WindowExpires(s, c) ==
    LET b == buckets[s][c]
    IN
    /\ b.state = "Active"
    /\ clock >= b.window_expires
    /\ buckets' = [buckets EXCEPT ![s][c] =
        [state            |-> "Idle",
         remaining        |-> MaxTokens,
         cooldown_expires |-> 0,
         window_expires   |-> 0]]
    /\ delivered' = [delivered EXCEPT ![s][c] = 0]
    /\ UNCHANGED <<clock, history>>

\* Action: CooldownExpires
\*
\* When a bucket is in Cooldown state and the clock has reached or passed
\* its cooldown_expires time, reset the bucket to Idle with full tokens.

CooldownExpires(s, c) ==
    LET b == buckets[s][c]
    IN
    /\ b.state = "Cooldown"
    /\ clock >= b.cooldown_expires
    /\ buckets' = [buckets EXCEPT ![s][c] =
        [state            |-> "Idle",
         remaining        |-> MaxTokens,
         cooldown_expires |-> 0,
         window_expires   |-> 0]]
    /\ delivered' = [delivered EXCEPT ![s][c] = 0]
    /\ UNCHANGED <<clock, history>>

-----------------------------------------------------------------------------
\* Next-state relation

Next ==
    \/ \E s \in Senders, c \in Channels : SendMessage(s, c)
    \/ Tick
    \/ \E s \in Senders, c \in Channels : WindowExpires(s, c)
    \/ \E s \in Senders, c \in Channels : EnterCooldown(s, c)
    \/ \E s \in Senders, c \in Channels : CooldownExpires(s, c)

\* Fairness: Tick, WindowExpires, EnterCooldown, and CooldownExpires must
\* eventually happen (weak fairness) so that liveness properties hold.

Fairness ==
    /\ WF_vars(Tick)
    /\ \A s \in Senders, c \in Channels : WF_vars(WindowExpires(s, c))
    /\ \A s \in Senders, c \in Channels : WF_vars(EnterCooldown(s, c))
    /\ \A s \in Senders, c \in Channels : WF_vars(CooldownExpires(s, c))

Spec == Init /\ [][Next]_vars /\ Fairness

-----------------------------------------------------------------------------
\* SAFETY PROPERTIES (invariants)

\* Property 1: Budget never negative
BudgetNonNegative ==
    \A s \in Senders, c \in Channels : buckets[s][c].remaining >= 0

\* Property 2: No delivery after exhaustion -- if a bucket is Exhausted or
\* in Cooldown, no Allowed decision is possible. Checked as an invariant:
\* the delivered count can only increase when state is Idle or Active.
\* Equivalently: remaining = 0 implies state is Exhausted or Cooldown,
\* and in those states no delivery occurs.
NoDeliveryAfterExhaustion ==
    \A s \in Senders, c \in Channels :
        buckets[s][c].state \in {"Exhausted", "Cooldown"} =>
            buckets[s][c].remaining = 0

\* Property 4: Isolation -- sender A's messages never modify sender B's bucket.
\* Structural: SendMessage(s, c) only modifies buckets[s][c].
\* Verified by inspection. Not expressible as a state invariant without
\* auxiliary history variables tracking per-action modifications.

\* Property 5: Monotonic decrement -- remaining decreases by exactly 1 per
\* Allowed decision. Structural: ConsumeToken sets remaining to
\* b.remaining - 1. Verified by inspection.

\* Property 6: Delivered messages in a window never exceed MaxTokens.
\* This is independently checkable because TypeOK does NOT constrain
\* delivered to 0..MaxTokens.
DeliveredBound ==
    \A s \in Senders, c \in Channels : delivered[s][c] <= MaxTokens

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
        buckets[s][c].state = "Exhausted" ~> buckets[s][c].state = "Idle"

\* Every Cooldown eventually resolves
CooldownResolves ==
    \A s \in Senders, c \in Channels :
        buckets[s][c].state = "Cooldown" ~> buckets[s][c].state = "Idle"

\* Active buckets with expired windows eventually reset
WindowResets ==
    \A s \in Senders, c \in Channels :
        (buckets[s][c].state = "Active" /\ clock >= buckets[s][c].window_expires)
            ~> buckets[s][c].state = "Idle"

-----------------------------------------------------------------------------
\* VIEW definition for model checking
\*
\* history grows without bound (it's an append-only sequence), making the
\* state space infinite if TLC includes it in state comparison. Define a
\* view that projects state to only the variables that matter for property
\* checking. TLC uses VIEW to decide state equality -- two states that
\* differ only in history are treated as the same state.
\*
\* Use this in the .cfg file: VIEW StateView

StateView == <<buckets, clock, delivered>>

=============================================================================
\* Modification History
\* Revised 2026-06-08 -- TLC temporal checker fix:
\*   - Restructured all variables from tuple-keyed [<<s,c>>] to nested
\*     [s][c] functions to work around TLC ArrayIndexOutOfBoundsException
\*     in temporal property checker (known TLC limitation with tuple keys
\*     inside ~> formulas)
\*   - Fixed TypeOK: timer fields now allow 0..(MaxTime + CooldownLen/WindowLen)
\*     since timers set near MaxTime can exceed it
\* Revised 2026-06-08 -- ariadne (review fix round 2):
\*   - Added StateView (VIEW) to exclude history from state comparison (fixes
\*     infinite state space caused by unbounded history sequences)
\*   - Added EnterCooldown action: Exhausted -> Cooldown transition (fixes
\*     unreachable Cooldown state / vacuously true CooldownResolves)
\*   - CooldownExpires now only matches Cooldown (not Exhausted)
\*   - Added MC.tla and RateLimiter.cfg for model checking
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
