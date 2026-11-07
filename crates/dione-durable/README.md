# Dione durable queues

`dione-durable` owns generic durable admission, replay suppression, leases,
consumer registration, and resident binding. It retains accepted operations
through asynchronous caller cancellation. Draining waits until accepted workers
release their storage resources, including after caller cancellation or worker
panic. Adapter-selected codecs preserve
their own payload and snapshot formats.

`QueueSnapshot` and the records in `state` are detached codec transfer values.
The queue validates decoded state before installing it. Its live store, engine,
and resident capability fields remain private.

The `atomic_snapshot` module supplies the shared write, file-sync, rename, and
directory-sync protocol. A failure after rename explicitly reports visible
state whose crash durability is uncertain. Callers retain their own path and
private-directory admission policies.

## Test support

The default-off `test-support` feature exposes clock controls, persistence fault
injection, completion pauses, and snapshot inspection or mutation for adapter
integration tests. Enable it only for testing. It is additive Cargo functionality,
not a security boundary. Ordinary production dependencies do not select it;
Dione selects it through a dev-dependency so tests can preserve their existing
fault and cancellation coverage.

## Private audit files

`private_file` supplies admitted append, read, and atomic replacement for audit
owners. On Unix it requires current-user regular files with one link and mode
0600. Legacy 0644 files migrate through their descriptor only under a verified
owner-only 0700 parent; exposed files and aliases are rejected. Missing owned
parents are created with mode 0700. The existing-parent append operation preserves
callers that require a missing directory to fail.

Replacement uses exclusive unique staging, preserves the original on pre-rename
failure, and retains the published object after post-commit cleanup failure.
Legacy sibling cleanup is limited to the admitted exact sibling and assumes the
trusted parent and serialized owner; it is not a defense against a same-user
concurrent namespace writer. Ordinary unlink failures can leave staging behind,
and abrupt process termination is outside the cleanup guarantee. Non-Unix
behavior is preserved; these guarantees do not claim Windows ACL enforcement.
