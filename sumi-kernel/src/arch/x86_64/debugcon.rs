use super::debugcon_write_byte;
use core::fmt;
use spin::Mutex;

struct DebugconWriter;

impl fmt::Write for DebugconWriter {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for &b in s.as_bytes() {
            debugcon_write_byte(b);
        }
        Ok(())
    }
}

// Serialise concurrent `kprintln!` calls from multiple CPUs so bytes
// from different lines don't interleave. A spin lock is appropriate
// here: debugcon output is rare (a few lines per boot) and contention
// is negligible.
static DEBUGCON_LOCK: Mutex<()> = Mutex::new(());

#[doc(hidden)]
pub fn _print(args: fmt::Arguments) {
    use fmt::Write;
    let _guard = DEBUGCON_LOCK.lock();
    DebugconWriter.write_fmt(args).ok();
}

/// Force-release the debugcon serialization lock.
///
/// Used by the kernel panic handler to recover from a panic that
/// occurred mid-print. Calling this from anywhere else is a bug.
///
/// # Safety
///
/// Caller must guarantee that no other CPU currently holds the lock
/// with intent to drop it normally — i.e. only the panic handler
/// may call this.
pub unsafe fn force_unlock_debugcon() {
    // SAFETY: caller guarantees the panic handler is the only writer
    // to the lock at this point. spin::Mutex::force_unlock() is the
    // approved API for panic-handler recovery.
    unsafe { DEBUGCON_LOCK.force_unlock() };
}

#[macro_export]
macro_rules! kprint {
    ($($arg:tt)*) => {
        $crate::arch::debugcon::_print(format_args!($($arg)*))
    };
}

#[macro_export]
macro_rules! kprintln {
    () => { $crate::kprint!("\n") };
    ($($arg:tt)*) => {
        $crate::kprint!("{}\n", format_args!($($arg)*))
    };
}
