# Rook gateway observation boundary

This internal capability observes authorized Discord messages independently of
ordinary assistant mention handling. Production startup leaves it disabled.
There is no Rook subscription, webhook delivery, durable acceptance or outbound
publication in this change.

An enabled owner supplies room and sender scope, its own actor identities, and a
bounded receiver. Current Dione room, sender, ignore, mute and verified webhook
restrictions still apply. EveryoneInRoom removes only the additional recipient
sender filter. Self transport and verified represented identities are excluded.
Message content cannot change policy. Observations never enter the existing
reply-authority ledger.

Creates identify their exact source-qualified message. Edits additionally carry
the authoritative UTC edit instant at its original precision. Duplicate and
out-of-order observations retain those identities; this receiver does not
perform durable deduplication. The common context and Discord native supplement
retain reference coordinates and attachment metadata without recursive parent
bodies. Observing a child does not authorize reading its parent.

The complete observation preprocessing, including topology, exact reads and
verification, has one five-second deadline and at most four concurrent operations
per ingress owner. Cheap room, self, mute and known sender exclusions precede IO.
An exact recipient room denial overrides parent thread inclusion.

A partial or reference-ambiguous cached edit can make one authoritative exact
read. Saturated admission returns
Busy immediately. A fetched newer revision cannot substitute for the observed
edit. No owner, full receiver and closed receiver skip hydration. Failures have
finite typed outcomes. New dispatch diagnostics contain coordinates and finite
outcomes, not message bodies or attachment URLs. Existing Discord lookup
logging is unchanged.

The pinned Serenity 0.12.5 MessageUpdateEvent decoder collapses an absent
message_reference and an explicit JSON null into the same outer None. A supplied
Some(None) is preserved and checked, but the typed callback cannot recover wire
presence lost by the SDK. A cached same-revision snapshot can retain a removed
reference, so absent typed
reference presence always requires an authoritative exact read. Only a matching
authoritative result establishes the reference state in that case. This change
does not claim lossless raw gateway patch capture.

Offered means placed in a bounded in-memory receiver. Full and closed outcomes
remain failures and do not prevent ordinary notification processing. Retained
observations have a conservative size budget of one MiB. The subsequent durable
owner must handle retention, current authorization, retry/replay, status and
restart recovery before production enablement.
