//! Guest-side hypercall stubs.
//!
//! Each hypercall is a single 8-byte volatile MMIO write to
//! `HYPERCALL_MMIO_BASE + offset`. The MMIO page is unmapped on the
//! host side, so the write traps as `VcpuExit::MmioWrite` and is
//! decoded by `sumi_vm::vm::HypercallContext`. See
//! `sumi_abi::hypercall` for the wire format and
//! `docs/design/multithreading-v2.md` §4.5 / §13 Phase 2.

use sumi_abi::arch::layout::HYPERCALL_MMIO_BASE;
use sumi_abi::hypercall::{HC_KICK_CPU, HC_SHUTDOWN};

use crate::KERNEL_DIRECT_MAP;

#[inline(always)]
fn raw_hypercall(offset: usize, arg: u64) {
    let vaddr = HYPERCALL_MMIO_BASE.add(offset).to_virtual(&KERNEL_DIRECT_MAP);
    // SAFETY: HYPERCALL_MMIO_BASE is a fixed physical address inside
    // the direct map (set up by sumi-vm with 1 GiB huge pages
    // covering the full 128 TB physical range). The page is
    // intentionally outside any KVM memslot, so the write traps to
    // VcpuExit::MmioWrite and the host dispatches based on the
    // (addr - HYPERCALL_MMIO_BASE) offset. No memory is actually
    // accessed; the write is solely a trap vehicle.
    unsafe {
        core::ptr::write_volatile(vaddr.as_ptr::<u64>(), arg);
    }
}

/// Wake `target_cpu_id` out of `KVM_RUN`. No-op if the target is
/// not currently parked. Used by the scheduler IPI path (Phase 3+).
#[inline]
pub fn kick_cpu(target_cpu_id: u32) {
    raw_hypercall(HC_KICK_CPU, target_cpu_id as u64);
}

/// Terminate the VM with the given exit code. Never returns: the
/// host tears down all vCPU threads. The post-write `loop { hlt }`
/// is a safety net for the case where the host fails to act on the
/// hypercall (e.g. a future bug in `HypercallContext::dispatch_mmio`).
#[inline]
pub fn shutdown(exit_code: i32) -> ! {
    raw_hypercall(HC_SHUTDOWN, exit_code as u32 as u64);
    loop {
        // SAFETY: ring-0 hlt with no memory access; if we ever
        // execute this it means the host did not terminate the VM
        // and we're parking until it does (or a SIGUSR1 from a
        // misbehaving peer wakes us up).
        unsafe {
            core::arch::asm!("hlt", options(nomem, nostack));
        }
    }
}
