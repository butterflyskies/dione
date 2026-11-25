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
