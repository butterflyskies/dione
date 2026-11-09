use regex::{Regex, RegexBuilder};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

fn assert_toml_has_no_git_sources(name: &str, contents: &str) {
    fn visit(value: &toml::Value, path: &str) {
        match value {
            toml::Value::Table(table) => {
                for (key, child) in table {
                    let child_path = format!("{path}.{key}");
                    assert_ne!(key, "git", "{child_path} declares a git dependency");
                    if key == "source" {
                        assert!(
                            !child
                                .as_str()
                                .is_some_and(|source| source.starts_with("git+")),
                            "{child_path} contains a git package source"
                        );
                    }
                    visit(child, &child_path);
                }
            }
            toml::Value::Array(values) => {
                for (index, child) in values.iter().enumerate() {
                    visit(child, &format!("{path}[{index}]"));
                }
            }
            _ => {}
        }
    }

    let document: toml::Value = toml::from_str(contents)
        .unwrap_or_else(|error| panic!("{name} must contain valid TOML: {error}"));
    visit(&document, name);
}

fn private_dependency_name() -> String {
    ["cingu", "late"].concat()
}

fn extracted_dependency_name() -> String {
    ["auspex", "-core"].concat()
}

fn git_output(root: &Path, args: &[&str]) -> Output {
    Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .expect("git must execute")
}

fn commit_fixture(root: &Path, message: &str) -> String {
    assert!(git_output(root, &["add", "."]).status.success());
    let commit = Command::new("git")
        .args([
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
        ])
        .current_dir(root)
        .output()
        .expect("git commit must execute");
    assert!(
        commit.status.success(),
        "fixture commit failed: {}",
        String::from_utf8_lossy(&commit.stderr)
    );
    let revision = git_output(root, &["rev-parse", "HEAD"]);
    assert!(revision.status.success());
    String::from_utf8(revision.stdout)
        .expect("git revision must be UTF-8")
        .trim()
        .to_owned()
}

fn run_release_hygiene(
    root: &Path,
    event_name: &str,
    pr_base: &str,
    pr_head: &str,
    push_before: &str,
    push_after: &str,
) -> Output {
    Command::new("sh")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/.forgejo/scripts/release-hygiene.sh"
        ))
        .env("EVENT_NAME", event_name)
        .env("PR_BASE", pr_base)
        .env("PR_HEAD", pr_head)
        .env("PUSH_BEFORE", push_before)
        .env("PUSH_AFTER", push_after)
        .current_dir(root)
        .output()
        .expect("release-hygiene script must execute")
}

fn compile_patterns(source: &str) -> Vec<(String, Regex)> {
    let private_name = private_dependency_name();
    let long_canary = ["Mir", "anda"].concat();
    let short_canary = ["Mi", "ra"].concat();
    source
        .lines()
        .map(|line| {
            let (rule, pattern) = line
                .split_once('\t')
                .expect("structural package rule must have an ID and pattern");
            let pattern = pattern
                .replace("{PRIVATE_DEP}", &private_name)
                .replace("{CANARY_LONG}", &long_canary)
                .replace("{CANARY_SHORT}", &short_canary);
            let regex = RegexBuilder::new(&pattern)
                .case_insensitive(true)
                .build()
                .unwrap_or_else(|error| {
                    panic!("structural package rule {rule} is invalid: {error}")
                });
            (rule.to_owned(), regex)
        })
        .collect()
}

fn structural_patterns() -> Vec<(String, Regex)> {
    compile_patterns(include_str!(
        "../scripts/public-package-structural-patterns.txt"
    ))
}

fn member_patterns() -> Vec<(String, Regex)> {
    compile_patterns(include_str!(
        "../scripts/public-package-member-patterns.txt"
    ))
}

fn assert_public_tree(
    directory: &Path,
    patterns: &[(String, Regex)],
    member_patterns: &[(String, Regex)],
) {
    for entry in fs::read_dir(directory).expect("source directory must be readable") {
        let path = entry.expect("source entry must be readable").path();
        if path.is_dir() {
            if path
                .file_name()
                .is_some_and(|name| name == ".git" || name == "target")
            {
                continue;
            }
            assert_public_tree(&path, patterns, member_patterns);
        } else {
            let relative = path
                .strip_prefix(env!("CARGO_MANIFEST_DIR"))
                .expect("public tree path must remain inside the manifest root")
                .to_string_lossy();
            for (rule, pattern) in patterns {
                assert!(
                    !pattern.is_match(&relative),
                    "public tree filename violates structural package rule {rule}"
                );
            }
            for (rule, pattern) in member_patterns {
                assert!(
                    !pattern.is_match(&relative),
                    "public tree filename violates member package rule {rule}"
                );
            }
            let bytes = fs::read(&path).expect("public tree file must be readable");
            if let Ok(contents) = std::str::from_utf8(&bytes) {
                for (rule, pattern) in patterns {
                    assert!(
                        !pattern.is_match(contents),
                        "public tree content violates structural package rule {rule}"
                    );
                }
                if path.file_name().is_some_and(|name| name == "Cargo.toml") {
                    assert_toml_has_no_git_sources(&relative, contents);
                }
            }
        }
    }
}

#[test]
fn public_package_graph_has_no_private_adapter_dependency() {
    for (name, contents) in [
        ("Cargo.toml", include_str!("../Cargo.toml")),
        ("Cargo.lock", include_str!("../Cargo.lock")),
    ] {
        assert!(
            !contents
                .to_ascii_lowercase()
                .contains(&private_dependency_name()),
            "{name} contains a private package-graph reference"
        );
    }
}

#[test]
fn dione_no_longer_resolves_or_packages_the_extracted_crate() {
    let extracted = extracted_dependency_name();
    for (name, contents) in [
        ("Cargo.toml", include_str!("../Cargo.toml")),
        ("Cargo.lock", include_str!("../Cargo.lock")),
        (
            ".forgejo/workflows/linux.yml",
            include_str!("../.forgejo/workflows/linux.yml"),
        ),
        (
            ".github/workflows/publish-crate.yml",
            include_str!("../.github/workflows/publish-crate.yml"),
        ),
    ] {
        assert!(
            !contents.contains(&extracted),
            "{name} still resolves or packages the extracted crate"
        );
    }

    let metadata = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
        .args(["metadata", "--locked", "--format-version", "1"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("cargo metadata must execute");
    assert!(
        metadata.status.success(),
        "cargo metadata failed: {}",
        String::from_utf8_lossy(&metadata.stderr)
    );
    let graph: serde_json::Value =
        serde_json::from_slice(&metadata.stdout).expect("cargo metadata must be JSON");
    assert!(
        graph["packages"]
            .as_array()
            .expect("metadata packages must be an array")
            .iter()
            .all(|package| package["name"] != extracted),
        "the resolved package graph still contains the extracted crate"
    );

    let package = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
        .args([
            "package",
            "-p",
            "dione",
            "--list",
            "--locked",
            "--allow-dirty",
        ])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("cargo package --list must execute");
    assert!(
        package.status.success(),
        "cargo package --list failed: {}",
        String::from_utf8_lossy(&package.stderr)
    );
    let removed_path = ["crates/", &extracted].concat();
    assert!(
        !String::from_utf8_lossy(&package.stdout).contains(&removed_path),
        "Dione's package still contains the extracted crate"
    );
}

#[test]
fn public_package_graph_has_no_git_dependencies() {
    assert_public_tree(
        Path::new(env!("CARGO_MANIFEST_DIR")),
        &structural_patterns(),
        &member_patterns(),
    );
    assert_toml_has_no_git_sources("Cargo.lock", include_str!("../Cargo.lock"));
}

#[test]
fn public_rust_sources_have_no_private_adapter_or_wiring() {
    assert_public_tree(
        Path::new(env!("CARGO_MANIFEST_DIR")),
        &structural_patterns(),
        &member_patterns(),
    );
}

fn create_package_fixture(temp: &tempfile::TempDir, name: &str, contents: &[u8]) -> PathBuf {
    let package_root = temp.path().join(format!("{name}-1.0.0"));
    fs::create_dir(&package_root).expect("package fixture directory must be created");
    fs::write(package_root.join("payload"), contents).expect("package fixture must be written");

    let archive = temp.path().join(format!("{name}.crate"));
    let status = Command::new("tar")
        .args(["czf"])
        .arg(&archive)
        .args(["-C"])
        .arg(temp.path())
        .arg(format!("{name}-1.0.0"))
        .status()
        .expect("tar must execute");
    assert!(status.success(), "package fixture must be archived");
    archive
}

fn create_cargo_package_with_file(temp: &tempfile::TempDir, file_name: &str) -> PathBuf {
    let package_root = temp.path().join("package");
    fs::create_dir_all(package_root.join("src"))
        .expect("Cargo package fixture source directory must be created");
    fs::write(
        package_root.join("Cargo.toml"),
        "[workspace]\n\n[package]\nname = \"newline-fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )
    .expect("Cargo package fixture manifest must be written");
    fs::write(package_root.join("src/lib.rs"), "pub fn fixture() {}\n")
        .expect("Cargo package fixture source must be written");
    let fixture_file = package_root.join(file_name);
    if let Some(parent) = fixture_file.parent() {
        fs::create_dir_all(parent).expect("Cargo package fixture parent must be created");
    }
    fs::write(fixture_file, "public payload\n").expect("newline-name fixture must be written");

    let target = temp.path().join("target");
    let status = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
        .args(["package", "--allow-dirty", "--no-verify"])
        .arg("--manifest-path")
        .arg(package_root.join("Cargo.toml"))
        .arg("--target-dir")
        .arg(&target)
        .status()
        .expect("cargo package must execute");
    assert!(status.success(), "Cargo package fixture must be archived");
    target.join("package/newline-fixture-0.1.0.crate")
}

fn verify_package_privacy(archive: &Path, marker_file: Option<&Path>) -> std::process::Output {
    let mut command = Command::new("sh");
    command.arg("scripts/verify-public-package-privacy.sh");
    if let Some(marker_file) = marker_file {
        command.args(["--forbidden-markers", marker_file.to_str().unwrap()]);
    }
    command
        .arg(archive)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("package privacy verifier must execute")
}

#[test]
fn package_privacy_verifier_allows_public_name_and_binary_payloads() {
    let temp = tempfile::tempdir().expect("temporary directory must be created");
    let private_name = private_dependency_name();
    let mut contents = format!(
        "Historical note: Cingulate was the public project name.\n{}",
        ["https://github.com/butterflyskies/", &private_name, ".git"].concat()
    )
    .into_bytes();
    contents.insert(0, 0);
    let archive = create_package_fixture(&temp, "allowed", &contents);

    let output = verify_package_privacy(&archive, None);
    assert!(
        output.status.success(),
        "public name or binary fixture was rejected: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let text_archive = create_package_fixture(
        &temp,
        "public-name",
        b"Historical note: Cingulate was the public project name. See https://example.org/projects/Cingulate/history.\n",
    );
    let output = verify_package_privacy(&text_archive, None);
    assert!(
        output.status.success(),
        "the public historical name must remain allowed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn package_privacy_verifier_rejects_consented_name_canaries_case_insensitively() {
    for (name, canary) in [
        ("long-name", ["Mir", "anda"].concat()),
        ("short-name", ["Mi", "ra"].concat()),
    ] {
        for spelling in [canary.to_lowercase(), canary.to_uppercase()] {
            let temp = tempfile::tempdir().expect("temporary directory must be created");
            let archive = create_package_fixture(&temp, name, spelling.as_bytes());
            let output = verify_package_privacy(&archive, None);
            assert!(
                !output.status.success(),
                "{name} negative canary must be rejected case-insensitively"
            );
            assert!(
                !String::from_utf8_lossy(&output.stderr).contains(&spelling),
                "negative canary must not be echoed"
            );
        }
    }
}

#[test]
fn package_privacy_verifier_rejects_each_private_material_class() {
    let private_name = private_dependency_name();
    let fixtures = [
        (
            "source-url",
            ["https://github.com/butterflyskies/", &private_name, ".git"].concat(),
        ),
        (
            "source-path",
            ["/home/operator/dev/", &private_name, "/patterns.toml"].concat(),
        ),
        (
            "relative-source-path",
            ["../", &private_name, "/patterns.toml"].concat(),
        ),
        (
            "cargo-bare-relative-path",
            ["path = \"", &private_name, "\""].concat(),
        ),
        (
            "cargo-nested-relative-path",
            ["path = \"vendor/", &private_name, "\""].concat(),
        ),
        (
            "cargo-renamed-relative-path",
            [
                "adapter = { package = \"",
                &private_name,
                "\", path = \"",
                &private_name,
                "\" }",
            ]
            .concat(),
        ),
        (
            "portable-absolute-path",
            ["/opt/private/", &private_name, "/patterns.toml"].concat(),
        ),
        (
            "endpoint",
            [&private_name, ".classifier.svc.echoes"].concat(),
        ),
        ("adapter", [&private_name, "::PatternSet"].concat()),
        ("module", ["mod ", &private_name, ";"].concat()),
        ("use", ["use ", &private_name, ";"].concat()),
        (
            "crate-module",
            ["crate::", &private_name, "::PatternSet"].concat(),
        ),
    ];

    for (name, contents) in fixtures {
        let temp = tempfile::tempdir().expect("temporary directory must be created");
        let archive = create_package_fixture(&temp, name, contents.as_bytes());
        let output = verify_package_privacy(&archive, None);
        assert!(
            !output.status.success(),
            "{name} private-material fixture must be rejected"
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("public package contains forbidden private material"),
            "{name} rejection must identify the package boundary"
        );
        assert!(
            !stderr.contains(&contents),
            "{name} rejection must not echo private material"
        );
    }
}

#[test]
fn package_privacy_verifier_rejects_private_source_with_crlf_pattern_files() {
    let temp = tempfile::tempdir().expect("temporary directory must be created");
    let scripts = temp.path().join("scripts");
    fs::create_dir(&scripts).expect("scripts directory must be created");
    let source_scripts = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts");
    fs::copy(
        source_scripts.join("verify-public-package-privacy.sh"),
        scripts.join("verify-public-package-privacy.sh"),
    )
    .expect("privacy verifier must be copied");
    for name in [
        "public-package-structural-patterns.txt",
        "public-package-member-patterns.txt",
    ] {
        let source = fs::read(source_scripts.join(name)).expect("pattern file must be read");
        let crlf = String::from_utf8(source)
            .expect("pattern file must be UTF-8")
            .replace("\r\n", "\n")
            .replace('\n', "\r\n");
        fs::write(scripts.join(name), crlf).expect("CRLF pattern file must be written");
    }

    let verify = |archive: &Path| {
        Command::new("sh")
            .arg("scripts/verify-public-package-privacy.sh")
            .arg(archive)
            .current_dir(temp.path())
            .output()
            .expect("privacy verifier must execute")
    };

    let public_archive =
        create_package_fixture(&temp, "public-crlf-patterns", b"use serde::Serialize;\n");
    let public_output = verify(&public_archive);
    assert!(
        public_output.status.success(),
        "CRLF pattern files must still allow public source: {}",
        String::from_utf8_lossy(&public_output.stderr)
    );

    let source = format!("use {}::PatternSet;\n", private_dependency_name());
    let archive = create_package_fixture(&temp, "private-crlf-patterns", source.as_bytes());
    let output = verify(&archive);
    assert!(
        !output.status.success(),
        "CRLF pattern files must still reject private source"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("rule=structural-content:rust-use"),
        "privacy verifier must identify the private use rule"
    );
}

#[test]
fn package_privacy_verifier_uses_external_forbidden_markers_without_echoing_them() {
    for (name, marker, line_ending) in [
        ("synthetic-marker", ["fixture", "8f13c2"].join(":"), "\r\n"),
        (
            "private-data",
            ["house-only", "payload-7f3a"].join("-"),
            "\n",
        ),
    ] {
        let temp = tempfile::tempdir().expect("temporary directory must be created");
        let marker_file = temp.path().join("forbidden-markers");
        fs::write(&marker_file, format!("{marker}{line_ending}"))
            .expect("external marker fixture must be written");
        let archive = create_package_fixture(&temp, name, marker.as_bytes());

        let output = verify_package_privacy(&archive, Some(&marker_file));
        assert!(!output.status.success(), "{name} marker must be rejected");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("public package contains forbidden private material"));
        assert!(
            !stderr.contains(&marker),
            "private marker must not be echoed"
        );
    }
}

#[test]
fn package_privacy_verifier_scans_cargo_newline_member_names_without_logging_them() {
    let temp = tempfile::tempdir().expect("temporary directory must be created");
    let marker = ["newline", "private", "marker"].join("-");
    let file_name = format!("archive-controlled-prefix\n{marker}.txt");
    let archive = create_cargo_package_with_file(&temp, &file_name);
    let marker_file = temp.path().join("forbidden-markers");
    fs::write(&marker_file, format!("{marker}\n"))
        .expect("external marker fixture must be written");

    let output = verify_package_privacy(&archive, Some(&marker_file));
    assert!(
        !output.status.success(),
        "newline member marker must be rejected"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("rule=external-name"));
    assert!(
        !stderr.contains(&marker),
        "private marker must not be echoed"
    );
    assert!(
        !stderr.contains("archive-controlled-prefix"),
        "archive-controlled filename must not be echoed"
    );
}

#[test]
fn package_privacy_verifier_rejects_private_cargo_member_component_without_logging_it() {
    let benign_temp = tempfile::tempdir().expect("temporary directory must be created");
    let benign_archive =
        create_cargo_package_with_file(&benign_temp, "vendor/public-adapter/patterns.toml");
    let output = verify_package_privacy(&benign_archive, None);
    assert!(
        output.status.success(),
        "benign nested Cargo member must remain allowed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let private_temp = tempfile::tempdir().expect("temporary directory must be created");
    let private_name = private_dependency_name();
    let private_member = ["vendor/", &private_name, "/patterns.toml"].concat();
    let private_archive = create_cargo_package_with_file(&private_temp, &private_member);
    let output = verify_package_privacy(&private_archive, None);
    assert!(
        !output.status.success(),
        "nested private Cargo member must be rejected"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("rule=structural-member:private-path-component"));
    assert!(
        !stderr.contains(&private_name),
        "private member component must not be logged"
    );
    assert!(
        !stderr.contains("vendor/"),
        "archive member path must not be logged"
    );
}

#[cfg(unix)]
#[test]
fn package_privacy_verifier_fails_closed_without_logging_on_grep_read_error() {
    let temp = tempfile::tempdir().expect("temporary directory must be created");
    let archive = create_package_fixture(&temp, "read-error", b"public payload");
    let bin = temp.path().join("bin");
    fs::create_dir(&bin).expect("test bin directory must be created");
    let real_grep = Command::new("sh")
        .args(["-c", "command -v grep"])
        .output()
        .expect("grep lookup must execute");
    assert!(real_grep.status.success(), "grep must be available");
    let real_grep = String::from_utf8(real_grep.stdout)
        .expect("grep path must be UTF-8")
        .trim()
        .to_owned();
    let wrapper = bin.join("grep");
    fs::write(
        &wrapper,
        format!("#!/bin/sh\ncase $* in *payload*) exit 2 ;; esac\nexec {real_grep} \"$@\"\n"),
    )
    .expect("grep wrapper must be written");
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755))
        .expect("grep wrapper must be executable");

    let path = std::env::var_os("PATH").unwrap_or_default();
    let output = Command::new("sh")
        .arg("scripts/verify-public-package-privacy.sh")
        .arg(&archive)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .env(
            "PATH",
            format!("{}:{}", bin.display(), path.to_string_lossy()),
        )
        .output()
        .expect("package privacy verifier must execute");
    assert!(!output.status.success(), "grep read error must fail closed");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("rule=file-scan-error"));
    assert!(
        !stderr.contains("payload"),
        "scanned path must not be logged"
    );
}

#[test]
fn package_privacy_verifier_fails_closed_for_missing_or_empty_marker_source() {
    let temp = tempfile::tempdir().expect("temporary directory must be created");
    let archive = create_package_fixture(&temp, "marker-source", b"public payload");

    for marker_file in [
        temp.path().join("missing-markers"),
        temp.path().join("empty-markers"),
    ] {
        if marker_file.ends_with("empty-markers") {
            fs::write(&marker_file, []).expect("empty marker fixture must be written");
        }
        let output = verify_package_privacy(&archive, Some(&marker_file));
        assert_eq!(output.status.code(), Some(2));
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains("forbidden-marker file must exist and be non-empty")
        );
    }

    let blank_marker_file = temp.path().join("blank-markers");
    fs::write(&blank_marker_file, b"valid-marker\r\n \t\r\n")
        .expect("blank marker fixture must be written");
    let output = verify_package_privacy(&archive, Some(&blank_marker_file));
    assert_eq!(output.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("forbidden-marker file contains a blank marker")
    );
}

#[test]
fn release_version_helper_reads_dione_package() {
    let output = Command::new("sh")
        .arg("scripts/workspace-package-version.sh")
        .arg("dione")
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("version helper must execute");

    assert!(
        output.status.success(),
        "version helper failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        env!("CARGO_PKG_VERSION")
    );
}

#[test]
fn release_preserves_tokenless_dione_verification_and_reconciliation() {
    let workflow = include_str!("../.github/workflows/publish-crate.yml");
    let (verify, remaining) = workflow.split_once("\n  publish-rendering:").unwrap();
    let (publish_rendering, remaining) = remaining.split_once("\n  reconcile-rendering:").unwrap();
    let (reconcile_rendering, remaining) = remaining.split_once("\n  publish-durable:").unwrap();
    let (publish_durable, remaining) = remaining.split_once("\n  reconcile-durable:").unwrap();
    let (reconcile_durable, remaining) = remaining.split_once("\n  publish-contracts:").unwrap();
    let (publish_contracts, remaining) = remaining.split_once("\n  reconcile-contracts:").unwrap();
    let (reconcile_contracts, remaining) = remaining.split_once("\n  publish-rook:").unwrap();
    let (publish_rook, remaining) = remaining.split_once("\n  reconcile-rook:").unwrap();
    let (reconcile_rook, remaining) = remaining.split_once("\n  publish-attention:").unwrap();
    let (publish_attention, remaining) = remaining.split_once("\n  reconcile-attention:").unwrap();
    let (reconcile_attention, remaining) = remaining.split_once("\n  publish-evidence:").unwrap();
    let (publish_evidence, remaining) = remaining.split_once("\n  reconcile-evidence:").unwrap();
    let (reconcile_evidence, remaining) = remaining.split_once("\n  publish-messages:").unwrap();
    let (publish_messages, remaining) = remaining.split_once("\n  reconcile-messages:").unwrap();
    let (reconcile_messages, remaining) =
        remaining.split_once("\n  publish-rate-limiter:").unwrap();
    let (publish_rate_limiter, remaining) =
        remaining.split_once("\n  reconcile-rate-limiter:").unwrap();
    let (reconcile_rate_limiter, remaining) = remaining.split_once("\n  publish-vaelii:").unwrap();
    let (publish_vaelii, remaining) = remaining.split_once("\n  reconcile-vaelii:").unwrap();
    let (reconcile_vaelii, remaining) = remaining.split_once("\n  publish-gaie:").unwrap();
    let (publish_gaie, remaining) = remaining.split_once("\n  reconcile-gaie:").unwrap();
    let (reconcile_gaie, remaining) = remaining
        .split_once("\n  publish-identity-display:")
        .unwrap();
    let (publish_identity_display, remaining) = remaining
        .split_once("\n  reconcile-identity-display:")
        .unwrap();
    let (reconcile_identity_display, remaining) =
        remaining.split_once("\n  publish-harness:").unwrap();
    let (publish_harness, remaining) = remaining.split_once("\n  reconcile-harness:").unwrap();
    let (reconcile_harness, remaining) = remaining.split_once("\n  publish-presentation:").unwrap();
    let (publish_presentation, remaining) =
        remaining.split_once("\n  reconcile-presentation:").unwrap();
    let (reconcile_presentation, remaining) = remaining
        .split_once("\n  publish-contradictionary:")
        .unwrap();
    let (publish_contradictionary, remaining) = remaining
        .split_once("\n  reconcile-contradictionary:")
        .unwrap();
    let (reconcile_contradictionary, remaining) =
        remaining.split_once("\n  publish-bell-rings:").unwrap();
    let (publish_bell_rings, remaining) =
        remaining.split_once("\n  reconcile-bell-rings:").unwrap();
    let (reconcile_bell_rings, remaining) = remaining
        .split_once("\n  publish-delivery-scheduler:")
        .unwrap();
    let (publish_delivery_scheduler, remaining) = remaining
        .split_once("\n  reconcile-delivery-scheduler:")
        .unwrap();
    let (reconcile_delivery_scheduler, remaining) = remaining
        .split_once("\n  publish-access-requests:")
        .unwrap();
    let (publish_access_requests, remaining) = remaining
        .split_once("\n  reconcile-access-requests:")
        .unwrap();
    let (reconcile_access_requests, remaining) =
        remaining.split_once("\n  publish-mutes:").unwrap();
    let (publish_mutes, remaining) = remaining.split_once("\n  reconcile-mutes:").unwrap();
    let (reconcile_mutes, remaining) = remaining.split_once("\n  publish-coordination:").unwrap();
    let (publish_coordination, remaining) =
        remaining.split_once("\n  reconcile-coordination:").unwrap();
    let (reconcile_coordination, remaining) =
        remaining.split_once("\n  publish-pre-send:").unwrap();
    let (publish_pre_send, remaining) = remaining.split_once("\n  reconcile-pre-send:").unwrap();
    let (reconcile_pre_send, remaining) = remaining
        .split_once("\n  publish-teams-identities:")
        .unwrap();
    let (publish_teams_identities, remaining) = remaining
        .split_once("\n  reconcile-teams-identities:")
        .unwrap();
    let (reconcile_teams_identities, remaining) = remaining
        .split_once("\n  publish-discord-identities:")
        .unwrap();
    let (publish_discord_identities, remaining) = remaining
        .split_once("\n  reconcile-discord-identities:")
        .unwrap();
    let (reconcile_discord_identities, remaining) = remaining
        .split_once("\n  publish-pluralkit-identities:")
        .unwrap();
    let (publish_pluralkit_identities, remaining) = remaining
        .split_once("\n  reconcile-pluralkit-identities:")
        .unwrap();
    let (reconcile_pluralkit_identities, remaining) =
        remaining.split_once("\n  publish-config:").unwrap();
    let (publish_config, remaining) = remaining.split_once("\n  reconcile-config:").unwrap();
    let (reconcile_config, remaining) = remaining
        .split_once("\n  publish-outbound-consent:")
        .unwrap();
    let (publish_outbound_consent, remaining) = remaining
        .split_once("\n  reconcile-outbound-consent:")
        .unwrap();
    let (reconcile_outbound_consent, remaining) =
        remaining.split_once("\n  publish-mcp-tools:").unwrap();
    let (publish_mcp_tools, remaining) = remaining.split_once("\n  reconcile-mcp-tools:").unwrap();
    let (reconcile_mcp_tools, remaining) = remaining
        .split_once("\n  publish-provider-delivery:")
        .unwrap();
    let (publish_provider_delivery, remaining) = remaining
        .split_once("\n  reconcile-provider-delivery:")
        .unwrap();
    let (reconcile_provider_delivery, remaining) =
        remaining.split_once("\n  publish-teams:").unwrap();
    let (publish_teams, remaining) = remaining.split_once("\n  reconcile-teams:").unwrap();
    let (reconcile_teams, remaining) = remaining.split_once("\n  publish-omp:").unwrap();
    let (publish_omp, remaining) = remaining.split_once("\n  reconcile-omp:").unwrap();
    let (reconcile_omp, remaining) = remaining.split_once("\n  publish-codex:").unwrap();
    let (publish_codex, remaining) = remaining.split_once("\n  reconcile-codex:").unwrap();
    let (reconcile_codex, remaining) = remaining.split_once("\n  publish-claude:").unwrap();
    let (publish_claude, remaining) = remaining.split_once("\n  reconcile-claude:").unwrap();
    let (reconcile_claude, remaining) = remaining
        .split_once("\n  publish-discord-presence:")
        .unwrap();
    let (publish_discord_presence, remaining) = remaining
        .split_once("\n  reconcile-discord-presence:")
        .unwrap();
    let (reconcile_discord_presence, remaining) =
        remaining.split_once("\n  publish-discord-state:").unwrap();
    let (publish_discord_state, remaining) = remaining
        .split_once("\n  reconcile-discord-state:")
        .unwrap();
    let (reconcile_discord_state, remaining) =
        remaining.split_once("\n  publish-discord-events:").unwrap();
    let (publish_discord_events, remaining) = remaining
        .split_once("\n  reconcile-discord-events:")
        .unwrap();
    let (reconcile_discord_events, remaining) = remaining
        .split_once("\n  publish-tracing-channel:")
        .unwrap();
    let (publish_tracing_channel, remaining) = remaining
        .split_once("\n  reconcile-tracing-channel:")
        .unwrap();
    let (reconcile_tracing_channel, remaining) =
        remaining.split_once("\n  publish-config-watcher:").unwrap();
    let (publish_config_watcher, remaining) = remaining
        .split_once("\n  reconcile-config-watcher:")
        .unwrap();
    let (reconcile_config_watcher, remaining) =
        remaining.split_once("\n  publish-drop-ledger:").unwrap();
    let (publish_drop_ledger, remaining) =
        remaining.split_once("\n  reconcile-drop-ledger:").unwrap();
    let (reconcile_drop_ledger, remaining) =
        remaining.split_once("\n  publish-permissions:").unwrap();
    let (publish_permissions, remaining) =
        remaining.split_once("\n  reconcile-permissions:").unwrap();
    let (reconcile_permissions, remaining) =
        remaining.split_once("\n  publish-discord-bells:").unwrap();
    let (publish_discord_bells, remaining) = remaining
        .split_once("\n  reconcile-discord-bells:")
        .unwrap();
    let (reconcile_discord_bells, remaining) =
        remaining.split_once("\n  publish-discord-gate:").unwrap();
    let (publish_discord_gate, remaining) =
        remaining.split_once("\n  reconcile-discord-gate:").unwrap();
    let (reconcile_discord_gate, remaining) = remaining.split_once("\n  publish-oneshot:").unwrap();
    let (publish_oneshot, remaining) = remaining.split_once("\n  reconcile-oneshot:").unwrap();
    let (reconcile_oneshot, remaining) = remaining
        .split_once("\n  publish-harness-runtime:")
        .unwrap();
    let (publish_harness_runtime, remaining) = remaining
        .split_once("\n  reconcile-harness-runtime:")
        .unwrap();
    let (reconcile_harness_runtime, remaining) = remaining
        .split_once("\n  publish-discord-scheduling:")
        .unwrap();
    let (publish_discord_scheduling, remaining) = remaining
        .split_once("\n  reconcile-discord-scheduling:")
        .unwrap();
    let (reconcile_discord_scheduling, remaining) =
        remaining.split_once("\n  publish-discord-typing:").unwrap();
    let (_, remaining) = remaining
        .split_once("\n  reconcile-discord-typing:")
        .unwrap();
    let (reconcile_discord_typing, remaining) = remaining
        .split_once("\n  publish-discord-introspection:")
        .unwrap();
    let (_, remaining) = remaining
        .split_once("\n  reconcile-discord-introspection:")
        .unwrap();
    let (reconcile_discord_introspection, remaining) = remaining
        .split_once("\n  publish-discord-queries:")
        .unwrap();
    let (_, remaining) = remaining
        .split_once("\n  reconcile-discord-queries:")
        .unwrap();
    let (reconcile_discord_queries, remaining) = remaining
        .split_once("\n  publish-attention-admission:")
        .unwrap();
    let (_, remaining) = remaining
        .split_once("\n  reconcile-attention-admission:")
        .unwrap();
    let (reconcile_attention_admission, remaining) = remaining
        .split_once("\n  publish-discord-context:")
        .unwrap();
    let (_, remaining) = remaining
        .split_once("\n  reconcile-discord-context:")
        .unwrap();
    let (reconcile_discord_context, remaining) = remaining
        .split_once("\n  publish-discord-tool-arguments:")
        .unwrap();
    let (_, remaining) = remaining
        .split_once("\n  reconcile-discord-tool-arguments:")
        .unwrap();
    let (reconcile_discord_tool_arguments, remaining) = remaining
        .split_once("\n  publish-discord-messaging-contracts:")
        .unwrap();
    let (_, remaining) = remaining
        .split_once("\n  reconcile-discord-messaging-contracts:")
        .unwrap();
    let (reconcile_discord_messaging_contracts, remaining) = remaining
        .split_once("\n  publish-discord-tool-contracts:")
        .unwrap();
    let (_, remaining) = remaining
        .split_once("\n  reconcile-discord-tool-contracts:")
        .unwrap();
    let (reconcile_discord_tool_contracts, remaining) = remaining
        .split_once("\n  publish-pluralkit-resolver:")
        .unwrap();
    let (_, remaining) = remaining
        .split_once("\n  reconcile-pluralkit-resolver:")
        .unwrap();
    let (reconcile_pluralkit_resolver, remaining) =
        remaining.split_once("\n  publish-mcp-protocol:").unwrap();
    let (_, remaining) = remaining.split_once("\n  reconcile-mcp-protocol:").unwrap();
    let (reconcile_mcp_protocol, remaining) = remaining
        .split_once("\n  publish-discord-runtime:")
        .unwrap();
    let (_, remaining) = remaining
        .split_once("\n  reconcile-discord-runtime:")
        .unwrap();
    let (reconcile_discord_runtime, remaining) =
        remaining.split_once("\n  publish-crate:").unwrap();
    let (publish_dione, reconcile_dione) = remaining.split_once("\n  reconcile-dione:").unwrap();

    assert_eq!(
        workflow.matches("scripts/package-preflight.sh").count(),
        workflow.matches("cargo package -p").count()
            + 55 * workflow.matches("cargo package --workspace").count(),
        "every workspace archive must cross the privacy verifier"
    );
    for job in [
        verify,
        reconcile_rendering,
        reconcile_durable,
        reconcile_contracts,
        reconcile_rook,
        reconcile_attention,
        reconcile_evidence,
        reconcile_messages,
        reconcile_rate_limiter,
        reconcile_vaelii,
        reconcile_gaie,
        reconcile_identity_display,
        reconcile_harness,
        reconcile_presentation,
        reconcile_contradictionary,
        reconcile_bell_rings,
        reconcile_delivery_scheduler,
        reconcile_access_requests,
        reconcile_mutes,
        reconcile_coordination,
        reconcile_pre_send,
        reconcile_teams_identities,
        reconcile_discord_identities,
        reconcile_pluralkit_identities,
        reconcile_config,
        reconcile_outbound_consent,
        reconcile_mcp_tools,
        reconcile_provider_delivery,
        reconcile_teams,
        reconcile_omp,
        reconcile_codex,
        reconcile_claude,
        reconcile_discord_presence,
        reconcile_discord_state,
        reconcile_discord_events,
        reconcile_tracing_channel,
        reconcile_config_watcher,
        reconcile_drop_ledger,
        reconcile_permissions,
        reconcile_discord_bells,
        reconcile_discord_gate,
        reconcile_oneshot,
        reconcile_harness_runtime,
        reconcile_discord_scheduling,
        reconcile_discord_typing,
        reconcile_discord_introspection,
        reconcile_discord_queries,
        reconcile_attention_admission,
        reconcile_discord_context,
        reconcile_discord_tool_arguments,
        reconcile_discord_messaging_contracts,
        reconcile_discord_tool_contracts,
        reconcile_pluralkit_resolver,
        reconcile_mcp_protocol,
        reconcile_discord_runtime,
        reconcile_dione,
    ] {
        assert!(!job.contains("CARGO_REGISTRY_TOKEN"));
        assert!(!job.contains("id-token: write"));
    }
    assert!(verify.contains("cargo package --workspace --locked"));
    assert!(workflow.contains("scripts/package-preflight.sh dione "));
    assert!(workflow.contains("scripts/package-preflight.sh dione-rendering "));
    assert!(workflow.contains("github:butterflyskies:lacuna-blinkers"));
    assert!(workflow.contains("github:butterflyskies:superadmins"));
    assert!(publish_dione.contains(
        "needs: [verify-dione, reconcile-rendering, reconcile-durable, reconcile-contracts, reconcile-rook, reconcile-attention, reconcile-evidence, reconcile-messages, reconcile-rate-limiter, reconcile-vaelii, reconcile-gaie, reconcile-identity-display, reconcile-harness, reconcile-presentation, reconcile-contradictionary, reconcile-bell-rings, reconcile-delivery-scheduler, reconcile-access-requests, reconcile-mutes, reconcile-coordination, reconcile-pre-send, reconcile-teams-identities, reconcile-discord-identities, reconcile-pluralkit-identities, reconcile-config, reconcile-outbound-consent, reconcile-mcp-tools, reconcile-provider-delivery, reconcile-teams, reconcile-omp, reconcile-codex, reconcile-claude, reconcile-discord-presence, reconcile-discord-state, reconcile-discord-events, reconcile-tracing-channel, reconcile-config-watcher, reconcile-drop-ledger, reconcile-permissions, reconcile-discord-bells, reconcile-discord-gate, reconcile-oneshot, reconcile-harness-runtime, reconcile-discord-scheduling, reconcile-discord-typing, reconcile-discord-introspection, reconcile-discord-queries, reconcile-attention-admission, reconcile-discord-context, reconcile-discord-tool-arguments, reconcile-discord-messaging-contracts, reconcile-discord-tool-contracts, reconcile-pluralkit-resolver, reconcile-mcp-protocol, reconcile-discord-runtime]"
    ));
    assert!(
        reconcile_dione.contains(
            "needs: [verify-dione, reconcile-rendering, reconcile-durable, reconcile-contracts, reconcile-rook, reconcile-attention, reconcile-evidence, reconcile-messages, reconcile-rate-limiter, reconcile-vaelii, reconcile-gaie, reconcile-identity-display, reconcile-harness, reconcile-presentation, reconcile-contradictionary, reconcile-bell-rings, reconcile-delivery-scheduler, reconcile-access-requests, reconcile-mutes, reconcile-coordination, reconcile-pre-send, reconcile-teams-identities, reconcile-discord-identities, reconcile-pluralkit-identities, reconcile-config, reconcile-outbound-consent, reconcile-mcp-tools, reconcile-provider-delivery, reconcile-teams, reconcile-omp, reconcile-codex, reconcile-claude, reconcile-discord-presence, reconcile-discord-state, reconcile-discord-events, reconcile-tracing-channel, reconcile-config-watcher, reconcile-drop-ledger, reconcile-permissions, reconcile-discord-bells, reconcile-discord-gate, reconcile-oneshot, reconcile-harness-runtime, reconcile-discord-scheduling, reconcile-discord-typing, reconcile-discord-introspection, reconcile-discord-queries, reconcile-attention-admission, reconcile-discord-context, reconcile-discord-tool-arguments, reconcile-discord-messaging-contracts, reconcile-discord-tool-contracts, reconcile-pluralkit-resolver, reconcile-mcp-protocol, publish-crate, reconcile-discord-runtime]"
        )
    );
    assert!(reconcile_dione.contains("needs.reconcile-rendering.result == 'success'"));
    assert!(reconcile_rendering.contains("needs: [verify-dione, publish-rendering]"));
    assert!(reconcile_rendering.contains("--ignore-vcs-info"));
    assert!(reconcile_durable.contains("needs: [verify-dione, publish-durable]"));
    assert!(reconcile_durable.contains("--ignore-vcs-info"));
    assert!(reconcile_contracts.contains("needs: [verify-dione, publish-contracts]"));
    assert!(reconcile_contracts.contains("--ignore-vcs-info"));
    assert!(publish_rook.contains("needs: [verify-dione, reconcile-durable, reconcile-contracts]"));
    assert!(reconcile_rook.contains("needs: [verify-dione, publish-rook]"));
    assert!(reconcile_rook.contains("--ignore-vcs-info"));
    assert!(publish_attention.contains("needs: [verify-dione, reconcile-contracts]"));
    assert!(reconcile_attention.contains("needs: [verify-dione, publish-attention]"));
    assert!(reconcile_attention.contains("--ignore-vcs-info"));
    assert!(publish_evidence.contains("needs: verify-dione"));
    assert!(reconcile_evidence.contains("needs: [verify-dione, publish-evidence]"));
    assert!(reconcile_evidence.contains("--ignore-vcs-info"));
    assert!(publish_messages.contains(
        "needs: [verify-dione, reconcile-contracts, reconcile-attention, reconcile-evidence]"
    ));
    assert!(reconcile_messages.contains("needs: [verify-dione, publish-messages]"));
    assert!(reconcile_messages.contains("--ignore-vcs-info"));
    assert!(publish_rate_limiter.contains("needs: verify-dione"));
    assert!(reconcile_rate_limiter.contains("needs: [verify-dione, publish-rate-limiter]"));
    assert!(reconcile_rate_limiter.contains("--ignore-vcs-info"));
    assert!(publish_vaelii.contains("needs: verify-dione"));
    assert!(reconcile_vaelii.contains("needs: [verify-dione, publish-vaelii]"));
    assert!(reconcile_vaelii.contains("--ignore-vcs-info"));
    assert!(publish_gaie.contains("needs: verify-dione"));
    assert!(reconcile_gaie.contains("needs: [verify-dione, publish-gaie]"));
    assert!(reconcile_gaie.contains("--ignore-vcs-info"));
    assert!(publish_identity_display.contains("needs: verify-dione"));
    assert!(reconcile_identity_display.contains("needs: [verify-dione, publish-identity-display]"));
    assert!(reconcile_identity_display.contains("--ignore-vcs-info"));
    assert!(
        publish_harness.contains("needs: [verify-dione, reconcile-contracts, reconcile-messages]")
    );
    assert!(reconcile_harness.contains("needs: [verify-dione, publish-harness]"));
    assert!(reconcile_harness.contains("--ignore-vcs-info"));
    assert!(publish_presentation.contains("needs: [verify-dione, reconcile-contracts, reconcile-attention, reconcile-evidence, reconcile-messages, reconcile-identity-display]"));
    assert!(reconcile_presentation.contains("needs: [verify-dione, publish-presentation]"));
    assert!(reconcile_presentation.contains("--ignore-vcs-info"));
    assert!(publish_contradictionary.contains("needs: [verify-dione, reconcile-evidence]"));
    assert!(reconcile_contradictionary.contains("needs: [verify-dione, publish-contradictionary]"));
    assert!(reconcile_contradictionary.contains("--ignore-vcs-info"));
    assert!(reconcile_dione.contains("needs.reconcile-contradictionary.result == 'success'"));
    assert!(workflow.contains("scripts/package-preflight.sh dione-contradictionary "));
    assert!(publish_bell_rings.contains("needs: verify-dione"));
    assert!(reconcile_bell_rings.contains("needs: [verify-dione, publish-bell-rings]"));
    assert!(reconcile_bell_rings.contains("--ignore-vcs-info"));
    assert!(reconcile_dione.contains("needs.reconcile-bell-rings.result == 'success'"));
    assert!(workflow.contains("scripts/package-preflight.sh dione-bell-rings "));
    assert!(publish_delivery_scheduler.contains("needs: [verify-dione, reconcile-contracts]"));
    assert!(
        reconcile_delivery_scheduler
            .contains("needs: [verify-dione, reconcile-contracts, publish-delivery-scheduler]")
    );
    assert!(reconcile_delivery_scheduler.contains("--ignore-vcs-info"));
    assert!(reconcile_dione.contains("needs.reconcile-delivery-scheduler.result == 'success'"));
    assert!(workflow.contains("scripts/package-preflight.sh dione-delivery-scheduler "));
    assert!(
        publish_access_requests
            .contains("needs: [verify-dione, reconcile-config, reconcile-messages]")
    );
    assert!(reconcile_access_requests.contains(
        "needs: [verify-dione, reconcile-config, reconcile-messages, publish-access-requests]"
    ));
    for dependency in ["config", "messages"] {
        assert!(
            reconcile_access_requests
                .contains(&format!("needs.reconcile-{dependency}.result == 'success'"))
        );
    }
    assert!(
        publish_access_requests
            .contains("    permissions:\n      contents: read\n      id-token: write\n")
    );
    assert!(reconcile_access_requests.contains("    permissions:\n      contents: read\n"));
    assert!(reconcile_access_requests.contains("--ignore-vcs-info"));
    assert!(reconcile_dione.contains("needs.reconcile-access-requests.result == 'success'"));
    assert!(workflow.contains("scripts/package-preflight.sh dione-access-requests "));
    assert!(publish_mutes.contains("needs: [verify-dione, reconcile-durable]"));
    assert!(reconcile_mutes.contains("needs: [verify-dione, reconcile-durable, publish-mutes]"));
    assert!(reconcile_mutes.contains("needs.reconcile-durable.result == 'success'"));
    assert!(reconcile_mutes.contains("--ignore-vcs-info"));
    assert!(reconcile_dione.contains("needs.reconcile-mutes.result == 'success'"));
    assert!(workflow.contains("scripts/package-preflight.sh dione-mutes "));
    assert!(publish_coordination.contains("needs: verify-dione"));
    assert!(reconcile_coordination.contains("needs: [verify-dione, publish-coordination]"));
    assert!(reconcile_coordination.contains("--ignore-vcs-info"));
    assert!(reconcile_dione.contains("needs.reconcile-coordination.result == 'success'"));
    assert!(workflow.contains("scripts/package-preflight.sh dione-coordination "));
    assert!(publish_teams_identities.contains("needs: verify-dione"));
    assert!(reconcile_teams_identities.contains("needs: [verify-dione, publish-teams-identities]"));
    assert!(reconcile_teams_identities.contains("--ignore-vcs-info"));
    assert!(reconcile_dione.contains("needs.reconcile-teams-identities.result == 'success'"));
    assert!(workflow.contains("scripts/package-preflight.sh dione-teams-identities "));
    assert!(publish_discord_identities.contains("needs: verify-dione"));
    assert!(
        reconcile_discord_identities.contains("needs: [verify-dione, publish-discord-identities]")
    );
    assert!(reconcile_discord_identities.contains("--ignore-vcs-info"));
    assert!(reconcile_dione.contains("needs.reconcile-discord-identities.result == 'success'"));
    assert!(workflow.contains("scripts/package-preflight.sh dione-discord-identities "));
    let capabilities = [
        "attention",
        "bell-rings",
        "contradictionary",
        "coordination",
        "discord-identities",
        "gaie",
        "identity-display",
        "messages",
        "pluralkit-identities",
        "pre-send",
        "rate-limiter",
        "teams-identities",
        "vaelii",
    ];
    for (job, publish) in [(publish_config, false), (reconcile_config, true)] {
        let needs = job
            .lines()
            .find(|line| line.trim_start().starts_with("needs:"))
            .unwrap();
        let actual: std::collections::BTreeSet<_> = needs
            .trim()
            .strip_prefix("needs: [")
            .unwrap()
            .strip_suffix(']')
            .unwrap()
            .split(", ")
            .collect();
        let mut expected: std::collections::BTreeSet<String> = capabilities
            .iter()
            .map(|name| format!("reconcile-{name}"))
            .collect();
        expected.insert("verify-dione".into());
        if publish {
            expected.insert("publish-config".into());
        }
        assert_eq!(actual, expected.iter().map(String::as_str).collect());
    }
    for name in capabilities {
        assert!(reconcile_config.contains(&format!("needs.reconcile-{name}.result == 'success'")));
    }
    assert!(reconcile_config.contains("always()"));
    assert!(reconcile_config.contains("needs.verify-dione.result == 'success'"));
    assert!(reconcile_config.contains(
        "needs.publish-config.result == 'success' || needs.publish-config.result == 'skipped'"
    ));
    assert!(reconcile_dione.contains("needs.reconcile-config.result == 'success'"));
    assert!(workflow.contains("scripts/package-preflight.sh dione-config "));
    let capabilities = [
        "durable",
        "config",
        "evidence",
        "contradictionary",
        "messages",
    ];
    for (job, publish) in [
        (publish_outbound_consent, false),
        (reconcile_outbound_consent, true),
    ] {
        let needs = job
            .lines()
            .find(|line| line.trim_start().starts_with("needs:"))
            .unwrap();
        let actual: std::collections::BTreeSet<_> = needs
            .trim()
            .strip_prefix("needs: [")
            .unwrap()
            .strip_suffix(']')
            .unwrap()
            .split(", ")
            .collect();
        let mut expected: std::collections::BTreeSet<String> = capabilities
            .iter()
            .map(|name| format!("reconcile-{name}"))
            .collect();
        expected.insert("verify-dione".into());
        if publish {
            expected.insert("publish-outbound-consent".into());
        }
        assert_eq!(actual, expected.iter().map(String::as_str).collect());
    }
    for name in capabilities {
        assert!(
            reconcile_outbound_consent
                .contains(&format!("needs.reconcile-{name}.result == 'success'"))
        );
    }
    assert!(reconcile_outbound_consent.contains("always()"));
    assert!(reconcile_outbound_consent.contains("needs.verify-dione.result == 'success'"));
    assert!(reconcile_outbound_consent.contains(
        "needs.publish-outbound-consent.result == 'success' || needs.publish-outbound-consent.result == 'skipped'"
    ));
    assert!(reconcile_dione.contains("needs.reconcile-outbound-consent.result == 'success'"));
    assert!(workflow.contains("scripts/package-preflight.sh dione-outbound-consent "));
    let capabilities = ["messages", "teams-identities"];
    for (job, publish) in [
        (publish_provider_delivery, false),
        (reconcile_provider_delivery, true),
    ] {
        let needs = job
            .lines()
            .find(|line| line.trim_start().starts_with("needs:"))
            .unwrap();
        let actual: std::collections::BTreeSet<_> = needs
            .trim()
            .strip_prefix("needs: [")
            .unwrap()
            .strip_suffix(']')
            .unwrap()
            .split(", ")
            .collect();
        let mut expected: std::collections::BTreeSet<String> = capabilities
            .iter()
            .map(|name| format!("reconcile-{name}"))
            .collect();
        expected.insert("verify-dione".into());
        if publish {
            expected.insert("publish-provider-delivery".into());
        }
        assert_eq!(actual, expected.iter().map(String::as_str).collect());
    }
    for name in capabilities {
        assert!(
            reconcile_provider_delivery
                .contains(&format!("needs.reconcile-{name}.result == 'success'"))
        );
    }
    assert!(reconcile_provider_delivery.contains("always()"));
    assert!(reconcile_provider_delivery.contains("needs.verify-dione.result == 'success'"));
    assert!(reconcile_provider_delivery.contains(
        "needs.publish-provider-delivery.result == 'success' || needs.publish-provider-delivery.result == 'skipped'"
    ));
    assert!(reconcile_dione.contains("needs.reconcile-provider-delivery.result == 'success'"));
    assert!(workflow.contains("scripts/package-preflight.sh dione-provider-delivery "));
    let capabilities = [
        "messages",
        "teams-identities",
        "provider-delivery",
        "mcp-tools",
    ];
    for (job, publish) in [(publish_teams, false), (reconcile_teams, true)] {
        let needs = job
            .lines()
            .find(|line| line.trim_start().starts_with("needs:"))
            .unwrap();
        let actual: std::collections::BTreeSet<_> = needs
            .trim()
            .strip_prefix("needs: [")
            .unwrap()
            .strip_suffix(']')
            .unwrap()
            .split(", ")
            .collect();
        let mut expected: std::collections::BTreeSet<String> = capabilities
            .iter()
            .map(|name| format!("reconcile-{name}"))
            .collect();
        expected.insert("verify-dione".into());
        if publish {
            expected.insert("publish-teams".into());
        }
        assert_eq!(actual, expected.iter().map(String::as_str).collect());
    }
    for name in capabilities {
        assert!(reconcile_teams.contains(&format!("needs.reconcile-{name}.result == 'success'")));
    }
    assert!(reconcile_teams.contains("always()"));
    assert!(reconcile_teams.contains("needs.verify-dione.result == 'success'"));
    assert!(reconcile_teams.contains(
        "needs.publish-teams.result == 'success' || needs.publish-teams.result == 'skipped'"
    ));
    assert!(reconcile_dione.contains("needs.reconcile-teams.result == 'success'"));
    assert!(workflow.contains("scripts/package-preflight.sh dione-teams "));
    let capabilities = [
        "durable",
        "harness",
        "messages",
        "provider-delivery",
        "teams-identities",
        "contracts",
        "presentation",
    ];
    for (job, publish) in [(publish_omp, false), (reconcile_omp, true)] {
        let needs = job
            .lines()
            .find_map(|line| line.trim().strip_prefix("needs: ["))
            .unwrap()
            .trim_end_matches(']');
        let actual: std::collections::BTreeSet<_> = needs.split(", ").map(str::to_owned).collect();
        let mut expected: std::collections::BTreeSet<_> = capabilities
            .iter()
            .map(|name| format!("reconcile-{name}"))
            .collect();
        expected.insert("verify-dione".into());
        if publish {
            expected.insert("publish-omp".into());
        }
        assert_eq!(actual, expected);
    }
    for name in capabilities {
        assert!(reconcile_omp.contains(&format!("needs.reconcile-{name}.result == 'success'")));
    }
    assert!(reconcile_omp.contains("always()"));
    assert!(reconcile_omp.contains("needs.verify-dione.result == 'success'"));
    assert!(reconcile_omp.contains(
        "needs.publish-omp.result == 'success' || needs.publish-omp.result == 'skipped'"
    ));
    assert!(reconcile_dione.contains("needs.reconcile-omp.result == 'success'"));
    assert!(workflow.contains("scripts/package-preflight.sh dione-omp "));

    let capabilities = [
        "attention",
        "config",
        "contracts",
        "discord-identities",
        "durable",
        "harness",
        "messages",
        "presentation",
        "provider-delivery",
    ];
    for (job, publish) in [(publish_codex, false), (reconcile_codex, true)] {
        let needs = job
            .lines()
            .find_map(|line| line.trim().strip_prefix("needs: ["))
            .unwrap()
            .trim_end_matches(']');
        let actual: std::collections::BTreeSet<_> = needs.split(", ").map(str::to_owned).collect();
        let mut expected: std::collections::BTreeSet<_> = capabilities
            .iter()
            .map(|name| format!("reconcile-{name}"))
            .collect();
        expected.insert("verify-dione".into());
        if publish {
            expected.insert("publish-codex".into());
        }
        assert_eq!(actual, expected);
    }
    for name in capabilities {
        assert!(reconcile_codex.contains(&format!("needs.reconcile-{name}.result == 'success'")));
    }
    assert!(reconcile_codex.contains("always()"));
    assert!(reconcile_codex.contains("needs.verify-dione.result == 'success'"));
    assert!(reconcile_codex.contains(
        "needs.publish-codex.result == 'success' || needs.publish-codex.result == 'skipped'"
    ));
    assert!(reconcile_dione.contains("needs.reconcile-codex.result == 'success'"));
    assert!(workflow.contains("scripts/package-preflight.sh dione-codex "));
    assert!(workflow.contains("scripts/workspace-package-version.sh dione-codex"));
    assert!(verify.contains("echo \"codex-version=${codex_version}\""));
    assert!(workflow.contains("codex-version: ${{ steps.package.outputs.codex-version }}"));
    assert!(workflow.contains("publish-codex: ${{ steps.package.outputs.publish-codex }}"));
    assert!(reconcile_codex.contains("--ignore-vcs-info"));

    let capabilities = ["contracts", "harness", "messages", "presentation"];
    for (job, publish) in [(publish_claude, false), (reconcile_claude, true)] {
        let needs = job
            .lines()
            .find_map(|line| line.trim().strip_prefix("needs: ["))
            .unwrap()
            .trim_end_matches(']');
        let actual: std::collections::BTreeSet<_> = needs.split(", ").map(str::to_owned).collect();
        let mut expected: std::collections::BTreeSet<_> = capabilities
            .iter()
            .map(|name| format!("reconcile-{name}"))
            .collect();
        expected.insert("verify-dione".into());
        if publish {
            expected.insert("publish-claude".into());
        }
        assert_eq!(actual, expected);
    }
    for name in capabilities {
        assert!(reconcile_claude.contains(&format!("needs.reconcile-{name}.result == 'success'")));
    }
    assert!(reconcile_claude.contains("always()"));
    assert!(reconcile_claude.contains("needs.verify-dione.result == 'success'"));
    assert!(reconcile_claude.contains(
        "needs.publish-claude.result == 'success' || needs.publish-claude.result == 'skipped'"
    ));
    assert!(reconcile_dione.contains("needs.reconcile-claude.result == 'success'"));
    assert!(workflow.contains("scripts/package-preflight.sh dione-claude "));
    assert!(workflow.contains("scripts/workspace-package-version.sh dione-claude"));
    assert!(verify.contains("echo \"claude-version=${claude_version}\""));
    assert!(workflow.contains("claude-version: ${{ steps.package.outputs.claude-version }}"));
    assert!(workflow.contains("publish-claude: ${{ steps.package.outputs.publish-claude }}"));
    assert!(reconcile_claude.contains("--ignore-vcs-info"));

    assert!(publish_mcp_tools.contains("needs: [verify-dione, reconcile-evidence]"));
    assert!(publish_mcp_tools.contains("needs.reconcile-evidence.result == 'success'"));
    assert!(
        reconcile_mcp_tools
            .contains("needs: [verify-dione, reconcile-evidence, publish-mcp-tools]")
    );
    assert!(reconcile_mcp_tools.contains("needs.reconcile-evidence.result == 'success'"));
    assert!(reconcile_mcp_tools.contains("always()"));
    assert!(reconcile_mcp_tools.contains("needs.verify-dione.result == 'success'"));
    assert!(reconcile_mcp_tools.contains(
        "needs.publish-mcp-tools.result == 'success' || needs.publish-mcp-tools.result == 'skipped'"
    ));
    assert!(reconcile_mcp_tools.contains("--ignore-vcs-info"));
    assert!(reconcile_dione.contains("needs.reconcile-mcp-tools.result == 'success'"));
    assert!(workflow.contains("scripts/package-preflight.sh dione-mcp-tools "));
    assert_eq!(
        publish_discord_presence
            .lines()
            .find(|line| line.trim_start().starts_with("needs:"))
            .unwrap()
            .trim(),
        "needs: verify-dione"
    );
    assert_eq!(
        reconcile_discord_presence
            .lines()
            .find(|line| line.trim_start().starts_with("needs:"))
            .unwrap()
            .trim(),
        "needs: [verify-dione, publish-discord-presence]"
    );
    assert!(workflow.contains("scripts/workspace-package-version.sh dione-discord-presence"));
    assert!(verify.contains("echo \"discord-presence-version=${discord_presence_version}\""));
    assert!(workflow.contains(
        "discord-presence-version: ${{ steps.package.outputs.discord-presence-version }}"
    ));
    assert!(workflow.contains(
        "publish-discord-presence: ${{ steps.package.outputs.publish-discord-presence }}"
    ));
    assert!(reconcile_discord_presence.contains("always()"));
    assert!(reconcile_discord_presence.contains("needs.verify-dione.result == 'success'"));
    assert!(reconcile_discord_presence.contains(
        "needs.publish-discord-presence.result == 'success' || needs.publish-discord-presence.result == 'skipped'"
    ));
    assert!(reconcile_discord_presence.contains("--ignore-vcs-info"));
    assert!(reconcile_dione.contains("needs.reconcile-discord-presence.result == 'success'"));
    assert!(workflow.contains("scripts/package-preflight.sh dione-discord-presence "));
    assert_eq!(
        publish_discord_state
            .lines()
            .find(|line| line.trim_start().starts_with("needs:"))
            .unwrap()
            .trim(),
        "needs: verify-dione"
    );
    assert_eq!(
        reconcile_discord_state
            .lines()
            .find(|line| line.trim_start().starts_with("needs:"))
            .unwrap()
            .trim(),
        "needs: [verify-dione, publish-discord-state]"
    );
    assert!(workflow.contains("scripts/workspace-package-version.sh dione-discord-state"));
    assert!(verify.contains("echo \"discord-state-version=${discord_state_version}\""));
    assert!(
        workflow
            .contains("discord-state-version: ${{ steps.package.outputs.discord-state-version }}")
    );
    assert!(
        workflow
            .contains("publish-discord-state: ${{ steps.package.outputs.publish-discord-state }}")
    );
    assert!(reconcile_discord_state.contains("always()"));
    assert!(reconcile_discord_state.contains("needs.verify-dione.result == 'success'"));
    assert!(reconcile_discord_state.contains(
        "needs.publish-discord-state.result == 'success' || needs.publish-discord-state.result == 'skipped'"
    ));
    assert!(reconcile_discord_state.contains("--ignore-vcs-info"));
    assert!(reconcile_dione.contains("needs.reconcile-discord-state.result == 'success'"));
    assert!(workflow.contains("scripts/package-preflight.sh dione-discord-state "));
    assert_eq!(
        publish_discord_events
            .lines()
            .find(|line| line.trim_start().starts_with("needs:"))
            .unwrap()
            .trim(),
        "needs: [verify-dione, reconcile-attention, reconcile-bell-rings, reconcile-messages, reconcile-coordination, reconcile-evidence, reconcile-contracts]"
    );
    assert_eq!(
        reconcile_discord_events
            .lines()
            .find(|line| line.trim_start().starts_with("needs:"))
            .unwrap()
            .trim(),
        "needs: [verify-dione, reconcile-attention, reconcile-bell-rings, reconcile-messages, reconcile-coordination, reconcile-evidence, reconcile-contracts, publish-discord-events]"
    );
    assert!(workflow.contains("scripts/workspace-package-version.sh dione-discord-events"));
    assert!(verify.contains("echo \"discord-events-version=${discord_events_version}\""));
    assert!(
        workflow.contains(
            "discord-events-version: ${{ steps.package.outputs.discord-events-version }}"
        )
    );
    assert!(
        workflow.contains(
            "publish-discord-events: ${{ steps.package.outputs.publish-discord-events }}"
        )
    );
    assert!(reconcile_discord_events.contains("always()"));
    assert!(reconcile_discord_events.contains("needs.verify-dione.result == 'success'"));
    assert!(reconcile_discord_events.contains(
        "needs.publish-discord-events.result == 'success' || needs.publish-discord-events.result == 'skipped'"
    ));
    assert!(reconcile_discord_events.contains("--ignore-vcs-info"));
    assert!(reconcile_dione.contains("needs.reconcile-discord-events.result == 'success'"));
    assert!(workflow.contains("scripts/package-preflight.sh dione-discord-events "));
    for dependency in [
        "attention",
        "bell-rings",
        "messages",
        "coordination",
        "evidence",
        "contracts",
    ] {
        assert!(
            reconcile_discord_events
                .contains(&format!("needs.reconcile-{dependency}.result == 'success'"))
        );
    }
    assert_eq!(
        publish_tracing_channel
            .lines()
            .find(|line| line.trim_start().starts_with("needs:"))
            .unwrap()
            .trim(),
        "needs: [verify-dione, reconcile-discord-events]"
    );
    assert_eq!(
        reconcile_tracing_channel
            .lines()
            .find(|line| line.trim_start().starts_with("needs:"))
            .unwrap()
            .trim(),
        "needs: [verify-dione, reconcile-discord-events, publish-tracing-channel]"
    );
    assert!(workflow.contains("scripts/workspace-package-version.sh dione-tracing-channel"));
    assert!(verify.contains("echo \"tracing-channel-version=${tracing_channel_version}\""));
    assert!(
        workflow.contains(
            "tracing-channel-version: ${{ steps.package.outputs.tracing-channel-version }}"
        )
    );
    assert!(
        workflow.contains(
            "publish-tracing-channel: ${{ steps.package.outputs.publish-tracing-channel }}"
        )
    );
    assert!(reconcile_tracing_channel.contains("always()"));
    assert!(reconcile_tracing_channel.contains("needs.verify-dione.result == 'success'"));
    assert!(reconcile_tracing_channel.contains(
        "needs.publish-tracing-channel.result == 'success' || needs.publish-tracing-channel.result == 'skipped'"
    ));
    assert!(reconcile_tracing_channel.contains("--ignore-vcs-info"));
    assert!(reconcile_dione.contains("needs.reconcile-tracing-channel.result == 'success'"));
    assert!(workflow.contains("scripts/package-preflight.sh dione-tracing-channel "));
    assert!(
        reconcile_tracing_channel.contains("needs.reconcile-discord-events.result == 'success'")
    );
    assert_eq!(
        publish_config_watcher
            .lines()
            .find(|line| line.trim_start().starts_with("needs:"))
            .unwrap()
            .trim(),
        "needs: [verify-dione, reconcile-config, reconcile-discord-events]"
    );
    assert_eq!(
        reconcile_config_watcher
            .lines()
            .find(|line| line.trim_start().starts_with("needs:"))
            .unwrap()
            .trim(),
        "needs: [verify-dione, reconcile-config, reconcile-discord-events, publish-config-watcher]"
    );
    assert!(workflow.contains("scripts/workspace-package-version.sh dione-config-watcher"));
    assert!(verify.contains("echo \"config-watcher-version=${config_watcher_version}\""));
    assert!(
        workflow.contains(
            "config-watcher-version: ${{ steps.package.outputs.config-watcher-version }}"
        )
    );
    assert!(
        workflow.contains(
            "publish-config-watcher: ${{ steps.package.outputs.publish-config-watcher }}"
        )
    );
    assert!(reconcile_config_watcher.contains("always()"));
    assert!(reconcile_config_watcher.contains("needs.verify-dione.result == 'success'"));
    assert!(reconcile_config_watcher.contains(
        "needs.publish-config-watcher.result == 'success' || needs.publish-config-watcher.result == 'skipped'"
    ));
    assert!(reconcile_config_watcher.contains("--ignore-vcs-info"));
    assert!(reconcile_dione.contains("needs.reconcile-config-watcher.result == 'success'"));
    assert!(workflow.contains("scripts/package-preflight.sh dione-config-watcher "));
    assert!(
        reconcile_config_watcher.contains("needs.reconcile-discord-events.result == 'success'")
    );
    assert!(reconcile_config_watcher.contains("needs.reconcile-config.result == 'success'"));
    assert_eq!(
        publish_permissions
            .lines()
            .find(|line| line.trim_start().starts_with("needs:"))
            .unwrap()
            .trim(),
        "needs: [verify-dione, reconcile-config, reconcile-discord-state, reconcile-discord-events]"
    );
    assert_eq!(
        reconcile_permissions
            .lines()
            .find(|line| line.trim_start().starts_with("needs:"))
            .unwrap()
            .trim(),
        "needs: [verify-dione, reconcile-config, reconcile-discord-state, reconcile-discord-events, publish-permissions]"
    );
    assert!(workflow.contains("scripts/workspace-package-version.sh dione-permissions"));
    assert!(verify.contains("echo \"permissions-version=${permissions_version}\""));
    assert!(
        workflow.contains("permissions-version: ${{ steps.package.outputs.permissions-version }}")
    );
    assert!(
        workflow.contains("publish-permissions: ${{ steps.package.outputs.publish-permissions }}")
    );
    assert!(reconcile_permissions.contains("always()"));
    assert!(reconcile_permissions.contains("needs.verify-dione.result == 'success'"));
    assert!(reconcile_permissions.contains(
        "needs.publish-permissions.result == 'success' || needs.publish-permissions.result == 'skipped'"
    ));
    assert!(reconcile_permissions.contains("--ignore-vcs-info"));
    assert!(reconcile_dione.contains("needs.reconcile-permissions.result == 'success'"));
    assert!(workflow.contains("scripts/package-preflight.sh dione-permissions "));
    assert!(reconcile_permissions.contains("needs.reconcile-discord-events.result == 'success'"));
    assert!(reconcile_permissions.contains("needs.reconcile-config.result == 'success'"));
    assert!(reconcile_permissions.contains("needs.reconcile-discord-state.result == 'success'"));
    assert_eq!(
        publish_discord_bells
            .lines()
            .find(|line| line.trim_start().starts_with("needs:"))
            .unwrap()
            .trim(),
        "needs: [verify-dione, reconcile-bell-rings, reconcile-config, reconcile-discord-events]"
    );
    assert_eq!(
        reconcile_discord_bells
            .lines()
            .find(|line| line.trim_start().starts_with("needs:"))
            .unwrap()
            .trim(),
        "needs: [verify-dione, reconcile-bell-rings, reconcile-config, reconcile-discord-events, publish-discord-bells]"
    );
    assert!(workflow.contains("scripts/workspace-package-version.sh dione-discord-bells"));
    assert!(verify.contains("echo \"discord-bells-version=${discord_bells_version}\""));
    assert!(
        workflow
            .contains("discord-bells-version: ${{ steps.package.outputs.discord-bells-version }}")
    );
    assert!(
        workflow
            .contains("publish-discord-bells: ${{ steps.package.outputs.publish-discord-bells }}")
    );
    assert!(reconcile_discord_bells.contains("always()"));
    assert!(reconcile_discord_bells.contains("needs.verify-dione.result == 'success'"));
    assert!(reconcile_discord_bells.contains(
        "needs.publish-discord-bells.result == 'success' || needs.publish-discord-bells.result == 'skipped'"
    ));
    assert!(reconcile_discord_bells.contains("--ignore-vcs-info"));
    assert!(reconcile_dione.contains("needs.reconcile-discord-bells.result == 'success'"));
    assert!(workflow.contains("scripts/package-preflight.sh dione-discord-bells "));
    assert!(reconcile_discord_bells.contains("needs.reconcile-discord-events.result == 'success'"));
    assert!(reconcile_discord_bells.contains("needs.reconcile-config.result == 'success'"));
    assert!(reconcile_discord_bells.contains("needs.reconcile-bell-rings.result == 'success'"));
    assert_eq!(
        publish_discord_gate
            .lines()
            .find(|line| line.trim_start().starts_with("needs:"))
            .unwrap()
            .trim(),
        "needs: [verify-dione, reconcile-mutes, reconcile-config, reconcile-discord-events]"
    );
    assert_eq!(
        reconcile_discord_gate
            .lines()
            .find(|line| line.trim_start().starts_with("needs:"))
            .unwrap()
            .trim(),
        "needs: [verify-dione, reconcile-mutes, reconcile-config, reconcile-discord-events, publish-discord-gate]"
    );
    assert!(workflow.contains("scripts/workspace-package-version.sh dione-discord-gate"));
    assert!(verify.contains("echo \"discord-gate-version=${discord_gate_version}\""));
    assert!(
        workflow
            .contains("discord-gate-version: ${{ steps.package.outputs.discord-gate-version }}")
    );
    assert!(
        workflow
            .contains("publish-discord-gate: ${{ steps.package.outputs.publish-discord-gate }}")
    );
    assert!(reconcile_discord_gate.contains("always()"));
    assert!(reconcile_discord_gate.contains("needs.verify-dione.result == 'success'"));
    assert!(reconcile_discord_gate.contains(
        "needs.publish-discord-gate.result == 'success' || needs.publish-discord-gate.result == 'skipped'"
    ));
    assert!(reconcile_discord_gate.contains("--ignore-vcs-info"));
    assert!(reconcile_dione.contains("needs.reconcile-discord-gate.result == 'success'"));
    assert!(workflow.contains("scripts/package-preflight.sh dione-discord-gate "));
    assert!(reconcile_discord_gate.contains("needs.reconcile-discord-events.result == 'success'"));
    assert!(reconcile_discord_gate.contains("needs.reconcile-config.result == 'success'"));
    assert!(reconcile_discord_gate.contains("needs.reconcile-mutes.result == 'success'"));
    assert!(
        publish_discord_gate
            .contains("    permissions:\n      contents: read\n      id-token: write\n")
    );
    assert!(reconcile_discord_gate.contains("    permissions:\n      contents: read\n"));
    assert_eq!(
        publish_oneshot
            .lines()
            .find(|line| line.trim_start().starts_with("needs:"))
            .unwrap()
            .trim(),
        "needs: [verify-dione, reconcile-config, reconcile-discord-gate, reconcile-outbound-consent, reconcile-pre-send]"
    );
    assert_eq!(
        reconcile_oneshot
            .lines()
            .find(|line| line.trim_start().starts_with("needs:"))
            .unwrap()
            .trim(),
        "needs: [verify-dione, reconcile-config, reconcile-discord-gate, reconcile-outbound-consent, reconcile-pre-send, publish-oneshot]"
    );
    assert!(workflow.contains("scripts/workspace-package-version.sh dione-oneshot"));
    assert!(verify.contains("echo \"oneshot-version=${oneshot_version}\""));
    assert!(workflow.contains("oneshot-version: ${{ steps.package.outputs.oneshot-version }}"));
    assert!(workflow.contains("publish-oneshot: ${{ steps.package.outputs.publish-oneshot }}"));
    assert!(reconcile_oneshot.contains("always()"));
    assert!(reconcile_oneshot.contains("needs.verify-dione.result == 'success'"));
    assert!(reconcile_oneshot.contains(
        "needs.publish-oneshot.result == 'success' || needs.publish-oneshot.result == 'skipped'"
    ));
    assert!(reconcile_oneshot.contains("--ignore-vcs-info"));
    assert!(reconcile_dione.contains("needs.reconcile-oneshot.result == 'success'"));
    assert!(workflow.contains("scripts/package-preflight.sh dione-oneshot "));
    assert!(reconcile_oneshot.contains("needs.reconcile-outbound-consent.result == 'success'"));
    assert!(reconcile_oneshot.contains("needs.reconcile-config.result == 'success'"));
    assert!(reconcile_oneshot.contains("needs.reconcile-pre-send.result == 'success'"));
    assert!(
        publish_oneshot.contains("    permissions:\n      contents: read\n      id-token: write\n")
    );
    assert!(reconcile_oneshot.contains("    permissions:\n      contents: read\n"));
    assert!(reconcile_oneshot.contains("needs.reconcile-discord-gate.result == 'success'"));
    assert_eq!(
        publish_harness_runtime
            .lines()
            .find(|line| line.trim_start().starts_with("needs:"))
            .unwrap()
            .trim(),
        "needs: [verify-dione, reconcile-harness, reconcile-config, reconcile-attention, reconcile-codex, reconcile-omp, reconcile-claude]"
    );
    assert_eq!(
        reconcile_harness_runtime
            .lines()
            .find(|line| line.trim_start().starts_with("needs:"))
            .unwrap()
            .trim(),
        "needs: [verify-dione, reconcile-harness, reconcile-config, reconcile-attention, reconcile-codex, reconcile-omp, reconcile-claude, publish-harness-runtime]"
    );
    assert!(workflow.contains("scripts/workspace-package-version.sh dione-harness-runtime"));
    assert!(verify.contains("echo \"harness-runtime-version=${harness_runtime_version}\""));
    assert!(
        workflow.contains(
            "harness-runtime-version: ${{ steps.package.outputs.harness-runtime-version }}"
        )
    );
    assert!(
        workflow.contains(
            "publish-harness-runtime: ${{ steps.package.outputs.publish-harness-runtime }}"
        )
    );
    assert!(reconcile_harness_runtime.contains("always()"));
    assert!(reconcile_harness_runtime.contains("needs.verify-dione.result == 'success'"));
    assert!(reconcile_harness_runtime.contains(
        "needs.publish-harness-runtime.result == 'success' || needs.publish-harness-runtime.result == 'skipped'"
    ));
    assert!(reconcile_harness_runtime.contains("--ignore-vcs-info"));
    assert!(reconcile_dione.contains("needs.reconcile-harness-runtime.result == 'success'"));
    assert!(workflow.contains("scripts/package-preflight.sh dione-harness-runtime "));
    assert!(reconcile_harness_runtime.contains("needs.reconcile-attention.result == 'success'"));
    assert!(reconcile_harness_runtime.contains("needs.reconcile-config.result == 'success'"));
    assert!(reconcile_harness_runtime.contains("needs.reconcile-codex.result == 'success'"));
    assert!(
        publish_harness_runtime
            .contains("    permissions:\n      contents: read\n      id-token: write\n")
    );
    assert!(reconcile_harness_runtime.contains("    permissions:\n      contents: read\n"));
    assert!(reconcile_harness_runtime.contains("needs.reconcile-harness.result == 'success'"));
    assert!(reconcile_harness_runtime.contains("needs.reconcile-omp.result == 'success'"));
    assert!(reconcile_harness_runtime.contains("needs.reconcile-claude.result == 'success'"));
    assert_eq!(
        publish_discord_scheduling
            .lines()
            .find(|line| line.trim_start().starts_with("needs:"))
            .unwrap()
            .trim(),
        "needs: [verify-dione, reconcile-contracts, reconcile-delivery-scheduler, reconcile-discord-events]"
    );
    assert_eq!(
        reconcile_discord_scheduling
            .lines()
            .find(|line| line.trim_start().starts_with("needs:"))
            .unwrap()
            .trim(),
        "needs: [verify-dione, reconcile-contracts, reconcile-delivery-scheduler, reconcile-discord-events, publish-discord-scheduling]"
    );
    assert!(workflow.contains("scripts/workspace-package-version.sh dione-discord-scheduling"));
    assert!(verify.contains("echo \"discord-scheduling-version=${discord_scheduling_version}\""));
    assert!(workflow.contains(
        "discord-scheduling-version: ${{ steps.package.outputs.discord-scheduling-version }}"
    ));
    assert!(workflow.contains(
        "publish-discord-scheduling: ${{ steps.package.outputs.publish-discord-scheduling }}"
    ));
    assert!(reconcile_discord_scheduling.contains("always()"));
    assert!(reconcile_discord_scheduling.contains("needs.verify-dione.result == 'success'"));
    assert!(reconcile_discord_scheduling.contains(
        "needs.publish-discord-scheduling.result == 'success' || needs.publish-discord-scheduling.result == 'skipped'"
    ));
    assert!(reconcile_discord_scheduling.contains("--ignore-vcs-info"));
    assert!(reconcile_dione.contains("needs.reconcile-discord-scheduling.result == 'success'"));
    assert!(workflow.contains("scripts/package-preflight.sh dione-discord-scheduling "));
    assert!(
        publish_discord_scheduling
            .contains("    permissions:\n      contents: read\n      id-token: write\n")
    );
    assert!(reconcile_discord_scheduling.contains("    permissions:\n      contents: read\n"));
    assert!(reconcile_discord_scheduling.contains("needs.reconcile-contracts.result == 'success'"));
    assert!(
        reconcile_discord_scheduling
            .contains("needs.reconcile-delivery-scheduler.result == 'success'")
    );
    assert!(
        reconcile_discord_scheduling.contains("needs.reconcile-discord-events.result == 'success'")
    );
    assert_eq!(
        publish_drop_ledger
            .lines()
            .find(|line| line.trim_start().starts_with("needs:"))
            .unwrap()
            .trim(),
        "needs: verify-dione"
    );
    assert_eq!(
        reconcile_drop_ledger
            .lines()
            .find(|line| line.trim_start().starts_with("needs:"))
            .unwrap()
            .trim(),
        "needs: [verify-dione, publish-drop-ledger]"
    );
    assert!(workflow.contains("scripts/workspace-package-version.sh dione-drop-ledger"));
    assert!(verify.contains("echo \"drop-ledger-version=${drop_ledger_version}\""));
    assert!(
        workflow.contains("drop-ledger-version: ${{ steps.package.outputs.drop-ledger-version }}")
    );
    assert!(
        workflow.contains("publish-drop-ledger: ${{ steps.package.outputs.publish-drop-ledger }}")
    );
    assert!(reconcile_drop_ledger.contains("always()"));
    assert!(reconcile_drop_ledger.contains("needs.verify-dione.result == 'success'"));
    assert!(reconcile_drop_ledger.contains(
        "needs.publish-drop-ledger.result == 'success' || needs.publish-drop-ledger.result == 'skipped'"
    ));
    assert!(reconcile_drop_ledger.contains("--ignore-vcs-info"));
    assert!(reconcile_dione.contains("needs.reconcile-drop-ledger.result == 'success'"));
    assert!(workflow.contains("scripts/package-preflight.sh dione-drop-ledger "));
    assert!(publish_pluralkit_identities.contains("needs: verify-dione"));
    assert!(
        reconcile_pluralkit_identities
            .contains("needs: [verify-dione, publish-pluralkit-identities]")
    );
    assert!(reconcile_pluralkit_identities.contains("--ignore-vcs-info"));
    assert!(reconcile_dione.contains("needs.reconcile-pluralkit-identities.result == 'success'"));
    assert!(workflow.contains("scripts/package-preflight.sh dione-pluralkit-identities "));
    assert!(publish_pre_send.contains("needs: [verify-dione, reconcile-evidence]"));
    assert!(
        reconcile_pre_send.contains("needs: [verify-dione, reconcile-evidence, publish-pre-send]")
    );
    assert!(reconcile_pre_send.contains("needs.reconcile-evidence.result == 'success'"));
    assert!(reconcile_dione.contains("needs.reconcile-pre-send.result == 'success'"));
    assert!(workflow.contains("scripts/package-preflight.sh dione-pre-send "));

    assert!(reconcile_dione.contains("needs.reconcile-presentation.result == 'success'"));
    assert!(workflow.contains("scripts/package-preflight.sh dione-presentation "));
    assert!(reconcile_dione.contains("needs.reconcile-harness.result == 'success'"));
    assert!(workflow.contains("scripts/package-preflight.sh dione-harness "));
    assert!(reconcile_dione.contains("needs.reconcile-identity-display.result == 'success'"));
    assert!(workflow.contains("scripts/package-preflight.sh dione-identity-display "));
    assert!(reconcile_dione.contains("needs.reconcile-gaie.result == 'success'"));
    assert!(workflow.contains("scripts/package-preflight.sh dione-gaie "));
    assert!(reconcile_dione.contains("needs.reconcile-vaelii.result == 'success'"));
    assert!(workflow.contains("scripts/package-preflight.sh dione-vaelii "));
    assert!(reconcile_dione.contains("needs.reconcile-rate-limiter.result == 'success'"));
    assert!(workflow.contains("scripts/package-preflight.sh dione-rate-limiter "));
    assert!(reconcile_dione.contains("needs.reconcile-messages.result == 'success'"));
    assert!(workflow.contains("scripts/package-preflight.sh dione-messages "));
    assert!(reconcile_dione.contains("needs.reconcile-evidence.result == 'success'"));
    assert!(workflow.contains("scripts/package-preflight.sh dione-evidence "));
    assert!(reconcile_dione.contains("needs.reconcile-attention.result == 'success'"));
    assert!(workflow.contains("scripts/package-preflight.sh dione-attention "));
    assert!(reconcile_dione.contains("needs.reconcile-rook.result == 'success'"));
    assert!(workflow.contains("scripts/package-preflight.sh dione-rook "));
    assert!(reconcile_dione.contains("needs.reconcile-contracts.result == 'success'"));
    assert!(workflow.contains("scripts/package-preflight.sh dione-contracts "));
    assert!(reconcile_dione.contains("needs.reconcile-durable.result == 'success'"));
    assert!(workflow.contains("scripts/package-preflight.sh dione-durable "));
    assert!(!reconcile_dione.contains("--ignore-vcs-info"));
    assert!(workflow.contains("group: publish-crate-${{ inputs.tag }}"));
    assert!(publish_dione.contains("cargo package -p dione --locked --no-verify"));
    assert!(reconcile_dione.contains("cargo package -p dione --locked\n"));

    for upload_job in [
        publish_rendering,
        publish_durable,
        publish_contracts,
        publish_rook,
        publish_attention,
        publish_evidence,
        publish_messages,
        publish_rate_limiter,
        publish_vaelii,
        publish_gaie,
        publish_identity_display,
        publish_harness,
        publish_presentation,
        publish_contradictionary,
        publish_bell_rings,
        publish_delivery_scheduler,
        publish_access_requests,
        publish_mutes,
        publish_coordination,
        publish_pre_send,
        publish_teams_identities,
        publish_discord_identities,
        publish_pluralkit_identities,
        publish_config,
        publish_outbound_consent,
        publish_mcp_tools,
        publish_provider_delivery,
        publish_teams,
        publish_omp,
        publish_codex,
        publish_claude,
        publish_discord_presence,
        publish_discord_state,
        publish_discord_events,
        publish_tracing_channel,
        publish_config_watcher,
        publish_drop_ledger,
        publish_permissions,
        publish_discord_bells,
        publish_discord_gate,
        publish_oneshot,
        publish_harness_runtime,
        publish_discord_scheduling,
        publish_dione,
    ] {
        assert!(upload_job.contains("id-token: write"));
        let upload_step = upload_job
            .split_once("- name: Upload pre-verified")
            .unwrap()
            .1;
        let (command, environment) = upload_step.split_once("\n        env:").unwrap();
        assert!(command.contains("continue-on-error: true"));
        assert!(command.contains("run: cargo publish"));
        assert!(environment.contains("CARGO_REGISTRY_TOKEN"));
        assert!(!command.contains("scripts/crates-io-package-state.sh"));
        assert!(!command.contains("cargo package"));
        assert!(!environment.contains("scripts/crates-io-package-state.sh"));
        assert!(!environment.contains("cargo package"));
    }
}

#[test]
fn forgejo_ci_gates_pull_requests_and_tags_only_qualified_trusted_main() {
    let workflow = include_str!("../.forgejo/workflows/linux.yml");
    let release_hygiene_script = include_str!("../.forgejo/scripts/release-hygiene.sh");
    let semver_helper = include_str!("../scripts/semver-is-greater.sh");
    let rust_toolchain = include_str!("../rust-toolchain.toml");
    let github_ci = include_str!("../.github/workflows/build.yml");
    let github_release = include_str!("../.github/workflows/release.yml");

    let forgejo_triggers = "on:\n  push:\n    branches: [main]\n  pull_request:\n    branches:\n      - main\n      - refactor/plugin-crates\n      - refactor/access-decisions-continuation\n      - refactor/discord-typing\n      - \"refactor/*-crate\"\n      - refactor/vertical-slices\n      - refactor/teams-identities-publication\n      - fix/reply-secure-publication\n      - publication/secure-management-tools\n      - publication/secure-query-tools\n      - publication/secure-reply-teams-base\n      - refactor/owned-durable-admission\n      - refactor/discord-query-observation-errors\n      - refactor/generic-durable-store\n      - feat/rook-event-ingress\n";
    assert!(workflow.contains(forgejo_triggers));
    assert!(!workflow.contains("\n  workflow_dispatch:"));
    assert_eq!(workflow.matches("toolchain: \"1.98.0\"").count(), 7);
    assert!(!workflow.contains("1.95.0"));
    assert!(workflow.contains("cargo +1.98.0 check --workspace --all-targets --locked"));
    assert!(!workflow.contains("toolchain: stable"));
    assert!(workflow.contains("run: cargo fmt --all -- --check"));
    assert!(!workflow.contains("--config imports_granularity"));
    assert!(rust_toolchain.contains("channel = \"1.98.0\""));
    assert!(rust_toolchain.contains("components = [\"clippy\", \"rustfmt\"]"));

    let hygiene = workflow
        .split_once("\n  hygiene:")
        .expect("release hygiene must be part of the required workflow")
        .1
        .split_once("\n  format:")
        .expect("format must follow release hygiene")
        .0;
    assert!(hygiene.contains("EVENT_NAME: ${{ forgejo.event_name }}"));
    assert!(hygiene.contains("PR_BASE: ${{ forgejo.event.pull_request.base.sha }}"));
    assert!(hygiene.contains("PR_HEAD: ${{ forgejo.event.pull_request.head.sha }}"));
    assert!(hygiene.contains("PUSH_BEFORE: ${{ forgejo.event.before }}"));
    assert!(hygiene.contains("PUSH_AFTER: ${{ forgejo.sha }}"));
    assert!(hygiene.contains("run: sh .forgejo/scripts/release-hygiene.sh"));
    assert!(release_hygiene_script.contains("git merge-base \"$PR_BASE\" \"$PR_HEAD\""));
    assert!(release_hygiene_script.contains("VERSION_BASE=\"$PR_BASE\""));
    assert!(release_hygiene_script.contains("AFTER=\"$PR_HEAD\""));
    assert!(release_hygiene_script.contains("CHANGE_BASE=\"$PUSH_BEFORE\""));
    assert!(release_hygiene_script.contains("VERSION_BASE=\"$CHANGE_BASE\""));
    assert!(release_hygiene_script.contains("AFTER=\"$PUSH_AFTER\""));
    assert!(release_hygiene_script.contains("sh scripts/semver-is-greater.sh"));
    assert!(semver_helper.contains("if (!valid_semver(left) || !valid_semver(right)) exit 2"));
    assert!(release_hygiene_script.contains("git diff --name-only \"$CHANGE_BASE\" \"$AFTER\""));
    assert!(release_hygiene_script.contains("git show \"${VERSION_BASE}:Cargo.toml\""));
    assert!(release_hygiene_script.contains("git show \"${AFTER}:Cargo.toml\""));
    assert!(release_hygiene_script.contains("git show \"${AFTER}:CHANGELOG.md\""));
    assert!(release_hygiene_script.contains("awk -v heading=\"## [${new_version}]\""));
    assert!(release_hygiene_script.contains("$0 == heading { found = 1 }"));
    assert!(release_hygiene_script.contains("index($0, heading \" - \") == 1"));

    let package = workflow
        .split_once("\n  package:")
        .expect("Forgejo CI must retain its public package boundary job")
        .1
        .split_once("\n  msrv:")
        .expect("the package boundary must remain separate from MSRV")
        .0;
    assert_eq!(
        package
            .matches("cargo package --workspace --locked\n")
            .count(),
        1
    );
    assert_eq!(
        package
            .matches("python3 scripts/verify-workspace-package-privacy.py")
            .count(),
        1,
        "every archive-producing workspace call must cross the Cargo-membership privacy procedure"
    );
    assert!(package.contains(
        "cargo package --workspace --locked\n          python3 scripts/verify-workspace-package-privacy.py"
    ));

    let required = workflow
        .split_once("\n  required:")
        .expect("all PR checks must feed one required status")
        .1
        .split_once("\n  release-tag:")
        .expect("required checks must precede release tagging")
        .0;
    assert!(required.starts_with("\n    name: Required checks\n"));
    assert!(required.contains("needs: [hygiene, format, lint, test, package, msrv, build, audit]"));
    assert!(required.contains("if: ${{ always() }}"));
    assert!(
        required.contains("run: |\n          sh .forgejo/scripts/require-successful-checks.sh")
    );
    for job in [
        "hygiene", "format", "lint", "test", "package", "msrv", "build", "audit",
    ] {
        assert_eq!(
            required
                .matches(&format!("\"{job}=${{{{ needs.{job}.result }}}}\""))
                .count(),
            1,
            "required status must inspect {job} exactly once"
        );
    }
    let legacy_hygiene = include_str!("../.forgejo/workflows/release-hygiene.yml");
    assert!(legacy_hygiene.contains("name: Release Hygiene"));
    assert!(legacy_hygiene.contains("name: Version + changelog accompany source changes"));
    assert!(legacy_hygiene.contains(forgejo_triggers));
    assert!(legacy_hygiene.contains("run: sh .forgejo/scripts/release-hygiene.sh"));

    let release_tag = workflow
        .split_once("\n  release-tag:")
        .expect("Forgejo CI must retain its trusted-main release tag job")
        .1;
    assert!(release_tag.starts_with(
        "\n    name: Annotated release tag\n    if: ${{ forgejo.event_name == 'push' && forgejo.ref == 'refs/heads/main' }}"
    ));
    assert!(release_tag.contains("needs: [required]"));
    assert!(release_tag.contains("fetch-depth: 0"));
    assert!(release_tag.contains("ref: ${{ forgejo.sha }}"));
    assert_eq!(workflow.matches("persist-credentials: true").count(), 0);
    assert_eq!(workflow.matches("persist-credentials: false").count(), 10);
    assert!(release_tag.contains("persist-credentials: false"));
    assert!(!release_tag.contains("token:"));
    assert!(!release_tag.contains("contents: write"));
    assert!(!workflow.contains("cargo publish"));
    assert!(!workflow.contains("release-artifact"));
    assert!(!workflow.contains("upload-artifact"));
    assert!(release_tag.contains("EXPECTED_COMMIT: ${{ forgejo.sha }}"));
    assert!(release_tag.contains("PUSH_BEFORE: ${{ forgejo.event.before }}"));
    assert!(release_tag.contains("run: sh scripts/tag-qualified-release.sh"));
    assert!(release_tag.contains(
        "uses: https://github.com/dtolnay/rust-toolchain@3c5f7ea28cd621ae0bf5283f0e981fb97b8a7af9"
    ));
    assert!(release_tag.contains("toolchain: \"1.98.0\""));

    let tagger = include_str!("../scripts/tag-qualified-release.sh");
    assert!(
        tagger.contains("git merge-base --is-ancestor \"${PUSH_BEFORE}\" \"${actual_commit}\"")
    );
    assert!(
        tagger.contains("git diff --quiet \"${PUSH_BEFORE}\" \"${actual_commit}\" -- Cargo.toml")
    );
    assert!(tagger.contains("sh scripts/semver-is-greater.sh"));
    assert!(tagger.contains("bootstrap_untagged_version=0.42.0"));
    assert!(tagger.contains("pre-automation untagged bootstrap version; not backfilling"));
    assert!(!tagger.contains("${PUSH_BEFORE}:scripts/tag-qualified-release.sh"));
    assert!(tagger.contains("awk -v heading=\"## [${version}]\""));
    assert!(tagger.contains("$0 == heading { found = 1 }"));
    assert!(tagger.contains("index($0, heading \" - \") == 1"));
    assert!(tagger.contains("git tag --annotate \"${tag}\""));
    let pushes = tagger
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with("release_git push"))
        .collect::<Vec<_>>();
    assert_eq!(
        pushes,
        ["release_git push \"$release_remote\" \"refs/tags/${tag}\""]
    );
    assert!(!tagger.contains("--force"));
    assert!(!tagger.contains("--tags"));
    assert!(!tagger.contains("+refs/"));

    assert!(!std::path::Path::new(".github/workflows/tag-release.yml").exists());
    assert!(github_release.contains("gh release create \"$TAG_NAME\" --verify-tag"));

    assert!(github_native_build_contract(github_ci));
    assert!(!github_ci.contains("universal-apple-darwin"));
    assert!(github_release.contains("dione-${TAG_NAME}-${{ matrix.target }}"));
    assert!(!workflow.contains("macos-latest"));
    assert!(!workflow.contains("universal-apple-darwin"));

    let msrv = workflow
        .split_once("\n  msrv:")
        .expect("Forgejo CI must preserve the GitHub MSRV gate")
        .1
        .split_once("\n  build:")
        .expect("MSRV must remain separate from the release build")
        .0;
    assert!(msrv.contains("toolchain: \"1.98.0\""));
    assert!(msrv.contains("cargo +1.98.0 check --workspace --all-targets --locked"));
    assert!(msrv.contains("persist-credentials: false"));
    let build = workflow
        .split_once("\n  build:")
        .expect("Forgejo CI must build release binaries")
        .1
        .split_once("\n  audit:")
        .expect("the release build must remain separate from the audit")
        .0;
    assert!(build.contains("cargo build --release --workspace --bins --locked"));
    assert!(build.contains("./target/release/dione --version"));
    assert!(build.contains("persist-credentials: false"));
    assert!(github_ci.contains("toolchain: \"1.98.0\""));
    assert!(github_ci.contains("key: msrv-1.98"));
    assert!(github_ci.contains("cargo +1.98.0 check --workspace --all-targets --locked"));
}

fn github_native_build_contract(workflow: &str) -> bool {
    let Some((_, native_build)) = workflow.split_once("\n  native-build:") else {
        return false;
    };
    let native_build = native_build
        .split_inclusive('\n')
        .take_while(|line| {
            let heading = line.split_once('#').map_or(*line, |(heading, _)| heading);
            !(line.starts_with("  ")
                && !line.starts_with("   ")
                && heading.trim_end().ends_with(':'))
        })
        .collect::<String>();

    native_build.contains("name: Native release build (${{ matrix.os }})")
        && native_build.contains("os: [ubuntu-latest, macos-latest]")
        && native_build.contains("runs-on: ${{ matrix.os }}")
        && native_build.contains("key: ${{ matrix.os }}")
        && native_build.contains("\n      - run: cargo build --release\n")
        && !native_build.contains("--target")
}

#[test]
fn github_native_build_rejects_targeted_or_moved_build_steps() {
    let workflow = include_str!("../.github/workflows/build.yml");
    let build_step = "      - run: cargo build --release\n";
    assert!(github_native_build_contract(workflow));

    let targeted = workflow.replacen(
        build_step,
        "      - run: cargo build --release --target aarch64-apple-darwin\n",
        1,
    );
    assert_ne!(targeted, workflow);
    assert!(!github_native_build_contract(&targeted));

    let moved = workflow.replacen(build_step, "", 1).replacen(
        "      - run: python3 scripts/run-tests.py --workspace --no-fail-fast\n",
        &format!(
            "      - run: python3 scripts/run-tests.py --workspace --no-fail-fast\n{build_step}"
        ),
        1,
    );
    assert_ne!(moved, workflow);
    let test_job = moved
        .split_once("\n  native-build:")
        .expect("mutated workflow must retain the native-build job")
        .0;
    assert!(test_job.contains(build_step));
    assert!(!github_native_build_contract(&moved));

    let intervening_sibling = workflow.replacen(build_step, "", 1).replacen(
        "\n  msrv:",
        &format!("\n  extra-build:\n    runs-on: ubuntu-latest\n    steps:\n{build_step}\n  msrv:"),
        1,
    );
    assert_ne!(intervening_sibling, workflow);
    assert!(intervening_sibling.contains("\n  extra-build:\n"));
    assert_eq!(intervening_sibling.matches(build_step).count(), 1);
    assert!(!github_native_build_contract(&intervening_sibling));

    let commented_sibling = intervening_sibling.replacen(
        "\n  extra-build:\n",
        "\n  extra-build: # separate sibling job\n",
        1,
    );
    assert_ne!(commented_sibling, intervening_sibling);
    assert_eq!(commented_sibling.matches(build_step).count(), 1);
    assert!(!github_native_build_contract(&commented_sibling));

    let msrv_step = "      - run: cargo +1.98.0 check --workspace --all-targets --locked\n";
    let commented_msrv = workflow
        .replacen(build_step, "", 1)
        .replacen("\n  msrv:\n", "\n  msrv: # separate job\n", 1)
        .replacen(msrv_step, &format!("{msrv_step}{build_step}"), 1);
    assert_ne!(commented_msrv, workflow);
    assert!(commented_msrv.contains("\n  msrv: # separate job\n"));
    assert_eq!(commented_msrv.matches(build_step).count(), 1);
    let msrv_job = commented_msrv
        .split_once("\n  msrv: # separate job\n")
        .expect("mutated workflow must retain the commented MSRV job")
        .1;
    assert!(msrv_job.contains(&format!("{msrv_step}{build_step}")));
    assert!(!github_native_build_contract(&commented_msrv));
}

#[test]
fn forgejo_workflows_reject_ignored_permissions_fields() {
    for entry in fs::read_dir(".forgejo/workflows").expect("Forgejo workflows must be readable") {
        let path = entry.expect("workflow entry must be readable").path();
        if !matches!(
            path.extension().and_then(|extension| extension.to_str()),
            Some("yml" | "yaml")
        ) {
            continue;
        }
        let workflow = fs::read_to_string(&path).expect("Forgejo workflow must be UTF-8");
        assert!(
            !workflow
                .lines()
                .any(|line| line.trim_start().starts_with("permissions:")),
            "{} declares a permissions field that Forgejo ignores",
            path.display()
        );
    }
}

#[test]
fn forgejo_required_check_rejects_failed_skipped_and_missing_jobs() {
    let script = ".forgejo/scripts/require-successful-checks.sh";
    let jobs = [
        "hygiene", "format", "lint", "test", "package", "msrv", "build", "audit",
    ];
    let mut results = jobs.map(|job| format!("{job}=success"));
    let run = |values: &[String]| {
        Command::new("sh")
            .arg(script)
            .args(values)
            .output()
            .expect("required-check script must execute")
    };

    assert!(run(&results).status.success());
    for (index, job) in jobs.iter().enumerate() {
        for outcome in ["failure", "skipped", "cancelled", ""] {
            results[index] = format!("{job}={outcome}");
            let output = run(&results);
            assert!(
                !output.status.success(),
                "a {outcome:?} {job} job must block the required status"
            );
            assert!(String::from_utf8_lossy(&output.stderr).contains(&format!("{job}=")));
        }
        results[index] = format!("{job}=success");
    }
    assert!(!run(&results[..7]).status.success());
    let unknown = jobs.map(|_| "x=success".to_owned());
    assert!(!run(&unknown).status.success(), "job names must be checked");
}

#[test]
fn every_ci_workflow_pins_rust_toolchains_exactly() {
    let floating_stable = Regex::new(r#"(?m)^\s*toolchain:\s*[\"']?stable[\"']?\s*$"#)
        .expect("floating-stable detector must compile");

    for directory in [".github/workflows", ".forgejo/workflows"] {
        for entry in fs::read_dir(directory).expect("workflow directory must be readable") {
            let path = entry.expect("workflow entry must be readable").path();
            if !matches!(
                path.extension().and_then(|extension| extension.to_str()),
                Some("yml" | "yaml")
            ) {
                continue;
            }
            let workflow = fs::read_to_string(&path).expect("workflow must be readable as UTF-8");
            assert!(
                !floating_stable.is_match(&workflow),
                "{} contains a floating stable Rust toolchain",
                path.display()
            );
        }
    }
}

#[test]
fn release_hygiene_checks_the_event_change_set_in_real_git_dags() {
    let temp = tempfile::tempdir().expect("temporary repository must be created");
    let root = temp.path();
    fs::create_dir(root.join("src")).expect("fixture source directory must be created");
    fs::create_dir(root.join("scripts")).expect("fixture scripts directory must be created");
    fs::write(
        root.join("scripts/semver-is-greater.sh"),
        include_str!("../scripts/semver-is-greater.sh"),
    )
    .expect("SemVer helper fixture must be written");
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\n",
    )
    .expect("fixture manifest must be written");
    fs::write(root.join("CHANGELOG.md"), "## [0.1.0]\n\nInitial.\n")
        .expect("fixture changelog must be written");
    fs::write(root.join("src/lib.rs"), "pub fn initial() {}\n")
        .expect("fixture source must be written");
    fs::write(root.join("README.md"), "fixture\n").expect("fixture readme must be written");
    assert!(git_output(root, &["init", "--quiet"]).status.success());
    let common_base = commit_fixture(root, "base");

    assert!(
        git_output(
            root,
            &["checkout", "--quiet", "-b", "unbumped-pr", &common_base]
        )
        .status
        .success()
    );
    fs::write(root.join("src/lib.rs"), "pub fn unbumped() {}\n")
        .expect("unbumped source must be written");
    let unbumped_pr = commit_fixture(root, "unbumped source PR");

    assert!(
        git_output(
            root,
            &["checkout", "--quiet", "-b", "advanced-main", &common_base]
        )
        .status
        .success()
    );
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.2.0\"\n",
    )
    .expect("advanced manifest must be written");
    fs::write(
        root.join("CHANGELOG.md"),
        "## [0.2.0]\n\nMain release.\n\n## [0.1.0]\n\nInitial.\n",
    )
    .expect("advanced changelog must be written");
    let advanced_main = commit_fixture(root, "main release");

    let behind_main =
        run_release_hygiene(root, "pull_request", &advanced_main, &unbumped_pr, "", "");
    assert_eq!(
        behind_main.status.code(),
        Some(1),
        "behind-main check returned stdout={} stderr={}",
        String::from_utf8_lossy(&behind_main.stdout),
        String::from_utf8_lossy(&behind_main.stderr)
    );
    assert!(
        String::from_utf8_lossy(&behind_main.stdout)
            .contains("version 0.1.0 is not greater than current base version 0.2.0")
    );

    assert!(
        git_output(
            root,
            &["checkout", "--quiet", "-b", "colliding-pr", &common_base]
        )
        .status
        .success()
    );
    fs::write(root.join("src/lib.rs"), "pub fn colliding() {}\n")
        .expect("colliding source must be written");
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.2.0\"\n",
    )
    .expect("colliding manifest must be written");
    fs::write(
        root.join("CHANGELOG.md"),
        "## [0.2.0]\n\nPR release.\n\n## [0.1.0]\n\nInitial.\n",
    )
    .expect("colliding changelog must be written");
    let colliding_pr = commit_fixture(root, "colliding source PR");
    let collision =
        run_release_hygiene(root, "pull_request", &advanced_main, &colliding_pr, "", "");
    assert_eq!(collision.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&collision.stdout)
            .contains("version 0.2.0 is not greater than current base version 0.2.0")
    );

    assert!(
        git_output(
            root,
            &["checkout", "--quiet", "-b", "downgrade-pr", &advanced_main]
        )
        .status
        .success()
    );
    fs::write(root.join("src/lib.rs"), "pub fn downgraded() {}\n")
        .expect("downgraded source must be written");
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\n",
    )
    .expect("downgraded manifest must be written");
    let downgrade_pr = commit_fixture(root, "downgraded source PR");
    let downgrade =
        run_release_hygiene(root, "pull_request", &advanced_main, &downgrade_pr, "", "");
    assert_eq!(downgrade.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&downgrade.stdout)
            .contains("version 0.1.0 is not greater than current base version 0.2.0")
    );

    assert!(
        git_output(
            root,
            &["checkout", "--quiet", "-b", "invalid-pr", &advanced_main]
        )
        .status
        .success()
    );
    fs::write(root.join("src/lib.rs"), "pub fn invalid() {}\n")
        .expect("invalid-version source must be written");
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"banana\"\n",
    )
    .expect("invalid manifest must be written");
    fs::write(
        root.join("CHANGELOG.md"),
        "## [banana]\n\nInvalid release.\n\n## [0.2.0]\n\nMain release.\n",
    )
    .expect("invalid-version changelog must be written");
    let invalid_pr = commit_fixture(root, "invalid-version source PR");
    let invalid = run_release_hygiene(root, "pull_request", &advanced_main, &invalid_pr, "", "");
    assert_eq!(invalid.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&invalid.stdout).contains("must be valid SemVer"));

    assert!(
        git_output(
            root,
            &["checkout", "--quiet", "-b", "near-match-pr", &advanced_main]
        )
        .status
        .success()
    );
    fs::write(root.join("src/lib.rs"), "pub fn near_match() {}\n")
        .expect("near-match source must be written");
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"1.2.3\"\n",
    )
    .expect("near-match manifest must be written");
    fs::write(
        root.join("CHANGELOG.md"),
        "## [1x2y3]\n\nNot the requested release.\n\n## [0.2.0]\n\nMain release.\n",
    )
    .expect("near-match changelog must be written");
    let near_match_pr = commit_fixture(root, "near-match changelog PR");
    let near_match =
        run_release_hygiene(root, "pull_request", &advanced_main, &near_match_pr, "", "");
    assert_eq!(near_match.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&near_match.stdout)
            .contains("CHANGELOG.md has no '## [1.2.3]' entry")
    );

    assert!(
        git_output(
            root,
            &[
                "checkout",
                "--quiet",
                "-b",
                "prerelease-near-match-pr",
                &advanced_main,
            ]
        )
        .status
        .success()
    );
    fs::write(
        root.join("src/lib.rs"),
        "pub fn prerelease_near_match() {}\n",
    )
    .expect("prerelease near-match source must be written");
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"1.2.3-rc.1\"\n",
    )
    .expect("prerelease near-match manifest must be written");
    fs::write(
        root.join("CHANGELOG.md"),
        "## [1.2.3-rcX1]\n\nNot the requested prerelease.\n\n## [0.2.0]\n\nMain release.\n",
    )
    .expect("prerelease near-match changelog must be written");
    let prerelease_near_match_pr = commit_fixture(root, "prerelease near-match changelog PR");
    let prerelease_near_match = run_release_hygiene(
        root,
        "pull_request",
        &advanced_main,
        &prerelease_near_match_pr,
        "",
        "",
    );
    assert_eq!(prerelease_near_match.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&prerelease_near_match.stdout)
            .contains("CHANGELOG.md has no '## [1.2.3-rc.1]' entry")
    );

    assert!(
        git_output(
            root,
            &[
                "checkout",
                "--quiet",
                "-b",
                "malformed-date-pr",
                &advanced_main,
            ]
        )
        .status
        .success()
    );
    fs::write(
        root.join("src/lib.rs"),
        "pub fn malformed_release_date() {}\n",
    )
    .expect("malformed release date source must be written");
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"1.2.4\"\n",
    )
    .expect("malformed release date manifest must be written");
    fs::write(
        root.join("CHANGELOG.md"),
        "## [1.2.4] - eventually\n\nNot a canonical dated release.\n\n## [0.2.0]\n\nMain release.\n",
    )
    .expect("malformed release date changelog must be written");
    let malformed_date_pr = commit_fixture(root, "malformed release date PR");
    let malformed_date = run_release_hygiene(
        root,
        "pull_request",
        &advanced_main,
        &malformed_date_pr,
        "",
        "",
    );
    assert_eq!(malformed_date.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&malformed_date.stdout)
            .contains("CHANGELOG.md has no '## [1.2.4]' entry")
    );

    assert!(
        git_output(
            root,
            &[
                "checkout",
                "--quiet",
                "-b",
                "dated-release-pr",
                &advanced_main,
            ]
        )
        .status
        .success()
    );
    fs::write(root.join("src/lib.rs"), "pub fn dated_release() {}\n")
        .expect("dated release source must be written");
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.3.1\"\n",
    )
    .expect("dated release manifest must be written");
    fs::write(
        root.join("CHANGELOG.md"),
        "## [0.3.1] - 2026-09-14\n\nDated release.\n\n## [0.2.0]\n\nMain release.\n",
    )
    .expect("dated release changelog must be written");
    let dated_release_pr = commit_fixture(root, "dated release PR");
    assert!(
        run_release_hygiene(
            root,
            "pull_request",
            &advanced_main,
            &dated_release_pr,
            "",
            "",
        )
        .status
        .success()
    );

    assert!(
        git_output(
            root,
            &["checkout", "--quiet", "-b", "valid-pr", &advanced_main]
        )
        .status
        .success()
    );
    fs::write(root.join("src/lib.rs"), "pub fn valid() {}\n")
        .expect("valid source must be written");
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.3.0\"\n",
    )
    .expect("valid manifest must be written");
    fs::write(
        root.join("CHANGELOG.md"),
        "## [0.3.0]\n\nPR release.\n\n## [0.2.0]\n\nMain release.\n",
    )
    .expect("valid changelog must be written");
    let valid_pr = commit_fixture(root, "valid source PR");
    assert!(
        run_release_hygiene(root, "pull_request", &advanced_main, &valid_pr, "", "")
            .status
            .success()
    );

    assert!(
        git_output(
            root,
            &["checkout", "--quiet", "-b", "metadata-pr", &advanced_main]
        )
        .status
        .success()
    );
    fs::write(root.join("README.md"), "metadata only\n")
        .expect("metadata-only change must be written");
    let metadata_pr = commit_fixture(root, "metadata-only PR");
    assert!(
        run_release_hygiene(root, "pull_request", &advanced_main, &metadata_pr, "", "")
            .status
            .success()
    );

    assert!(
        !run_release_hygiene(root, "push", "", "", &common_base, &unbumped_pr)
            .status
            .success()
    );
    assert!(
        run_release_hygiene(root, "push", "", "", &advanced_main, &valid_pr)
            .status
            .success()
    );
}

#[test]
fn source_stable_crate_comparison_ignores_only_generated_vcs_info() {
    let temp = tempfile::tempdir().expect("temporary directory must be created");
    let local_root = temp.path().join("local/example-1.0.0");
    let remote_root = temp.path().join("remote/example-1.0.0");
    fs::create_dir_all(&local_root).expect("local crate tree must be created");
    fs::create_dir_all(&remote_root).expect("remote crate tree must be created");

    for root in [&local_root, &remote_root] {
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname='example'\nversion='1.0.0'\n",
        )
        .expect("manifest fixture must be written");
        fs::create_dir(root.join("nested")).expect("nested fixture directory must be created");
        fs::write(root.join("nested/.cargo_vcs_info.json"), "domain content")
            .expect("nested fixture must be written");
    }
    fs::write(local_root.join(".cargo_vcs_info.json"), r#"{"sha1":"old"}"#)
        .expect("local VCS fixture must be written");
    fs::write(
        remote_root.join(".cargo_vcs_info.json"),
        r#"{"sha1":"new"}"#,
    )
    .expect("remote VCS fixture must be written");

    let local_crate = temp.path().join("local.crate");
    let remote_crate = temp.path().join("remote.crate");
    for (parent, archive) in [
        (local_root.parent().unwrap(), &local_crate),
        (remote_root.parent().unwrap(), &remote_crate),
    ] {
        let status = Command::new("tar")
            .args(["czf"])
            .arg(archive)
            .args(["-C"])
            .arg(parent)
            .arg("example-1.0.0")
            .status()
            .expect("tar must execute");
        assert!(status.success(), "crate fixture must be archived");
    }

    let compare = || {
        Command::new("sh")
            .arg("scripts/compare-crate-contents.sh")
            .arg(&local_crate)
            .arg(&remote_crate)
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .status()
            .expect("crate comparison must execute")
    };
    assert!(
        compare().success(),
        "VCS receipt differences must be ignored"
    );

    fs::write(
        remote_root.join("nested/.cargo_vcs_info.json"),
        "changed domain content",
    )
    .expect("changed nested fixture must be written");
    let status = Command::new("tar")
        .args(["czf"])
        .arg(&remote_crate)
        .args(["-C"])
        .arg(remote_root.parent().unwrap())
        .arg("example-1.0.0")
        .status()
        .expect("tar must execute");
    assert!(status.success(), "changed crate fixture must be archived");
    assert!(
        !compare().success(),
        "nested files named like the generated root receipt must remain protected"
    );

    fs::write(
        remote_root.join("nested/.cargo_vcs_info.json"),
        "domain content",
    )
    .expect("nested fixture must be restored");
    fs::write(
        remote_root.join("Cargo.toml"),
        "[package]\nname='example'\nversion='1.0.1'\n",
    )
    .expect("changed manifest fixture must be written");
    let status = Command::new("tar")
        .args(["czf"])
        .arg(&remote_crate)
        .args(["-C"])
        .arg(remote_root.parent().unwrap())
        .arg("example-1.0.0")
        .status()
        .expect("tar must execute");
    assert!(status.success(), "changed crate fixture must be archived");
    assert!(
        !compare().success(),
        "publishable source differences must still be rejected"
    );
}

#[test]
fn typing_release_preserves_direct_owner_reconciliation() {
    let workflow = include_str!("../.github/workflows/publish-crate.yml");
    let publish = workflow
        .split_once("\n  publish-discord-typing:\n")
        .unwrap()
        .1
        .split_once("\n  reconcile-discord-typing:\n")
        .unwrap()
        .0;
    let reconcile = workflow
        .split_once("\n  reconcile-discord-typing:\n")
        .unwrap()
        .1
        .split_once("\n  publish-discord-introspection:\n")
        .unwrap()
        .0;
    assert!(publish.contains(
        "needs: [verify-dione, reconcile-config, reconcile-discord-gate, reconcile-discord-state]"
    ));
    assert!(reconcile.contains("needs: [verify-dione, reconcile-config, reconcile-discord-gate, reconcile-discord-state, publish-discord-typing]"));
    for dependency in ["config", "discord-gate", "discord-state"] {
        assert!(reconcile.contains(&format!("needs.reconcile-{dependency}.result == 'success'")));
    }
    assert!(publish.contains("scripts/package-preflight.sh dione-discord-typing "));
    assert!(publish.contains("    permissions:\n      contents: read\n      id-token: write\n"));
    assert!(reconcile.contains("    permissions:\n      contents: read\n"));
    assert!(reconcile.contains("--ignore-vcs-info"));
    assert!(workflow.contains("needs.reconcile-discord-typing.result == 'success'"));
}

#[test]
fn introspection_release_preserves_direct_owner_reconciliation() {
    let workflow = include_str!("../.github/workflows/publish-crate.yml");
    let publish = workflow
        .split_once("\n  publish-discord-introspection:\n")
        .unwrap()
        .1
        .split_once("\n  reconcile-discord-introspection:\n")
        .unwrap()
        .0;
    let reconcile = workflow
        .split_once("\n  reconcile-discord-introspection:\n")
        .unwrap()
        .1
        .split_once("\n  publish-discord-queries:\n")
        .unwrap()
        .0;
    assert!(publish.contains("needs: [verify-dione, reconcile-config, reconcile-messages]"));
    assert!(reconcile.contains(
        "needs: [verify-dione, reconcile-config, reconcile-messages, publish-discord-introspection]"
    ));
    for dependency in ["config", "messages"] {
        let success = format!("needs.reconcile-{dependency}.result == 'success'");
        assert!(publish.contains(&success));
        assert!(reconcile.contains(&success));
    }
    assert!(publish.contains("scripts/package-preflight.sh dione-discord-introspection "));
    assert!(publish.contains("    permissions:\n      contents: read\n      id-token: write\n"));
    assert!(reconcile.contains("    permissions:\n      contents: read\n"));
    assert!(workflow.contains("needs.reconcile-discord-introspection.result == 'success'"));
}

#[test]
fn query_release_preserves_direct_owner_reconciliation() {
    let workflow = include_str!("../.github/workflows/publish-crate.yml");
    let publish = workflow
        .split_once("\n  publish-discord-queries:\n")
        .unwrap()
        .1
        .split_once("\n  reconcile-discord-queries:\n")
        .unwrap()
        .0;
    let reconcile = workflow
        .split_once("\n  reconcile-discord-queries:\n")
        .unwrap()
        .1
        .split_once("\n  publish-attention-admission:\n")
        .unwrap()
        .0;
    assert!(publish.contains("needs: [verify-dione, reconcile-config, reconcile-contracts, reconcile-discord-gate, reconcile-discord-identities, reconcile-discord-state, reconcile-evidence, reconcile-messages, reconcile-pre-send, reconcile-vaelii]"));
    assert!(reconcile.contains("needs: [verify-dione, reconcile-config, reconcile-contracts, reconcile-discord-gate, reconcile-discord-identities, reconcile-discord-state, reconcile-evidence, reconcile-messages, reconcile-pre-send, reconcile-vaelii, publish-discord-queries]"));
    for dependency in [
        "config",
        "contracts",
        "discord-gate",
        "discord-identities",
        "discord-state",
        "evidence",
        "messages",
        "pre-send",
        "vaelii",
    ] {
        let success = format!("needs.reconcile-{dependency}.result == 'success'");
        assert!(publish.contains(&success));
        assert!(reconcile.contains(&success));
    }
    assert!(publish.contains("scripts/package-preflight.sh dione-discord-queries "));
    assert!(reconcile.contains("always()"));
    assert!(workflow.contains("needs.reconcile-discord-queries.result == 'success'"));
}

#[test]
fn admission_release_preserves_direct_owner_reconciliation() {
    let workflow = include_str!("../.github/workflows/publish-crate.yml");
    let publish = workflow
        .split_once("\n  publish-attention-admission:\n")
        .unwrap()
        .1
        .split_once("\n  reconcile-attention-admission:\n")
        .unwrap()
        .0;
    let reconcile = workflow
        .split_once("\n  reconcile-attention-admission:\n")
        .unwrap()
        .1
        .split_once("\n  publish-discord-context:\n")
        .unwrap()
        .0;
    assert!(publish.contains(
        "needs: [verify-dione, reconcile-attention, reconcile-config, reconcile-discord-events]"
    ));
    assert!(reconcile.contains("needs: [verify-dione, reconcile-attention, reconcile-config, reconcile-discord-events, publish-attention-admission]"));
    for dependency in ["attention", "config", "discord-events"] {
        let success = format!("needs.reconcile-{dependency}.result == 'success'");
        assert!(publish.contains(&success));
        assert!(reconcile.contains(&success));
    }
    assert!(publish.contains("scripts/package-preflight.sh dione-attention-admission "));
    assert!(reconcile.contains("always()"));
    assert!(workflow.contains("needs.reconcile-attention-admission.result == 'success'"));
}

#[test]
fn context_release_preserves_direct_owner_reconciliation() {
    let workflow = include_str!("../.github/workflows/publish-crate.yml");
    let publish = workflow
        .split_once("\n  publish-discord-context:\n")
        .unwrap()
        .1
        .split_once("\n  reconcile-discord-context:\n")
        .unwrap()
        .0;
    let reconcile = workflow
        .split_once("\n  reconcile-discord-context:\n")
        .unwrap()
        .1
        .split_once("\n  publish-discord-tool-arguments:\n")
        .unwrap()
        .0;
    assert!(publish.contains(
        "needs: [verify-dione, reconcile-contracts, reconcile-discord-queries, reconcile-rook]"
    ));
    assert!(reconcile.contains("needs: [verify-dione, reconcile-contracts, reconcile-discord-queries, reconcile-rook, publish-discord-context]"));
    for dependency in ["contracts", "discord-queries", "rook"] {
        let success = format!("needs.reconcile-{dependency}.result == 'success'");
        assert!(publish.contains(&success));
        assert!(reconcile.contains(&success));
    }
    assert!(publish.contains("scripts/package-preflight.sh dione-discord-context "));
    assert!(reconcile.contains("always()"));
    assert!(workflow.contains("needs.reconcile-discord-context.result == 'success'"));
}

#[test]
fn tool_argument_release_preserves_direct_owner_reconciliation() {
    let workflow = include_str!("../.github/workflows/publish-crate.yml");
    let publish = workflow
        .split_once("\n  publish-discord-tool-arguments:\n")
        .unwrap()
        .1
        .split_once("\n  reconcile-discord-tool-arguments:\n")
        .unwrap()
        .0;
    let reconcile = workflow
        .split_once("\n  reconcile-discord-tool-arguments:\n")
        .unwrap()
        .1
        .split_once("\n  publish-discord-messaging-contracts:\n")
        .unwrap()
        .0;
    assert!(publish.contains(
        "needs: [verify-dione, reconcile-discord-identities, reconcile-discord-introspection, reconcile-evidence, reconcile-pre-send]"
    ));
    assert!(reconcile.contains("needs: [verify-dione, reconcile-discord-identities, reconcile-discord-introspection, reconcile-evidence, reconcile-pre-send, publish-discord-tool-arguments]"));
    for dependency in [
        "discord-identities",
        "discord-introspection",
        "evidence",
        "pre-send",
    ] {
        let success = format!("needs.reconcile-{dependency}.result == 'success'");
        assert!(publish.contains(&success));
        assert!(reconcile.contains(&success));
    }
    assert!(publish.contains("scripts/package-preflight.sh dione-discord-tool-arguments "));
    assert!(reconcile.contains("always()"));
    assert!(workflow.contains("needs.reconcile-discord-tool-arguments.result == 'success'"));
}

#[test]
fn messaging_contract_release_preserves_direct_owner_reconciliation() {
    let workflow = include_str!("../.github/workflows/publish-crate.yml");
    let publish = workflow
        .split_once("\n  publish-discord-messaging-contracts:\n")
        .unwrap()
        .1
        .split_once("\n  reconcile-discord-messaging-contracts:\n")
        .unwrap()
        .0;
    let reconcile = workflow
        .split_once("\n  reconcile-discord-messaging-contracts:\n")
        .unwrap()
        .1
        .split_once("\n  publish-discord-tool-contracts:\n")
        .unwrap()
        .0;
    assert!(publish.contains(
        "needs: [verify-dione, reconcile-contradictionary, reconcile-discord-queries, reconcile-evidence, reconcile-outbound-consent, reconcile-pre-send]"
    ));
    assert!(reconcile.contains("needs: [verify-dione, reconcile-contradictionary, reconcile-discord-queries, reconcile-evidence, reconcile-outbound-consent, reconcile-pre-send, publish-discord-messaging-contracts]"));
    for dependency in [
        "contradictionary",
        "discord-queries",
        "evidence",
        "outbound-consent",
        "pre-send",
    ] {
        let success = format!("needs.reconcile-{dependency}.result == 'success'");
        assert!(publish.contains(&success));
        assert!(reconcile.contains(&success));
    }
    assert!(publish.contains("scripts/package-preflight.sh dione-discord-messaging-contracts "));
    assert!(reconcile.contains("always()"));
    assert!(workflow.contains("needs.reconcile-discord-messaging-contracts.result == 'success'"));
}

#[test]
fn tool_contract_release_preserves_direct_owner_reconciliation() {
    let workflow = include_str!("../.github/workflows/publish-crate.yml");
    let publish = workflow
        .split_once("\n  publish-discord-tool-contracts:\n")
        .unwrap()
        .1
        .split_once("\n  reconcile-discord-tool-contracts:\n")
        .unwrap()
        .0;
    let reconcile = workflow
        .split_once("\n  reconcile-discord-tool-contracts:\n")
        .unwrap()
        .1
        .split_once("\n  publish-pluralkit-resolver:\n")
        .unwrap()
        .0;
    assert!(publish.contains(
        "needs: [verify-dione, reconcile-discord-identities, reconcile-discord-introspection, reconcile-discord-messaging-contracts, reconcile-discord-queries, reconcile-discord-tool-arguments, reconcile-evidence, reconcile-mcp-tools, reconcile-pre-send]"
    ));
    assert!(reconcile.contains("needs: [verify-dione, reconcile-discord-identities, reconcile-discord-introspection, reconcile-discord-messaging-contracts, reconcile-discord-queries, reconcile-discord-tool-arguments, reconcile-evidence, reconcile-mcp-tools, reconcile-pre-send, publish-discord-tool-contracts]"));
    for dependency in [
        "discord-identities",
        "discord-introspection",
        "discord-messaging-contracts",
        "discord-queries",
        "discord-tool-arguments",
        "evidence",
        "mcp-tools",
        "pre-send",
    ] {
        let success = format!("needs.reconcile-{dependency}.result == 'success'");
        assert!(publish.contains(&success));
        assert!(reconcile.contains(&success));
    }
    assert!(publish.contains("scripts/package-preflight.sh dione-discord-tool-contracts "));
    assert!(reconcile.contains("always()"));
    assert!(workflow.contains("needs.reconcile-discord-tool-contracts.result == 'success'"));
}

#[test]
fn pluralkit_resolver_release_reconciles_before_the_root() {
    let workflow = include_str!("../.github/workflows/publish-crate.yml");
    let owner = workflow
        .split_once("\n  reconcile-pluralkit-resolver:\n")
        .unwrap()
        .1
        .split_once("\n  publish-crate:\n")
        .unwrap()
        .0;
    assert!(owner.contains("needs: [verify-dione, publish-pluralkit-resolver]"));
    let root = workflow.split_once("\n  publish-crate:\n").unwrap().1;
    assert!(root.contains("reconcile-pluralkit-resolver"));
    assert!(root.contains("needs.reconcile-pluralkit-resolver.result == 'success'"));
}

#[test]
fn mcp_protocol_reconciliation_retains_every_normal_owner_and_success_guard() {
    let workflow = include_str!("../.github/workflows/publish-crate.yml");
    let publish = workflow
        .split_once("\n  publish-mcp-protocol:\n")
        .unwrap()
        .1
        .split_once("\n  reconcile-mcp-protocol:\n")
        .unwrap()
        .0;
    let reconcile = workflow
        .split_once("\n  reconcile-mcp-protocol:\n")
        .unwrap()
        .1
        .split_once("\n  publish-crate:\n")
        .unwrap()
        .0;
    for section in [publish, reconcile] {
        for owner in [
            "attention",
            "discord-presence",
            "discord-tool-arguments",
            "evidence",
        ] {
            assert!(
                section
                    .lines()
                    .next()
                    .unwrap()
                    .contains(&format!("reconcile-{owner}"))
            );
            assert!(section.contains(&format!("needs.reconcile-{owner}.result == 'success'")));
        }
    }
    let root = workflow.split_once("\n  publish-crate:\n").unwrap().1;
    assert!(root.contains("reconcile-mcp-protocol"));
    assert!(root.contains("needs.reconcile-mcp-protocol.result == 'success'"));
}

#[test]
fn gateway_runtime_release_waits_for_its_actual_packaged_dependencies() {
    let workflow = include_str!("../.github/workflows/publish-crate.yml");
    let (_, publish) = workflow.split_once("\n  publish-discord-runtime:").unwrap();
    let (publish, reconcile) = publish
        .split_once("\n  reconcile-discord-runtime:")
        .unwrap();
    let (reconcile, root) = reconcile.split_once("\n  publish-crate:").unwrap();
    let manifest: toml::Value =
        toml::from_str(include_str!("../crates/dione-discord-runtime/Cargo.toml")).unwrap();
    for dependency in ["dependencies", "dev-dependencies", "build-dependencies"]
        .into_iter()
        .filter_map(|section| manifest.get(section).and_then(toml::Value::as_table))
        .flat_map(|dependencies| dependencies.keys())
        .filter(|name| name.starts_with("dione-"))
    {
        let name = dependency.strip_prefix("dione-").unwrap();
        for job in [publish, reconcile] {
            assert!(
                job.lines()
                    .find(|line| line.trim_start().starts_with("needs:"))
                    .unwrap()
                    .contains(&format!("reconcile-{name}")),
                "{dependency}"
            );
            assert!(
                job.contains(&format!("needs.reconcile-{name}.result == 'success'")),
                "{dependency}"
            );
        }
    }
    assert!(reconcile.contains("needs.verify-dione.result == 'success'"));
    assert!(root.contains("needs.reconcile-discord-runtime.result == 'success'"));
    assert!(publish.contains("cargo publish -p dione-discord-runtime --locked --no-verify"));
}
