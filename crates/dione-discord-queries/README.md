# dione-discord-queries

Authorized Discord history, exact message and pin reads, attachment downloads and guild search, with typed observations and v1/v2 wire projections. QueryCtx carries only HTTP, current configuration, live channel-policy state, inbox directory and receipt identity. It carries no sealed event admission or delivery authority. Root callers retain tool registration, resident admission, argument parsing and call-entry configuration selection.

Default search transport targets Discord v10. A caller selecting an explicit HTTP API endpoint must trust that destination to receive its Discord authorization token. Typed observations do not grant authority to disclose content elsewhere. SDK errors retain their typed sources; v2 wire failures preserve fixed safe categories.
