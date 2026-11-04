# Teams SDK compatibility reference

Dione retains its Rust Teams adapter. The protocol reference is Microsoft Teams
SDK for TypeScript `v2.1.1`, commit
`79ed749cb65c7f14bb9b1f94b8cfdc7b9c5a5b81`, released October 16, 2026.
The [release](https://github.com/microsoft/teams.ts/releases/tag/v2.1.1)
includes the Entra Agent ID caller-validation fix. This is a versioned target,
not a Microsoft certification or a claim that Dione runs the official SDK.

## Current behavior

The V01 fixture covers classic Connector authentication of one synthetic
personal text message and Dione's existing correlated reply construction.
The HTTP ingress regression covers rejected headers, malformed tokens, malformed
Activity JSON, successful text admission, and one single-use reply to a local
fake Connector. Existing Teams tests cover additional durable admission,
restart, replay, and uncertain-send boundaries.

These test definitions do not establish a live Teams round trip, qualification
of any tenant or cloud, or behavior equivalence with the official SDK runtime.
The [fixture provenance](../../tests/fixtures/teams/README.md) identifies the
pinned source and synthetic data. Native verification results belong to the PR
at its exact source commit.

The current adapter admits text messages only. It drops optional message context,
fetches Connector keys per request, and exposes one plain-text reply tool. Schema
and policy failures still return HTTP 401 in V01. V02 changes those HTTP outcomes;
V03 and later cards extend authentication. The reply fixture records current
Dione bytes, not an SDK send/reply/quote oracle.

## Capability inventory

The [Teams issue board](https://forgejo.svc.echoes/lacuna/dione/issues/580)
contains the accepted delivery order, dependencies, adoption, and live status.
The rows below are target slices, not supported-feature declarations. Each issue
owns its acceptance evidence and identifies any incomplete qualification.

| Slice | Target behavior | Acceptance owner |
| --- | --- | --- |
| V01 | Pin compatibility baseline and finish rejection diagnostics | [#581](https://forgejo.svc.echoes/lacuna/dione/issues/581) |
| V02 | Correct HTTP outcomes | [#582](https://forgejo.svc.echoes/lacuna/dione/issues/582) |
| V03 | Match classic Connector JWT rules | [#583](https://forgejo.svc.echoes/lacuna/dione/issues/583) |
| V04 | Survive signing-key rotation | [#584](https://forgejo.svc.echoes/lacuna/dione/issues/584) |
| V05 | Support configured Azure clouds | [#585](https://forgejo.svc.echoes/lacuna/dione/issues/585) |
| V06 | Reuse valid bot access tokens | [#586](https://forgejo.svc.echoes/lacuna/dione/issues/586) |
| V07 | Support user-assigned managed identity | [#587](https://forgejo.svc.echoes/lacuna/dione/issues/587) |
| V08 | Support federated credentials | [#588](https://forgejo.svc.echoes/lacuna/dione/issues/588) |
| V09 | Support Entra Agent ID authentication | [#589](https://forgejo.svc.echoes/lacuna/dione/issues/589) |
| V10 | Preserve complete inbound message context | [#590](https://forgejo.svc.echoes/lacuna/dione/issues/590) |
| V11 | Match send, threaded reply and quoted reply semantics | [#591](https://forgejo.svc.echoes/lacuna/dione/issues/591) |
| V12 | Handle Connector rejection and throttling correctly | [#592](https://forgejo.svc.echoes/lacuna/dione/issues/592) |
| V13 | Process installation and membership lifecycle | [#593](https://forgejo.svc.echoes/lacuna/dione/issues/593) |
| V14 | Deliver inbound message edits and deletions | [#594](https://forgejo.svc.echoes/lacuna/dione/issues/594) |
| V15 | Return synchronous invoke responses | [#595](https://forgejo.svc.echoes/lacuna/dione/issues/595) |
| V16 | Send rich text, mentions and suggested actions | [#596](https://forgejo.svc.echoes/lacuna/dione/issues/596) |
| V17 | Send Adaptive Cards | [#597](https://forgejo.svc.echoes/lacuna/dione/issues/597) |
| V18 | Handle Adaptive Card interactions | [#598](https://forgejo.svc.echoes/lacuna/dione/issues/598) |
| V19 | Update and delete bot-authored messages | [#599](https://forgejo.svc.echoes/lacuna/dione/issues/599) |
| V20 | Send proactive messages | [#600](https://forgejo.svc.echoes/lacuna/dione/issues/600) |
| V21 | Send targeted messages | [#601](https://forgejo.svc.echoes/lacuna/dione/issues/601) |
| V22 | Receive and send reactions | [#602](https://forgejo.svc.echoes/lacuna/dione/issues/602) |
| V23 | Handle typing indicators and read receipts | [#603](https://forgejo.svc.echoes/lacuna/dione/issues/603) |
| V24 | Stream responses | [#604](https://forgejo.svc.echoes/lacuna/dione/issues/604) |
| V25 | Support user OAuth sign-in and sign-out | [#605](https://forgejo.svc.echoes/lacuna/dione/issues/605) |
| V26 | Support SSO and delegated Graph access | [#606](https://forgejo.svc.echoes/lacuna/dione/issues/606) |
| V27 | Receive files and inline images | [#607](https://forgejo.svc.echoes/lacuna/dione/issues/607) |
| V28 | Send inline images | [#608](https://forgejo.svc.echoes/lacuna/dione/issues/608) |
| V29 | Send files with consent | [#609](https://forgejo.svc.echoes/lacuna/dione/issues/609) |
| V30 | Add AI labels, citations and feedback | [#610](https://forgejo.svc.echoes/lacuna/dione/issues/610) |
| V31 | Support dialogs and multistep forms | [#611](https://forgejo.svc.echoes/lacuna/dione/issues/611) |
| V32 | Support search message extensions | [#612](https://forgejo.svc.echoes/lacuna/dione/issues/612) |
| V33 | Support action extensions and link unfurling | [#613](https://forgejo.svc.echoes/lacuna/dione/issues/613) |
| V34 | Handle meeting and remaining event routes | [#614](https://forgejo.svc.echoes/lacuna/dione/issues/614) |
| V35 | Validate packaging and prove compatibility | [#615](https://forgejo.svc.echoes/lacuna/dione/issues/615) |

## Compatibility boundaries

Dione retains explicit tenant, conversation, service-host, and channel policy.
SDK compatibility does not authorize an arbitrary destination or additional
identity. Tokens, raw Activities, and arbitrary deserialization errors do not
belong in rejection logs. Validated IIS ARR UUIDs provide request correlation.

Event formats and replay identities migrate before new event families use them.
The strict OMP receipt format and previous-version queues remain part of the
compatibility boundary. An uncertain outbound POST does not receive a blind
retry. Ordinary asynchronous message acknowledgment remains HTTP 202 after
committed admission; V01 makes no new invoke or response-status claim.

Embedded tabs, TeamsJS browser APIs, and real-time calling or media are outside
this conversational plugin inventory. A future SDK upgrade requires a new pin,
reviewed differences, and refreshed behavioral evidence.
