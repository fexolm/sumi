use crate::syscall::errno::*;
use crate::syscall::{SyscallArgs, SyscallResult};

pub fn sys_getrandom(args: &SyscallArgs) -> SyscallResult {
    let buf = args.arg0 as *mut u8;
    let buflen = args.arg1 as usize;
    let _flags = args.arg2 as u32;
    if buf.is_null() {
        return EFAULT;
    }
    fill_random(buf, buflen)
}

#[cfg(not(test))]
fn rdrand64() -> Option<u64> {
    let val: u64;
    let ok: u8;
    // SAFETY: RDRAND is available on Sandy Bridge+ CPUs under KVM.
    unsafe {
        core::arch::asm!(
            "rdrand {val}",
            "setc {ok}",
            val = out(reg) val,
            ok = out(reg_byte) ok,
            options(nomem, nostack),
        );
    }
    if ok != 0 { Some(val) } else { None }
}

// In tests rdrand is not available; returning None exercises the fallback path.
#[cfg(test)]
fn rdrand64() -> Option<u64> {
    None
}

fn fill_random(buf: *mut u8, len: usize) -> SyscallResult {
    let mut offset = 0usize;
    while offset < len {
        let remaining = len - offset;
        let mut val = None;
        for _ in 0..10 {
            if let Some(v) = rdrand64() {
                val = Some(v);
                break;
            }
        }
        let bytes = match val {
            Some(v) => v.to_le_bytes(),
            None => {
                // Last-resort, non-cryptographic fallback: derive bytes from the
                // RNG seed supplied by the hypervisor at boot. This is used when
                // RDRAND is unavailable (e.g. in tests or on older hardware).
                let seed = match crate::RNG_SEED.get() {
                    Some(s) => s,
                    None => return offset as SyscallResult,
                };

                let idx = offset % 32;
                let mut b = [0u8; 8];
                for i in 0..8 {
                    b[i] = seed[(idx + i) % 32] ^ (offset as u8).wrapping_add(i as u8);
                }
                b
            }
        };
        let chunk = core::cmp::min(remaining, 8);
        // SAFETY: The caller (user program running in ring-0) is responsible for
        // passing valid, aligned pointers. Invalid pointers will cause a fault.
        unsafe { core::ptr::copy_nonoverlapping(bytes.as_ptr(), buf.add(offset), chunk) };
        offset += chunk;
    }
    len as SyscallResult
}

#[cfg(test)]
mod tests {
    use super::*;

    // RNG_SEED is a spin::Once — it can only be initialized once per process.
    // We initialize it once here via call_once; subsequent calls are no-ops.
    fn ensure_rng_seed() {
        crate::RNG_SEED.call_once(|| [0x42u8; 32]);
    }

    #[test]
    fn test_getrandom_zero_len() {
        ensure_rng_seed();
        // A zero-length buffer request must return 0 immediately.
        let args = SyscallArgs {
            nr: 318,
            arg0: 0x1000, // non-null pointer (never dereferenced when len=0)
            arg1: 0,
            arg2: 0,
            arg3: 0,
            arg4: 0,
            arg5: 0,
        };
        assert_eq!(
            sys_getrandom(&args),
            0,
            "sys_getrandom with buflen=0 must return 0"
        );
    }

    #[test]
    fn test_getrandom_null_buf() {
        // A null buffer pointer must return EFAULT regardless of length.
        let args = SyscallArgs {
            nr: 318,
            arg0: 0, // null
            arg1: 64,
            arg2: 0,
            arg3: 0,
            arg4: 0,
            arg5: 0,
        };
        assert_eq!(
            sys_getrandom(&args),
            EFAULT,
            "sys_getrandom with null buf must return EFAULT"
        );
    }

    #[test]
    fn test_getrandom_fills_buffer() {
        ensure_rng_seed();
        // In test mode rdrand64 returns None, so the fallback path is exercised.
        // The buffer must be filled with non-zero bytes (seed is 0x42, not zero).
        let mut buf = [0u8; 64];
        let result = fill_random(buf.as_mut_ptr(), 64);
        assert_eq!(result, 64, "fill_random must return the number of bytes filled");
        assert!(
            buf.iter().any(|&b| b != 0),
            "fill_random must modify the buffer — all-zero output suggests fallback did not run"
        );
    }

    #[test]
    fn test_getrandom_various_sizes() {
        ensure_rng_seed();
        // Verify fill_random succeeds and returns correct counts for various sizes,
        // including sizes that straddle 8-byte chunk boundaries.
        for &size in &[1usize, 7, 8, 16, 31, 32, 33, 64] {
            let mut buf = vec![0u8; size];
            let result = fill_random(buf.as_mut_ptr(), size);
            assert_eq!(
                result, size as i64,
                "fill_random({size}) must return {size}"
            );
        }
    }
}
