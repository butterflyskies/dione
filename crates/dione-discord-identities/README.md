# Dione Discord identities

Snowflake validates one unclassified external numeric input with a private
NonZeroU64 representation. new rejects exactly zero. The four consuming channel,
message,user and guild methods preserve the input in the corresponding existing
Serenity SDK type. Nonzero syntax proves neither existence nor permission.

The unit-test numeric getter remains cfg(test),outside the production API.
Consumers inspect returned SDK values. No serde,generic parser,semantic ID family
or alternate authorization mechanism is added. Actual tool/config/search/codec
adapters retain their distinct string,numeric,null,empty and error policies.
config_store::DiscordId remains with its existing editor-input owner.

Serenity retains the workspace feature declaration. The explicit workspace
native-tls dependency selects its existing vendored feature for standalone native
build closure,following the current attention/evidence/coordination pattern.
No model-only minimum or TLS-free build is claimed. Verify this selected feature configuration with member and archive execution;
resolved versions are unchanged.

Consumers import dione_discord_identities directly. The former application
Discord ids module is removed,with no forwarding alias or extra getter API.
