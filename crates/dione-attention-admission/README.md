# dione-attention-admission

The complete process-local admission controller for already-authorized Discord observations. It owns deduplication, judgment cancellation, incarnation/configuration fences and per-channel FIFO release, including invalidation and deferred committed results.

Callers supply events only after existing ingress and rate-limit checks. A returned decision or ready event is metadata, not authority to deliver: the embedding runtime still validates current source/policy, commits durable decisions and applies its final delivery guard. Controller fields remain private; no network, sealed ingress, durable-store or notification-sink ownership moves here.
