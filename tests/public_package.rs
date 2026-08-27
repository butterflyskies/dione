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

fn run_public_artifact_verifier(
    root: &Path,
    binary_contents: &[u8],
    receipt: &[u8],
    checksum_suffix: &[u8],
    package_inputs: &str,
    metadata: &str,
    path_prefix: Option<&Path>,
) -> Output {
    let binary = root.join("dione");
    let checksum = root.join("dione.sha256");
    let receipt_path = root.join("build-receipt.txt");
    let package_inputs_path = root.join("package-inputs.txt");
    let metadata_path = root.join("cargo-metadata.json");
    fs::write(&binary, binary_contents).expect("binary fixture must be written");
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o755))
        .expect("binary fixture must be executable");
    let digest = Command::new("sha256sum")
        .arg(&binary)
        .output()
        .expect("sha256sum must execute");
    let digest = String::from_utf8(digest.stdout).expect("sha256 output must be UTF-8");
    let digest = digest
        .split_whitespace()
        .next()
        .expect("sha256 output must contain a digest");
    let mut checksum_contents = format!("{digest}  dione\n").into_bytes();
    checksum_contents.extend_from_slice(checksum_suffix);
    fs::write(&checksum, checksum_contents).expect("checksum fixture must be written");
    fs::write(&receipt_path, receipt).expect("receipt fixture must be written");
    fs::write(&package_inputs_path, package_inputs).expect("package input fixture must be written");
    fs::write(&metadata_path, metadata).expect("metadata fixture must be written");

    let mut command = Command::new("scripts/verify-public-artifact.sh");
    if let Some(prefix) = path_prefix {
        command.env(
            "PATH",
            format!(
                "{}:{}",
                prefix.display(),
                std::env::var("PATH").expect("test PATH must be set")
            ),
        );
    }
    command
        .arg(&binary)
        .arg(&checksum)
        .arg(&receipt_path)
        .arg("0123456789abcdef0123456789abcdef01234567")
        .arg("0.1.0")
        .arg(&package_inputs_path)
        .arg(&metadata_path)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("artifact verifier must execute")
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
fn release_version_helper_reads_workspace_path_package() {
    let output = Command::new("sh")
        .arg("scripts/workspace-package-version.sh")
        .arg("auspex-core")
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("version helper must execute");

    assert!(
        output.status.success(),
        "version helper failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "0.3.0");
}

#[test]
fn release_waits_for_cargo_registry_resolution() {
    let workflow = include_str!("../.github/workflows/publish-crate.yml");
    let (verify_auspex, remaining) = workflow
        .split_once("\n  publish-auspex:")
        .expect("release workflow must separate Auspex verification from publication");
    let (publish_auspex, remaining) = remaining
        .split_once("\n  reconcile-auspex:")
        .expect("release workflow must reconcile Auspex outside the OIDC job");
    let (reconcile_auspex, remaining) = remaining
        .split_once("\n  verify-dione:")
        .expect("release workflow must re-enter an unprivileged Dione verification job");
    let (verify_dione, remaining) = remaining
        .split_once("\n  publish-crate:")
        .expect("release workflow must separate Dione verification from publication");
    let (publish_dione, reconcile_dione) = remaining
        .split_once("\n  reconcile-dione:")
        .expect("release workflow must reconcile Dione outside the OIDC job");

    assert!(workflow.contains("scripts/workspace-package-version.sh auspex-core"));
    assert_eq!(
        workflow
            .matches("scripts/verify-public-package-privacy.sh")
            .count(),
        workflow.matches("cargo package -p").count(),
        "every exact package archive must cross the privacy verifier"
    );
    assert!(workflow.contains("cargo info --registry crates-io"));
    assert!(workflow.contains("scripts/crates-io-package-state.sh auspex-core"));
    assert!(workflow.contains("${auspex_crate}\" --ignore-vcs-info"));
    assert!(workflow.contains("scripts/crates-io-package-state.sh dione"));
    assert!(workflow.contains("scripts/verify-crates-io-owners.sh auspex-core"));
    assert!(workflow.contains("scripts/verify-crates-io-owners.sh dione"));
    assert!(workflow.contains("github:butterflyskies:lacuna-blinkers"));
    assert!(workflow.contains("github:butterflyskies:superadmins"));
    assert!(workflow.contains("needs its protected first publication"));
    assert!(!verify_auspex.contains("CARGO_REGISTRY_TOKEN"));
    assert!(!verify_auspex.contains("id-token: write"));
    assert!(!verify_dione.contains("CARGO_REGISTRY_TOKEN"));
    assert!(!verify_dione.contains("id-token: write"));
    assert!(!reconcile_auspex.contains("CARGO_REGISTRY_TOKEN"));
    assert!(!reconcile_auspex.contains("id-token: write"));
    assert!(!reconcile_dione.contains("CARGO_REGISTRY_TOKEN"));
    assert!(!reconcile_dione.contains("id-token: write"));
    assert!(reconcile_auspex.contains("needs: [verify-auspex, publish-auspex]"));
    assert!(reconcile_auspex.contains(
        "always() &&\n      needs.verify-auspex.result == 'success' &&\n      (needs.publish-auspex.result == 'success' || needs.publish-auspex.result == 'skipped')"
    ));
    assert!(reconcile_dione.contains("needs: [verify-dione, publish-crate]"));
    assert!(reconcile_dione.contains(
        "always() &&\n      needs.verify-dione.result == 'success' &&\n      (needs.publish-crate.result == 'success' || needs.publish-crate.result == 'skipped')"
    ));
    assert!(publish_auspex.contains("id-token: write"));
    assert!(publish_dione.contains("id-token: write"));
    assert!(workflow.contains("group: publish-crate-${{ inputs.tag }}"));
    assert!(publish_auspex.contains("cargo package -p auspex-core --locked --no-verify"));
    assert!(!publish_auspex.contains("cargo package -p auspex-core --locked\n"));
    assert!(publish_auspex.contains("cargo publish -p auspex-core --locked --no-verify"));
    assert!(publish_dione.contains("cargo package -p dione --locked --no-verify"));
    assert!(!publish_dione.contains("cargo package -p dione --locked\n"));
    assert!(publish_dione.contains("cargo publish -p dione --locked --no-verify"));
    assert!(reconcile_auspex.contains("Reconcile exact Auspex registry state"));
    assert!(reconcile_auspex.contains("cargo package -p auspex-core --locked\n"));
    assert!(reconcile_dione.contains("Reconcile exact Dione registry state"));
    assert!(reconcile_dione.contains("cargo package -p dione --locked\n"));

    for job in [publish_auspex, publish_dione] {
        let upload_step = job
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
}

#[test]
fn forgejo_ci_preserves_trusted_main_release_artifact_contract() {
    let workflow = include_str!("../.forgejo/workflows/linux.yml");
    let github_ci = include_str!("../.github/workflows/build.yml");
    let github_release = include_str!("../.github/workflows/release.yml");

    assert!(workflow.contains("on:\n  push:\n    branches: [main]\n"));
    assert!(workflow.contains("\npermissions: {}\n"));
    assert!(!workflow.contains("\n  pull_request:"));
    assert!(!workflow.contains("\n  workflow_dispatch:"));

    let package = workflow
        .split_once("\n  package:")
        .expect("Forgejo CI must retain its public package boundary job")
        .1
        .split_once("\n  msrv:")
        .expect("the package boundary must remain separate from MSRV")
        .0;
    assert_eq!(
        package
            .matches("cargo package -p auspex-core --locked\n")
            .count(),
        1
    );
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
    assert_eq!(archive_package_calls.len(), 2);
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
        "cargo package -p auspex-core --locked\n          auspex_version=\"$(scripts/workspace-package-version.sh auspex-core)\"\n          auspex_crate=\"target/package/auspex-core-${auspex_version}.crate\"\n          scripts/verify-public-package-privacy.sh \"${auspex_crate}\""
    ));
    assert!(package.contains(
        "cargo package -p dione --locked\n              dione_version=\"$(scripts/workspace-package-version.sh dione)\"\n              dione_crate=\"target/package/dione-${dione_version}.crate\"\n              scripts/verify-public-package-privacy.sh \"${dione_crate}\""
    ));
    assert!(package.contains("cargo package -p dione --list --locked > /dev/null"));

    let artifact = workflow
        .split_once("\n  release-artifact:")
        .expect("Forgejo CI must retain its trusted-main release artifact job")
        .1;
    assert!(artifact.contains("needs: [format, lint, test, package, msrv, audit]"));
    assert!(artifact.contains("ref: ${{ forgejo.sha }}"));
    assert_eq!(artifact.matches("persist-credentials: false").count(), 1);
    assert!(!artifact.contains("token:"));
    assert!(!artifact.contains("contents: write"));
    assert!(artifact.contains("cargo build --release --locked --target \"${BUILD_TARGET}\""));
    assert!(artifact.contains("actual_commit=\"$(git rev-parse HEAD)\""));
    assert!(artifact.contains("EXPECTED_COMMIT: ${{ forgejo.sha }}"));
    assert!(artifact.contains("scripts/workspace-package-version.sh dione"));
    assert!(artifact.contains("(cd dist && sha256sum dione > dione.sha256)"));
    assert!(artifact.contains("commit=%s\\n"));
    assert!(artifact.contains("version=%s\\n"));
    assert!(!artifact.contains("binary_sha256=%s\\n"));
    assert!(!artifact.contains("rustc=%s\\n"));
    assert!(!artifact.contains("cargo=%s\\n"));
    assert!(artifact.contains("cargo package -p dione --list --locked"));
    assert!(artifact.contains("cargo metadata --locked --no-deps --format-version 1"));
    assert!(artifact.contains("scripts/verify-public-artifact.sh"));
    assert!(!artifact.contains("sh scripts/verify-public-artifact.sh"));
    assert!(artifact.contains(
        "https://data.forgejo.org/actions/upload-artifact@ea165f8d65b6e75b540449e92b4886f43607fa02"
    ));
    assert!(artifact.contains("if-no-files-found: error"));
    assert!(artifact.contains("retention-days: 14"));
    assert!(artifact.contains(
        "path: |\n            dist/dione\n            dist/dione.sha256\n            dist/build-receipt.txt"
    ));
    assert!(!artifact.contains("path: dist/"));
    assert!(!artifact.contains("target/${BUILD_TARGET}/release/\n"));
    assert!(!artifact.contains("artifact-audit/\n"));
    assert!(!artifact.contains("tar czf"));

    assert!(github_ci.contains("cross-compile:"));
    assert!(github_ci.contains("target: x86_64-unknown-linux-gnu"));
    assert!(github_ci.contains("cargo build --release"));
    assert!(github_release.contains("dione-${TAG_NAME}-${{ matrix.target }}"));
    assert!(artifact.contains("BUILD_TARGET: x86_64-unknown-linux-gnu"));
    assert!(!workflow.contains("macos-latest"));
    assert!(!workflow.contains("universal-apple-darwin"));
    assert_ne!(
        fs::metadata("scripts/verify-public-artifact.sh")
            .expect("artifact verifier must be present")
            .permissions()
            .mode()
            & 0o111,
        0,
        "artifact verifier must remain directly executable"
    );

    let msrv = workflow
        .split_once("\n  msrv:")
        .expect("Forgejo CI must preserve the GitHub MSRV gate")
        .1
        .split_once("\n  audit:")
        .expect("MSRV must remain a separate gate")
        .0;
    assert!(msrv.contains("toolchain: \"1.95.0\""));
    assert!(msrv.contains("cargo check --locked --all-targets"));
    assert!(msrv.contains("persist-credentials: false"));
    assert!(github_ci.contains("toolchain: \"1.95.0\""));
    assert!(github_ci.contains("cargo check --locked --all-targets"));
}

#[test]
fn public_artifact_verifier_accepts_only_the_minimal_public_receipt() {
    let temp = tempfile::tempdir().expect("temporary directory must be created");
    let output = run_public_artifact_verifier(
        temp.path(),
        b"safe public dione binary fixture",
        b"commit=0123456789abcdef0123456789abcdef01234567\nversion=0.1.0\n",
        b"",
        "Cargo.toml\nsrc/main.rs\n",
        r#"{"packages":[{"name":"dione","version":"0.1.0"}]}"#,
        None,
    );
    assert!(
        output.status.success(),
        "safe public artifact must pass: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        fs::read_dir(temp.path())
            .expect("fixture directory must remain readable")
            .all(|entry| !entry
                .expect("fixture entry must be readable")
                .file_name()
                .to_string_lossy()
                .starts_with(".expected-")),
        "byte-comparison scratch files must be cleaned up"
    );
}

#[test]
fn public_artifact_verifier_does_not_confuse_canary_substrings_with_bare_names() {
    let temp = tempfile::tempdir().expect("temporary directory must be created");
    let output = run_public_artifact_verifier(
        temp.path(),
        b"admiral mirage MIRAGE ADMIRAL",
        b"commit=0123456789abcdef0123456789abcdef01234567\nversion=0.1.0\n",
        b"",
        "Cargo.toml\nsrc/main.rs\n",
        r#"{"packages":[{"name":"dione"}]}"#,
        None,
    );
    assert!(
        output.status.success(),
        "only bare case-insensitive canary names should fail: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn public_artifact_verifier_rejects_authorized_canaries_and_receipt_growth() {
    let canaries = [
        ["Mir", "anda"].concat(),
        ["Mi", "ra"].concat(),
        ["mIr", "AnDa"].concat(),
        ["mI", "rA"].concat(),
    ];
    for canary in canaries {
        let temp = tempfile::tempdir().expect("temporary directory must be created");
        let output = run_public_artifact_verifier(
            temp.path(),
            format!("safe-prefix {canary} safe-suffix").as_bytes(),
            b"commit=0123456789abcdef0123456789abcdef01234567\nversion=0.1.0\n",
            b"",
            "Cargo.toml\nsrc/main.rs\n",
            r#"{"packages":[{"name":"dione"}]}"#,
            None,
        );
        assert!(
            !output.status.success(),
            "case-insensitive authorized canary must fail closed"
        );
    }

    for (package_inputs, metadata) in [
        (
            ["Cargo.toml\ncrates/", "Mi", "ra", "/src/lib.rs\n"].concat(),
            r#"{"packages":[{"name":"dione"}]}"#.to_owned(),
        ),
        (
            "Cargo.toml\nsrc/main.rs\n".to_owned(),
            [r#"{"packages":[{"name":""#, "mIr", "AnDa", r#""}]}"#].concat(),
        ),
    ] {
        let temp = tempfile::tempdir().expect("temporary directory must be created");
        let output = run_public_artifact_verifier(
            temp.path(),
            b"safe public dione binary fixture",
            b"commit=0123456789abcdef0123456789abcdef01234567\nversion=0.1.0\n",
            b"",
            &package_inputs,
            &metadata,
            None,
        );
        assert!(
            !output.status.success(),
            "authorized canary in package inputs must fail closed"
        );
    }

    let temp = tempfile::tempdir().expect("temporary directory must be created");
    let output = run_public_artifact_verifier(
        temp.path(),
        b"safe public dione binary fixture",
        b"commit=0123456789abcdef0123456789abcdef01234567\nversion=0.1.0\nrunner=private\n",
        b"",
        "Cargo.toml\nsrc/main.rs\n",
        r#"{"packages":[{"name":"dione"}]}"#,
        None,
    );
    assert!(
        !output.status.success(),
        "expanded receipt must fail closed"
    );
}

#[test]
fn public_artifact_verifier_compares_receipt_and_checksum_as_exact_bytes() {
    let exact_receipt = b"commit=0123456789abcdef0123456789abcdef01234567\nversion=0.1.0\n";
    let receipt_variants = [
        exact_receipt[..exact_receipt.len() - 1].to_vec(),
        [exact_receipt.as_slice(), b"\n".as_slice()].concat(),
        [exact_receipt.as_slice(), b"\0".as_slice()].concat(),
    ];
    for receipt in receipt_variants {
        let temp = tempfile::tempdir().expect("temporary directory must be created");
        let output = run_public_artifact_verifier(
            temp.path(),
            b"safe public dione binary fixture",
            &receipt,
            b"",
            "Cargo.toml\nsrc/main.rs\n",
            r#"{"packages":[{"name":"dione"}]}"#,
            None,
        );
        assert!(
            !output.status.success(),
            "missing newline, extra newline, and NUL receipt bytes must fail"
        );
    }

    for checksum_suffix in [b"\n".as_slice(), b"\0".as_slice(), b"x".as_slice()] {
        let temp = tempfile::tempdir().expect("temporary directory must be created");
        let output = run_public_artifact_verifier(
            temp.path(),
            b"safe public dione binary fixture",
            exact_receipt,
            checksum_suffix,
            "Cargo.toml\nsrc/main.rs\n",
            r#"{"packages":[{"name":"dione"}]}"#,
            None,
        );
        assert!(
            !output.status.success(),
            "extra newline, NUL, and ordinary checksum bytes must fail"
        );
    }
}

#[test]
fn public_artifact_verifier_fails_closed_when_canary_scan_errors() {
    let temp = tempfile::tempdir().expect("temporary directory must be created");
    let mock_bin = temp.path().join("mock-bin");
    fs::create_dir(&mock_bin).expect("mock binary directory must be created");
    let mock_grep = mock_bin.join("grep");
    fs::write(&mock_grep, "#!/bin/sh\nexit 2\n").expect("mock grep must be written");
    fs::set_permissions(&mock_grep, fs::Permissions::from_mode(0o755))
        .expect("mock grep must be executable");

    let output = run_public_artifact_verifier(
        temp.path(),
        b"safe public dione binary fixture",
        b"commit=0123456789abcdef0123456789abcdef01234567\nversion=0.1.0\n",
        b"",
        "Cargo.toml\nsrc/main.rs\n",
        r#"{"packages":[{"name":"dione"}]}"#,
        Some(&mock_bin),
    );
    assert!(!output.status.success(), "grep status 2 must fail closed");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("canary scan failed"),
        "scan failure must remain distinguishable from a canary match"
    );
}

#[test]
fn public_artifact_verifier_fails_closed_when_binary_hashing_errors() {
    let temp = tempfile::tempdir().expect("temporary directory must be created");
    let mock_bin = temp.path().join("mock-bin");
    fs::create_dir(&mock_bin).expect("mock binary directory must be created");
    let mock_sha256sum = mock_bin.join("sha256sum");
    fs::write(&mock_sha256sum, "#!/bin/sh\nexit 2\n").expect("mock sha256sum must be written");
    fs::set_permissions(&mock_sha256sum, fs::Permissions::from_mode(0o755))
        .expect("mock sha256sum must be executable");

    let output = run_public_artifact_verifier(
        temp.path(),
        b"safe public dione binary fixture",
        b"commit=0123456789abcdef0123456789abcdef01234567\nversion=0.1.0\n",
        b"",
        "Cargo.toml\nsrc/main.rs\n",
        r#"{"packages":[{"name":"dione"}]}"#,
        Some(&mock_bin),
    );
    assert!(
        !output.status.success(),
        "sha256sum failure must fail closed"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("binary hashing failed"),
        "hash failure must remain distinguishable from a checksum mismatch"
    );
    assert!(
        fs::read_dir(temp.path())
            .expect("fixture directory must remain readable")
            .all(|entry| {
                let name = entry.expect("fixture entry must be readable").file_name();
                let name = name.to_string_lossy();
                !name.starts_with(".expected-") && !name.starts_with(".actual-")
            }),
        "hashing failure must clean up all scratch files"
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
