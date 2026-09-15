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
    let (verify_dione, remaining) = workflow
        .split_once("\n  publish-crate:")
        .expect("release workflow must separate Dione verification from publication");
    let (publish_dione, reconcile_dione) = remaining
        .split_once("\n  reconcile-dione:")
        .expect("release workflow must reconcile Dione outside the OIDC job");

    assert_eq!(
        workflow
            .matches("scripts/verify-public-package-privacy.sh")
            .count(),
        workflow.matches("cargo package -p").count(),
        "every exact package archive must cross the privacy verifier"
    );
    assert!(workflow.contains("scripts/crates-io-package-state.sh dione"));
    assert!(workflow.contains("scripts/verify-crates-io-owners.sh dione"));
    assert!(workflow.contains("github:butterflyskies:lacuna-blinkers"));
    assert!(workflow.contains("github:butterflyskies:superadmins"));
    assert!(!verify_dione.contains("CARGO_REGISTRY_TOKEN"));
    assert!(!verify_dione.contains("id-token: write"));
    assert!(!reconcile_dione.contains("CARGO_REGISTRY_TOKEN"));
    assert!(!reconcile_dione.contains("id-token: write"));
    assert!(reconcile_dione.contains("needs: [verify-dione, publish-crate]"));
    assert!(reconcile_dione.contains(
        "always() &&\n      needs.verify-dione.result == 'success' &&\n      (needs.publish-crate.result == 'success' || needs.publish-crate.result == 'skipped')"
    ));
    assert!(publish_dione.contains("id-token: write"));
    assert!(workflow.contains("group: publish-crate-${{ inputs.tag }}"));
    assert!(publish_dione.contains("cargo package -p dione --locked --no-verify"));
    assert!(!publish_dione.contains("cargo package -p dione --locked\n"));
    assert!(publish_dione.contains("cargo publish -p dione --locked --no-verify"));
    assert!(reconcile_dione.contains("Reconcile exact Dione registry state"));
    assert!(reconcile_dione.contains("cargo package -p dione --locked\n"));

    let upload_step = publish_dione
        .split_once("- name: Upload pre-verified")
        .expect("publish workflow must have a privileged upload step")
        .1;
    let (upload_command, upload_env) = upload_step
        .split_once("\n        env:")
        .expect("upload step must scope its token in an env block");
    assert!(upload_command.contains("continue-on-error: true"));
    assert!(upload_command.contains("run: cargo publish"));
    assert!(upload_env.contains("CARGO_REGISTRY_TOKEN"));
    assert!(!upload_command.contains("scripts/crates-io-package-state.sh"));
    assert!(!upload_command.contains("cargo package"));
    assert!(!upload_env.contains("scripts/crates-io-package-state.sh"));
    assert!(!upload_env.contains("cargo package"));
}

#[test]
fn forgejo_ci_gates_pull_requests_and_tags_only_qualified_trusted_main() {
    let workflow = include_str!("../.forgejo/workflows/linux.yml");
    let release_hygiene = include_str!("../.forgejo/workflows/release-hygiene.yml");
    let release_hygiene_script = include_str!("../.forgejo/scripts/release-hygiene.sh");
    let semver_helper = include_str!("../scripts/semver-is-greater.sh");
    let rust_toolchain = include_str!("../rust-toolchain.toml");
    let github_ci = include_str!("../.github/workflows/build.yml");
    let github_release = include_str!("../.github/workflows/release.yml");

    let forgejo_triggers =
        "on:\n  push:\n    branches: [main]\n  pull_request:\n    branches: [main]\n";
    assert!(workflow.contains(forgejo_triggers));
    assert!(release_hygiene.contains(forgejo_triggers));
    assert!(workflow.contains("\npermissions: {}\n"));
    assert!(release_hygiene.contains("\npermissions: {}\n"));
    assert!(!workflow.contains("\n  workflow_dispatch:"));
    assert!(!release_hygiene.contains("\n  workflow_dispatch:"));
    assert_eq!(workflow.matches("toolchain: \"1.98.0\"").count(), 6);
    assert!(!workflow.contains("1.95.0"));
    assert!(workflow.contains("cargo +1.98.0 check --workspace --all-targets --locked"));
    assert!(!workflow.contains("toolchain: stable"));
    assert!(workflow.contains("run: cargo fmt -- --check"));
    assert!(!workflow.contains("--config imports_granularity"));
    assert!(rust_toolchain.contains("channel = \"1.98.0\""));
    assert!(rust_toolchain.contains("components = [\"clippy\", \"rustfmt\"]"));

    assert!(release_hygiene.contains("EVENT_NAME: ${{ forgejo.event_name }}"));
    assert!(release_hygiene.contains("PR_BASE: ${{ forgejo.event.pull_request.base.sha }}"));
    assert!(release_hygiene.contains("PR_HEAD: ${{ forgejo.event.pull_request.head.sha }}"));
    assert!(release_hygiene.contains("PUSH_BEFORE: ${{ forgejo.event.before }}"));
    assert!(release_hygiene.contains("PUSH_AFTER: ${{ forgejo.sha }}"));
    assert!(release_hygiene.contains("run: sh .forgejo/scripts/release-hygiene.sh"));
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
        package.matches("cargo package -p dione --locked\n").count(),
        1
    );
    let package_lines = package.lines().map(str::trim).collect::<Vec<_>>();
    let archive_package_calls = package_lines
        .iter()
        .enumerate()
        .filter(|(_, line)| line.starts_with("cargo package -p ") && !line.contains(" --list"))
        .collect::<Vec<_>>();
    assert_eq!(archive_package_calls.len(), 1);
    assert_eq!(
        package
            .matches("scripts/verify-public-package-privacy.sh")
            .count(),
        archive_package_calls.len(),
        "every archive-producing package call must cross the privacy verifier"
    );
    for (index, _) in archive_package_calls {
        assert!(
            package_lines
                .get(index + 2)
                .is_some_and(|line| line.contains("_crate=\"target/package/")),
            "each package archive must resolve to its exact versioned path"
        );
        assert!(
            package_lines
                .get(index + 3)
                .is_some_and(|line| line.starts_with("scripts/verify-public-package-privacy.sh")),
            "each resolved package archive must immediately cross the privacy verifier"
        );
    }
    assert!(package.contains(
        "cargo package -p dione --locked\n          dione_version=\"$(scripts/workspace-package-version.sh dione)\"\n          dione_crate=\"target/package/dione-${dione_version}.crate\"\n          scripts/verify-public-package-privacy.sh \"${dione_crate}\""
    ));

    let release_tag = workflow
        .split_once("\n  release-tag:")
        .expect("Forgejo CI must retain its trusted-main release tag job")
        .1;
    assert!(release_tag.starts_with(
        "\n    name: Annotated release tag\n    if: ${{ forgejo.event_name == 'push' && forgejo.ref == 'refs/heads/main' }}"
    ));
    assert!(release_tag.contains("needs: [format, lint, test, package, msrv, audit]"));
    assert!(release_tag.contains("fetch-depth: 0"));
    assert!(release_tag.contains("ref: ${{ forgejo.sha }}"));
    assert_eq!(release_tag.matches("persist-credentials: true").count(), 1);
    assert_eq!(workflow.matches("persist-credentials: true").count(), 1);
    assert_eq!(workflow.matches("persist-credentials: false").count(), 6);
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
        .filter(|line| line.starts_with("git push"))
        .collect::<Vec<_>>();
    assert_eq!(pushes, ["git push origin \"refs/tags/${tag}\""]);
    assert!(!tagger.contains("--force"));
    assert!(!tagger.contains("--tags"));
    assert!(!tagger.contains("+refs/"));

    assert!(!std::path::Path::new(".github/workflows/tag-release.yml").exists());
    assert!(github_release.contains("gh release create \"$TAG_NAME\" --verify-tag"));

    assert!(github_ci.contains("cross-compile:"));
    assert!(github_ci.contains("target: x86_64-unknown-linux-gnu"));
    assert!(github_ci.contains("cargo build --release"));
    assert!(github_release.contains("dione-${TAG_NAME}-${{ matrix.target }}"));
    assert!(!workflow.contains("macos-latest"));
    assert!(!workflow.contains("universal-apple-darwin"));

    let msrv = workflow
        .split_once("\n  msrv:")
        .expect("Forgejo CI must preserve the GitHub MSRV gate")
        .1
        .split_once("\n  audit:")
        .expect("MSRV must remain a separate gate")
        .0;
    assert!(msrv.contains("toolchain: \"1.98.0\""));
    assert!(msrv.contains("cargo +1.98.0 check --workspace --all-targets --locked"));
    assert!(msrv.contains("persist-credentials: false"));
    assert!(github_ci.contains("toolchain: \"1.98.0\""));
    assert!(github_ci.contains("key: msrv-1.98"));
    assert!(github_ci.contains("cargo +1.98.0 check --workspace --all-targets --locked"));
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
