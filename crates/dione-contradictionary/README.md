# Dione Contradictionary

Match configured words and substrings, inspect descriptive hits, and propose
conservative automatic rewrites. The crate also owns sidecar entry parsing and
compatibility rules. It reuses the shared Markdown scanner from `dione-evidence`.

Construct `Contradictionary` from `Entry` values, then call `check` or `apply_auto`.
An absent rewrite means the original text still needs judgment. Malformed Auto
rules become Block rules; a rewrite that creates another gating hit is rejected.
Existing matching, offset, case-preservation, and constructor behavior is retained.

`load_sidecar_entries` returns no entries for a missing file and rejects an invalid
file as a whole. Dione resolves paths, combines inline and sidecar rules, and keeps
the last valid configuration when a replacement fails.

Application settings, hold consent, current send authorization, diary decisions,
timestamps, truncation, and JSONL persistence remain in Dione. A hit or rewrite
does not establish permission or successful delivery. The existing descriptive
fields retain their public representation.
