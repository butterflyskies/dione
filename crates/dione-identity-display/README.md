# Dione identity display

This crate owns literal identity formatting and the existing nameplate and
PronounDB enrichment clients. It uses supplied identifiers and names; the
application retains identity authority, configuration, and bot/human routing.

`format_identity` preserves supplied text. `display_name_with_pronouns` preserves
an existing recognized suffix over fetched enrichment. Both clients use that
same formatter. They retain separate HTTP, cache, and failure policies.

Nameplate resolution fetches the configured YAML file, sanitizes appended
pronouns, and serves stale cached entries after refresh failure. Its caller
enforces exclusions. Pronoun resolution also checks exclusions internally,
sends uncached Discord IDs to PronounDB, and caches failures as absence.
Explicit suffix handling does not bypass lookup; cache hits can avoid HTTP.

The nameplate body limit is checked after buffering when Content-Length is
absent. Existing request URL, redirect, deadline and user-agent behavior stays
unchanged. Send and body-read failures retain debug diagnostics with fixed safe
categories instead of formatting request errors that can carry private URLs. Nameplate names and biographies do not replace the supplied display
name. HTTP client constructors panic if client construction fails.

Formatter and client assertions are retained. Provider-failure tests use private
unit-test-only local HTTP endpoints, with actual request-count checks for cached
absence, literal successful values, exclusions and stale nameplate recovery.
Ordinary tests do not call PronounDB. The test endpoint seam is absent from
production builds and adds no public override API. The crate includes its HTTP
dependencies and has no messages, attention, or platform-crate dependency.

This package uses Rust 1.98 or newer and the MIT OR Apache-2.0 license.
