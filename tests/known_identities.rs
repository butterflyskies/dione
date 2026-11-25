use std::{fs, path::Path, process::Command};

const SCRIPT: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/scripts/scan-known-identities.sh"
);
// Invented identities only: the fixture must not become the roster the scan guards against.
const MARKERS: &str = "Quillon Vey\nquillon\n900000000000000001\n";

fn repository(files: &[(&str, &str)]) -> tempfile::TempDir {
    let temp = tempfile::tempdir().expect("temporary directory must be created");
    for (path, contents) in files {
        let file = temp.path().join(path);
        fs::create_dir_all(file.parent().unwrap()).expect("fixture directory must be created");
        fs::write(file, contents).expect("fixture file must be written");
    }
    for args in [
        &["init", "-q"][..],
        &["add", "-A"],
        &[
            "-c",
            "user.name=fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "-qm",
            "fixture",
        ],
    ] {
        let status = Command::new("git")
            .args(args)
            .current_dir(temp.path())
            .status()
            .expect("git must execute");
        assert!(status.success(), "git {args:?} must succeed");
    }
    temp
}

fn scan(dir: &Path, markers: &str, allow: Option<&str>) -> (Option<i32>, String) {
    let list = tempfile::NamedTempFile::new().expect("identity list must be created");
    fs::write(list.path(), markers).expect("identity list must be written");
    let mut command = Command::new("sh");
    command
        .arg(SCRIPT)
        .arg("--markers")
        .arg(list.path())
        .current_dir(dir);
    let allow_file = tempfile::NamedTempFile::new().expect("allow list must be created");
    fs::write(allow_file.path(), allow.unwrap_or("")).expect("allow list must be written");
    command.arg("--allow").arg(allow_file.path());
    let output = command.output().expect("scan must execute");
    (
        output.status.code(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[test]
fn reports_content_and_path_hits_by_location_without_echoing_the_identity_or_the_line() {
    let repo = repository(&[
        (
            "src/a.rs",
            "fn f() {}\nlet user = \"Quillon Vey\"; // secret-line-text\n",
        ),
        ("src/quillon_fixture.rs", "fn g() {}\n"),
        ("tests/ids.rs", "const ID: u64 = 900000000000000001;\n"),
    ]);
    let (status, stderr) = scan(repo.path(), MARKERS, None);
    assert_eq!(status, Some(1));
    assert!(stderr.contains("known identity: src/a.rs:2\n"), "{stderr}");
    assert!(
        stderr.contains("known identity: tests/ids.rs:1\n"),
        "{stderr}"
    );
    assert!(
        stderr.contains("known identity: src/quillon_fixture.rs (path)\n"),
        "{stderr}"
    );
    assert!(stderr.contains("3 hit(s)"), "{stderr}");
    for secret in ["Quillon Vey", "900000000000000001", "secret-line-text"] {
        assert!(
            !stderr.contains(secret),
            "{secret} must not be printed: {stderr}"
        );
    }
}

#[test]
fn matches_whole_words_only() {
    let repo = repository(&[("src/a.rs", "let quillonish = xquillon + quillon2;\n")]);
    assert_eq!(
        scan(repo.path(), "quillon\n", None).0,
        Some(0),
        "a name inside a longer word is not an identity"
    );

    for line in [
        "let owner = \"quillon-construct\";\n",
        "let quillon_scope = 1;\n",
        "// QUILLON wrote this\n",
    ] {
        let repo = repository(&[("src/a.rs", line)]);
        assert_eq!(
            scan(repo.path(), "quillon\n", None).0,
            Some(1),
            "{line:?} names the identity"
        );
    }

    let repo = repository(&[(
        "src/a.rs",
        "let path = \"/srv/a.b/c\";\nlet other = \"/srv/aXb/c\";\n",
    )]);
    let (status, stderr) = scan(repo.path(), "/srv/a.b\n", None);
    assert_eq!(status, Some(1));
    assert!(
        stderr.contains("1 hit(s)"),
        "regex metacharacters in an entry match literally: {stderr}"
    );
}

#[test]
fn allow_list_exempts_only_the_listed_paths() {
    let repo = repository(&[
        ("docs/history.md", "Quillon wrote this.\n"),
        ("src/a.rs", "// Quillon\n"),
    ]);
    let (status, stderr) = scan(repo.path(), MARKERS, Some("# exempt\ndocs/history.md\n"));
    assert_eq!(status, Some(1));
    assert!(!stderr.contains("docs/history.md"), "{stderr}");
    assert!(stderr.contains("known identity: src/a.rs:1\n"), "{stderr}");
}

#[test]
fn clean_tree_passes() {
    let repo = repository(&[("src/a.rs", "let user = \"fixture-person\";\n")]);
    assert_eq!(scan(repo.path(), MARKERS, None).0, Some(0));
}

#[test]
fn blank_list_is_an_error_not_a_match_all_or_a_pass() {
    let repo = repository(&[("src/a.rs", "fn f() {}\n")]);
    for markers in ["", "\n\r\n   \n"] {
        let (status, stderr) = scan(repo.path(), markers, None);
        assert_eq!(status, Some(2), "{markers:?}: {stderr}");
    }
}

#[test]
fn a_failed_search_is_an_error_not_a_clean_result() {
    let outside = tempfile::tempdir().expect("temporary directory must be created");
    let (status, stderr) = scan(outside.path(), MARKERS, None);
    assert_eq!(status, Some(2), "{stderr}");
    assert!(!stderr.contains("clean"), "{stderr}");
}

#[test]
fn padded_entries_still_match() {
    let repo = repository(&[("src/a.rs", "let quillon_scope = 1;\n")]);
    for markers in ["quillon \n", "  quillon\t\n", "\tquillon \r\n"] {
        let (status, stderr) = scan(repo.path(), markers, None);
        assert_eq!(status, Some(1), "{markers:?} must still match: {stderr}");
    }
}

#[test]
fn a_failed_report_step_is_an_error_not_a_clean_result() {
    let repo = repository(&[("src/a.rs", "let user = \"Quillon\";\n")]);
    let bin = tempfile::tempdir().expect("shim directory must be created");
    let shim = bin.path().join("cut");
    fs::write(&shim, "#!/bin/sh\nexit 1\n").expect("failing cut shim must be written");
    fs::set_permissions(&shim, std::os::unix::fs::PermissionsExt::from_mode(0o755))
        .expect("shim must be executable");
    let list = tempfile::NamedTempFile::new().expect("identity list must be created");
    fs::write(list.path(), MARKERS).expect("identity list must be written");
    let output = Command::new("sh")
        .arg(SCRIPT)
        .arg("--markers")
        .arg(list.path())
        .current_dir(repo.path())
        .env(
            "PATH",
            format!(
                "{}:{}",
                bin.path().display(),
                std::env::var("PATH").unwrap()
            ),
        )
        .output()
        .expect("scan must execute");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(2), "{stderr}");
    assert!(!String::from_utf8_lossy(&output.stdout).contains("clean"));
}

#[test]
fn accented_names_fold_the_same_in_contents_and_paths() {
    let repo = repository(&[
        ("src/a.rs", "let user = \"ZÉPHYRINE\";\n"),
        ("src/ZÉPHYRINE_fixture.rs", "fn f() {}\n"),
    ]);
    let (status, stderr) = scan(repo.path(), "zéphyrine\n", None);
    assert_eq!(status, Some(1), "{stderr}");
    assert!(stderr.contains("known identity: src/a.rs:1\n"), "{stderr}");
    assert!(
        stderr.contains("known identity: src/ZÉPHYRINE_fixture.rs (path)\n"),
        "{stderr}"
    );
}

const ADMIT: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/scripts/run-known-identities-scan.sh"
);

/// Runs the CI admission helper as a workflow step would, with `roster` as the secret.
fn admit(
    dir: &Path,
    mode: &str,
    roster: Option<&str>,
    from_fork: bool,
) -> (Option<i32>, String, usize) {
    let tmp = tempfile::tempdir().expect("private TMPDIR must be created");
    let mut command = Command::new("sh");
    command
        .arg(ADMIT)
        .arg(mode)
        .current_dir(dir)
        .env("TMPDIR", tmp.path())
        .env_remove("KNOWN_IDENTITIES")
        .env_remove("FROM_FORK");
    if let Some(roster) = roster {
        command.env("KNOWN_IDENTITIES", roster);
    }
    if from_fork {
        command.env("FROM_FORK", "true");
    }
    let output = command.output().expect("admission helper must execute");
    let left = fs::read_dir(tmp.path()).unwrap().count();
    let text = String::from_utf8_lossy(&output.stdout).into_owned()
        + &String::from_utf8_lossy(&output.stderr);
    (output.status.code(), text, left)
}

#[test]
fn a_missing_roster_fails_except_on_fork_pull_requests() {
    let repo = repository(&[("src/a.rs", "fn f() {}\n")]);
    assert_eq!(
        admit(repo.path(), "pull-request", None, false).0,
        Some(2),
        "same-repo PR or push"
    );
    let (status, text, _) = admit(repo.path(), "pull-request", None, true);
    assert_eq!(status, Some(0));
    assert!(text.contains("skipped"), "{text}");
    assert_eq!(
        admit(repo.path(), "publish", None, true).0,
        Some(2),
        "publishing never skips"
    );
    assert_eq!(admit(repo.path(), "publish", Some(""), false).0, Some(2));
    assert_eq!(
        admit(repo.path(), "release", Some("quillon"), false).0,
        Some(2),
        "unknown mode"
    );
}

#[test]
fn a_present_roster_reaches_the_scan_and_its_result_propagates() {
    let named = repository(&[("src/a.rs", "let user = \"Quillon\";\n")]);
    let clean = repository(&[("src/a.rs", "let user = \"fixture-person\";\n")]);
    for mode in ["pull-request", "publish"] {
        let (status, text, _) = admit(named.path(), mode, Some(MARKERS), false);
        assert_eq!(status, Some(1), "{mode}: {text}");
        assert!(
            text.contains("known identity: src/a.rs:1"),
            "{mode}: {text}"
        );
        assert_eq!(
            admit(clean.path(), mode, Some(MARKERS), false).0,
            Some(0),
            "{mode}"
        );
    }
    let outside = tempfile::tempdir().expect("temporary directory must be created");
    assert_eq!(
        admit(outside.path(), "publish", Some(MARKERS), false).0,
        Some(2),
        "scan errors propagate"
    );
}

#[test]
fn the_roster_file_is_removed_after_every_outcome() {
    let named = repository(&[("src/a.rs", "let user = \"Quillon\";\n")]);
    let clean = repository(&[("src/a.rs", "fn f() {}\n")]);
    let outside = tempfile::tempdir().expect("temporary directory must be created");
    for (dir, expected) in [(clean.path(), 0), (named.path(), 1), (outside.path(), 2)] {
        let (status, _, left) = admit(dir, "publish", Some(MARKERS), false);
        assert_eq!(status, Some(expected));
        assert_eq!(
            left, 0,
            "roster and scratch files must not outlive exit {expected}"
        );
    }
}

#[test]
fn both_workflows_admit_the_roster_only_through_the_helper() {
    let ci = include_str!("../.forgejo/workflows/linux.yml");
    let publish = include_str!("../.github/workflows/publish-crate.yml");
    assert!(ci.contains("run: sh scripts/run-known-identities-scan.sh pull-request\n"));
    assert!(publish.contains("run: sh scripts/run-known-identities-scan.sh publish\n"));
    for workflow in [ci, publish] {
        assert_eq!(
            workflow.matches("${{ secrets.KNOWN_IDENTITIES }}").count(),
            1
        );
    }
}

#[test]
fn case_folding_does_not_depend_on_the_callers_locale() {
    let repo = repository(&[
        ("src/a.rs", "let user = \"ZÉPHYRINE\";\n"),
        ("src/ZÉPHYRINE_fixture.rs", "fn f() {}\n"),
    ]);
    let list = tempfile::NamedTempFile::new().expect("identity list must be created");
    fs::write(list.path(), "zéphyrine\n").expect("identity list must be written");
    for locale in ["C", "POSIX", "C.UTF-8"] {
        let output = Command::new("sh")
            .arg(SCRIPT)
            .args(["--markers"])
            .arg(list.path())
            .args(["--allow", "/dev/null"])
            .current_dir(repo.path())
            .env("LC_ALL", locale)
            .output()
            .expect("scan must execute");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("2 hit(s)"), "LC_ALL={locale}: {stderr}");
    }
}

fn real_sed() -> String {
    let out = Command::new("sh")
        .args(["-c", "command -v sed"])
        .output()
        .expect("sh must run");
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

#[test]
fn a_roster_step_that_fails_partway_is_an_error_not_a_partial_scan() {
    // The roster's first entry matches nothing; the second names the fixture. A `sed` that writes
    // one line and then fails would leave a one-entry roster that scans clean.
    let repo = repository(&[("src/a.rs", "let user = \"Quillon\";\n")]);
    let bin = tempfile::tempdir().expect("shim directory must be created");
    let shim = bin.path().join("sed");
    fs::write(
        &shim,
        "#!/bin/sh\ncase \"$*\" in *'[[:space:]]*$'*) ;; *) exec \"$REAL_SED\" \"$@\" ;; esac\nfor last; do :; done\nif [ -f \"$last\" ]; then head -n 1 \"$last\"; else head -n 1; fi\nexit 1\n",
    )
    .expect("partial sed shim must be written");
    fs::set_permissions(&shim, std::os::unix::fs::PermissionsExt::from_mode(0o755))
        .expect("shim must be executable");
    let list = tempfile::NamedTempFile::new().expect("identity list must be created");
    fs::write(list.path(), "fixture-nobody\nquillon\n").expect("identity list must be written");
    let output = Command::new("sh")
        .arg(SCRIPT)
        .arg("--markers")
        .arg(list.path())
        .current_dir(repo.path())
        .env(
            "PATH",
            format!(
                "{}:{}",
                bin.path().display(),
                std::env::var("PATH").unwrap()
            ),
        )
        .env("REAL_SED", real_sed())
        .output()
        .expect("scan must execute");
    assert_eq!(
        output.status.code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!String::from_utf8_lossy(&output.stdout).contains("clean"));
}

#[test]
fn files_git_treats_as_binary_are_still_searched() {
    let repo = repository(&[
        (".gitattributes", "fixture.txt -diff\n"),
        ("fixture.txt", "user Quillon here\n"),
        ("blob.bin", "abc\0def Quillon\n"),
        ("clean.bin", "abc\0nothing to see\n"),
    ]);
    let (status, stderr) = scan(repo.path(), MARKERS, None);
    assert_eq!(status, Some(1), "{stderr}");
    assert!(
        stderr.contains("known identity: fixture.txt:1\n"),
        "-diff text: {stderr}"
    );
    assert!(
        stderr.contains("known identity: blob.bin:1\n"),
        "after a NUL: {stderr}"
    );
    assert!(
        !stderr.contains("clean.bin"),
        "binary without a name: {stderr}"
    );
    assert!(stderr.contains("2 hit(s)"), "{stderr}");
}

#[test]
fn a_setup_failure_is_an_error_in_both_entrypoints() {
    let repo = repository(&[("src/a.rs", "let user = \"Quillon\";\n")]);
    let list = tempfile::NamedTempFile::new().expect("identity list must be created");
    fs::write(list.path(), MARKERS).expect("identity list must be written");
    let scan_status = Command::new("sh")
        .arg(SCRIPT)
        .arg("--markers")
        .arg(list.path())
        .current_dir(repo.path())
        .env("TMPDIR", "/nonexistent-known-identities-tmp")
        .status()
        .expect("scan must execute");
    assert_eq!(
        scan_status.code(),
        Some(2),
        "scanner without a scratch directory"
    );
    let admit_status = Command::new("sh")
        .arg(ADMIT)
        .arg("publish")
        .current_dir(repo.path())
        .env("TMPDIR", "/nonexistent-known-identities-tmp")
        .env("KNOWN_IDENTITIES", MARKERS)
        .status()
        .expect("admission helper must execute");
    assert_eq!(admit_status.code(), Some(2), "helper without a roster file");
}

#[test]
fn a_failed_report_formatting_step_is_an_error() {
    let repo = repository(&[("src/a.rs", "let user = \"Quillon\";\n")]);
    let bin = tempfile::tempdir().expect("shim directory must be created");
    let shim = bin.path().join("sort");
    fs::write(&shim, "#!/bin/sh\nexit 3\n").expect("failing sort shim must be written");
    fs::set_permissions(&shim, std::os::unix::fs::PermissionsExt::from_mode(0o755))
        .expect("shim must be executable");
    let list = tempfile::NamedTempFile::new().expect("identity list must be created");
    fs::write(list.path(), MARKERS).expect("identity list must be written");
    let output = Command::new("sh")
        .arg(SCRIPT)
        .arg("--markers")
        .arg(list.path())
        .current_dir(repo.path())
        .env(
            "PATH",
            format!(
                "{}:{}",
                bin.path().display(),
                std::env::var("PATH").unwrap()
            ),
        )
        .output()
        .expect("scan must execute");
    assert_eq!(
        output.status.code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
