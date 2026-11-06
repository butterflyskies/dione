# Dione Discord Gate

Owns inbound DM, guild, reaction, reply-parent, mention, principal translation,
and outbound channel/file policy. The guild check reads a supplied borrowed live
MuteStore; root composition retains the single existing global store. Callers
still own verified-action and transport authority. Reactions intentionally gate
only ignored identities here, with guild mute checked by the caller.

Reply-parent resolution defaults to fail-open preview redaction; ignored authors
drop, ignored parents redact. File checks retain canonicalization and the inbox
exception, including inherited behavior when the state directory cannot resolve.
This extraction preserves existing policy; it does not redesign authorization.
