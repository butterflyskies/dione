# dione-bell-rings

Scoped memory retrieval over Streamable HTTP MCP. A reusable evaluator owns
reconnectable clients, provider fan-out, response admission, deterministic ranking,
retrieval status and compact rendering. It accepts query text, validated provider
values and validated recall options. No Discord events or application configuration
are required.

Connections run concurrently with a five-second bound per provider. After they
settle, connected providers recall concurrently within the configured recall
timeout. Parsing follows those calls. The recall timeout is not an end-to-end
deadline. A successful empty response still counts as successful retrieval.

Consumers own eligibility, credentials, alias policy and publication. URLs reject
embedded user/password credentials but may contain query parameters. Bell fields
describe retrieved observations and do not confer authorization. Client reuse and
scope-keyed caching do not promise generation fencing or an explicit shutdown/drain
protocol.
