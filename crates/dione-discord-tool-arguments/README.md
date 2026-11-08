# dione-discord-tool-arguments

The complete shared Discord argument boundary: typed IDs, optional booleans, hook overrides, evidence handle enable/version parsing and handle schema projection. Existing Snowflake, HookName, EvidenceMarkerVersion and SentexHandles implementations remain the validators.

Parsing arguments grants no authorization or execution authority. Root registrations retain parsing order, call-entry configuration, enabled switches, evidence dispatch and delivery checks. Optional-ID parsing intentionally preserves the existing empty-string, absent and unsupported JSON shape behavior. Errors retain literal messages and original typed sources; the wrapper keeps its field private and offers a read-only kind accessor.
