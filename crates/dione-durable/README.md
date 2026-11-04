# Dione durable queues

`dione-durable` owns generic durable admission, replay suppression, leases,
consumer registration, and resident binding. It retains accepted operations
through asynchronous caller cancellation. Adapter-selected codecs preserve
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
