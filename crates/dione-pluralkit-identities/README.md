# Dione PluralKit identities

PkUuid validates configured identifiers as exactly 36 UTF-8 bytes with ASCII hex
and dashes at positions 8, 13, 18, and 23. Accepted input becomes lowercase.
Nil UUIDs and all hex version/variant values remain accepted. Short IDs,
whitespace, non-ASCII input, misplaced dashes, and trailing content are rejected.

The private String is available through parse, as_str, and Display. Errors expose
an exhaustive Empty or InvalidFormat category without retaining rejected input.
Diagnostics now use lowercase fragments instead of the former string errors.
No serde, FromStr, mutable access, or network dependency is added.

PLURALKIT_APPLICATION_ID identifies the official deployment. The application
retains configured trust, verified-event construction, and represented-principal
authority. The HTTP resolver retains its existing uuid::Uuid response parser.
