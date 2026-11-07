# dione-discord-state

Shared Discord runtime caches and pending-permission bookkeeping. State remains an Arc<RwLock<SharedState>> shared by native operations, attention and MCP. Existing ordinary public fields and methods retain their contract.

observe_webhook_creator performs the real HTTP observation on a cache miss. Only a successful response with a creator stores a positive fact. Creator insertion, timestamps, FIFO cache and TTL lookup remain private. The returned ID is an ordinary fact; native verification must still bind event coordinates and evaluate current policy. This member does not expose a sealed authorization token.

Authenticated-send producers retain responsibility for note_sent inputs. Existing public fields are not a newly sealed authority boundary. Root owns native policy, Serenity event translation and protocol projection. No persistence or new cache policy is introduced.
