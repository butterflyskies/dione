# Discord management tool ownership

Four owner registrations replace the central pin_message, unpin_message,
delete_message, and create_thread routes. Typed MessageCommand and thread requests
reuse existing authorization, target checks, HTTP operations, and error causes.
The wire module moves intact into Discord. Schemas, parsing order, optional-ID
permissiveness, current configuration, and resident exposure remain unchanged.

Independent source review's test-only import finding was corrected in frozen R2;
no remaining findings were reported. Native initial-r2 verification completed
with feature Clippy and 571 focused tests passing. Nineteen before/after test
binary hashes match. All ten manifest entries, including the old wire deletion,
were compared against native source bytes after reviewed formatter copyback.

Coverage includes literal HTTP requests for three mutations and both thread
forms, invalid and denied calls followed by permitted calls after config reload,
revocation, shared thread-parent state, registration preservation and collisions,
and resident admission/discovery restrictions. Existing target/authority and
wire/error tests remain.

Version 0.75.0 and release metadata were added after the focused run. The full
signed-head gate, decisive mutation, final candidate review, and official approval
remain pending. No publication or complete MCP independence is claimed.
