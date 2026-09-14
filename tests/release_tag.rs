#[cfg(unix)]
mod unix {
    use std::{
        fs,
        os::unix::fs::PermissionsExt,
        path::{Path, PathBuf},
        process::{Command, Output},
    };

    struct Fixture {
        _temp: tempfile::TempDir,
        repo: PathBuf,
        origin: PathBuf,
        initial: String,
    }

    fn run(root: &Path, program: &str, args: &[&str]) -> Output {
        Command::new(program)
            .args(args)
            .current_dir(root)
            .output()
            .unwrap_or_else(|error| panic!("{program} must execute: {error}"))
    }

    fn git(root: &Path, args: &[&str]) -> Output {
        run(root, "git", args)
    }

    fn assert_success(output: Output, operation: &str) {
        assert!(
            output.status.success(),
            "{operation} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn write_release(repo: &Path, version: &str, changelog_heading: &str) {
        fs::write(
            repo.join("Cargo.toml"),
            format!("[workspace]\nmembers = [\".\"]\n\n[package]\nname = \"dione\"\nversion = \"{version}\"\nedition = \"2024\"\n"),
        )
        .expect("fixture manifest must be written");
        fs::write(
            repo.join("CHANGELOG.md"),
            format!("# Changelog\n\n## [Unreleased]\n\n{changelog_heading}\n"),
        )
        .expect("fixture changelog must be written");
        assert_success(
            run(repo, env!("CARGO"), &["generate-lockfile", "--quiet"]),
            "fixture lockfile generation",
        );
    }

    fn commit(repo: &Path, message: &str) -> String {
        assert_success(git(repo, &["add", "."]), "git add");
        assert_success(
            git(
                repo,
                &[
                    "-c",
                    "user.name=Dione Tests",
                    "-c",
                    "user.email=dione-tests@example.invalid",
                    "-c",
                    "commit.gpgsign=false",
                    "commit",
                    "--quiet",
                    "-m",
                    message,
                ],
            ),
            "git commit",
        );
        String::from_utf8(git(repo, &["rev-parse", "HEAD"]).stdout)
            .expect("commit ID must be UTF-8")
            .trim()
            .to_owned()
    }

    fn fixture_at(version: &str) -> Fixture {
        let temp = tempfile::tempdir().expect("temporary fixture root must be created");
        let repo = temp.path().join("repo");
        let origin = temp.path().join("origin.git");
        fs::create_dir(&repo).expect("fixture repository must be created");
        assert_success(
            git(&repo, &["init", "--quiet", "--initial-branch=main"]),
            "git init",
        );
        assert_success(
            git(temp.path(), &["init", "--quiet", "--bare", "origin.git"]),
            "bare init",
        );
        assert_success(
            git(
                &repo,
                &["remote", "add", "origin", origin.to_str().unwrap()],
            ),
            "remote add",
        );

        fs::create_dir(repo.join("src")).expect("fixture source directory must be created");
        fs::write(repo.join("src/lib.rs"), "pub fn fixture() {}\n")
            .expect("fixture source must be written");
        write_release(&repo, version, &format!("## [{version}]"));
        let initial = commit(&repo, "initial release");
        assert_success(
            git(&repo, &["push", "-u", "origin", "main"]),
            "initial push",
        );

        fs::create_dir(repo.join("scripts")).expect("fixture scripts directory must be created");
        for (name, contents) in [
            (
                "tag-qualified-release.sh",
                include_str!("../scripts/tag-qualified-release.sh"),
            ),
            (
                "workspace-package-version.sh",
                include_str!("../scripts/workspace-package-version.sh"),
            ),
            (
                "semver-is-greater.sh",
                include_str!("../scripts/semver-is-greater.sh"),
            ),
        ] {
            let path = repo.join("scripts").join(name);
            fs::write(&path, contents).expect("fixture script must be written");
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755))
                .expect("fixture script must be executable");
        }

        Fixture {
            _temp: temp,
            repo,
            origin,
            initial,
        }
    }

    fn fixture() -> Fixture {
        fixture_at("0.1.0")
    }

    fn run_tagger(repo: &Path, expected_commit: &str, push_before: &str) -> Output {
        Command::new("sh")
            .arg("scripts/tag-qualified-release.sh")
            .current_dir(repo)
            .env("EXPECTED_COMMIT", expected_commit)
            .env("PUSH_BEFORE", push_before)
            .output()
            .expect("release tagger must execute")
    }

    fn remote_ref(fixture: &Fixture, reference: &str) -> Output {
        git(
            &fixture.repo,
            &[
                "--git-dir",
                fixture.origin.to_str().unwrap(),
                "rev-parse",
                reference,
            ],
        )
    }

    #[test]
    fn workflow_exposes_tag_writes_only_after_six_trusted_main_gates() {
        let workflow = include_str!("../.forgejo/workflows/linux.yml");
        let release_tag = workflow
            .split_once("\n  release-tag:")
            .expect("Forgejo CI must contain the release tag job")
            .1;

        assert!(release_tag.starts_with(
            "\n    name: Annotated release tag\n    if: ${{ forgejo.event_name == 'push' && forgejo.ref == 'refs/heads/main' }}"
        ));
        assert!(release_tag.contains("needs: [format, lint, test, package, msrv, audit]"));
        assert!(release_tag.contains("ref: ${{ forgejo.sha }}"));
        assert!(release_tag.contains("persist-credentials: true"));
        assert!(release_tag.contains("EXPECTED_COMMIT: ${{ forgejo.sha }}"));
        assert!(release_tag.contains("PUSH_BEFORE: ${{ forgejo.event.before }}"));
        assert!(release_tag.contains("run: sh scripts/tag-qualified-release.sh"));
        assert!(release_tag.contains(
            "uses: https://github.com/dtolnay/rust-toolchain@3c5f7ea28cd621ae0bf5283f0e981fb97b8a7af9"
        ));
        assert!(release_tag.contains("toolchain: \"1.98.0\""));
        let toolchain_step = release_tag
            .find("dtolnay/rust-toolchain")
            .expect("release tag job must install Rust");
        let checkout_step = release_tag
            .find("data.forgejo.org/actions/checkout")
            .expect("release tag job must check out its exact event commit");
        let tagger_step = release_tag
            .find("run: sh scripts/tag-qualified-release.sh")
            .expect("release tag job must invoke the tagger");
        assert!(toolchain_step < checkout_step);
        assert!(checkout_step < tagger_step);
        assert_eq!(workflow.matches("persist-credentials: true").count(), 1);
        assert_eq!(workflow.matches("persist-credentials: false").count(), 6);
        assert!(!workflow.contains("\n  release-artifact:"));
        assert!(!workflow.contains("upload-artifact"));
        assert!(!Path::new(".github/workflows/tag-release.yml").exists());
        assert!(
            include_str!("../.github/workflows/release.yml")
                .contains("gh release create \"$TAG_NAME\" --verify-tag")
        );
    }

    #[test]
    fn version_bump_pushes_one_idempotent_annotated_exact_head_tag() {
        let fixture = fixture();
        write_release(&fixture.repo, "0.2.0", "## [0.2.0]");
        let head = commit(&fixture.repo, "release 0.2.0");
        assert_success(
            git(&fixture.repo, &["push", "origin", "main"]),
            "release push",
        );

        assert_success(
            run_tagger(&fixture.repo, &head, &fixture.initial),
            "release tagger",
        );
        assert_eq!(
            String::from_utf8(git(&fixture.repo, &["cat-file", "-t", "v0.2.0"]).stdout)
                .unwrap()
                .trim(),
            "tag"
        );
        assert_eq!(
            String::from_utf8(remote_ref(&fixture, "refs/tags/v0.2.0^{commit}").stdout)
                .unwrap()
                .trim(),
            head
        );
        let tag_object =
            String::from_utf8(remote_ref(&fixture, "refs/tags/v0.2.0").stdout).unwrap();

        let retry = run_tagger(&fixture.repo, &head, &fixture.initial);
        assert_success(retry, "release tagger retry");
        assert_eq!(
            String::from_utf8(remote_ref(&fixture, "refs/tags/v0.2.0").stdout).unwrap(),
            tag_object
        );
    }

    #[test]
    fn valid_dated_changelog_heading_is_accepted() {
        let fixture = fixture();
        write_release(&fixture.repo, "0.2.0", "## [0.2.0] - 2026-09-14");
        let head = commit(&fixture.repo, "dated release");
        assert_success(
            run_tagger(&fixture.repo, &head, &fixture.initial),
            "dated release tagger",
        );
    }

    #[test]
    fn bootstrap_version_is_never_backfilled_on_landing_or_later_pushes() {
        let fixture = fixture_at("0.42.0");
        fs::write(fixture.repo.join("README.md"), "install tagger\n")
            .expect("fixture README must be written");
        let landing = commit(&fixture.repo, "install release tagger");
        assert_success(
            git(&fixture.repo, &["push", "origin", "main"]),
            "tagger landing push",
        );

        let landing_output = run_tagger(&fixture.repo, &landing, &fixture.initial);
        assert_success(landing_output.clone(), "tagger landing");
        assert!(
            String::from_utf8_lossy(&landing_output.stdout)
                .contains("pre-automation untagged bootstrap version; not backfilling")
        );
        assert!(
            git(&fixture.repo, &["tag", "--list", "v0.42.0"])
                .stdout
                .is_empty()
        );

        fs::write(fixture.repo.join("README.md"), "later documentation\n")
            .expect("fixture README must be updated");
        let later = commit(&fixture.repo, "later same-version push");
        assert_success(
            git(&fixture.repo, &["push", "origin", "main"]),
            "later bootstrap-version push",
        );
        assert_success(
            run_tagger(&fixture.repo, &later, &landing),
            "later bootstrap-version tagger",
        );
        assert!(
            git(&fixture.repo, &["tag", "--list", "v0.42.0"])
                .stdout
                .is_empty()
        );
    }

    #[test]
    fn later_same_version_push_recovers_a_missing_release_tag() {
        let fixture = fixture_at("0.42.0");
        fs::write(fixture.repo.join("README.md"), "install tagger\n")
            .expect("fixture README must be written");
        let tagger_landing = commit(&fixture.repo, "install release tagger");
        assert_success(
            git(&fixture.repo, &["push", "origin", "main"]),
            "tagger landing push",
        );
        assert_success(
            run_tagger(&fixture.repo, &tagger_landing, &fixture.initial),
            "initial tagger landing",
        );
        assert!(
            git(&fixture.repo, &["tag", "--list", "v0.42.0"])
                .stdout
                .is_empty()
        );

        write_release(&fixture.repo, "0.43.0", "## [0.43.0]");
        let failed_attempt = commit(&fixture.repo, "qualified release whose tag push failed");
        assert_success(
            git(&fixture.repo, &["push", "origin", "main"]),
            "untagged release push",
        );
        fs::write(
            fixture.repo.join("README.md"),
            "repair release automation\n",
        )
        .expect("repair fixture must be written");
        let repair = commit(&fixture.repo, "repair release automation");
        assert_success(
            git(&fixture.repo, &["push", "origin", "main"]),
            "release repair push",
        );

        let output = run_tagger(&fixture.repo, &repair, &failed_attempt);
        assert_success(output.clone(), "release tag recovery");
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("reconciling its missing release tag")
        );
        assert_eq!(
            String::from_utf8(remote_ref(&fixture, "refs/tags/v0.43.0^{commit}").stdout)
                .unwrap()
                .trim(),
            repair
        );
    }

    #[test]
    fn released_unchanged_version_keeps_its_annotated_ancestor_tag() {
        let fixture = fixture_at("0.42.0");
        fs::write(fixture.repo.join("README.md"), "install tagger\n")
            .expect("fixture README must be written");
        let landing = commit(&fixture.repo, "install release tagger");

        write_release(&fixture.repo, "0.43.0", "## [0.43.0]");
        let release = commit(&fixture.repo, "release 0.43.0");
        assert_success(
            run_tagger(&fixture.repo, &release, &landing),
            "release tagger",
        );
        let original_tag =
            String::from_utf8(remote_ref(&fixture, "refs/tags/v0.43.0").stdout).unwrap();

        fs::write(fixture.repo.join("README.md"), "post-release repair\n")
            .expect("fixture README must be updated");
        let later = commit(&fixture.repo, "post-release same-version push");
        let output = run_tagger(&fixture.repo, &later, &release);
        assert_success(output.clone(), "post-release tagger");
        assert!(
            String::from_utf8_lossy(&output.stdout)
                .contains("already annotates a released ancestor; leaving it unchanged")
        );
        assert_eq!(
            String::from_utf8(remote_ref(&fixture, "refs/tags/v0.43.0").stdout).unwrap(),
            original_tag
        );
        assert_eq!(
            String::from_utf8(remote_ref(&fixture, "refs/tags/v0.43.0^{commit}").stdout)
                .unwrap()
                .trim(),
            release
        );
    }

    #[test]
    fn unchanged_version_rejects_lightweight_nonancestor_and_wrong_version_tags() {
        let lightweight = fixture_at("0.42.0");
        commit(&lightweight.repo, "install release tagger");
        write_release(&lightweight.repo, "0.43.0", "## [0.43.0]");
        let release = commit(&lightweight.repo, "release 0.43.0");
        assert_success(
            git(
                &lightweight.repo,
                &["tag", "--no-sign", "v0.43.0", &release],
            ),
            "lightweight collision",
        );
        assert_success(
            git(&lightweight.repo, &["push", "origin", "refs/tags/v0.43.0"]),
            "lightweight collision push",
        );
        fs::write(lightweight.repo.join("README.md"), "later push\n")
            .expect("fixture README must be written");
        let later = commit(&lightweight.repo, "later same-version push");
        let output = run_tagger(&lightweight.repo, &later, &release);
        assert!(
            !output.status.success(),
            "lightweight collision unexpectedly succeeded: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stderr).contains("is not annotated"));

        let wrong_version = fixture_at("0.42.0");
        commit(&wrong_version.repo, "install release tagger");
        assert_success(
            git(
                &wrong_version.repo,
                &[
                    "-c",
                    "user.name=Dione Tests",
                    "-c",
                    "user.email=dione-tests@example.invalid",
                    "tag",
                    "--annotate",
                    "v0.43.0",
                    "--message",
                    "wrong-version collision",
                    &wrong_version.initial,
                ],
            ),
            "wrong-version collision",
        );
        assert_success(
            git(
                &wrong_version.repo,
                &["push", "origin", "refs/tags/v0.43.0"],
            ),
            "wrong-version collision push",
        );
        write_release(&wrong_version.repo, "0.43.0", "## [0.43.0]");
        let release = commit(&wrong_version.repo, "release 0.43.0");
        fs::write(wrong_version.repo.join("README.md"), "later push\n")
            .expect("fixture README must be written");
        let later = commit(&wrong_version.repo, "later same-version push");
        let output = run_tagger(&wrong_version.repo, &later, &release);
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains("points to a commit with Dione version 0.42.0, not 0.43.0")
        );

        let nonancestor = fixture_at("0.42.0");
        commit(&nonancestor.repo, "install release tagger");
        assert_success(
            git(&nonancestor.repo, &["checkout", "-q", "-b", "collision"]),
            "collision branch",
        );
        write_release(&nonancestor.repo, "0.43.0", "## [0.43.0]");
        let collision = commit(&nonancestor.repo, "unmerged release collision");
        assert_success(
            git(
                &nonancestor.repo,
                &[
                    "-c",
                    "user.name=Dione Tests",
                    "-c",
                    "user.email=dione-tests@example.invalid",
                    "tag",
                    "--annotate",
                    "v0.43.0",
                    "--message",
                    "nonancestor collision",
                    &collision,
                ],
            ),
            "nonancestor collision",
        );
        assert_success(
            git(&nonancestor.repo, &["push", "origin", "refs/tags/v0.43.0"]),
            "nonancestor collision push",
        );
        assert_success(
            git(&nonancestor.repo, &["checkout", "-q", "main"]),
            "checkout main",
        );
        write_release(&nonancestor.repo, "0.43.0", "## [0.43.0]");
        let release = commit(&nonancestor.repo, "release 0.43.0");
        fs::write(nonancestor.repo.join("README.md"), "later push\n")
            .expect("fixture README must be written");
        let later = commit(&nonancestor.repo, "later same-version push");
        let output = run_tagger(&nonancestor.repo, &later, &release);
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("does not point to an ancestor"));
    }

    #[test]
    fn hostile_changelog_heading_near_matches_are_rejected() {
        for heading in [
            "## [0.2.0] malicious suffix",
            "prefix ## [0.2.0]",
            "## [0.2.0] - 2026-9-14",
            "## [0.2.0] - 2026-09-14 trailing",
            "## [0.2.0-rc.1]",
        ] {
            let fixture = fixture();
            write_release(&fixture.repo, "0.2.0", heading);
            let head = commit(&fixture.repo, "hostile heading");
            let output = run_tagger(&fixture.repo, &head, &fixture.initial);
            assert!(
                !output.status.success(),
                "near-match heading unexpectedly qualified: {heading}"
            );
            assert!(
                String::from_utf8_lossy(&output.stderr)
                    .contains("CHANGELOG.md has no exact heading"),
                "near-match heading did not reach the heading oracle: {heading}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(
                git(&fixture.repo, &["tag", "--list", "v0.2.0"])
                    .stdout
                    .is_empty()
            );
        }
    }

    #[test]
    fn multi_commit_push_qualifies_the_range_and_tags_its_exact_tip() {
        let fixture = fixture();
        write_release(&fixture.repo, "0.2.0", "## [0.2.0]");
        commit(&fixture.repo, "version bump");
        fs::write(fixture.repo.join("README.md"), "release notes\n")
            .expect("fixture README must be written");
        let tip = commit(&fixture.repo, "later commit in the same push");
        assert_success(
            git(&fixture.repo, &["push", "origin", "main"]),
            "range push",
        );

        assert_success(
            run_tagger(&fixture.repo, &tip, &fixture.initial),
            "multi-commit release tagger",
        );
        assert_eq!(
            String::from_utf8(remote_ref(&fixture, "refs/tags/v0.2.0^{commit}").stdout)
                .unwrap()
                .trim(),
            tip
        );
    }

    #[test]
    fn merge_push_qualifies_first_parent_range_and_tags_merge_tip() {
        let fixture = fixture();
        assert_success(
            git(&fixture.repo, &["checkout", "-q", "-b", "release"]),
            "branch",
        );
        write_release(&fixture.repo, "0.2.0", "## [0.2.0]");
        commit(&fixture.repo, "release change");

        assert_success(
            git(&fixture.repo, &["checkout", "-q", "main"]),
            "checkout main",
        );
        fs::write(fixture.repo.join("README.md"), "main advanced\n")
            .expect("fixture README must be written");
        let previous_main = commit(&fixture.repo, "advance main");
        assert_success(
            git(
                &fixture.repo,
                &[
                    "-c",
                    "user.name=Dione Tests",
                    "-c",
                    "user.email=dione-tests@example.invalid",
                    "-c",
                    "commit.gpgsign=false",
                    "merge",
                    "--quiet",
                    "--no-ff",
                    "release",
                    "-m",
                    "merge release",
                ],
            ),
            "merge release",
        );
        let merge_tip = String::from_utf8(git(&fixture.repo, &["rev-parse", "HEAD"]).stdout)
            .unwrap()
            .trim()
            .to_owned();
        assert_success(
            git(&fixture.repo, &["push", "origin", "main"]),
            "merge push",
        );

        assert_success(
            run_tagger(&fixture.repo, &merge_tip, &previous_main),
            "merge release tagger",
        );
        assert_eq!(
            String::from_utf8(remote_ref(&fixture, "refs/tags/v0.2.0^{commit}").stdout)
                .unwrap()
                .trim(),
            merge_tip
        );
    }

    #[test]
    fn existing_tag_is_never_created_moved_or_replaced() {
        for annotated in [false, true] {
            let fixture = fixture();
            let mut args = vec![
                "-c",
                "user.name=Dione Tests",
                "-c",
                "user.email=dione-tests@example.invalid",
                "tag",
            ];
            if annotated {
                args.extend(["--annotate", "--message", "existing release"]);
            } else {
                args.push("--no-sign");
            }
            args.extend(["v0.2.0", &fixture.initial]);
            assert_success(git(&fixture.repo, &args), "existing tag");
            assert_success(
                git(&fixture.repo, &["push", "origin", "refs/tags/v0.2.0"]),
                "existing tag push",
            );
            let original =
                String::from_utf8(remote_ref(&fixture, "refs/tags/v0.2.0").stdout).unwrap();

            write_release(&fixture.repo, "0.2.0", "## [0.2.0]");
            let head = commit(&fixture.repo, "release collision");
            let output = run_tagger(&fixture.repo, &head, &fixture.initial);
            assert!(!output.status.success());
            assert_eq!(
                String::from_utf8(remote_ref(&fixture, "refs/tags/v0.2.0").stdout).unwrap(),
                original
            );
        }
    }

    #[test]
    fn rejected_downgrade_stays_rejected_during_partial_recovery() {
        let fixture = fixture();
        write_release(&fixture.repo, "1.0.0", "## [1.0.0]");
        let released = commit(&fixture.repo, "release 1.0.0");
        assert_success(
            run_tagger(&fixture.repo, &released, &fixture.initial),
            "historical release",
        );

        write_release(&fixture.repo, "0.1.0", "## [0.1.0]");
        let downgrade = commit(&fixture.repo, "regress to 0.1.0");
        let rejected = run_tagger(&fixture.repo, &downgrade, &released);
        assert!(!rejected.status.success());
        assert!(
            String::from_utf8_lossy(&rejected.stderr)
                .contains("not greater than previous main version")
        );

        write_release(&fixture.repo, "0.9.0", "## [0.9.0]");
        let partial_recovery = commit(&fixture.repo, "partially recover to 0.9.0");
        let output = run_tagger(&fixture.repo, &partial_recovery, &downgrade);
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains("not greater than historical release 1.0.0")
        );
        assert!(
            git(&fixture.repo, &["tag", "--list", "v0.9.0"])
                .stdout
                .is_empty()
        );
    }

    #[test]
    fn malformed_high_release_tags_are_ignored() {
        let fixture = fixture();
        for tag in ["v999.0.0-alpha_1", "v999.0.0-01", "v999.0.0+bad_meta"] {
            assert_success(
                git(
                    &fixture.repo,
                    &[
                        "-c",
                        "user.name=Dione Tests",
                        "-c",
                        "user.email=dione-tests@example.invalid",
                        "tag",
                        "--annotate",
                        tag,
                        "--message",
                        "malformed historical tag",
                        &fixture.initial,
                    ],
                ),
                "malformed historical tag",
            );
            assert_success(
                git(
                    &fixture.repo,
                    &["push", "origin", &format!("refs/tags/{tag}")],
                ),
                "malformed historical tag push",
            );
        }

        write_release(&fixture.repo, "0.2.0", "## [0.2.0]");
        let head = commit(&fixture.repo, "release 0.2.0");
        assert_success(
            run_tagger(&fixture.repo, &head, &fixture.initial),
            "release after malformed tags",
        );
        assert_eq!(
            String::from_utf8(remote_ref(&fixture, "refs/tags/v0.2.0^{commit}").stdout)
                .unwrap()
                .trim(),
            head
        );
    }

    #[test]
    fn remote_tag_race_fails_without_moving_the_winning_tag() {
        let fixture = fixture();
        write_release(&fixture.repo, "0.2.0", "## [0.2.0]");
        let head = commit(&fixture.repo, "release race");

        let wrapper_dir = fixture.repo.join("git-wrapper");
        fs::create_dir(&wrapper_dir).expect("wrapper directory must be created");
        let wrapper = wrapper_dir.join("git");
        let real_git = String::from_utf8(run(&fixture.repo, "which", &["git"]).stdout)
            .unwrap()
            .trim()
            .to_owned();
        fs::write(
            &wrapper,
            format!(
                "#!/bin/sh\nset -eu\nif [ \"${{1:-}}\" = push ] && [ \"${{2:-}}\" = origin ] && [ \"${{3:-}}\" = refs/tags/v0.2.0 ]; then\n  {real_git} --git-dir=\"$RACE_ORIGIN\" -c user.name='Race Winner' -c user.email='race@example.invalid' tag --annotate v0.2.0 --message='winning tag' \"$RACE_COMMIT\"\nfi\nexec {real_git} \"$@\"\n"
            ),
        )
        .expect("git wrapper must be written");
        fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755))
            .expect("git wrapper must be executable");

        let output = Command::new("sh")
            .arg("scripts/tag-qualified-release.sh")
            .current_dir(&fixture.repo)
            .env("EXPECTED_COMMIT", &head)
            .env("PUSH_BEFORE", &fixture.initial)
            .env("RACE_ORIGIN", &fixture.origin)
            .env("RACE_COMMIT", &fixture.initial)
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    wrapper_dir.display(),
                    std::env::var("PATH").expect("test PATH must be set")
                ),
            )
            .output()
            .expect("racing release tagger must execute");
        assert!(!output.status.success());
        assert_eq!(
            String::from_utf8(remote_ref(&fixture, "refs/tags/v0.2.0^{commit}").stdout)
                .unwrap()
                .trim(),
            fixture.initial
        );
    }

    #[test]
    fn non_ancestor_push_boundary_and_wrong_checkout_fail_closed() {
        let fixture = fixture();
        write_release(&fixture.repo, "0.2.0", "## [0.2.0]");
        let head = commit(&fixture.repo, "release 0.2.0");

        let wrong_checkout = run_tagger(&fixture.repo, &fixture.initial, &fixture.initial);
        assert!(!wrong_checkout.status.success());

        assert_success(
            git(&fixture.repo, &["checkout", "-q", "--orphan", "unrelated"]),
            "orphan checkout",
        );
        assert_success(
            git(&fixture.repo, &["rm", "-q", "-rf", "."]),
            "orphan clear",
        );
        fs::create_dir_all(fixture.repo.join("src")).expect("source directory must exist");
        fs::write(fixture.repo.join("src/lib.rs"), "pub fn unrelated() {}\n")
            .expect("source fixture must be written");
        write_release(&fixture.repo, "9.0.0", "## [9.0.0]");
        fs::create_dir_all(fixture.repo.join("scripts")).expect("scripts directory must exist");
        fs::write(
            fixture.repo.join("scripts/tag-qualified-release.sh"),
            include_str!("../scripts/tag-qualified-release.sh"),
        )
        .unwrap();
        fs::write(
            fixture.repo.join("scripts/workspace-package-version.sh"),
            include_str!("../scripts/workspace-package-version.sh"),
        )
        .unwrap();
        fs::write(
            fixture.repo.join("scripts/semver-is-greater.sh"),
            include_str!("../scripts/semver-is-greater.sh"),
        )
        .unwrap();
        let unrelated = commit(&fixture.repo, "unrelated history");
        let output = run_tagger(&fixture.repo, &unrelated, &fixture.initial);
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("is not an ancestor"));

        assert_ne!(head, unrelated);
    }
}
