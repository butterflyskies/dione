# Dione Teams identities

Six distinct opaque types own tenant, application, bot, actor, conversation and
activity coordinates. Their private String representations preserve original
spelling, case and spaces. Parsing rejects empty values, more than 512 UTF-8
bytes and Unicode control characters, in that order. Whitespace-only values
remain accepted. Validating deserialization uses the same parser as ordinary
construction. Display, FromStr, TryFrom and String conversion retain the value.

Errors retain semantic identity and constraint categories without storing the
rejected value. These exhaustive typed errors and the existing validator move
unchanged. No generic identity, normalization, new authorization or lifecycle
mechanism is added. A syntactically valid coordinate does not grant reply
authority. Application config, authentication and persisted reply authority
continue to apply their current checks.

Consumers import this owner directly. The old application Teams identity module
and compatibility-only facade paths are removed. Member tests exercise external
serde, literal historical-shaped records, byte boundaries, private construction
and type distinction. Actual persisted config/auth/store consumers remain
application tests; a standalone identity roundtrip is not their acceptance proof.
