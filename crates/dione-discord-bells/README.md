# Dione Discord Bells

Owns native message eligibility and shadow/live Bell enrichment. Generic evaluator,
sessions, ranking, and rendering remain in dione-bell-rings. Callers still own
admission, revalidation, cancellation, and delivery policy.

Shadow detaches retrieval and returns unchanged metadata; it supplies no join or
drain guarantee. Live awaits the evaluator, replaces only supplied status, and
injects only nonempty rendered bells. Skip and empty-result paths retain metadata.
Configured channel triggers and directed targeting select native eligibility.

Application tests retain the real HTTP evaluator and typed Codex queue projection.
Their stateless HTTP fixture does not prove SSE closure or cancellation drain.
