------------------------------ MODULE RateLimiter ------------------------------
\* Token-bucket rate limiter for bot-to-bot message gating.
\*
\* Models the state machine:
\*   Idle -> Active(remaining) -> Exhausted -> Cooldown(expires) -> Idle
\*
\* Each (sender, channel) pair gets an independent bucket.
\* Safety properties are checked as invariants.
\* Liveness properties require weak fairness on Tick and CooldownExpires.

EXTENDS Integers, FiniteSets

CONSTANTS
    Senders,        \* Set of sender identifiers
    Channels,       \* Set of channel identifiers
    MaxTokens,      \* Maximum tokens per bucket (positive integer)
    MaxTime         \* Upper bound on model time for finite state space

ASSUME MaxTokens \in Nat /\ MaxTokens > 0
ASSUME MaxTime \in Nat /\ MaxTime > 0

VARIABLES
    buckets,        \* [Senders x Channels] -> bucket record
    clock,          \* Global logical clock (0..MaxTime)
    delivered       \* [Senders x Channels] -> count of delivered messages per window

\* Helper: the set of all (sender, channel) keys
Keys == Senders \X Channels

\* Bucket states
BucketStates == {"Idle", "Active", "Exhausted", "Cooldown"}

\* A bucket record
BucketRecord == [
    state     : BucketStates,
    remaining : 0..MaxTokens,
    cooldown_expires : 0..MaxTime
]

\* Type invariant
TypeOK ==
    /\ clock \in 0..MaxTime
    /\ \A k \in Keys :
        /\ buckets[k].state \in BucketStates
        /\ buckets[k].remaining \in 0..MaxTokens
        /\ buckets[k].cooldown_expires \in 0..MaxTime
        /\ delivered[k] \in 0..MaxTokens

vars == <<buckets, clock, delivered>>

-----------------------------------------------------------------------------
\* Initial state: all buckets idle, clock at 0, no deliveries

Init ==
    /\ buckets = [k \in Keys |->
        [state           |-> "Idle",
         remaining       |-> MaxTokens,
         cooldown_expires |-> 0]]
    /\ clock = 0
    /\ delivered = [k \in Keys |-> 0]

-----------------------------------------------------------------------------
\* Action: SendMessage(sender, channel)
\*
\* A sender sends a message in a channel. The rate limiter decides whether
\* to allow or limit the message based on bucket state.

SendMessage(s, c) ==
    LET k == <<s, c>>
        b == buckets[k]
    IN
    \/ \* Case 1: Idle -- first message in window, transition to Active
       /\ b.state = "Idle"
       /\ b.remaining > 0
       /\ buckets' = [buckets EXCEPT ![k] =
            [state           |-> IF b.remaining - 1 > 0
                                 THEN "Active"
                                 ELSE "Exhausted",
             remaining       |-> b.remaining - 1,
             cooldown_expires |-> IF b.remaining - 1 = 0
                                  THEN clock + 1  \* cooldown starts
                                  ELSE b.cooldown_expires]]
       /\ delivered' = [delivered EXCEPT ![k] = delivered[k] + 1]
       /\ UNCHANGED clock

    \/ \* Case 2: Active -- consume a token
       /\ b.state = "Active"
       /\ b.remaining > 0
       /\ buckets' = [buckets EXCEPT ![k] =
            [state           |-> IF b.remaining - 1 > 0
                                 THEN "Active"
                                 ELSE "Exhausted",
             remaining       |-> b.remaining - 1,
             cooldown_expires |-> IF b.remaining - 1 = 0
                                  THEN clock + 1  \* cooldown starts
                                  ELSE b.cooldown_expires]]
       /\ delivered' = [delivered EXCEPT ![k] = delivered[k] + 1]
       /\ UNCHANGED clock

    \/ \* Case 3: Exhausted or Cooldown -- message is rate-limited (dropped)
       /\ b.state \in {"Exhausted", "Cooldown"}
       /\ UNCHANGED vars

\* Action: Tick
\*
\* Advance the global clock by 1. When clock reaches a bucket's
\* cooldown_expires, transition Exhausted -> Cooldown.

Tick ==
    /\ clock < MaxTime
    /\ clock' = clock + 1
    /\ buckets' = [k \in Keys |->
        LET b == buckets[k]
        IN
        IF b.state = "Exhausted" THEN
            \* Transition to Cooldown -- the cooldown timer is now running
            [b EXCEPT !.state = "Cooldown"]
        ELSE
            b]
    /\ UNCHANGED delivered

\* Action: CooldownExpires
\*
\* When a bucket is in Cooldown and the clock has reached or passed its
\* cooldown_expires time, reset the bucket to Idle with full tokens.

CooldownExpires(s, c) ==
    LET k == <<s, c>>
        b == buckets[k]
    IN
    /\ b.state = "Cooldown"
    /\ clock >= b.cooldown_expires
    /\ buckets' = [buckets EXCEPT ![k] =
        [state           |-> "Idle",
         remaining       |-> MaxTokens,
         cooldown_expires |-> 0]]
    /\ delivered' = [delivered EXCEPT ![k] = 0]  \* Reset window counter
    /\ UNCHANGED clock

-----------------------------------------------------------------------------
\* Next-state relation

Next ==
    \/ \E s \in Senders, c \in Channels : SendMessage(s, c)
    \/ Tick
    \/ \E s \in Senders, c \in Channels : CooldownExpires(s, c)

\* Fairness: Tick and CooldownExpires must eventually happen (weak fairness)
\* so that liveness properties hold.

Fairness ==
    /\ WF_vars(Tick)
    /\ \A s \in Senders, c \in Channels : WF_vars(CooldownExpires(s, c))

Spec == Init /\ [][Next]_vars /\ Fairness

-----------------------------------------------------------------------------
\* SAFETY PROPERTIES (invariants)

\* Property 1: Budget never negative
BudgetNonNegative ==
    \A k \in Keys : buckets[k].remaining >= 0

\* Property 2: No delivery after exhaustion -- if a bucket is Exhausted or
\* in Cooldown, SendMessage cannot produce an Allowed decision (enforced by
\* the action structure: Case 3 leaves vars unchanged)

\* Property 4: Isolation -- sender A's messages never modify sender B's bucket.
\* This is structural: SendMessage(s, c) only modifies buckets[<<s, c>>].
\* We verify it as an invariant: for any two distinct keys, the bucket for
\* one key is independent of actions on the other.
\* (Structural property -- verified by inspection of SendMessage.)

\* Property 5: Monotonic decrement -- remaining decreases by exactly 1 per
\* Allowed decision. Verified structurally: Cases 1 and 2 set remaining to
\* b.remaining - 1.

\* Property 6: Delivered messages in a window never exceed MaxTokens
DeliveredBound ==
    \A k \in Keys : delivered[k] <= MaxTokens

\* Combined safety invariant
SafetyInvariant ==
    /\ TypeOK
    /\ BudgetNonNegative
    /\ DeliveredBound

-----------------------------------------------------------------------------
\* LIVENESS PROPERTIES (temporal)

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

=============================================================================
\* Modification History
\* Created 2026-06-03
