# dione-evidence

This crate owns terminal sentex syntax, canonical handle encoding, role-separated
tool arguments, JSON projection, and the shared Markdown block scanner.
Chunking, status lint, and contradictionary can use the grammar without depending
on message delivery or attention.

The checked tool parser retains aggregate count checks before individual handle
decoding, followed by marker-byte validation. Handle representations and token
decoders remain private. Public consumers use parsed locators, borrowed handle
slices, and marker formatting operations.

Author projection associates parsed locators with the Discord `UserId` supplied
by the caller. It does not authenticate that ID, check current access, or verify
evidence truth. Discord ingress remains responsible for authenticated source
authorship.

Markdown consumers choose quote scope. The default scanner retains evidence's
across-block spans; `BlockScanner::block_scoped` confines spans to their quote
block. Both retain the existing fence and code exclusion rules. The same scanner
supports evidence parsing, chunking, lint, and filtering.

The existing Serenity ID dependency and vendored native TLS configuration remain
unchanged. The member declares native TLS explicitly so standalone builds retain
Dione's OpenSSL configuration.

This package uses Rust 1.98 or newer and the MIT OR Apache-2.0 license.
