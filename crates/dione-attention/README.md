# dione-attention

The attention engine owns bounded TypeSafe judgments, source-bound decision
metadata, attributed feedback, replay, fitting, evaluation, and promotion.
The application owns current source access, export consent, scheduling, and
transport.

`AttentionStore` retains identities, hashes, judgments, and learning provenance.
It has no field for Discord message text. Storage and learning share private
snapshot state so sampling, evaluation, and promotion use the same durable
records. Public record lookup and representative review operations keep that
state private.

`TypeSafeProvider` checks export authority before each request attempt. The
application supplies already authorized excerpts and remains responsible for
current policy. Delivery descriptors retain source evidence; possession of an
identity, descriptor, score, or learned artifact does not authorize delivery.

The existing source records use Serenity's Discord ID types. This extraction
preserves those types and HTTP/TLS settings. It does not make the engine platform
neutral.
The member declares the workspace's vendored native TLS dependency so standalone
builds retain Dione's OpenSSL configuration.

The default-off `test-support` feature exposes the existing loopback-only HTTP
constructor to hermetic application fixtures. It preserves the URL checks and is
additive Cargo functionality, not a security boundary. Production dependencies
do not enable it.

This package uses Rust 1.98 or newer and the MIT OR Apache-2.0 license.

Holdout anti-reuse reservations use the configured record-retention horizon
(default seven days) from opening, with checked, persisted deadlines that never
extend. Expiry deletes declarations, sealed provenance and receipts and withdraws
dependent active/rollback authority. Invalidated reservations remain until that
boundary; new evidence after expiry must satisfy the ordinary source, temporal
partition and fitting constraints. Confirmed source loss removes review and
sealed-evaluation provenance together; unavailable sources remain Unknown.

`control::AttentionCommand` and `control::schema` describe the complete authenticated control surface. Commands retain closed decoding and descriptive mutation classification; they do not grant execution permission. The application rechecks current recipient, source and mutation authority at its effect boundaries. The existing default-off `test-support` feature also enables command serialization for hermetic application fixtures.
