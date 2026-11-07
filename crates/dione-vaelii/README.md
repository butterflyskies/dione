# Dione Vaelii client

This client writes the existing six provenance-only sentexes for an explicit
message read. Callers supply optional endpoint configuration, the message ID,
and their construct identity. It does not retrieve message content or authorize
the read. Dione retains those application checks and response handling.

An absent or blank URL disables writing. Configured requests validate the actor
symbol and HTTP/HTTPS endpoint, append `/op` when needed, disable redirects, and
use the existing timeout clamp. A nonempty `VAELII_API_TOKEN` environment variable
adds bearer authentication. Non-success responses retain their status without
projecting the response body.

The complete client and its seven existing tests live here. Tests cover EDN
batching, symbol/endpoint checks, disabled operation, exact loopback POST content,
and status-only refusal. No new service protocol or client abstraction is added.

This crate is part of the Dione workspace and uses its MIT OR Apache-2.0 license.
