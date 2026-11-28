use std::{
    fs,
    io::{self, Read as _},
    path::Path,
};

use crash::{BuildIdentifier, Record};
use getargs_derive::{GetArgs, Subcommand};
use serde_json::json;

use crate::Result;

#[derive(Debug, GetArgs)]
pub struct CrashCommandLine<'a> {
    #[command(subcommand)]
    command: CrashCommand<'a>,
}

#[derive(Debug, Subcommand)]
pub enum CrashCommand<'a> {
    /// Read a binary or Base64 report and show its decoded frames.
    Decode(DecodeArguments<'a>),
}

#[derive(Debug, GetArgs)]
pub struct DecodeArguments<'a> {
    /// Report file. Use - to read standard input.
    #[arg(value_name = "REPORT")]
    report: &'a str,
    /// ELF file for the crashed firmware or process.
    #[arg(long, value_name = "ELF", conflicts_with = "elf_directory")]
    elf: Option<&'a str>,
    /// Directory of ELF files. Select the one with the recorded identifier.
    #[arg(long, value_name = "DIRECTORY")]
    elf_directory: Option<&'a str>,
    /// Use the named ELF even if the build identifier differs or is missing.
    #[arg(long, requires = "elf")]
    allow_mismatch: bool,
    /// Unwind the captured stack even when the report contains frames.
    #[arg(long)]
    unwind_stack: bool,
    /// Write JSON.
    #[arg(long)]
    json: bool,
}

pub fn execute(arguments: CrashCommandLine<'_>) -> Result<()> {
    let CrashCommand::Decode(arguments) = arguments.command;
    let input: Box<dyn io::Read> = if arguments.report == "-" {
        Box::new(io::stdin().lock())
    } else {
        Box::new(fs::File::open(arguments.report)?)
    };
    // A complete Base64 report is less than twice the binary capacity.
    let limit = 2 * crash::NATIVE_RECORD_CAPACITY as u64;
    let mut bytes = Vec::new();
    input.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err("Crash report exceeds the input limit".into());
    }
    let bytes = crash::report_bytes(bytes)?;
    let record = Record::read(&bytes)?;
    let (elf_path, elf) = match (arguments.elf, arguments.elf_directory) {
        (Some(elf), None) => (Path::new(elf).to_owned(), fs::read(elf)?),
        (None, Some(directory)) => {
            crash::find_elf(Path::new(directory), record.header.build_identifier)?
        }
        _ => return Err("Specify --elf or --elf-directory".into()),
    };
    let decoded = crash::decode(
        &record,
        &elf,
        arguments.allow_mismatch,
        arguments.unwind_stack,
    )?;
    if !decoded.identifier_matches {
        eprintln!("Warning: The ELF build identifier could not be verified");
    }
    if arguments.json {
        print_json(&record, &decoded, &elf_path)?;
    } else {
        println!(
            "{} {:?} ({:?})\n{}\nELF {}",
            record.header.target_name.escape_debug(),
            record.header.kind,
            record.header.reset_reason,
            record.message.escape_debug(),
            elf_path.display(),
        );
        for address in decoded.addresses {
            println!("{} {:#x}", address.source, address.runtime_address);
            for location in address.locations {
                let function = location.function.as_deref().unwrap_or("?");
                let file = location.file.as_deref().unwrap_or("?");
                let line = location.line.unwrap_or(0);
                println!("  {function} at {file}:{line}");
            }
        }
        for line in record.log_tail.lines() {
            println!("{}", line.escape_debug());
        }
    }
    Ok(())
}

fn print_json(record: &Record<'_>, decoded: &crash::Decoded, elf: &Path) -> Result<()> {
    let header = record.header;
    let (identifier_kind, identifier_bytes) = match &header.build_identifier {
        BuildIdentifier::None => ("none", &[][..]),
        BuildIdentifier::ElfSha256(bytes) => ("elf_sha256", bytes.as_slice()),
        BuildIdentifier::GnuBuildIdentifier(bytes) => ("gnu", bytes.as_slice()),
        BuildIdentifier::WasmFingerprint(bytes) => ("wasm_fingerprint", bytes.as_slice()),
    };
    let identifier = identifier_bytes
        .iter()
        .fold(String::new(), |mut text, byte| {
            use std::fmt::Write as _;
            let _ = write!(text, "{byte:02x}");
            text
        });
    let addresses: Vec<_> = decoded
        .addresses
        .iter()
        .map(|address| {
            let locations: Vec<_> = address
                .locations
                .iter()
                .map(|location| {
                    json!({
                        "function": location.function, "file": location.file,
                        "line": location.line, "column": location.column,
                    })
                })
                .collect();
            json!({
                "source": address.source, "runtime_address": address.runtime_address,
                "elf_address": address.elf_address, "locations": locations,
            })
        })
        .collect();
    let output = json!({
        "kind": format!("{:?}", header.kind), "architecture": format!("{:?}", header.architecture),
        "target": header.target_name, "firmware_version": header.firmware_version, "build_epoch": header.build_epoch,
        "reset_reason": format!("{:?}", header.reset_reason), "raw_reset_reason": header.raw_reset_reason,
        "exception_code": header.exception_code, "consecutive_crashes": header.consecutive_crashes, "flags": header.flags,
        "task_name": header.task_name, "task_identifier": header.task_identifier, "core_identifier": header.core_identifier,
        "uptime_microseconds": header.uptime_microseconds, "stack_pointer": header.stack_pointer,
        "fault_address": header.fault_address, "stack_base": header.stack_base, "stack_length": record.stack_window.len(),
        "code_load_offset": header.code_load_offset, "build_identifier_kind": identifier_kind, "build_identifier": identifier,
        "elf": elf.to_string_lossy(), "identifier_matches": decoded.identifier_matches, "message": record.message,
        "addresses": addresses, "log_tail": record.log_tail,
    });
    serde_json::to_writer_pretty(io::stdout().lock(), &output)?;
    println!();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_standard_input_with_a_matching_elf_option() {
        let arguments =
            DecodeArguments::from_args(["-", "--elf", "firmware.elf", "--json"].into_iter())
                .unwrap();
        assert_eq!(arguments.report, "-");
        assert_eq!(arguments.elf, Some("firmware.elf"));
        assert!(arguments.json);
        assert!(
            DecodeArguments::from_args(
                [
                    "report.xcr",
                    "--elf",
                    "firmware.elf",
                    "--elf-directory",
                    "builds"
                ]
                .into_iter()
            )
            .is_err()
        );
    }
}
