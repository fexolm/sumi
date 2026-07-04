//! Interrupt-safe locking primitive (F5/F13).
//!
//! `docs/design/multithreading-fixes.md` F5: the runqueue and zombie-list
//! locks are reachable both from ordinary syscall context and from the
//! timer ISR's `schedule_preempt` path. Every kernel entry point already
//! clears IF (`SFMASK` on `syscall`, interrupt gates on ISRs) and
//! `__switch_to_asm` now keeps IF at 0 across the whole switch (F2), so in
//! practice IF is always already 0 at every call site that takes these
//! locks. `IrqGuard` makes that invariant load-bearing rather than
//! incidental: it is real `cli`/`sti`-bracketed irqsave locking, so a
//! future call site that (incorrectly) holds one of these locks with IF=1
//! cannot self-deadlock via a timer tick re-entering `schedule_preempt`
//! while the lock is held.
//!
//! `cli`/`sti`/reading RFLAGS are ring-0-only (they no-op or `#GP` at ring
//! 3, same constraint as `percpu::init_for_cpu`'s MSR writes), so the actual
//! instructions are gated `#[cfg(not(test))]`; the type and its call sites
//! are identical in both configs, and `cargo test` exercises every other
//! consumer's real logic — only the CPU-privileged flag toggle is skipped.

/// RAII guard: disables interrupts on construction, restores the prior IF
/// state on drop. Bracket a `spin::Mutex::lock()` with one to make that
/// lock irqsave.
pub struct IrqGuard {
    #[cfg(not(test))]
    was_enabled: bool,
}

impl IrqGuard {
    #[cfg(not(test))]
    #[inline]
    pub fn new() -> Self {
        let flags: u64;
        // SAFETY: `pushfq`/`pop` reads RFLAGS into a GPR without touching
        // memory beyond the local stack slot the compiler picks; `cli`
        // disables interrupts. Ring 0, no other side effects.
        unsafe {
            core::arch::asm!(
                "pushfq",
                "pop {flags}",
                "cli",
                flags = out(reg) flags,
                options(preserves_flags),
            );
        }
        const IF_BIT: u64 = 1 << 9;
        Self {
            was_enabled: (flags & IF_BIT) != 0,
        }
    }

    #[cfg(test)]
    #[inline]
    pub fn new() -> Self {
        Self {}
    }
}

impl Default for IrqGuard {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(not(test))]
impl Drop for IrqGuard {
    #[inline]
    fn drop(&mut self) {
        if self.was_enabled {
            // SAFETY: ring 0; re-enables interrupts that were on before
            // this guard was constructed. No memory/stack side effects.
            unsafe {
                core::arch::asm!("sti", options(nomem, nostack, preserves_flags));
            }
        }
    }
}

/// Read the current value of RFLAGS.IF. Used by `schedule()`'s debug
/// assertion (F13) in place of the vacuous `preempt_count` check: every
/// path that can reach `schedule()` must already have interrupts disabled.
#[cfg(not(test))]
#[inline]
pub fn interrupts_enabled() -> bool {
    let flags: u64;
    // SAFETY: reads RFLAGS into a GPR; no memory/stack side effects.
    unsafe {
        core::arch::asm!(
            "pushfq",
            "pop {flags}",
            flags = out(reg) flags,
            options(preserves_flags),
        );
    }
    (flags & (1 << 9)) != 0
}
