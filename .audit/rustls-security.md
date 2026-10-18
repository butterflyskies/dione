# Rustls security update

## Scope and decision

The 2026-10-14 advisory refresh reports RUSTSEC-2026-0346 through
RUSTSEC-2026-0349. The patched release is Rustls 0.23.46. The old lockfile contains
both Rustls 0.22.4 and 0.23.45. Updating only the latter leaves a vulnerable
Discord gateway dependency.

The crates.io API reports Serenity 0.12.5 as the latest released version. It
requires tokio-tungstenite 0.21, whose Rustls backend requires Rustls 0.22.
[Serenity PR 3599](https://github.com/serenity-rs/serenity/pull/3599) proposes
tokio-tungstenite 0.23.1, but remains unmerged and unpublished at this check.
Using that git revision would violate the public package boundary. No dependency
source exception, advisory suppression, or registry source patch is introduced.

Use Serenity's supported native TLS backend while retaining its released API.
Native TLS uses OpenSSL on Linux. Enable its vendored feature so every builder
does not need OpenSSL development packages and the resulting binary does not
depend on a shared OpenSSL library. The lockfile selects native-tls 0.2.18,
openssl 0.10.81, and openssl-src 300.6.1+3.6.3. This adds a C/Make/Perl build
requirement. The production Linux runtime must supply its system CA roots.

Cargo feature unification also enables Reqwest's native TLS default. Existing
Dione HTTP constructors explicitly select Rustls to preserve their trust-store
and TLS behavior. Discord's Serenity clients use native TLS and system roots.
The validated lockfile retains Serenity on Reqwest 0.12.28, shared with Dione,
and RMCP on its existing Reqwest 0.13.5 with only Rustls enabled, using the
platform verifier rather than Dione's direct-client bundled roots. This preserves
Serenity's injected-client type and RMCP's default Rustls transport. Recheck
these resolved features when updating dependencies; Serenity's broad Reqwest
constraint permits a different choice in a freshly generated lockfile.
Externally supplied Reqwest clients remain the caller's responsibility. No
certificate or hostname verification is disabled. Revisit this backend choice
when a secure Serenity release supports the newer Rustls stack.

## Verification

- `cargo fmt --all` completed.
- `cargo deny --locked check advisories` passed against the refreshed advisory
  database. Rustls 0.22 and rustls-webpki 0.102 are absent. Four obsolete WebPKI
  exceptions were removed. Rustls 0.23.46 is the only resolved Rustls version.
- `cargo deny --locked check` passed advisories, bans, licenses, and sources.
- `tests/transport_tls.rs` exercises real local TLS handshakes for native HTTP
  and websocket clients. Each protocol has an authenticated successful response,
  rejection of an untrusted certificate, and rejection of a trusted certificate
  for the wrong hostname. The tests use no live service or credentials.
- Native compilation, these integration tests, both full test configurations,
  package checks, release build, and independent review remain pending. This
  document does not claim those checks passed.

## Integration route

This atomic dependency fix is based on accepted integration commit 43d2e5b.
Remaining PRs need this change in their base or an independently reviewed
composition of it before their dependency gate can pass. Do not reuse a prior
signed candidate's test receipt after changing its dependencies or source.
Retain the existing immutable candidates and regenerate each final gate at the
new signed head. Reconcile version entries when combining stacked releases.
