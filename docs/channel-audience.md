# Read a channel's effective audience

`channel_effective_audience(channel_id)` lists observed guild members who can
view a channel without requiring a candidate user list. It uses the same
effective permission evaluator as `can_view_channel`: owner and administrator
bypass, role and member overwrites, and parent permissions for threads.
These introspection tools use the bot's Discord read authority, not a separate
local channel allowlist.

The result separates:

- `members`: definite viewers, with user IDs, usernames, nicknames, and deciding
  permission reasons.
- `unknown_members`: observed members whose visibility could not be determined.
- `member_census_complete`: the REST member scan reached its end and included
  the observed guild owner.
- `complete`: the member census completed and no observed member has an
  unknown permission result.
- `census_reason`: why the scan stopped. API errors also retain an `error`.

The call examines at most 1,000 members over at most 11 REST pages, within a
30-second deadline. A final empty page proves that the observed member scan
ended; hitting a cap, a failed page, or a non-advancing/duplicate page instead
returns partial results. Omitted members are not denials. A prerequisite
failure returns an error with `complete: false`, not a complete empty roster.

Private-thread membership is not enumerated. The guild owner, administrators,
and members with effective `MANAGE_THREADS` can be definite viewers; other
members who can view the parent remain unknown. Use `can_view_channel` for a
bounded candidate membership check. Missing role permission data also remains
unknown rather than becoming a denial.

Guild member listing requires the application's Discord `GUILD_MEMBERS`
privileged capability. The REST restriction is independent of the Gateway
Identify intent bitmask. A failed REST read is not proof that changing a
Gateway flag will fix it; an operator must verify the application's actual
capability. This tool does not change application intents, permissions, or
channel configuration.

`observed_at` and `completed_at` bound the read. Discord permissions and guild
membership may change between requests: `snapshot_consistent` is always false.
`complete` means an exhausted observed scan, not an atomic or perpetually valid
audience snapshot. Recheck changeable evidence as needed, and do not treat this
observation alone as authorization to relay content. Discord visibility does
not establish another connector's configuration, delivery/mute state, or the
capabilities mounted in a construct's current seat.
