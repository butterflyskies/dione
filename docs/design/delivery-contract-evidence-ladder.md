# Delivery Contract: Four-Level Evidence Ladder

Four levels. Non-collapsible by construction. Each proves exactly one thing and nothing more.

```mermaid
flowchart TB
    subgraph ladder["Evidence Ladder — each level proves ONE thing"]
        direction TB

        L1["Level 1: PERSISTED\n━━━━━━━━━━━━━━━━━━━━\nEvent stored durably\n\nProves: the event exists\nin the canonical store\n\nDoes NOT prove: anyone\nknows about it yet"]

        L2["Level 2: TRANSPORT ACCEPTED\n━━━━━━━━━━━━━━━━━━━━\nProvider/harness handoff succeeded\n\nProves: the transport layer\naccepted the payload\n\nDoes NOT prove: the consumer's\nruntime received it"]

        L3["Level 3: HARNESS INJECTED\n━━━━━━━━━━━━━━━━━━━━\nConsumer runtime received it\n\nProves: the harness wrote it\ninto the consumer's context\n\nDoes NOT prove: the consumer\nprocessed or acted on it"]

        L4["Level 4: CONSUMER DISPOSITION\n━━━━━━━━━━━━━━━━━━━━\nConsumer explicitly recorded outcome\n\nProves: the consumer made\na deliberate decision\n(handled / deliberately_held /\nretry_requested)\n\nCoordinator/policy outcomes\n(expired / voided / dead_lettered)\nare IntentOutcome, not consumer\nclaims.\n\nThis is the outside check."]

        L1 -->|"transport picks it up"| L2
        L2 -->|"harness writes to runtime"| L3
        L3 -->|"consumer records decision"| L4
    end

    subgraph incident["FIELD RECEIPT: Vesper's Incident"]
        direction TB
        what["Dione claimed send ✓\nReply rendered on Discord ✓\nTurn never persisted to transcript ✗"]
        gap["L2 (transport accepted) said YES\nL4 (consumer disposition) was NEVER RECORDED\n\nThe gap between 2 and 4\nhid the failure with\nzero in-session signal"]
        found["Only caught by reconciling\nagainst Discord history after the fact"]
    end

    L2 -.-|"⚠ FAILURE HID HERE"| gap

    subgraph principle["Design Principle"]
        direction TB
        p1["The check lives outside\nthe thing it checks."]
        p2["Claude stdout can claim\nwrite/flush at best."]
        p3["ConsumerDisposition IS\nthe outside check."]
    end

    L4 -.- p3

    style L1 fill:#16213e,stroke:#0f3460,color:#eee
    style L2 fill:#1a1a2e,stroke:#e94560,color:#eee
    style L3 fill:#1a1a2e,stroke:#e94560,color:#eee
    style L4 fill:#0f3460,stroke:#00d2ff,color:#eee
    style gap fill:#2d0000,stroke:#ff0000,color:#ff6b6b
    style incident fill:#1a0000,stroke:#ff0000,color:#ff9999,stroke-dasharray: 5 5
    style principle fill:#0d0d0d,stroke:#533483,color:#ccc
    style p3 fill:#0d0d0d,stroke:#00d2ff,color:#00d2ff
```

## Why non-collapsible

Each level answers a different question to a different audience:

| Level | Question | Who answers |
|-------|----------|-------------|
| 1. Persisted | Does the event exist? | Canonical store |
| 2. Transport accepted | Did the handoff succeed? | Transport layer |
| 3. Harness injected | Did the runtime receive it? | Harness adapter |
| 4. Consumer disposition | Did the consumer act? | Consumer itself |

Collapsing any two creates a blind spot. Vesper's incident is the proof: levels 2 and 4 diverged silently. The system had no way to detect the gap because it trusted transport acceptance as proof of consumer processing.

The fix is structural, not operational. ConsumerDisposition is a separate record type precisely so that "the check lives outside the thing it checks."
