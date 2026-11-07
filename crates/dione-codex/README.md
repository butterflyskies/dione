# dione-codex

Owns the durable Codex inbox, historical persisted codec, ordinary tool adapter, app-server delivery worker and prepared/running session lifecycle.

`CodexSession::prepare` opens the inbox without resolving environment settings or connecting to the app server. Its connection supplies the actual Codex tool catalog and delivery adapter. `start` resolves live settings, commits the initial binding and starts the owned worker. Running shutdown cancels and joins that worker before draining pending durable operations.

`teams_delivery` assembles native Teams admission over the same queue. Resident handles, binding controllers, lease authority, persisted payload representation and fault controls remain private. Queue diagnostics and public pull registrations do not grant resident authority.

The historical codec preserves stored payload bytes and replay identities. Protocol acknowledgements record observed transport outcomes; they do not establish unobserved remote effects.

Owner-local tests retain private persistence faults, actual Unix WebSocket delivery and lifecycle checks. A private ignored fixture service supports application dispatch and native integration tests through descriptive test transport. It is absent from production builds.
