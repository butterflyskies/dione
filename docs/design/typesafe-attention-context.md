# Dione attention

Language for selecting which incoming Discord material reaches a recipient's working context. Delivery, attention, and participation are separate.

## People and source material

**Recipient**: The construct whose working context may receive an item.

**Wanted item**: Material the recipient wants to receive, whether or not they respond.
_Avoid_: Engaging message, reply-worthy message.

**Direct lane**: Source traffic designated for delivery without semantic filtering, still subject to ordinary access rules.

**Ambient traffic**: Otherwise eligible incoming material outside the direct lane.

**Source segment**: A bounded group of original messages needed to interpret a trigger, with their identities and versions.
_Avoid_: Generated summary, whole-channel context.

**Attention brief**: The recipient-authored description of interests, curiosity, open exchanges, and work that informs relevance.
_Avoid_: Personality profile, inferred identity.

**Provider eligibility**: Explicit permission for material to be sent to the classification provider, independent of the recipient's read access.

## Judgment and delivery

**Judgment**: An attributed semantic assessment of source material, distinct from a delivery decision or verified fact.

**Admission policy**: The rules that combine judgments and current circumstances into delivery decisions.

**Admitted item**: Material selected for delivery, not necessarily delivered yet.

**Deferred item**: Ambient material not currently admitted but available for authorized retrieval while its source remains available. Deferral does not promise eventual automatic delivery.

**Prompt attention**: Priority at the next supported safe delivery opportunity, not an instruction to abort current work.

**Next natural turn**: Delivery without requesting an interruption of the recipient's active work.

**Retrieval-only**: Availability through authorized retrieval without automatic injection into working context.

## Modes and failures

**Off (`off`)**: Operation without new TypeSafe judgments or classifier-derived changes to delivery.

**Shadow (`log`)**: Classification and hypothetical admission while actual delivery remains unchanged.
_Avoid_: Off, logging-only storage, no provider traffic.

**On (`on`)**: Operation in which a valid admission policy affects eligible ambient delivery.

**Degraded operation**: Ordinary delivery substituting for unavailable or invalid classification, with visible failure state.

**Notice policy**: The independently configurable choice of recipient-facing failure/recovery notifications, not the classifier's operating mode.

## Learning

**Recipient feedback**: An attributed assessment of whether and when the recipient wanted an item. Silence is not feedback.

**Candidate policy**: A versioned, unelected proposal derived from local judgments and feedback.

**Promotion**: The recipient's explicit selection of an evaluated, compatible policy artifact for enforcement.

**Compatibility signature**: The identities and meanings of the model, rubric, and features for which a learned policy was evaluated.

**Training membership**: The source-version and feedback records that contributed to a learned artifact, needed to evaluate invalidation.
