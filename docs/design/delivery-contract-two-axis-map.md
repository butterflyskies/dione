# Delivery Contract: Two-Axis System Map

Provider normalization on the left. Harness delivery on the right. Audit sits orthogonal — it observes, it doesn't gate.

```mermaid
flowchart LR
    subgraph providers["Messaging Providers"]
        direction TB
        discord["Discord"]
        slack["Slack"]
        signal["Signal"]
        mastodon["Mastodon"]
        bus["Message Bus"]
    end

    subgraph normalization["Normalization Layer"]
        direction TB
        da["Discord Adapter"]
        sa["Slack Adapter"]
        sga["Signal Adapter"]
        ma["Mastodon Adapter"]
        ba["Bus Adapter"]
    end

    subgraph canonical["Canonical Layer"]
        direction TB
        ce["CanonicalEvent\n― immutable occurrence ―\nEventId · source identity\npayload · timestamp"]
        di["DeliveryIntent\n― routing decision ―\nevent → consumer\npolicy · binding"]
    end

    subgraph delivery["Harness Delivery"]
        direction TB
        claude["Claude Adapter\n(stdout push)"]
        codex["Codex Adapter\n(pull + durable queue)"]
        future["Future Consumer\nAdapter"]
    end

    subgraph evidence["Evidence Records"]
        direction TB
        attempt["DeliveryAttempt\n― one leased try ―\ntransport evidence"]
        disposition["ConsumerDisposition\n― consumer-authored ―\nhandled · deliberately_held\nretry_requested"]
        outcome["IntentOutcome\n― coordinator/policy ―\nsatisfied · expired\nvoided · dead_lettered"]
    end

    subgraph orthogonal["Orthogonal Concerns"]
        direction TB
        audit["Audit Journal"]
        retention["Retention Policy"]
    end

    discord --> da
    slack --> sa
    signal --> sga
    mastodon --> ma
    bus --> ba

    da --> ce
    sa --> ce
    sga --> ce
    ma --> ce
    ba --> ce

    ce --> di

    di --> claude
    di --> codex
    di --> future

    claude --> attempt
    codex --> attempt
    future --> attempt

    attempt --> disposition
    attempt --> outcome

    ce -.- audit
    di -.- audit
    attempt -.- audit
    disposition -.- audit
    outcome -.- audit
    ce -.- retention

    style canonical fill:#1a1a2e,stroke:#e94560,color:#eee
    style evidence fill:#1a1a2e,stroke:#0f3460,color:#eee
    style orthogonal fill:#0d0d0d,stroke:#533483,color:#ccc,stroke-dasharray: 5 5
```

## Record flow

1. **CanonicalEvent** — created at normalization. Immutable. Its own `EventId`; source message identity (Discord message ID, etc.) is a field, not the primary key.
2. **DeliveryIntent** — created when routing decides this event should reach a specific consumer. Carries the policy and binding.
3. **DeliveryAttempt** — one transport try. Leased, timed. Carries transport-layer evidence.
4. **ConsumerDisposition** — the consumer's explicit answer: `handled`, `deliberately_held`, `retry_requested`. Consumer-authored facts only.
5. **IntentOutcome** — coordinator/policy decisions: `satisfied`, `expired`, `voided`, `dead_lettered`. Not consumer claims — carries decision provenance.

Audit observes all five. Retention is per record class (raw evidence, canonical content/metadata, intents, attempts, dispositions, derived indexes, audit receipts). Neither gates delivery.
