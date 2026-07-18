//! Lookup helpers for symbols that are present in a loaded Mach-O image but
//! are not exported through `dlsym`.
//!
//! The traversal follows the approach used by yabai's `macho_dlsym.h`.
//! yabai is Copyright (c) 2019 Åsmund Vikane and distributed under the MIT
//! license. See `THIRD_PARTY_NOTICES.md`.

use std::ffi::{CStr, c_char, c_void};
use std::mem::size_of;
use std::ptr::NonNull;

const LC_SYMTAB: u32 = 0x2;
const LC_SEGMENT_64: u32 = 0x19;
const SEG_LINKEDIT: &[u8] = b"__LINKEDIT";

#[repr(C)]
struct MachHeader64 {
    magic: u32,
    cpu_type: i32,
    cpu_subtype: i32,
    file_type: u32,
    command_count: u32,
    commands_size: u32,
    flags: u32,
    reserved: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct LoadCommand {
    command: u32,
    size: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct SegmentCommand64 {
    command: u32,
    size: u32,
    name: [c_char; 16],
    vm_address: u64,
    vm_size: u64,
    file_offset: u64,
    file_size: u64,
    max_protection: i32,
    initial_protection: i32,
    section_count: u32,
    flags: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct SymbolTableCommand {
    command: u32,
    size: u32,
    symbol_offset: u32,
    symbol_count: u32,
    string_offset: u32,
    string_size: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Symbol64 {
    string_index: u32,
    symbol_type: u8,
    section: u8,
    description: u16,
    value: u64,
}

unsafe extern "C" {
    fn _dyld_image_count() -> u32;
    fn _dyld_get_image_name(image_index: u32) -> *const c_char;
    fn _dyld_get_image_header(image_index: u32) -> *const MachHeader64;
    fn _dyld_get_image_vmaddr_slide(image_index: u32) -> isize;
}

/// Find a local or exported symbol in an image that dyld has already loaded.
///
/// # Safety
///
/// The returned address has no type information. The caller must use the exact
/// ABI and signature of `symbol`, and must not retain the pointer beyond the
/// lifetime of the loaded image.
pub(super) unsafe fn find_loaded_symbol(image: &CStr, symbol: &CStr) -> Option<NonNull<c_void>> {
    let (header, slide) = unsafe { find_image(image)? };
    let commands = unsafe { load_commands(header.as_ref())? };

    let mut linkedit = None;
    let mut symbol_table = None;
    for command in commands {
        let header = unsafe { command.as_ptr().read_unaligned() };
        if header.command == LC_SEGMENT_64 && header.size as usize >= size_of::<SegmentCommand64>()
        {
            let segment = unsafe { command.cast::<SegmentCommand64>().read_unaligned() };
            let name = segment
                .name
                .iter()
                .map(|byte| byte.cast_unsigned())
                .take_while(|byte| *byte != 0)
                .collect::<Vec<_>>();
            if name == SEG_LINKEDIT {
                linkedit = Some(segment);
            }
        } else if header.command == LC_SYMTAB
            && header.size as usize >= size_of::<SymbolTableCommand>()
        {
            symbol_table = Some(unsafe { command.cast::<SymbolTableCommand>().read_unaligned() });
        }
    }

    let linkedit = linkedit?;
    let symbol_table = symbol_table?;
    let linkedit_base = usize::try_from(linkedit.vm_address)
        .ok()?
        .checked_add_signed(slide)?
        .checked_sub(usize::try_from(linkedit.file_offset).ok()?)?;
    let strings = linkedit_base.checked_add(symbol_table.string_offset as usize)?;
    let symbols = linkedit_base.checked_add(symbol_table.symbol_offset as usize)?;
    let string_table = unsafe {
        std::slice::from_raw_parts(strings as *const u8, symbol_table.string_size as usize)
    };

    for index in 0..symbol_table.symbol_count as usize {
        let offset = index.checked_mul(size_of::<Symbol64>())?;
        let entry = symbols.checked_add(offset)? as *const Symbol64;
        let entry = unsafe { entry.read_unaligned() };
        let name_start = entry.string_index as usize;
        let Some(name) = string_table.get(name_start..) else {
            continue;
        };
        let name_end = name
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(name.len());
        if &name[..name_end] == symbol.to_bytes() {
            let address = usize::try_from(entry.value)
                .ok()?
                .checked_add_signed(slide)?;
            return NonNull::new(address as *mut c_void);
        }
    }

    None
}

unsafe fn find_image(image: &CStr) -> Option<(NonNull<MachHeader64>, isize)> {
    for index in 0..unsafe { _dyld_image_count() } {
        let name = unsafe { _dyld_get_image_name(index) };
        if name.is_null() || unsafe { CStr::from_ptr(name) } != image {
            continue;
        }

        let header = NonNull::new(unsafe { _dyld_get_image_header(index) }.cast_mut())?;
        return Some((header, unsafe { _dyld_get_image_vmaddr_slide(index) }));
    }
    None
}

unsafe fn load_commands(header: &MachHeader64) -> Option<Vec<NonNull<LoadCommand>>> {
    let start = (std::ptr::from_ref(header) as usize).checked_add(size_of::<MachHeader64>())?;
    let end = start.checked_add(header.commands_size as usize)?;
    let mut address = start;
    let mut commands = Vec::with_capacity(header.command_count as usize);

    for _ in 0..header.command_count {
        if address.checked_add(size_of::<LoadCommand>())? > end {
            return None;
        }
        let command = NonNull::new(address as *mut LoadCommand)?;
        let size = unsafe { command.as_ptr().read_unaligned() }.size as usize;
        if size < size_of::<LoadCommand>() || address.checked_add(size)? > end {
            return None;
        }
        commands.push(command);
        address = address.checked_add(size)?;
    }

    Some(commands)
}
