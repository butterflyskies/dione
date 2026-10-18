# Discord introspection tool ownership

Seven metadata tools now register through the Discord owner. Their schemas,
parsing, typed operations, wire projections, and current configuration behavior
are retained. Resident access belongs to registration metadata. The server keeps
all default Discord tools when adding optional registrations and validates the
initial set through the same collision checks. Server construction now exposes
an exhaustive, source-bearing error.

Independent frozen-source review found no P1/P2 issues. The native initial r1
check ran feature Clippy and 237 focused tests successfully; 19 before/after test
binary hashes matched. Tests include all seven registered dispatch routes,
invalid/allowed inputs, literal HTTP envelopes, config reload, collision checks,
and resident socket access with discovery denial. Source copyback was limited to
reviewed formatter changes; all 18 source manifest entries, including the moved
wire deletion, were compared against native bytes before metadata changes.

Version 0.74.0, changelog, and this audit were added after that focused run.
No complete signed-head gate, mutation test, official approval, or publication is
claimed. Final candidate review and the full pr-ready gate remain required.
This unit does not complete central MCP independence.
