# Dione harness core

This crate owns the coupled delivery/protocol connection and shared JSON-line
output. Both facets come from one adapter instance. Application factories pass
the same output used by their adapter. Private connection fields prevent later
independent facet replacement; they do not establish an arbitrary adapter's
internal behavior.

`ProtocolOutput` clones share a writer mutex and poison state. Each frame has one
ten-second deadline covering mutex acquisition, writing, and flushing. JSON
serialization precedes that deadline. Failure and cancellation preserve existing
poisoning and completion-uncertainty classifications. Concrete error causes
remain available through the error chain.

The application retains provider factories and runtimes, session configuration,
request-origin authorization, reserved control names, and tool registration.
The private-control trait hook requires application-authorized routing; exposing
the trait grants no remote caller access. Claude, Codex, and Oh My Pi adapters
remain in Dione.

Core writer and source-cause tests move here. The adapter uncertainty test stays
with the application. The added shared-state test exercises both facets across
connection clones. Compiler examples protect private facet fields and the public
construction path. Tests require Tokio's test-util feature only as a dev edge.

This package uses Rust 1.98 or newer and the MIT OR Apache-2.0 license.
