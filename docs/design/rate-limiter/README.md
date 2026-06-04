# Rate Limiter Design

**Status:** Draft

Token-bucket rate limiter for bot-to-bot (and eventually human) message gating,
enforced server-side in dione. Each dione instance runs independently with no
shared state -- enforcement happens in the message delivery path before MCP
channel forwarding, so bots cannot bypass it.

## Artifacts

| Document | Description |
|----------|-------------|
| [spec.md](spec.md) | Full design specification -- problem, state machine, types, config, properties, test plan |
| [RateLimiter.tla](RateLimiter.tla) | TLA+ formal model of the token-bucket state machine with safety and liveness properties |
