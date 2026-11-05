# Dione presentation

Project retained typed deliveries into shared content and metadata for provider
adapters. The crate owns single, managed, control, and compact batch formatting.
Provider envelopes, transport output, ingress conversion, and current authorization
remain with the embedding application.

Call `dione_presentation::project(&delivery)` and match `Presentation::Channel` or
`Presentation::Permission`. Channel metadata retains text, boolean, and locator-list
values. Provider adapters choose their envelope and metadata encoding.

Projection preserves supplied observations. It does not authenticate identities,
check current access, or establish delivery completion. Evidence visibility controls
whether retained locator metadata is included. It does not grant access to evidence.

Grouping and the projector's metadata helpers remain private. Ordinary singleton deliveries
use single-event formatting; batches group by source-qualified conversation and
preserve event order within each conversation.
