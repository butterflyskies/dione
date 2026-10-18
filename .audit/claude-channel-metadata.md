# Claude channel metadata correction

Claude channel notifications require string-valued metadata. Shared notifications contain booleans and structured evidence because other consumers need their original types. The Claude adapter now keeps string values and encodes other JSON values as compact JSON text. The original notification, content, permission relay, and other adapter output stay unchanged.

## Evidence

Baseline a6c50ab356564de8220624f0b593c991d0073b56 with the new writer-level regression produced one metadata assertion failure and one passing permission test. The failure compared actual serialized booleans and objects against required strings. After the correction, native Linux Clippy passed and 39 harness/notification tests passed. Logs are /home/syn/code/dione-claude-metadata-baseline.log, /home/syn/code/dione-claude-metadata-clippy.log, and /home/syn/code/dione-claude-metadata-focused.log. These checks preceded the version-only change to 0.63.1. The final committed-head gate is recorded in the PR.

The test covers strings, booleans, numbers, objects, arrays, null, original input preservation, optional metadata, allow/deny permission notifications, and a different method sharing the channel prefix. Output is read through the actual protocol writer. Existing Codex and Oh My Pi tests run in the focused set.

The official contract was inspected at https://code.claude.com/docs/en/channels-reference on 2026-10-12. Source verification does not establish a deployed Claude version or live channel acceptance. No deployment or construct test was performed.

## Review scope

The comment pass reported zero findings. Independent Design, Idiomacy, and Tests passes found no actionable P1-P3. Independent Safety, Security, and Privacy passes also found no actionable P1-P3. Review artifacts are retained in the local modular-core handoff directory. Local agent review is not official Forgejo approval.

Typed live-delivery contracts and removal of the shared Claude-shaped representation remain separate refactor units. This fix intentionally changes only Claude wire projection.
