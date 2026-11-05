# dione-teams

Owns native Teams JWT and scope validation, authenticated ingress, durable single-use reply authority, Connector replies, listener lifecycle and the Teams MCP tool. Application-specific resident queue adapters and process assembly remain in Dione.

Authenticated envelopes, metadata parsing, sealed authority, store records, token contents and test fault controls stay private. Shared replay identities and descriptive event/sink contracts come from dione-provider-delivery. Root composition supplies its existing sink and uses the public bridge and reply-authority operations.

The private fixture service is compiled only in the unit-test executable. Application tests receive its exact executable path from verification orchestration. It is not a shipped binary or public fixture API.
