# dione-pre-send

Synchronous outbound hooks, assessment routing, and the production Observe pipeline for Dione.

`PreSendPipeline` runs ordered `PreSendHook` implementations against private `HookContext` state. `HookName` and `ConstructId` retain validated private representations. `NoRly` contains validated hook bypasses and has no public constructor. Context, hook output, assessment streams, and pipeline outcomes retain their private fields.

`production_pipeline()` builds one private production roster containing the private status-packet lint. The lint reuses `dione-evidence` Markdown scanning. The crate does not expose a lint registry or the lint implementation. `observe_pipeline()` accepts explicit hooks, including the empty roster used by one-shot sending. `configured_pipeline()` preserves explicit disablement.

Observe preserves the original message bytes and returns Continue. Findings flow to the audit sink, while construct feedback stays empty. Enforce remains an explicit construction choice. It applies ordered rewrites and redirects, stops on Halt, and rejects hook panics. Sink routing preserves both streams under the existing Degrade and FailClosed policies. Named hook bypasses remain subject to validation against the receiving pipeline.

Application composition owns process installation and handle replacement. Application configuration, Discord permissions, redirect authorization, native delivery, and contradictionary consent remain outside this crate. A hook bypass does not grant consent or outbound authority.

## Compatibility and limits

The extraction removes seven existing `non_exhaustive` attributes from PipelineMode, ChannelType, OutboundDestination, HookDecision, SinkFailurePolicy, PipelineError, and PipelineOutcome. This follows the repository's type policy and permits existing exhaustive consumers across the crate boundary. It does not expose private outcome fields or alter runtime decisions.

Inherited type and error debt remains bounded to the moved contracts. `ConstructId::parse` returns a string error. `SinkError` wraps a diagnostic string. `PipelineError` keeps its existing manual Display and Error implementations and error text. `HookContext::new` still accepts channel classification and destination independently. Its optional attribution and metadata setters do not establish that every combination occurs in application assembly. The extraction adds no validation claim for those combinations.

The private lint also retains preexisting production expect calls for constant regex construction, UTF-8-preserving masking and its fixed hook name. These remain inherited panic-policy debt rather than new validated-construction guarantees.

The private lint preserves its existing categories, literal audit details, URL policy, scan ceilings, finding budget, and documented false positives. It always returns Continue. Audit events omit message content. Existing emitted-event tests observe real tracing sinks. Production-roster tests prove that the builder runs the lint. They do not observe whether application startup calls the builder. Consent, authorization, and installed-handle coverage belong to application consumers.

Serenity uses the existing workspace features. The mandatory dione-evidence dependency already enables the existing workspace native-tls vendored feature, so standalone TLS closure does not rely on application feature unification or a duplicate direct dependency. Regex, reqwest URL parsing, tracing, and dione-evidence use existing workspace versions. Tests use tracing-subscriber.
