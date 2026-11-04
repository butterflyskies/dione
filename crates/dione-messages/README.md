# dione-messages

This crate owns observed event values, typed deliveries, identity presentation,
and fixed-offset timestamps shared by platform adapters and agent harnesses.
It performs no source lookup, authorization, scheduling, or transport operation.

`ObservedText` retains its original bytes and the locators parsed from those
bytes. Ordinary deliveries require a first event, so their batches remain
nonempty. Managed deliveries retain supplied source evidence, a recorded
directive, and a decision ID. The application still checks current authority
before dispatch.

Event constructors retain supplied observations. An author classification,
targeting reason, permission notice, identity, or delivery descriptor does not
itself establish current authority. Private fields retain the existing
construction operations and readonly accessors.

`Timestamp` parses and serializes RFC 3339 values and supports the existing
`From<DateTime<FixedOffset>>` conversion. The application chooses timezone policy;
compact formatting accepts that choice explicitly. Configuration localization,
fallback, and DST tests remain with the application.

The crate depends on provider contracts, attention delivery types, and evidence
grammar. It preserves their existing public types and backend dependencies.
This extraction adds no plugin registry or alternative message representation.

This package uses Rust 1.98 or newer and the MIT OR Apache-2.0 license.
