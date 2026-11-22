//! End-to-end tests of `xila package`, which embeds package_manager's
//! `xila-package` command line.
use std::{fs, process::Command};

fn xila(arguments: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_xila"))
        .args(arguments)
        .output()
        .unwrap()
}

#[test]
fn package_bundles_are_built_signed_verified_and_archived() {
    let directory = std::env::temp_dir().join(format!("xila-tools-package-{}", std::process::id()));
    let _ = fs::remove_dir_all(&directory);
    fs::create_dir(&directory).unwrap();
    let path = |name: &str| directory.join(name).to_str().unwrap().to_owned();

    let steps: [Vec<String>; 5] = [
        vec![
            "key".into(),
            "generate".into(),
            path("developer.seed"),
            path("developer.public"),
        ],
        vec![
            "bundle".into(),
            "init".into(),
            path("source"),
            "community.example".into(),
            "1.0.0".into(),
            "wasm32-wasip1".into(),
            "--xila-version".into(),
            "0.1".into(),
            "--entrypoint".into(),
            "executables/example.wasm".into(),
        ],
        vec![
            "bundle".into(),
            "build".into(),
            path("source"),
            path("Example.app"),
            "--signing-key".into(),
            path("developer.seed"),
        ],
        vec![
            "bundle".into(),
            "verify".into(),
            path("Example.app"),
            "--public-key".into(),
            path("developer.public"),
        ],
        vec![
            "bundle".into(),
            "archive".into(),
            path("Example.app"),
            path("example.xpk"),
        ],
    ];
    for (index, step) in steps.iter().enumerate() {
        if index == 2 {
            fs::write(
                directory.join("source/executables/example.wasm"),
                b"\0asm\x01\0\0\0",
            )
            .unwrap();
        }
        let mut arguments = vec!["package"];
        arguments.extend(step.iter().map(String::as_str));
        let output = xila(&arguments);
        assert!(
            output.status.success(),
            "{arguments:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let developer_manifest =
        fs::read_to_string(directory.join("source/xila-package.json")).unwrap();
    assert!(developer_manifest.contains("\"xila_version\":\"0.1\""));
    assert!(!developer_manifest.contains("minimum_xila_abi"));
    let runtime_manifest =
        fs::read_to_string(directory.join("Example.app/.xila/manifest.json")).unwrap();
    assert!(runtime_manifest.contains("\"xila_version\":\"0.1\""));
    assert!(!runtime_manifest.contains("minimum_xila_abi"));
    let output = xila(&[
        "package",
        "verify",
        &path("example.xpk"),
        &path("developer.public"),
    ]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn package_errors_are_reported_by_xila() {
    let output = xila(&[
        "package",
        "verify",
        "/nonexistent.xpk",
        "/nonexistent.public",
    ]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).starts_with("xila: "));
}

#[test]
fn version_and_help_succeed() {
    let output = xila(&["--version"]);
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).starts_with("xila "));
    assert!(xila(&["package", "--help"]).status.success());
}
