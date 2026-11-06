# Dione drop ledger

Retains up to 1024 dropped message ids per scope, evicting oldest first. Recording
an existing id does not refresh its position. Scopes never evict one another.
Direct replies inherit a recorded parent's drop; the caller records each new hop.

This is retention, not admission authority. Native Dione policy derives configured
scopes, records appropriate drops and keeps identity-ignore stateless. The library
does not enforce opt-in or bound scope count. Restart loses all state; there is no
TTL or persistence. Mutex poison still panics. The singleton is shared by native
ingress and canonical fallback without a second root store.

Serenity uses the workspace's existing native TLS backend; the vendored native-tls
feature is required for the same standalone build closure as existing ID owners.
Run cargo nextest run -p dione-drop-ledger --locked for owner and external cases.
