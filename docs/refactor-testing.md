# Test the combined refactor

`refactor/vertical-slices` contains the integrated refactor. Forgejo Linux CI
runs on pushes to `main` and on PRs targeting supported branches, including this
branch. Open the specifically authorized PR before merge-bound CI/review work;
refactor branch pushes alone do not request CI. Release tagging remains restricted
to pushes to `main`. The integration workflow builds and checks
binaries; it does not upload artifacts or deploy a construct.

## Build a specific commit

Use a clean native Linux checkout, including under WSL. Record the full commit
SHA and run the local gate against the refactor's starting point:

```sh
git rev-parse HEAD
sh scripts/pr-ready.sh 30b58672b0d791d2aa7a3c2e63b9b67ba014262e
```

The gate builds `target/release/dione`, or
`$CARGO_TARGET_DIR/release/dione` when that variable is set. Keep the binary,
its SHA-256 checksum, the source commit SHA, and the gate log together. Use the
extension from the same commit for Oh My Pi. Do not identify a test build by
the version string alone; several integration commits can share a version.

## Construct validation

Use a dedicated test bot and isolated state. Follow the
[Oh My Pi setup and validation instructions](../extensions/oh-my-pi/README.md).

- [ ] Confirm the target construct can launch the exact recorded binary.
- [ ] Verify a real Discord event reaches the OMP main session once with its
  full attribution and metadata.
- [ ] Exercise an allowed reply and a denied action through Dione's tools.
- [ ] Verify child-agent isolation and the construct's OMP tool-approval policy.
- [ ] Check disconnect/reconnect behavior against the documented delivery limits.
- [ ] Record the build SHA, configuration, results, and remaining limitations.

Local checks and CI do not establish live construct compatibility. Completing
this checklist does not itself authorize merging the refactor into `main`.
