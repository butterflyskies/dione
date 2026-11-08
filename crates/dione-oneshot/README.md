# Dione Oneshot

Owns the complete stateless one-shot send driver: named token selection,
read-only configuration, numeric identity before POST, target/chunk/judge checks,
nonces, dry-run and ordered delivery outcomes. The dione-send binary retains
argv/stdin, process exits and stderr logging. No gateway or persistent service
state is initialized, and ambiguous delivery is never retried internally.

The oneshot-test-seam endpoint override is default-off. Root explicitly forwards
that feature for mock/process tests; production builds have no override flag or
mock driver entry point. Existing configuration, gate, consent/chunking and
pre-send owners remain separate and unchanged.

A successful message POST followed by an SDK response-decode failure is ambiguous:
its result requires reconciliation and never authorizes an automatic retry. This
also applies when earlier chunks have confirmed IDs; their IDs remain available.
