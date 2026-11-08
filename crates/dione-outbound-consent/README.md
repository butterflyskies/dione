# Dione outbound consent

This crate owns held-send consent, reservations, retry progress, contradictionary
diary policy, journal history, and fence-aware chunking. Existing DeliverReply
and OutboundJudge implementations connect it to native effects and evaluation.
The application retains current authorization, HTTP delivery bookkeeping, gate
installation, sweeping, and shutdown sequencing.

ReplyRequest constructs ordinary addressing and text. Deferred diary data is
attached explicitly and read through a slice. Fence state is opaque and comes
from existing chunks; request operations retain it across a partial retry.
PostedMessages requires a first message ID. Retry/failure representations and
reservation state remain private. Release sends held bytes, while rephrase is
judged again. Reservations exclude concurrent sends and expiry during a send.

The hold queue is process-local and assumes one principal. Handles are lookup
tokens, not unguessable authorization. Journal history survives restart, but its
existing append-failure policy logs and continues rather than revoking delivery.
No config-style fsync guarantee is added.

Original source remainder is distinct from presentation-only fence markers.
Deferred diary records settle once after complete delivery. Existing plain
chunking has a source-established tiny-limit multibyte edge outside this move;
this crate does not claim safety for every arbitrary byte limit. Existing
Serenity features and workspace vendored TLS closure remain selected.

A message POST whose completion is unknown consumes its handle terminally.
`DeliveryProgress::Unknown` carries only confirmed earlier IDs, never a retry
remainder or an invented ID for the uncertain chunk. The wire reply marks the
attempt ambiguous and non-retryable; reconcile remote state before a new send.
Known no-effect failures retain the existing retry and confirmed-prefix behavior.

Cancellation after delivery starts also consumes the handle and records `unknown`.
A private guard retains ownership across settlement lock contention and transfers
cancelled settlement to one runtime task; the queue lock protects exact-once local
resolution. This is process-local fencing, not a cross-restart replay ledger.
Runtime shutdown or journal append failure can still lose the asynchronous audit
record; no crash-atomic remote publication or fsync guarantee is claimed.
