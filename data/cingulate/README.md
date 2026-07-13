# Cingulate tier-1 snapshot

This directory vendors Vesper's immutable tier-1 handoff for Dione issue
[#168](https://github.com/butterflyskies/dione/issues/168).

Receipt:

- corrected snapshot tar SHA-256:
  `1750bc4b15712e7314b9418a0fd5746c14e862c0654213c6783e2efc5da21f1f`
- `tier1-patterns.toml`: 15,545 bytes, SHA-256
  `0f4a9c1161204558b8e894276a9099f247000560bf1a30dcbcfca5c730d3987b`
- `tier1-testcases.md`: 11,278 bytes, SHA-256
  `bd9b354665a5796e0784ff5b3b9a4838321aa57f17df125d3b36e3cb172fe162`

The source is `tier1-v3.1-draft1`, with Vesper's authoritative r2 testcase
erratum: 32 patterns and 129 test strings. The adapter preserves its `flag` and
`block` actions, but production registers the hook in **Observe mode only**.
The draft block subset must not be enabled for live enforcement until Miranda
and Lain review and approve it.

The snapshot defines no per-construct thresholds and no contextual heuristic
layer. Those remain explicitly deferred; the adapter does not invent them.
