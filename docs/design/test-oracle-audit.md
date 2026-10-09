# Test oracle audit

This pass asks whether a test fails when the behavior it claims to protect is
broken. It reviewed the integration tests and Rust test functions in `src/` on
the local `feat/pr-quality-gates` worktree. An AST inventory found 1,526 explicit
test functions after the edits below. Twelve lack a direct assertion macro;
inspection found assertions in called helpers, an expected panic, a compile-fail
test, and a WebSocket frame acceptance test. The count excludes tests generated
by macros and does not measure assertion quality.

| Test area | False pass before this pass | Change |
| --- | --- | --- |
| LaTeX rendering | Two tests accepted either result; a third only wrote image files for later viewing. | Removed the three tests. Existing PNG and error tests remain. |
| Public package privacy | The CRLF regression only checked rejection, so rejecting every package would pass. | Runs an allowed and a forbidden archive through the same CRLF rules and checks the specific private-use rule. |
| Permission request relay | Missing-ID tests only checked that notifications returned no response, which is true even when a request is sent. | Checks zero outbound Discord requests for missing and empty IDs, then one request for a valid ID. |
| Access queue persistence | An atomic-write test only checked absence of a temporary file; removal tests only checked absence after removal. | Reloads and checks exact saved data before checking cleanup or removal. |

The privacy scanner was exercised with CRLF rules in a temporary shell fixture:
the current script allowed public source and rejected private source. A copy with
private-pattern substitution broken allowed the private source. The four revised
Rust test binaries then passed all 106 tests. In a disposable Linux checkout, a
scanner mutation that substituted a harmless dependency made the CRLF privacy
test fail. Removing the permission request ID guard made the MCP relay test fail
because it observed an unwanted Discord request. Both tests passed again after
the source files were restored. These checks demonstrate sensitivity to those
two specific faults; they do not establish mutation coverage for every test.

After those checks, the full `oneshot-test-seam` Nextest run passed 1,569 tests
with one skipped. Clippy passed with warnings denied for all targets in both
the default and `oneshot-test-seam` configurations. These runs used a native
Linux verification checkout containing the edited tests and the local quality
gate files.

This is a focused pass, not certification of every test. Future changes should
state the failure each test is expected to catch, pair rejection with an allowed
case, and deliberately break high-risk behavior once to check that the targeted
test fails. The renderer's dollar-output guard still needs a deterministic
trigger or a separately testable sanitizer boundary before it can be counted as
covered.

The next pass added `tests/codex_inbox_upgrade.rs` with an inbox file written by
the tagged `v0.48.0` code. The test opens it in a child process under a ten
second deadline, checks both old messages, acknowledges them, and reopens the
upgraded file. Emptying the fixture made the test fail on missing entries.
Delaying the child before the load made the parent kill it at the deadline and
fail. Both mutations were confined to a disposable Linux checkout. This test
reaches the production Codex inbox loader used during startup; it does not
exercise Discord gateway readiness or the later delivery worker.

The required-check script passed with eight successful job results and rejected
failed, skipped, cancelled, empty, and missing results. Replacing it temporarily
with an unconditional success script made its targeted test fail. After restoring
the script and inbox fixture, both targeted tests passed. The full
`oneshot-test-seam` run passed 1,571 tests with one skipped; the production
`oneshot_send` run passed four tests; both warning-free Clippy runs passed.

The historical-failure review added a live-worker test with old inbox data.
Independent reviewers noticed that checking the old queue length after delivery
would miss overwritten payloads. The test now checks both old message IDs and
contents in the reopened persisted file. A disposable fixture mutation kept
the queue length at two but changed the first payload; the targeted test failed
on the literal old content. The permission-request test now returns a valid
DM channel from its fake Discord server and asserts the subsequent message POST
and both button IDs. Suppressing that send in a disposable source copy made the
test fail because only the channel request was observed. Both tests passed
again with the original source and fixture.
