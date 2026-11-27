//! Exercise the decoder through the installed command line.
#![cfg(target_os = "linux")]

use std::{
    ffi::OsStr,
    fs,
    io::Write as _,
    process::{Command, Output, Stdio},
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use object::Object as _;
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use xila_crash::{Architecture, BuildIdentifier, Header, Limits, RecordKind, Writer};

const EXECUTABLE: &str = env!("CARGO_BIN_EXE_xila");

fn decode(arguments: &[&OsStr], input: Option<&str>) -> Output {
    let mut command = Command::new(EXECUTABLE);
    command.args(["crash", "decode"]).args(arguments);
    if let Some(input) = input {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    } else {
        command.output().unwrap()
    }
}

fn report(header: &Header<'_>) -> Vec<u8> {
    let mut slot = [0; 512];
    let mut writer = Writer::new(&mut slot, header, Limits::NATIVE).unwrap();
    writer.push_frame(header.program_counter);
    writer
        .finish(format_args!("Test panic"), &[], |output| {
            output[..9].copy_from_slice(b"Last log\n");
            Some(9)
        })
        .to_vec()
}

#[test]
fn selects_elf_and_decodes_binary_and_pasted_reports() {
    let directory = std::env::temp_dir().join(format!("xila-tools-crash-{}", std::process::id()));
    fs::create_dir(&directory).unwrap();
    fs::create_dir(directory.join("ignored-directory")).unwrap();
    fs::write(directory.join("invalid.elf"), b"\x7fELF").unwrap();
    let elf = directory.join("xila.elf");
    fs::copy(EXECUTABLE, &elf).unwrap();
    let executable = fs::read(&elf).unwrap();
    let object = object::File::parse(executable.as_slice()).unwrap();
    let offset = 0x40_0000;
    let header = Header {
        target_name: "test-host",
        program_counter: object.entry() + offset,
        link_register: 1,
        code_load_offset: offset,
        build_identifier: BuildIdentifier::ElfSha256(Sha256::digest(&executable).into()),
        ..Header::new(RecordKind::KernelPanic, Architecture::NativeX86_64)
    };
    let path = directory.join("report.xcr");
    fs::write(&path, report(&header)).unwrap();
    let output = decode(
        &[
            path.as_os_str(),
            "--elf".as_ref(),
            elf.as_os_str(),
            "--json".as_ref(),
        ],
        None,
    );
    assert!(output.status.success());
    let json: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["build_identifier_kind"], "elf_sha256");

    let output = decode(
        &[
            path.as_os_str(),
            "--elf-directory".as_ref(),
            directory.as_os_str(),
        ],
        None,
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("test-host KernelPanic"));
    assert!(text.contains("Test panic"));
    assert!(text.contains("program_counter"));
    assert!(text.contains("Last log"));

    let (build_identifier, identifier_kind) = object
        .build_id()
        .unwrap()
        .and_then(|identifier| identifier.try_into().ok())
        .map_or((header.build_identifier, "elf_sha256"), |identifier| {
            (BuildIdentifier::GnuBuildIdentifier(identifier), "gnu")
        });
    let bytes = report(&Header {
        build_identifier,
        ..header
    });
    let encoded = STANDARD.encode(bytes);
    let wrapped = format!("{}\n{}\n", &encoded[..76], &encoded[76..]);
    let output = decode(
        &[
            "-".as_ref(),
            "--elf".as_ref(),
            elf.as_os_str(),
            "--json".as_ref(),
        ],
        Some(&wrapped),
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["identifier_matches"], true);
    assert_eq!(json["build_identifier_kind"], identifier_kind);
    assert_eq!(json["addresses"][0]["elf_address"], object.entry());
    assert_eq!(json["addresses"][1]["runtime_address"], 1);
    assert!(json["addresses"][1]["elf_address"].is_null());
    assert_eq!(json["log_tail"], "Last log\n");
    fs::write(
        &path,
        report(&Header {
            build_identifier: BuildIdentifier::ElfSha256([0; 32]),
            ..header
        }),
    )
    .unwrap();
    let output = decode(
        &[
            path.as_os_str(),
            "--elf-directory".as_ref(),
            directory.as_os_str(),
        ],
        None,
    );
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("No ELF in the directory matches"));
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn rejects_missing_identifiers_and_excess_input() {
    let header = Header::new(RecordKind::KernelPanic, Architecture::NativeX86_64);
    let encoded = STANDARD.encode(report(&header));
    let elf = EXECUTABLE.as_ref();
    let output = decode(&["-".as_ref(), "--elf".as_ref(), elf], Some(&encoded));
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("no build identifier"));

    let output = decode(
        &[
            "-".as_ref(),
            "--elf".as_ref(),
            elf,
            "--allow-mismatch".as_ref(),
            "--json".as_ref(),
        ],
        Some(&encoded),
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("could not be verified"));
    let json: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["identifier_matches"], false);
    assert_eq!(json["build_identifier_kind"], "none");

    let output = decode(&["-".as_ref()], Some(&encoded));
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("Specify --elf or --elf-directory"));

    let wasm = STANDARD.encode(report(&Header {
        build_identifier: BuildIdentifier::WasmFingerprint([1; 32]),
        ..header
    }));
    let output = decode(
        &[
            "-".as_ref(),
            "--elf".as_ref(),
            elf,
            "--allow-mismatch".as_ref(),
            "--json".as_ref(),
        ],
        Some(&wasm),
    );
    assert!(output.status.success());
    let json: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["build_identifier_kind"], "wasm_fingerprint");

    let oversized = "A".repeat(2 * xila_crash::NATIVE_RECORD_CAPACITY + 1);
    let output = decode(&["-".as_ref(), "--elf".as_ref(), elf], Some(&oversized));
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("exceeds the input limit"));
}
