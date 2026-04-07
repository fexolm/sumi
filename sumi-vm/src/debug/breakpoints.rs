use std::collections::HashMap;
use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};

/// Tracks software breakpoints (int3 patches) in guest memory.
pub struct BreakpointManager {
    /// Map from guest virtual address to the original byte we replaced with 0xCC.
    sw_breakpoints: HashMap<u64, u8>,
}

impl BreakpointManager {
    pub fn new() -> Self {
        Self {
            sw_breakpoints: HashMap::new(),
        }
    }

    /// Insert a software breakpoint at `vaddr`.
    /// Patches the guest memory byte at that address with 0xCC (int3).
    pub fn insert_sw(
        &mut self,
        vaddr: u64,
        mem: &GuestMemoryMmap<()>,
        cr3: u64,
    ) -> Result<(), BreakpointError> {
        if self.sw_breakpoints.contains_key(&vaddr) {
            return Ok(()); // already set
        }
        let paddr = translate_gva(mem, cr3, vaddr)?;
        let mut orig = [0u8; 1];
        mem.read_slice(&mut orig, GuestAddress(paddr))
            .map_err(|_| BreakpointError::BadAddress(vaddr))?;
        mem.write_slice(&[0xCCu8], GuestAddress(paddr))
            .map_err(|_| BreakpointError::BadAddress(vaddr))?;
        self.sw_breakpoints.insert(vaddr, orig[0]);
        Ok(())
    }

    /// Remove a software breakpoint at `vaddr`.
    /// Restores the original byte.
    pub fn remove_sw(
        &mut self,
        vaddr: u64,
        mem: &GuestMemoryMmap<()>,
        cr3: u64,
    ) -> Result<(), BreakpointError> {
        let orig = self
            .sw_breakpoints
            .remove(&vaddr)
            .ok_or(BreakpointError::NotFound(vaddr))?;
        let paddr = translate_gva(mem, cr3, vaddr)?;
        mem.write_slice(&[orig], GuestAddress(paddr))
            .map_err(|_| BreakpointError::BadAddress(vaddr))?;
        Ok(())
    }

    /// Check if we have a software breakpoint at this address.
    pub fn has_sw(&self, vaddr: u64) -> bool {
        self.sw_breakpoints.contains_key(&vaddr)
    }

    /// Temporarily restore the original byte so we can single-step past the breakpoint.
    pub fn suspend_sw(
        &self,
        vaddr: u64,
        mem: &GuestMemoryMmap<()>,
        cr3: u64,
    ) -> Result<(), BreakpointError> {
        let orig = self
            .sw_breakpoints
            .get(&vaddr)
            .ok_or(BreakpointError::NotFound(vaddr))?;
        let paddr = translate_gva(mem, cr3, vaddr)?;
        mem.write_slice(&[*orig], GuestAddress(paddr))
            .map_err(|_| BreakpointError::BadAddress(vaddr))?;
        Ok(())
    }

    /// Re-insert the int3 after single-stepping past the breakpoint.
    pub fn reinstate_sw(
        &self,
        vaddr: u64,
        mem: &GuestMemoryMmap<()>,
        cr3: u64,
    ) -> Result<(), BreakpointError> {
        if !self.sw_breakpoints.contains_key(&vaddr) {
            return Ok(());
        }
        let paddr = translate_gva(mem, cr3, vaddr)?;
        mem.write_slice(&[0xCCu8], GuestAddress(paddr))
            .map_err(|_| BreakpointError::BadAddress(vaddr))?;
        Ok(())
    }

    /// Remove all breakpoints, restoring original bytes.
    pub fn remove_all(&mut self, mem: &GuestMemoryMmap<()>, cr3: u64) {
        let addrs: Vec<u64> = self.sw_breakpoints.keys().copied().collect();
        for addr in addrs {
            let _ = self.remove_sw(addr, mem, cr3);
        }
    }
}

/// Walk guest page tables to translate a guest virtual address to guest physical.
/// Supports 1GB huge pages (PDPT level), 2MB huge pages (PD level), and 4KB pages.
pub fn translate_gva(
    mem: &GuestMemoryMmap<()>,
    cr3: u64,
    vaddr: u64,
) -> Result<u64, BreakpointError> {
    const ADDR_MASK_4K: u64 = 0x000F_FFFF_FFFF_F000;
    const ADDR_MASK_2M: u64 = 0x000F_FFFF_FFE0_0000;
    const ADDR_MASK_1G: u64 = 0x000F_FFFF_C000_0000;
    const PTE_PRESENT: u64 = 0x1;
    const PTE_PS: u64 = 0x80;

    let read_entry = |paddr: u64| -> Result<u64, BreakpointError> {
        let mut buf = [0u8; 8];
        mem.read_slice(&mut buf, GuestAddress(paddr))
            .map_err(|_| BreakpointError::BadAddress(vaddr))?;
        Ok(u64::from_le_bytes(buf))
    };

    // PML4
    let pml4_idx = (vaddr >> 39) & 0x1FF;
    let pml4e = read_entry((cr3 & ADDR_MASK_4K) + pml4_idx * 8)?;
    if pml4e & PTE_PRESENT == 0 {
        return Err(BreakpointError::PageNotPresent(vaddr));
    }

    // PDPT
    let pdpt_idx = (vaddr >> 30) & 0x1FF;
    let pdpte = read_entry((pml4e & ADDR_MASK_4K) + pdpt_idx * 8)?;
    if pdpte & PTE_PRESENT == 0 {
        return Err(BreakpointError::PageNotPresent(vaddr));
    }
    if pdpte & PTE_PS != 0 {
        // 1GB huge page
        return Ok((pdpte & ADDR_MASK_1G) | (vaddr & 0x3FFF_FFFF));
    }

    // PD
    let pd_idx = (vaddr >> 21) & 0x1FF;
    let pde = read_entry((pdpte & ADDR_MASK_4K) + pd_idx * 8)?;
    if pde & PTE_PRESENT == 0 {
        return Err(BreakpointError::PageNotPresent(vaddr));
    }
    if pde & PTE_PS != 0 {
        // 2MB huge page
        return Ok((pde & ADDR_MASK_2M) | (vaddr & 0x1F_FFFF));
    }

    // PT (4KB)
    let pt_idx = (vaddr >> 12) & 0x1FF;
    let pte = read_entry((pde & ADDR_MASK_4K) + pt_idx * 8)?;
    if pte & PTE_PRESENT == 0 {
        return Err(BreakpointError::PageNotPresent(vaddr));
    }
    Ok((pte & ADDR_MASK_4K) | (vaddr & 0xFFF))
}

/// Read guest memory by virtual address, handling page boundary crossings.
pub fn read_guest_memory(
    mem: &GuestMemoryMmap<()>,
    cr3: u64,
    vaddr: u64,
    len: usize,
) -> Result<Vec<u8>, BreakpointError> {
    let mut result = Vec::with_capacity(len);
    let mut remaining = len;
    let mut addr = vaddr;

    while remaining > 0 {
        let paddr = translate_gva(mem, cr3, addr)?;
        // Bytes until next 2MB page boundary (most common in sumi)
        let page_offset = paddr as usize & 0x1F_FFFF;
        let page_remaining = 0x20_0000 - page_offset;
        let chunk = remaining.min(page_remaining);

        let mut buf = vec![0u8; chunk];
        mem.read_slice(&mut buf, GuestAddress(paddr))
            .map_err(|_| BreakpointError::BadAddress(addr))?;
        result.extend_from_slice(&buf);

        addr += chunk as u64;
        remaining -= chunk;
    }
    Ok(result)
}

/// Write guest memory by virtual address.
pub fn write_guest_memory(
    mem: &GuestMemoryMmap<()>,
    cr3: u64,
    vaddr: u64,
    data: &[u8],
) -> Result<(), BreakpointError> {
    let mut remaining = data.len();
    let mut addr = vaddr;
    let mut offset = 0;

    while remaining > 0 {
        let paddr = translate_gva(mem, cr3, addr)?;
        let page_offset = paddr as usize & 0x1F_FFFF;
        let page_remaining = 0x20_0000 - page_offset;
        let chunk = remaining.min(page_remaining);

        mem.write_slice(&data[offset..offset + chunk], GuestAddress(paddr))
            .map_err(|_| BreakpointError::BadAddress(addr))?;

        addr += chunk as u64;
        offset += chunk;
        remaining -= chunk;
    }
    Ok(())
}

#[derive(Debug)]
pub enum BreakpointError {
    BadAddress(u64),
    PageNotPresent(u64),
    NotFound(u64),
}

impl std::fmt::Display for BreakpointError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadAddress(a) => write!(f, "bad guest address {:#x}", a),
            Self::PageNotPresent(a) => write!(f, "page not present for {:#x}", a),
            Self::NotFound(a) => write!(f, "no breakpoint at {:#x}", a),
        }
    }
}
