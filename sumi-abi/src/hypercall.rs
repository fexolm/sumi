//! Hypercall ABI shared between sumi-vm and sumi-kernel.
//!
//! Wire format: each hypercall is invoked by a single 8-byte little-endian
//! MMIO write to `HYPERCALL_MMIO_BASE + offset` (defined in
//! `sumi_abi::arch::layout`). The offset is the hypercall selector and
//! the data is the single 64-bit argument. The host (sumi-vm) traps the
//! MMIO write, decodes `(offset, data)`, and acts. There is no return
//! value channel: hypercalls that need to "return" do so by side effect
//! (HC_SHUTDOWN tears down the VM; HC_KICK_CPU is fire-and-forget).
//!
//! See `docs/design/multithreading-v2.md` for the host/guest scheduler use.

/// Wake the target vCPU out of `KVM_RUN`. Argument: target cpu_id.
/// Used by the in-guest scheduler to deliver IPI-like wakes to an
/// idle peer.
pub const HC_KICK_CPU: usize = 0x00;

// Offset 0x08 previously held HC_TLB_FLUSH, removed as dead code (no
// callers — TLB staleness is instead handled by the lazy
// generation-counter reload in `PerCpu::reload_tlb_if_stale`). Left
// unassigned rather than reused so a stale host/guest pairing fails loudly
// instead of silently misinterpreting an old build's hypercall. Re-add a
// real cross-CPU TLB-shootdown hypercall here if it becomes necessary.

/// Terminate the VM with the given exit code. Argument: i32 exit
/// code, zero-extended into the low 32 bits of the u64. The host
/// signals every peer vCPU out of KVM_RUN, joins them, and exits the
/// sumi-vm process with `code`.
pub const HC_SHUTDOWN: usize = 0x10;

/// Total reserved span of the hypercall MMIO range. One 4 KiB page
/// is enough for 512 hypercalls at the 8-byte stride. Reserved as a
/// single page so the host can range-check `addr` cheaply.
pub const HYPERCALL_MMIO_SIZE: usize = 0x1000;

/// Stride between consecutive hypercall slots, in bytes. A future
/// hypercall that needs more than one 64-bit argument can occupy
/// multiple stride slots, but the dispatcher matches on the offset
/// of the *trigger* write, so the natural way to grow the ABI is to
/// add new selector values that round to the next multiple of 8.
pub const HC_STRIDE: usize = 0x08;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offsets_are_distinct_and_aligned() {
        let offsets = [HC_KICK_CPU, HC_SHUTDOWN];
        for &o in &offsets {
            assert!(o.is_multiple_of(HC_STRIDE), "offset {o:#x} not aligned");
            assert!(o < HYPERCALL_MMIO_SIZE, "offset {o:#x} out of range");
        }
        // No duplicate offsets.
        let mut sorted = offsets;
        sorted.sort();
        for w in sorted.windows(2) {
            assert_ne!(w[0], w[1], "duplicate hypercall offset {:#x}", w[0]);
        }
    }

    #[test]
    fn shutdown_offset_matches_doc() {
        // Hard-coded values to catch accidental renumbering.
        assert_eq!(HC_KICK_CPU, 0x00);
        assert_eq!(HC_SHUTDOWN, 0x10);
    }

    #[test]
    fn hypercall_mmio_size_is_one_page() {
        assert_eq!(HYPERCALL_MMIO_SIZE, 0x1000);
    }
}
