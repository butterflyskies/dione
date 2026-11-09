# Discord gateway runtime

Owns the registered SDK gateway, private verification/admission proofs, lifecycle enforcement, observations and current source revalidation. Application code supplies concrete services and consumes enforcing facades. Observation remains disabled by default. The optional test-support feature exposes only a read-only verification trace.
