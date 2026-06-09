---- MODULE MC ----
\* Model-checking configuration for RateLimiter.
\* Small constants to keep the state space tractable.

EXTENDS RateLimiter

\* Concrete constant definitions for TLC
const_Senders == {"s1", "s2"}
const_Channels == {"c1"}
const_MaxTokens == 2
const_WindowLen == 3
const_CooldownLen == 2
const_MaxTime == 15

====
