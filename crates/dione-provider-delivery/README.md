# Dione provider delivery

Shared provider-qualified replay identities, descriptive Teams resident events,
borrowed resident admission sinks and classified outcomes. The existing Codex
and OMP runtimes own their separate persistence codecs and replay ordering.
Native authenticated envelopes, sealed reply records, current authorization,
prepare/consume/send sequencing and concrete composition adapters stay with
those owners.

ProviderEventKey preserves adjacent-tag JSON and validated native coordinate
spelling. TeamsResidentEvent is descriptive data and can contain an independent
provider key; consumers retain their native-coordinate equality checks.

ReplyHandle validates syntax while preserving historical UUID spelling. Typed
UUID conversion formats canonical handles. This crate exposes no random
handle generator, authenticated proof or reply authority constructor. A handle
alone does not authorize sending. TeamsDelivery clones share one private sink
and preserve distinct committed, duplicate, durability-uncertain and
completion-unknown outcomes and original causes.
