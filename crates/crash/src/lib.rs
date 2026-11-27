//! Verify and symbolize XCR1 crash reports with a matching ELF file.

mod unwind;

use std::{borrow::Cow, error::Error, fs, io, path::Path};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use gimli::{EndianSlice, RunTimeEndian};
use object::{Object as _, ObjectSection as _};
use sha2::{Digest as _, Sha256};
pub use xila_crash::{BuildIdentifier, Header, NATIVE_RECORD_CAPACITY, Record};

pub type Result<T> = std::result::Result<T, Box<dyn Error>>;

/// Decode a binary report or a Base64 export that may contain line breaks.
pub fn report_bytes(input: Vec<u8>) -> Result<Vec<u8>> {
    let bytes = if input.starts_with(&xila_crash::MAGIC) {
        input
    } else {
        let mut text = input;
        text.retain(|byte| !byte.is_ascii_whitespace());
        STANDARD.decode(text)?
    };
    let record = Record::read(&bytes)?;
    if record.bytes().len() != bytes.len() {
        return Err("Data follows the crash report".into());
    }
    Ok(bytes)
}

/// Find the ELF matching a recorded build identifier. Directory entries are
/// considered in name order so duplicate copies select the same file.
pub fn find_elf(
    directory: &Path,
    identifier: BuildIdentifier,
) -> Result<(std::path::PathBuf, Vec<u8>)> {
    if identifier == BuildIdentifier::None {
        return Err(
            "The report has no build identifier. Specify --elf and --allow-mismatch.".into(),
        );
    }
    let mut candidates = fs::read_dir(directory)?.collect::<io::Result<Vec<_>>>()?;
    candidates.sort_unstable_by_key(|entry| entry.file_name());
    for entry in candidates {
        if !entry.file_type()?.is_file() {
            continue;
        }
        let mut file = fs::File::open(entry.path())?;
        let mut magic = [0; 4];
        use io::{Read as _, Seek as _};
        if file.read(&mut magic)? != 4 || magic != *b"\x7fELF" {
            continue;
        }
        file.rewind()?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        if let Ok(object) = parse_elf(&bytes)
            && matches_identifier(&object, &bytes, identifier)?
        {
            return Ok((entry.path(), bytes));
        }
    }
    Err("No ELF in the directory matches the crash report".into())
}

fn parse_elf(bytes: &[u8]) -> Result<object::File<'_>> {
    if !bytes.starts_with(b"\x7fELF") {
        return Err("The symbols file must be an ELF file".into());
    }
    Ok(object::File::parse(bytes)?)
}

fn matches_identifier(
    object: &object::File<'_>,
    bytes: &[u8],
    identifier: BuildIdentifier,
) -> Result<bool> {
    Ok(match identifier {
        BuildIdentifier::ElfSha256(identifier) => Sha256::digest(bytes)[..] == identifier,
        BuildIdentifier::GnuBuildIdentifier(identifier) => object.build_id()? == Some(&identifier),
        BuildIdentifier::None | BuildIdentifier::WasmFingerprint(_) => false,
    })
}

#[derive(Debug)]
pub struct Location {
    pub function: Option<String>,
    pub file: Option<String>,
    pub line: Option<u32>,
    pub column: Option<u32>,
}

#[derive(Debug)]
pub struct Address {
    pub source: &'static str,
    pub runtime_address: u64,
    pub elf_address: Option<u64>,
    pub locations: Vec<Location>,
}

#[derive(Debug)]
pub struct Decoded {
    pub identifier_matches: bool,
    pub addresses: Vec<Address>,
}

/// Resolve the captured addresses, including inline call sites. When
/// requested or the record has no frames, use the captured stack and DWARF
/// call frame rules to find additional callers.
pub fn decode(
    record: &Record<'_>,
    elf: &[u8],
    allow_mismatch: bool,
    unwind_stack: bool,
) -> Result<Decoded> {
    let object = parse_elf(elf)?;
    let identifier_matches = matches_identifier(&object, elf, record.header.build_identifier)?;
    if !identifier_matches && !allow_mismatch {
        return Err(if record.header.build_identifier == BuildIdentifier::None {
            "The report has no build identifier. Use --allow-mismatch to use this ELF."
        } else {
            "The ELF build identifier does not match the report"
        }
        .into());
    }
    let endian = if object.is_little_endian() {
        RunTimeEndian::Little
    } else {
        RunTimeEndian::Big
    };
    let sections = gimli::DwarfSections::load(|id| section(&object, id.name()))?;
    let dwarf = sections.borrow(|section| EndianSlice::new(section, endian));
    let context = addr2line::Context::from_dwarf(dwarf)?;
    let symbols = object.symbol_map();
    let load_offset = record.header.code_load_offset;
    let mut addresses = Vec::new();
    let mut resolve = |source, runtime_address: u64| -> Result<()> {
        if runtime_address == 0 {
            return Ok(());
        }
        let elf_address = runtime_address.checked_sub(load_offset);
        let mut locations = Vec::new();
        if let Some(address) = elf_address {
            let mut frames = context.find_frames(address).skip_all_loads()?;
            while let Some(frame) = frames.next()? {
                let function = frame
                    .function
                    .as_ref()
                    .and_then(|name| name.raw_name().ok())
                    .map(|name| demangle(&name));
                let location = frame.location;
                locations.push(Location {
                    function,
                    file: location
                        .as_ref()
                        .and_then(|location| location.file)
                        .map(str::to_owned),
                    line: location.as_ref().and_then(|location| location.line),
                    column: location.as_ref().and_then(|location| location.column),
                });
            }
            if locations.iter().all(|location| location.function.is_none())
                && let Some(symbol) = symbols.get(address)
            {
                let function = Some(demangle(symbol.name()));
                if let Some(location) = locations.last_mut() {
                    location.function = function;
                } else {
                    locations.push(Location {
                        function,
                        file: None,
                        line: None,
                        column: None,
                    });
                }
            }
        }
        addresses.push(Address {
            source,
            runtime_address,
            elf_address,
            locations,
        });
        Ok(())
    };
    resolve("program_counter", record.header.program_counter)?;
    resolve("link_register", record.header.link_register)?;
    for frame in record.frames() {
        resolve("frame", frame)?;
    }
    if unwind_stack || record.frames().len() == 0 {
        let bytes = section(&object, ".debug_frame")?;
        for frame in unwind::frames(record, &bytes, endian)? {
            resolve("stack", frame)?;
        }
    }
    Ok(Decoded {
        identifier_matches,
        addresses,
    })
}

fn section<'a>(object: &object::File<'a>, name: &str) -> Result<Cow<'a, [u8]>> {
    Ok(match object.section_by_name(name) {
        Some(section) => section.uncompressed_data()?,
        None => Cow::Borrowed(&[]),
    })
}

fn demangle(name: &str) -> String {
    rustc_demangle::try_demangle(name).map_or_else(|_| name.to_owned(), |name| format!("{name:#}"))
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use object::ObjectSymbol as _;
    use xila_crash::{Architecture, Header, Limits, RecordKind, Writer};

    #[test]
    fn verifies_elf_hash_and_decodes_both_report_encodings() {
        let executable = fs::read(std::env::current_exe().unwrap()).unwrap();
        let identifier = BuildIdentifier::ElfSha256(Sha256::digest(&executable).into());
        let object = parse_elf(&executable).unwrap();
        let address = object
            .symbols()
            .find(|symbol| {
                symbol.name().is_ok_and(|name| {
                    name.contains("verifies_elf_hash_and_decodes_both_report_encodings")
                }) && symbol.kind() == object::SymbolKind::Text
            })
            .unwrap()
            .address();
        let header = Header {
            build_identifier: identifier,
            program_counter: address,
            link_register: address,
            ..Header::new(RecordKind::KernelPanic, Architecture::NativeX86_64)
        };
        let mut slot = [0; 512];
        let mut writer = Writer::new(&mut slot, &header, Limits::NATIVE).unwrap();
        writer.push_frame(address);
        let bytes = writer.finish(format_args!("test panic"), &[], |_| Some(0));

        let encoded = STANDARD.encode(bytes);
        let wrapped = format!("{}\n{}\n", &encoded[..76], &encoded[76..]);
        assert_eq!(report_bytes(wrapped.into_bytes()).unwrap(), bytes);
        assert_eq!(report_bytes(bytes.to_vec()).unwrap(), bytes);
        let mut damaged = bytes.to_vec();
        *damaged.last_mut().unwrap() ^= 1;
        assert!(report_bytes(damaged).is_err());
        let mut extra = bytes.to_vec();
        extra.push(0);
        assert!(
            report_bytes(extra)
                .unwrap_err()
                .to_string()
                .contains("Data follows")
        );
        assert!(parse_elf(b"not an ELF").is_err());
        assert!(find_elf(Path::new("."), BuildIdentifier::None).is_err());

        let report = Record::read(bytes).unwrap();
        let decoded = decode(&report, &executable, false, false).unwrap();
        assert!(decoded.identifier_matches);
        assert_eq!(decoded.addresses[0].elf_address, Some(address));
        assert!(decoded.addresses[0].locations.iter().any(|location| {
            location
                .function
                .as_ref()
                .is_some_and(|name| name.contains("verifies_elf_hash"))
                && location.file.is_some()
                && location.line.is_some()
        }));
        let mut mismatched = executable.clone();
        mismatched.push(0);
        assert!(decode(&report, &mismatched, false, false).is_err());
        assert!(
            !decode(&report, &mismatched, true, false)
                .unwrap()
                .identifier_matches
        );
    }
}
