# Dione Discord events

Owns descriptive Discord observations and the canonical conversion to dione-messages deliveries. Event fields and MentionKind report observations; they do not grant native authorization. The gateway handler, MentionDetector and private verified-action pipeline remain in the native runtime.

The delivery module exposes message, single and batch. Conversion retains exact identities, text, timestamps, attachments, targeting and control categories. DeliveryConversionError retains typed ControlError causes or the distinct MixedControl category.

The lifecycle_storage module owns serialized bounded lifecycle metadata, retention, content ordering, admission counters and tombstones. Callers hold opaque lineage through borrowed transactions and retain all lineage and policy decisions under the same storage lock. No lineage equality or cloning bound is required. The controller exposes no raw mutex, mutable record or application proof constructor. StorageError retains private typed failure state through its kind accessor.
