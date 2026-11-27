use gimli::{
    BaseAddresses, CfaRule, DebugFrame, Register, RegisterRule, RunTimeEndian, UnwindContext,
    UnwindSection as _,
};
use xila_crash::{Architecture, Record};

const MAXIMUM_FRAMES: usize = 64;
const MAXIMUM_REGISTER: usize = 64;
type Registers = [Option<u64>; MAXIMUM_REGISTER];

pub fn frames(record: &Record<'_>, data: &[u8], endian: RunTimeEndian) -> crate::Result<Vec<u64>> {
    if data.is_empty() || record.stack_window.is_empty() {
        return Ok(Vec::new());
    }
    let header = &record.header;
    let (stack_register, link_register, width) = match header.architecture {
        Architecture::XtensaEsp32 | Architecture::XtensaEsp32S3 => (1, Some(0), 4),
        Architecture::Riscv32 => (2, Some(1), 4),
        Architecture::ArmThumbv7em => (13, Some(14), 4),
        Architecture::NativeX86_64 => (7, None, 8),
        Architecture::NativeAarch64 => (31, Some(30), 8),
        Architecture::Wasm32 => return Ok(Vec::new()),
    };
    let mut section = DebugFrame::new(data, endian);
    section.set_address_size(width as u8);
    let bases = BaseAddresses::default();
    let mut context = UnwindContext::new();
    let mut registers = [None; MAXIMUM_REGISTER];
    registers[stack_register] = Some(header.stack_pointer);
    if let Some(register) = link_register {
        registers[register] = (header.link_register != 0).then_some(header.link_register);
    }
    let mut instruction = header.program_counter;
    let mut frames = Vec::new();
    let get = |registers: &Registers, register: Register| {
        registers.get(usize::from(register.0)).copied().flatten()
    };
    for _ in 0..MAXIMUM_FRAMES {
        let Some(address) = instruction.checked_sub(header.code_load_offset) else {
            break;
        };
        let entry = match section.fde_for_address(&bases, address, DebugFrame::cie_from_offset) {
            Ok(entry) => entry,
            Err(gimli::Error::NoUnwindInfoForAddress) => break,
            Err(error) => return Err(error.into()),
        };
        let row = entry.unwind_info_for_address(&section, &bases, &mut context, address)?;
        let CfaRule::RegisterAndOffset { register, offset } = *row.cfa() else {
            break;
        };
        let Some(cfa) =
            get(&registers, register).and_then(|value| value.checked_add_signed(offset))
        else {
            break;
        };
        if registers[stack_register].is_some_and(|stack| cfa < stack) {
            break;
        }
        let resolve = |register, rule| match rule {
            RegisterRule::SameValue => get(&registers, register),
            RegisterRule::Register(other) => get(&registers, other),
            RegisterRule::Offset(offset) => cfa
                .checked_add_signed(offset)
                .and_then(|address| read_stack(record, address, width, endian)),
            RegisterRule::ValOffset(offset) => cfa.checked_add_signed(offset),
            RegisterRule::Constant(value) => Some(value),
            _ => None,
        };
        let return_register = entry.cie().return_address_register();
        let Some(return_address) = resolve(return_register, row.register(return_register)) else {
            break;
        };
        let next = caller_instruction(header.architecture, return_address);
        if next == 0 || next == instruction {
            break;
        }
        frames.push(next);
        instruction = next;
        let mut caller_registers = [None; MAXIMUM_REGISTER];
        for &(register, ref rule) in row.registers() {
            if let Some(value) = caller_registers.get_mut(usize::from(register.0)) {
                *value = resolve(register, rule.clone());
            }
        }
        caller_registers[stack_register] = Some(cfa);
        registers = caller_registers;
    }
    Ok(frames)
}

fn read_stack(
    record: &Record<'_>,
    address: u64,
    width: usize,
    endian: RunTimeEndian,
) -> Option<u64> {
    let offset = usize::try_from(address.checked_sub(record.header.stack_base)?).ok()?;
    let bytes = record
        .stack_window
        .get(offset..offset.checked_add(width)?)?;
    if width == 4 {
        let bytes = bytes.try_into().ok()?;
        Some(
            match endian {
                RunTimeEndian::Little => u32::from_le_bytes(bytes),
                RunTimeEndian::Big => u32::from_be_bytes(bytes),
            }
            .into(),
        )
    } else {
        let bytes = bytes.try_into().ok()?;
        Some(match endian {
            RunTimeEndian::Little => u64::from_le_bytes(bytes),
            RunTimeEndian::Big => u64::from_be_bytes(bytes),
        })
    }
}

fn caller_instruction(architecture: Architecture, address: u64) -> u64 {
    if address == 0 {
        return 0;
    }
    match architecture {
        Architecture::XtensaEsp32 | Architecture::XtensaEsp32S3 => {
            ((address & 0x3fff_ffff) | 0x4000_0000).saturating_sub(3)
        }
        Architecture::ArmThumbv7em => (address & !1).saturating_sub(2),
        _ => address.saturating_sub(1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use xila_crash::{Header, Limits, RecordKind, Writer};

    fn frame_section(width: usize, endian: RunTimeEndian, register: u8, rules: &[u8]) -> Vec<u8> {
        let mut section = Vec::new();
        let word = |value: u32| match endian {
            RunTimeEndian::Little => value.to_le_bytes(),
            RunTimeEndian::Big => value.to_be_bytes(),
        };
        section.extend_from_slice(&word(11 + rules.len() as u32));
        section.extend_from_slice(&word(u32::MAX));
        let alignment = if width == 4 { 0x7c } else { 0x78 };
        section.extend_from_slice(&[4, 0, width as u8, 0, 1, alignment, register]);
        section.extend_from_slice(rules);
        section.extend_from_slice(&word((4 + 2 * width) as u32));
        section.extend_from_slice(&word(0));
        section.extend(integer(0x1000, width, endian));
        section.extend(integer(0x100, width, endian));
        section
    }

    fn integer(value: u64, width: usize, endian: RunTimeEndian) -> Vec<u8> {
        match endian {
            RunTimeEndian::Little => value.to_le_bytes()[..width].to_vec(),
            RunTimeEndian::Big => value.to_be_bytes()[8 - width..].to_vec(),
        }
    }

    fn report<'a>(slot: &'a mut [u8], header: &Header<'_>, stack: &[u8]) -> Record<'a> {
        let bytes = Writer::new(slot, header, Limits::NATIVE).unwrap().finish(
            format_args!("panic"),
            stack,
            |_| Some(0),
        );
        Record::read(bytes).unwrap()
    }

    #[test]
    fn recovers_saved_returns_for_each_native_architecture() {
        for (architecture, stack_register, return_register, width, endian, saved, expected) in [
            (
                Architecture::NativeX86_64,
                7,
                16,
                8,
                RunTimeEndian::Little,
                0x12001,
                0x12000,
            ),
            (
                Architecture::NativeAarch64,
                31,
                30,
                8,
                RunTimeEndian::Big,
                0x12001,
                0x12000,
            ),
            (
                Architecture::XtensaEsp32S3,
                1,
                0,
                4,
                RunTimeEndian::Little,
                0x8001_2003,
                0x4001_2000,
            ),
            (
                Architecture::Riscv32,
                2,
                1,
                4,
                RunTimeEndian::Little,
                0x12001,
                0x12000,
            ),
            (
                Architecture::ArmThumbv7em,
                13,
                14,
                4,
                RunTimeEndian::Big,
                0x12003,
                0x12000,
            ),
        ] {
            // Set CFA to SP plus one word. Read the return address one word below it.
            let rules = [0x0c, stack_register, width as u8, 0x80 + return_register, 1];
            let section = frame_section(width, endian, return_register, &rules);
            let header = Header {
                program_counter: 0x11014,
                code_load_offset: 0x10000,
                stack_base: 0x40000,
                stack_pointer: 0x40000,
                ..Header::new(RecordKind::KernelPanic, architecture)
            };
            let mut slot = [0; 512];
            let record = report(&mut slot, &header, &integer(saved, width, endian));
            assert_eq!(
                frames(&record, &section, endian).unwrap(),
                [expected],
                "{architecture:?}"
            );
        }
    }

    #[test]
    fn stops_at_unavailable_rules_and_stack_bounds() {
        let endian = RunTimeEndian::Little;
        let header = Header {
            program_counter: 0x1014,
            link_register: 0x2001,
            stack_base: 0x4000,
            stack_pointer: 0x4000,
            ..Header::new(RecordKind::KernelPanic, Architecture::Riscv32)
        };
        let mut slot = [0; 512];
        let record = report(&mut slot, &header, &0x2001u32.to_le_bytes());
        for (rules, expected) in [
            (&[0x0c, 2, 4, 0x08, 1][..], &[0x2000][..]), // Read the saved link register.
            (&[0x0c, 2, 4, 0x09, 1, 2], &[0x3fff]),      // The return address is in SP.
            (&[0x0c, 2, 4, 0x14, 1, 1], &[0x3fff]),      // The return address is CFA minus four.
            (&[0x0c, 3, 4, 0x81, 1], &[]),               // The CFA register is unavailable.
            (&[0x12, 2, 1, 0x81, 1], &[]),               // The CFA points below SP.
            (&[0x0f, 1, 0x30, 0x81, 1], &[]),            // The CFA uses an expression.
            (&[0x0c, 2, 4, 0x81, 2], &[]),               // The return address is outside the stack.
            (&[0x0c, 2, 4], &[]),                        // The return rule is absent.
        ] {
            let section = frame_section(4, endian, 1, rules);
            assert_eq!(
                frames(&record, &section, endian).unwrap(),
                expected,
                "{rules:?}"
            );
        }
        assert!(frames(&record, &[1, 0, 0, 0, 0], endian).is_err());
        assert!(frames(&record, &[], endian).unwrap().is_empty());

        let section = frame_section(4, endian, 1, &[0x0c, 2, 4, 0x81, 1]);
        for header in [
            Header {
                code_load_offset: 0x2000,
                ..header
            },
            Header {
                architecture: Architecture::Wasm32,
                ..header
            },
        ] {
            let record = report(&mut slot, &header, &[0; 4]);
            assert!(frames(&record, &section, endian).unwrap().is_empty());
        }
        for address in [0u32, 0x1015] {
            let record = report(&mut slot, &header, &address.to_le_bytes());
            assert!(frames(&record, &section, endian).unwrap().is_empty());
        }
    }

    #[test]
    fn decodes_an_elf_with_big_endian_stack_rules() {
        let endian = RunTimeEndian::Big;
        let section = frame_section(4, endian, 14, &[0x0c, 13, 4, 0x8e, 1]);
        let mut elf = object::write::Object::new(
            object::BinaryFormat::Elf,
            object::Architecture::Arm,
            object::Endianness::Big,
        );
        let identifier = elf.add_section(
            Vec::new(),
            b".debug_frame".to_vec(),
            object::SectionKind::Debug,
        );
        elf.append_section_data(identifier, &section, 4);
        let header = Header {
            program_counter: 0x1014,
            stack_base: 0x4000,
            stack_pointer: 0x4000,
            ..Header::new(RecordKind::KernelPanic, Architecture::ArmThumbv7em)
        };
        let mut slot = [0; 512];
        let record = report(&mut slot, &header, &0x2003u32.to_be_bytes());
        let decoded = crate::decode(&record, &elf.write().unwrap(), true, false).unwrap();
        assert_eq!(decoded.addresses[1].source, "stack");
        assert_eq!(decoded.addresses[1].elf_address, Some(0x2000));
    }
}
