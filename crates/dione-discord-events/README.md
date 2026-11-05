# Dione Discord events

Owns descriptive Discord observations and the canonical conversion to dione-messages deliveries. Event fields and MentionKind report observations; they do not grant native authorization. The gateway handler, MentionDetector and private verified-action pipeline remain in the native runtime.

The delivery module exposes message, single and batch. Conversion retains exact identities, text, timestamps, attachments, targeting and control categories. DeliveryConversionError retains typed ControlError causes or the distinct MixedControl category.
