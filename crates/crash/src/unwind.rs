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
    match architecture {
        Architecture::XtensaEsp32 | Architecture::XtensaEsp32S3 if address != 0 => {
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

    #[test]
    fn recovers_a_return_address_from_the_captured_stack() {
        let mut debug_frame = Vec::new();
        debug_frame.extend_from_slice(&16u32.to_le_bytes());
        debug_frame.extend_from_slice(&0xffff_ffffu32.to_le_bytes());
        // Version 4 CIE. CFA = RSP + 8. Read the return address at CFA - 8.
        debug_frame.extend_from_slice(&[4, 0, 8, 0, 1, 0x78, 16, 0x0c, 7, 8, 0x90, 1]);
        debug_frame.extend_from_slice(&20u32.to_le_bytes());
        debug_frame.extend_from_slice(&0u32.to_le_bytes());
        debug_frame.extend_from_slice(&0x1000u64.to_le_bytes());
        debug_frame.extend_from_slice(&0x100u64.to_le_bytes());

        let header = Header {
            program_counter: 0x11014,
            code_load_offset: 0x10000,
            stack_base: 0x40000,
            stack_pointer: 0x40000,
            ..Header::new(RecordKind::KernelPanic, Architecture::NativeX86_64)
        };
        let mut slot = [0; 512];
        let bytes = Writer::new(&mut slot, &header, Limits::NATIVE)
            .unwrap()
            .finish(format_args!("panic"), &0x12001u64.to_le_bytes(), |_| {
                Some(0)
            });
        let report = Record::read(bytes).unwrap();
        assert_eq!(
            frames(&report, &debug_frame, RunTimeEndian::Little).unwrap(),
            [0x12000]
        );
    }
}
