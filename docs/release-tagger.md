# Forgejo release tag writer

The Forgejo `linux.yml` workflow runs six checks on pull requests and main
pushes. Only its `release-tag` job can write a tag, and only after all six checks
pass on a push to `refs/heads/main`. The tag script verifies the exact checked
out commit, version increase, changelog heading, and existing tags before it
makes a non-force annotated tag push. Dione 0.42.0 remains intentionally
untagged; the script does not backfill it.

## Operator setup for a separate release account

The account and permission changes below are human-owned Forgejo setup. Do not
merge this workflow until they are configured. The account's exact login and
the generated audience are deployment values, not values to guess in code.

1. Create a dedicated Dione release account. Give it write access to
   `lacuna/dione`, without organization Owner access or unrelated repository
   permissions. Record the exact login in the release operations record.
2. In `lacuna/dione` Settings → Tags, edit the existing protected `v*` rule.
   Add that exact account to allowed users while preserving the existing rule,
   pattern, and other grants. Do not enable force pushes or delete tags.
3. While signed in as the release account, create a **Forgejo Actions (Local)**
   Authorized Integration under Settings → Authorized Integrations. Restrict it
   to source repository `lacuna/dione`, workflow file `linux.yml` (basename,
   without `.forgejo/workflows/`), Git reference `refs/heads/main`, and event
   `push`. Grant only repository write capability for `lacuna/dione`; do not
   grant other resources or permissions. The resulting requests authenticate
   as the account that owns this integration.
4. Save the integration and copy its generated Audience. Set the repository
   Actions variable `DIONE_RELEASE_AUDIENCE` to that value in
   `lacuna/dione` Settings → Actions → Variables. Audience is a public
   identifier, not a secret. Use the repository variable; an organization
   variable with the same name takes precedence in Forgejo.
5. Verify the account can read the repository and is shown in the existing
   `v*` allowed-users list. Review the integration's four restrictions and
   repository-only write capability before merging the workflow change.

The job discards the automatic checkout credential. When a tag operation is
needed, it requests a short-lived JWT for the configured audience and uses a
Bearer header for Git fetch and push over the workflow's Forgejo HTTPS URL.
The JWT is masked in runner logs and is not stored in a remote URL or Git
config file. If the audience, OIDC endpoint, or HTTPS remote is missing, the
tag job fails before a tag write. Pull-request jobs cannot use the integration
because its source event and ref are restricted to main pushes.

## Activation and recovery

After setup, merge the workflow through the normal reviewed path. A main push
at the unchanged bootstrap version should pass with no new tag. On the next
qualified version bump, verify all six gates pass and `release-tag` succeeds;
inspect the resulting `v<version>` ref and confirm it is annotated and points
to the exact main commit. A failed push may leave a local runner tag, but no
remote release: rerun on a later main push with the same version to reconcile
the missing tag. Never manually retarget an existing release tag.

To stop automated tag writes, disable the release account's Authorized
Integration or remove its allowed-user entry from the existing `v*` rule.
Keep the protected rule and existing release tags in place. Recheck the
account, integration, variable, and rule before re-enabling.

Forgejo v16 references: [Authorized Integrations](https://forgejo.org/docs/v16.0/user/api/authorized-integrations/),
[Actions variables and automatic token](https://forgejo.org/docs/v16.0/user/actions/basic-concepts/),
and [protected tags](https://forgejo.org/docs/v16.0/user/repository/protection/).
