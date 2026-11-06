# dione-omp

Durable OMP sessions and protocol delivery for Dione.

Open an `OmpSession`, obtain its admission and opaque control capabilities, and pass the control to `connect` with a shared `ProtocolOutput` and the application's actual static Codex tool names. Start the session with that same output and a process cancellation token. Await `RunningOmpSession::shutdown` to drain accepted work and release the durable queue lock.

The owner keeps version-1 persistence, binding generations, acknowledgment fencing and recovery receipts private. It does not authorize transport origins. Applications must gate private control requests and readiness at their actual transport boundary.

`teams_delivery(session.admission())` assembles the native resident delivery
capability over the actual OMP actor. Current binding, identity validation and
replay ownership stay in that actor; the adapter preserves committed, duplicate,
visible-uncertain and completion-unknown outcomes with their original causes.
