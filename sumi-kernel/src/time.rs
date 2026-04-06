use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

static TSC_FREQ_KHZ: AtomicU32 = AtomicU32::new(0);
static BOOT_TSC: AtomicU64 = AtomicU64::new(0);
static WALL_CLOCK_SEC: AtomicU64 = AtomicU64::new(0);
static WALL_CLOCK_NSEC: AtomicU32 = AtomicU32::new(0);

pub fn init(tsc_freq_khz: u32, wall_clock_sec: u64, wall_clock_nsec: u32) {
    // Clamp nsec to valid range — the hypervisor should never send an invalid
    // value, but guard against it to avoid arithmetic issues in clock_realtime.
    let nsec = if wall_clock_nsec >= 1_000_000_000 { 0 } else { wall_clock_nsec };
    // Single-threaded init — Relaxed ordering is sufficient.
    // If multi-vCPU support is added, use Release here and Acquire on reads.
    TSC_FREQ_KHZ.store(tsc_freq_khz, Ordering::Relaxed);
    BOOT_TSC.store(rdtsc(), Ordering::Relaxed);
    WALL_CLOCK_SEC.store(wall_clock_sec, Ordering::Relaxed);
    WALL_CLOCK_NSEC.store(nsec, Ordering::Relaxed);
}

#[cfg(not(test))]
#[inline(always)]
pub fn rdtsc() -> u64 {
    let lo: u32;
    let hi: u32;
    // SAFETY: RDTSC is always available in long mode.
    unsafe {
        core::arch::asm!(
            "rdtsc",
            out("eax") lo,
            out("edx") hi,
            options(nomem, nostack, preserves_flags),
        );
    }
    ((hi as u64) << 32) | (lo as u64)
}

#[cfg(test)]
static MOCK_TSC: AtomicU64 = AtomicU64::new(0);

#[cfg(test)]
pub fn rdtsc() -> u64 {
    MOCK_TSC.load(Ordering::Relaxed)
}

#[cfg(test)]
pub fn set_mock_tsc(val: u64) {
    MOCK_TSC.store(val, Ordering::Relaxed);
}

/// Nanoseconds elapsed since boot. Uses u128 intermediate to avoid overflow.
pub fn monotonic_ns() -> u64 {
    let freq = TSC_FREQ_KHZ.load(Ordering::Relaxed) as u64;
    if freq == 0 {
        return 0;
    }
    let delta = rdtsc().wrapping_sub(BOOT_TSC.load(Ordering::Relaxed));
    let whole_ms = delta / freq;
    let remainder = delta % freq;
    let ns = (whole_ms as u128 * 1_000_000u128)
        + ((remainder as u128 * 1_000_000u128) / freq as u128);
    ns as u64
}

/// CLOCK_MONOTONIC: (seconds, nanoseconds)
pub fn clock_monotonic() -> (u64, u32) {
    let ns = monotonic_ns();
    (ns / 1_000_000_000, (ns % 1_000_000_000) as u32)
}

/// CLOCK_REALTIME: (seconds, nanoseconds)
pub fn clock_realtime() -> (u64, u32) {
    let elapsed_ns = monotonic_ns();
    let boot_sec = WALL_CLOCK_SEC.load(Ordering::Relaxed);
    let boot_nsec = WALL_CLOCK_NSEC.load(Ordering::Relaxed) as u64;
    // Use u128 intermediate to avoid overflow when elapsed_ns is large.
    let total_nsec = boot_nsec as u128 + elapsed_ns as u128;
    let extra_sec = (total_nsec / 1_000_000_000) as u64;
    let nsec = (total_nsec % 1_000_000_000) as u32;
    (boot_sec.wrapping_add(extra_sec), nsec)
}

/// TSC resolution in nanoseconds. Minimum 1.
pub fn clock_resolution_ns() -> u64 {
    let freq = TSC_FREQ_KHZ.load(Ordering::Relaxed) as u64;
    if freq == 0 {
        return 1;
    }
    // 1 tick = 1_000_000 / freq_khz nanoseconds
    let res = 1_000_000 / freq;
    if res == 0 { 1 } else { res }
}

/// Busy-wait for at least `ns` nanoseconds.
pub fn busy_wait_ns(ns: u64) {
    let freq = TSC_FREQ_KHZ.load(Ordering::Relaxed) as u64;
    if freq == 0 {
        return;
    }
    let start = rdtsc();
    // ticks = ns * freq_khz / 1_000_000
    let ticks = (ns as u128 * freq as u128 / 1_000_000u128) as u64;
    while rdtsc().wrapping_sub(start) < ticks {
        core::hint::spin_loop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Each test must call set_mock_tsc before init(), because init() reads
    // rdtsc() to capture BOOT_TSC. The global state is shared across threads,
    // so tests are sequential in their own assertions after set+init.

    #[test]
    fn test_monotonic_ns_zero_at_boot() {
        // When the mock TSC does not advance between init() and monotonic_ns(),
        // the elapsed delta is zero.
        set_mock_tsc(1_000_000);
        init(1_000_000, 0, 0); // 1 GHz
        // Mock TSC is still 1_000_000 — no advance — delta = 0.
        assert_eq!(
            monotonic_ns(),
            0,
            "monotonic_ns must be 0 when TSC has not advanced since boot"
        );
    }

    #[test]
    fn test_monotonic_ns_basic() {
        // freq = 1000 kHz = 1 MHz.  1_000_000 ticks => 1 second = 1_000_000_000 ns.
        let boot = 500_000_000u64;
        set_mock_tsc(boot);
        init(1000, 0, 0);
        set_mock_tsc(boot + 1_000_000);
        assert_eq!(
            monotonic_ns(),
            1_000_000_000,
            "1M ticks at 1 MHz must equal 1 second in nanoseconds"
        );
    }

    #[test]
    fn test_monotonic_ns_large_delta() {
        // freq = 3_000_000 kHz = 3 GHz (3e9 ticks/second).
        // delta = 3_000_000_000_000_000_000 ticks = 3e18 / 3e9 = 1e9 seconds = 1e18 ns.
        // 3e18 < u64::MAX (~1.84e19), so the delta fits in u64.
        let boot = 0u64;
        set_mock_tsc(boot);
        init(3_000_000, 0, 0);
        set_mock_tsc(boot + 3_000_000_000_000_000_000u64);
        let expected_ns: u64 = 1_000_000_000_000_000_000u64; // 1e18 ns = 1e9 seconds
        assert_eq!(
            monotonic_ns(),
            expected_ns,
            "large TSC delta must not overflow: expected 1e18 ns"
        );
    }

    #[test]
    fn test_clock_realtime() {
        // wall_clock_sec=1700000000, wall_clock_nsec=500_000_000.
        // freq=1000 kHz (1 MHz = 1_000_000 ticks/second).
        // After 1_000_000 ticks => 1 second elapsed.
        // total_nsec = 500_000_000 + 1_000_000_000 = 1_500_000_000
        // => extra_sec=1, nsec=500_000_000 => sec=1_700_000_001
        let boot = 100_000u64;
        set_mock_tsc(boot);
        init(1000, 1_700_000_000, 500_000_000);
        set_mock_tsc(boot + 1_000_000); // 1 second elapsed at 1 MHz
        let (sec, nsec) = clock_realtime();
        assert_eq!(
            sec, 1_700_000_001,
            "realtime seconds must advance by 1 after 1 second elapsed"
        );
        assert_eq!(
            nsec, 500_000_000,
            "realtime nanosecond fraction must carry over unchanged"
        );
    }

    #[test]
    fn test_clock_resolution_high_freq() {
        // At 3 GHz (3_000_000 kHz), 1 tick = 1_000_000 / 3_000_000 ns < 1,
        // so resolution rounds up to 1.
        set_mock_tsc(0);
        init(3_000_000, 0, 0);
        assert_eq!(
            clock_resolution_ns(),
            1,
            "sub-nanosecond TSC tick must round up to 1 ns resolution"
        );
    }

    #[test]
    fn test_clock_resolution_low_freq() {
        // At 1000 kHz (1 MHz), 1 tick = 1_000_000 / 1000 = 1000 ns.
        set_mock_tsc(0);
        init(1000, 0, 0);
        assert_eq!(
            clock_resolution_ns(),
            1000,
            "1 MHz TSC must report 1000 ns per tick resolution"
        );
    }

    #[test]
    fn test_freq_zero_no_panic() {
        // freq=0: all time functions must return 0 or safe defaults without panicking.
        set_mock_tsc(0);
        init(0, 0, 0);
        assert_eq!(monotonic_ns(), 0, "monotonic_ns with freq=0 must return 0");
        let (sec, nsec) = clock_monotonic();
        assert_eq!(sec, 0, "clock_monotonic sec with freq=0 must be 0");
        assert_eq!(nsec, 0, "clock_monotonic nsec with freq=0 must be 0");
        // clock_resolution_ns returns 1 when freq=0 (documented safe default)
        assert_eq!(
            clock_resolution_ns(),
            1,
            "clock_resolution_ns with freq=0 must return 1"
        );
    }

    #[test]
    fn test_busy_wait_returns_with_zero_freq() {
        // With freq=0 busy_wait_ns must return immediately rather than spinning forever.
        set_mock_tsc(0);
        init(0, 0, 0);
        busy_wait_ns(1_000_000_000); // would hang if freq=0 not handled
    }

    #[test]
    fn test_init_clamps_invalid_nsec() {
        // wall_clock_nsec >= 1_000_000_000 is invalid; init must clamp it to 0.
        set_mock_tsc(0);
        init(1000, 0, 2_000_000_000);
        assert_eq!(
            WALL_CLOCK_NSEC.load(Ordering::Relaxed),
            0,
            "wall_clock_nsec >= 1e9 must be clamped to 0 by init"
        );
    }
}
