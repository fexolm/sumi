extern crate alloc;

use alloc::vec;
use alloc::vec::Vec;

use goblin::elf::Elf;
use goblin::elf::program_header::PT_LOAD;
use sumi_abi::{
    address::{PhysicalAddr, VirtualAddr},
    arch::layout::{
        DIRECT_MAP_OFFSET, INTERP_LOAD_BASE, PAGE_SIZE, PIE_LOAD_BASE, USER_STACK_SIZE,
        USER_STACK_TOP,
    },
    boot_info::{BOOT_INFO_FLAG_HAS_RUN_PATH, BOOT_INFO_MAGIC, BootInfo},
};

use crate::kprintln;
use crate::memory::errors::MemoryError;

const AT_NULL: u64 = 0;
const AT_PHDR: u64 = 3;
const AT_PHENT: u64 = 4;
const AT_PHNUM: u64 = 5;
const AT_PAGESZ: u64 = 6;
const AT_BASE: u64 = 7;
const AT_ENTRY: u64 = 9;
const AT_UID: u64 = 11;
const AT_EUID: u64 = 12;
const AT_GID: u64 = 13;
const AT_EGID: u64 = 14;
const AT_HWCAP: u64 = 16;
const AT_SECURE: u64 = 23;
const AT_RANDOM: u64 = 25;

#[derive(Debug)]
pub enum ExecError {
    Fs(i32),
    InvalidElf(&'static str),
    Memory(MemoryError),
}

struct ElfInfo {
    entry: u64,
    phdr_vaddr: u64,
    phnum: u64,
    phentsize: u64,
}

struct InterpInfo {
    base: u64,
    entry: u64,
}

/// Read boot info from the fixed physical address. Returns the run_path if present.
pub fn read_boot_info() -> Option<&'static str> {
    let boot_info_vaddr =
        sumi_abi::arch::layout::BOOT_INFO_ADDR.to_virtual(&crate::KERNEL_DIRECT_MAP);
    // SAFETY: The host wrote BootInfo at this address before starting the vCPU.
    // The memory is valid and mapped via the direct map.
    let info = unsafe { &*boot_info_vaddr.as_ptr::<BootInfo>() };

    if info.magic != BOOT_INFO_MAGIC || info.version < 1 {
        return None;
    }

    if info.version >= 2 {
        crate::time::init(info.tsc_freq_khz, info.wall_clock_sec, info.wall_clock_nsec);
        crate::RNG_SEED.call_once(|| info.rng_seed);
    }

    if info.flags & BOOT_INFO_FLAG_HAS_RUN_PATH == 0 {
        return None;
    }

    let path_ptr = boot_info_vaddr
        .add(info.run_path_offset as usize)
        .as_ptr::<u8>();
    let path_len = info.run_path_len as usize;
    // SAFETY: The host wrote the path string immediately after the BootInfo struct.
    let path_bytes = unsafe { core::slice::from_raw_parts(path_ptr, path_len) };
    core::str::from_utf8(path_bytes).ok()
}

/// Load and execute a user program from the given path. Never returns.
pub fn exec_user_program(path: &str) -> ! {
    kprintln!("[exec] loading {}", path);

    match exec_user_program_inner(path) {
        Ok(()) => {
            // exec_user_program_inner only returns Ok if jump_to_user somehow returned,
            // which should never happen.
            crate::arch::halt_forever()
        }
        Err(e) => {
            match e {
                ExecError::Fs(code) => kprintln!("[exec] error: fs error {}", code),
                ExecError::InvalidElf(msg) => kprintln!("[exec] error: invalid elf: {}", msg),
                ExecError::Memory(_) => kprintln!("[exec] error: memory error"),
            }
            crate::arch::halt_forever()
        }
    }
}

fn exec_user_program_inner(path: &str) -> Result<(), ExecError> {
    // 1. Read file from virtio-fs
    let file_data = read_file(path)?;

    kprintln!("[exec] read {} bytes", file_data.len());

    // 2. Parse ELF
    let elf = Elf::parse(&file_data).map_err(|_| ExecError::InvalidElf("parse failed"))?;

    if elf.header.e_machine != goblin::elf::header::EM_X86_64 {
        return Err(ExecError::InvalidElf("not x86_64"));
    }

    // Determine base address by binary type
    let base: u64 = match elf.header.e_type {
        goblin::elf::header::ET_EXEC => 0,
        goblin::elf::header::ET_DYN => PIE_LOAD_BASE,
        _ => {
            return Err(ExecError::InvalidElf(
                "unsupported ELF type (need ET_EXEC or ET_DYN)",
            ));
        }
    };

    // 3. Load main binary PT_LOAD segments
    let brk_base = load_segments_at_base(&file_data, &elf, base)?;

    // 4. Find phdr_vaddr for auxv
    let phdr_vaddr = if let Some(ph) = elf
        .program_headers
        .iter()
        .find(|ph| ph.p_type == goblin::elf::program_header::PT_PHDR)
    {
        base.checked_add(ph.p_vaddr)
            .ok_or(ExecError::InvalidElf("PT_PHDR vaddr overflow"))?
    } else {
        // Fallback: compute from first PT_LOAD segment.
        // Assumes first LOAD maps file offset 0 (true for all modern toolchains).
        let first_load = elf
            .program_headers
            .iter()
            .filter(|ph| ph.p_type == PT_LOAD)
            .min_by_key(|ph| ph.p_vaddr);
        match first_load {
            Some(ph) => {
                let mapped_base = base
                    .checked_add(ph.p_vaddr)
                    .ok_or(ExecError::InvalidElf("LOAD vaddr overflow"))?;
                // e_phoff is relative to file start; subtract p_offset of the LOAD segment
                // that contains offset 0, then add to mapped_base.
                let file_base_offset = ph.p_offset;
                mapped_base + (elf.header.e_phoff - file_base_offset)
            }
            None => return Err(ExecError::InvalidElf("no PT_LOAD segments")),
        }
    };

    let main_entry = base
        .checked_add(elf.entry)
        .ok_or(ExecError::InvalidElf("entry point overflow"))?;

    let elf_info = ElfInfo {
        entry: main_entry,
        phdr_vaddr,
        phnum: elf.header.e_phnum as u64,
        phentsize: elf.header.e_phentsize as u64,
    };

    // 5. Check for dynamic linker (PT_INTERP)
    let interp_info = if let Some(interp_path) = elf.interpreter {
        kprintln!("[exec] interpreter: {}", interp_path);

        let interp_data = read_file(interp_path).map_err(|e| match e {
            ExecError::Fs(-2) => {
                kprintln!(
                    "[exec] interpreter {} not found in share dir; pass `--share /` to expose the host filesystem",
                    interp_path
                );
                ExecError::Fs(-2)
            }
            other => other,
        })?;
        let interp_elf = Elf::parse(&interp_data)
            .map_err(|_| ExecError::InvalidElf("interpreter parse failed"))?;

        if interp_elf.header.e_type != goblin::elf::header::ET_DYN {
            return Err(ExecError::InvalidElf("interpreter must be ET_DYN"));
        }

        // Load interpreter segments at INTERP_LOAD_BASE
        load_segments_at_base(&interp_data, &interp_elf, INTERP_LOAD_BASE)?;

        let interp_entry = INTERP_LOAD_BASE
            .checked_add(interp_elf.entry)
            .ok_or(ExecError::InvalidElf("interpreter entry overflow"))?;

        let info = InterpInfo {
            base: INTERP_LOAD_BASE,
            entry: interp_entry,
        };

        // Leak interpreter data — we never return from jump_to_user
        core::mem::forget(interp_elf);
        core::mem::forget(interp_data);

        Some(info)
    } else {
        None
    };

    // 6. Set up stack (needs elf_info + interp_info for auxv)
    let sp = setup_stack(path, &elf_info, interp_info.as_ref())?;

    // 7. Set brk state
    *crate::BRK_BASE.lock() = brk_base;
    *crate::BRK_CURRENT.lock() = brk_base;

    // Entry point: interpreter's entry if present, else main binary's
    let entry = match &interp_info {
        Some(interp) => interp.entry,
        None => elf_info.entry,
    };

    kprintln!("[exec] jumping to entry {:#x}", entry);

    // 8. Intentionally leak elf and file_data — we're about to jump to user code
    // and never return. This avoids deallocation overhead.
    core::mem::forget(elf);
    core::mem::forget(file_data);

    // 9. Jump to user — never returns
    jump_to_user(entry, sp);
}

fn read_file(path: &str) -> Result<Vec<u8>, ExecError> {
    let fs = crate::fs();

    // Resolve path
    let nodeid = fs.resolve_path(path.as_bytes()).map_err(ExecError::Fs)?;

    // Get file size
    let attr = fs.getattr(nodeid).map_err(ExecError::Fs)?;
    let size = attr.attr.size as usize;

    // Open file
    let open_out = fs.open(nodeid, 0).map_err(ExecError::Fs)?; // O_RDONLY = 0
    let fh = open_out.fh;

    // Allocate buffer and read
    let mut buf = vec![0u8; size];
    let mut offset = 0u64;
    while offset < size as u64 {
        let chunk_size = core::cmp::min(size as u64 - offset, 1024 * 1024) as u32;
        let buf_vaddr = VirtualAddr::new(buf.as_mut_ptr() as usize + offset as usize);
        let buf_paddr = buf_vaddr
            .to_physical(&crate::KERNEL_DIRECT_MAP)
            .ok_or(ExecError::Fs(-14))?; // EFAULT
        let bytes_read = fs
            .read(fh, offset, buf_paddr, chunk_size)
            .map_err(ExecError::Fs)?;
        if bytes_read == 0 {
            break;
        }
        offset += bytes_read as u64;
    }

    // Close file
    fs.release(fh);
    fs.forget(nodeid, 1);

    Ok(buf)
}

fn load_segments_at_base(file_data: &[u8], elf: &Elf, base: u64) -> Result<VirtualAddr, ExecError> {
    let mut brk_end: u64 = 0;

    for ph in elf.program_headers.iter().filter(|p| p.p_type == PT_LOAD) {
        let vaddr = base
            .checked_add(ph.p_vaddr)
            .ok_or(ExecError::InvalidElf("segment vaddr overflow"))?;
        let seg_end = vaddr
            .checked_add(ph.p_memsz)
            .ok_or(ExecError::InvalidElf("segment end overflow"))?;

        // Validate user-space address
        if seg_end >= DIRECT_MAP_OFFSET.as_usize() as u64 {
            return Err(ExecError::InvalidElf("segment extends beyond user space"));
        }

        // Validate p_filesz <= p_memsz (file data must fit within the segment)
        if ph.p_filesz > ph.p_memsz {
            return Err(ExecError::InvalidElf("p_filesz > p_memsz"));
        }

        // Validate file data bounds
        let file_offset = ph.p_offset as usize;
        let file_size = ph.p_filesz as usize;
        if file_offset
            .checked_add(file_size)
            .is_none_or(|end| end > file_data.len())
        {
            return Err(ExecError::InvalidElf("segment data out of file bounds"));
        }

        let start = align_down_2mb(vaddr);
        let end = align_up_2mb(seg_end);

        let mut page_addr = start;
        while page_addr < end {
            let va = VirtualAddr::new(page_addr as usize);
            // Check if already mapped (two segments might share a 2 MB page)
            if crate::KERNEL_PAGE_TABLE
                .lock()
                .get_if_present(va)
                .map_err(ExecError::Memory)?
                .is_none()
            {
                let paddr = crate::PAGE_ALLOCATOR.alloc(1).map_err(ExecError::Memory)?;
                zero_page(paddr);
                crate::KERNEL_PAGE_TABLE
                    .lock()
                    .map_2mb(va, paddr)
                    .map_err(ExecError::Memory)?;
            }
            page_addr += PAGE_SIZE as u64;
        }

        // Copy segment data
        if file_size > 0 {
            // SAFETY: We validated file_offset + file_size <= file_data.len() above.
            // The pages at vaddr are mapped in the kernel page table.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    file_data.as_ptr().add(file_offset),
                    vaddr as *mut u8,
                    file_size,
                );
            }
        }
        // BSS is already zeroed (zero_page)

        brk_end = core::cmp::max(brk_end, seg_end);
    }

    if brk_end == 0 {
        return Err(ExecError::InvalidElf("no PT_LOAD segments"));
    }

    Ok(VirtualAddr::new(align_up_2mb(brk_end) as usize))
}

fn setup_stack(
    path: &str,
    elf_info: &ElfInfo,
    interp_info: Option<&InterpInfo>,
) -> Result<u64, ExecError> {
    // Align stack bottom down to 2MB page boundary
    let stack_bottom = (USER_STACK_TOP.as_usize() - USER_STACK_SIZE) & !(PAGE_SIZE - 1);
    let stack_top_aligned = (USER_STACK_TOP.as_usize() + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
    let pages = (stack_top_aligned - stack_bottom) / PAGE_SIZE;

    for i in 0..pages {
        let vaddr = VirtualAddr::new(stack_bottom + i * PAGE_SIZE);
        let paddr = crate::PAGE_ALLOCATOR.alloc(1).map_err(ExecError::Memory)?;
        zero_page(paddr);
        crate::KERNEL_PAGE_TABLE
            .lock()
            .map_2mb(vaddr, paddr)
            .map_err(ExecError::Memory)?;
    }

    Ok(prepare_initial_stack(
        USER_STACK_TOP,
        path,
        elf_info,
        interp_info,
    ))
}

fn prepare_initial_stack(
    stack_top: VirtualAddr,
    path: &str,
    info: &ElfInfo,
    interp_info: Option<&InterpInfo>,
) -> u64 {
    let mut sp = stack_top.as_u64();

    // Write argv[0] string (path + null terminator)
    sp -= (path.len() + 1) as u64;
    let argv0_addr = sp;
    // SAFETY: Stack pages are mapped, writing path string and null terminator.
    unsafe {
        core::ptr::copy_nonoverlapping(path.as_ptr(), sp as *mut u8, path.len());
        *(sp as *mut u8).add(path.len()) = 0;
    }

    // Write 16 "random" bytes for AT_RANDOM (musl reads this for stack canary)
    sp = (sp - 16) & !0xF; // align to 16 bytes
    let random_addr = sp;
    // SAFETY: Stack pages are mapped, writing random bytes for the stack canary.
    unsafe {
        let random: [u8; 16] = match crate::RNG_SEED.get() {
            Some(seed) => {
                let mut r = [0u8; 16];
                r.copy_from_slice(&seed[..16]);
                r
            }
            None => [
                0x73, 0x75, 0x6D, 0x69, 0x72, 0x61, 0x6E, 0x64, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06,
                0x07, 0x08,
            ],
        };
        core::ptr::copy_nonoverlapping(random.as_ptr(), sp as *mut u8, 16);
    }

    // Align to 16 bytes
    sp &= !0xF;

    // Auxiliary vector (push pairs from bottom to top in memory)
    sp = push_auxv(sp, AT_NULL, 0);
    sp = push_auxv(sp, AT_SECURE, 0);
    sp = push_auxv(sp, AT_EGID, 0);
    sp = push_auxv(sp, AT_GID, 0);
    sp = push_auxv(sp, AT_EUID, 0);
    sp = push_auxv(sp, AT_UID, 0);
    sp = push_auxv(sp, AT_RANDOM, random_addr);
    sp = push_auxv(sp, AT_PAGESZ, 4096);
    // glibc on x86_64 probes CPU features via CPUID in init-arch.c, so AT_HWCAP
    // is cosmetic on this architecture. Advertise 0 to avoid claiming hardware
    // features we have not validated. See docs/glibc-support-design.md §4.7.
    sp = push_auxv(sp, AT_HWCAP, 0);

    // AT_BASE: interpreter load base, or 0 if no interpreter
    let at_base = interp_info.map_or(0, |i| i.base);
    sp = push_auxv(sp, AT_BASE, at_base);

    // AT_ENTRY: always the main binary's entry point
    sp = push_auxv(sp, AT_ENTRY, info.entry);
    sp = push_auxv(sp, AT_PHNUM, info.phnum);
    sp = push_auxv(sp, AT_PHENT, info.phentsize);
    sp = push_auxv(sp, AT_PHDR, info.phdr_vaddr);

    // envp NULL terminator
    sp = push_u64(sp, 0);

    // argv NULL terminator + argv[0]
    sp = push_u64(sp, 0);
    sp = push_u64(sp, argv0_addr);

    // argc
    sp = push_u64(sp, 1);

    sp
}

fn push_u64(sp: u64, val: u64) -> u64 {
    let new_sp = sp - 8;
    // SAFETY: Stack pages are mapped, writing a u64 value.
    unsafe {
        *(new_sp as *mut u64) = val;
    }
    new_sp
}

fn push_auxv(sp: u64, key: u64, val: u64) -> u64 {
    let sp = push_u64(sp, val);
    push_u64(sp, key)
}

pub fn zero_page(paddr: PhysicalAddr) {
    let vaddr = paddr.to_virtual(&crate::KERNEL_DIRECT_MAP);
    // SAFETY: The physical page is mapped via the direct map, writing zeros.
    unsafe {
        core::ptr::write_bytes(vaddr.as_ptr::<u8>(), 0, PAGE_SIZE);
    }
}

pub fn align_down_2mb(addr: u64) -> u64 {
    addr & !(PAGE_SIZE as u64 - 1)
}

pub fn align_up_2mb(addr: u64) -> u64 {
    (addr + PAGE_SIZE as u64 - 1) & !(PAGE_SIZE as u64 - 1)
}

// Assembly trampoline — only for bare-metal target
#[cfg(not(test))]
unsafe extern "C" {
    fn jump_to_user_asm(entry: u64, sp: u64) -> !;
}

#[cfg(not(test))]
fn jump_to_user(entry: u64, sp: u64) -> ! {
    // SAFETY: entry is the validated ELF entry point, sp is the prepared stack pointer.
    // Both point to mapped memory in the kernel page table.
    unsafe { jump_to_user_asm(entry, sp) }
}

#[cfg(test)]
fn jump_to_user(_entry: u64, _sp: u64) -> ! {
    panic!("jump_to_user called in test mode");
}

#[cfg(not(test))]
core::arch::global_asm!(
    ".global jump_to_user_asm",
    "jump_to_user_asm:",
    "mov rsp, rsi",
    "mov rax, rdi",
    "xor rbx, rbx",
    "xor rcx, rcx",
    "xor rdx, rdx",
    "xor rdi, rdi",
    "xor rsi, rsi",
    "xor r8, r8",
    "xor r9, r9",
    "xor r10, r10",
    "xor r11, r11",
    "xor r12, r12",
    "xor r13, r13",
    "xor r14, r14",
    "xor r15, r15",
    "xor rbp, rbp",
    "cld",
    "jmp rax",
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn align_down_2mb_basic() {
        assert_eq!(align_down_2mb(0x401000), 0x400000);
        assert_eq!(align_down_2mb(0x400000), 0x400000);
        assert_eq!(align_down_2mb(0x5FFFFF), 0x400000);
        assert_eq!(align_down_2mb(0x600000), 0x600000);
        assert_eq!(align_down_2mb(0), 0);
    }

    #[test]
    fn align_up_2mb_basic() {
        assert_eq!(align_up_2mb(0x400001), 0x600000);
        assert_eq!(align_up_2mb(0x400000), 0x400000);
        assert_eq!(align_up_2mb(0), 0);
        assert_eq!(align_up_2mb(1), PAGE_SIZE as u64);
    }
}
