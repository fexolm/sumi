/// Kernel selftests — run inside the actual kernel under KVM.
///
/// Each suite runs if its preconditions are met. Suites are independent.
use crate::arch::debugcon_write_byte;
use crate::fs::virtio_fs::VirtioFsClient;
use crate::syscall::{SyscallArgs, syscall_dispatch};

mod fd_table;
mod syscalls;
mod virtio;

pub(crate) struct SelfTest {
    pub name: &'static str,
    pub func: fn() -> bool,
}

pub(crate) fn debugcon_puts(s: &str) {
    for &b in s.as_bytes() {
        debugcon_write_byte(b);
    }
}

fn run_suite(name: &str, tests: &[SelfTest]) -> bool {
    debugcon_puts("[suite] ");
    debugcon_puts(name);
    debugcon_puts("\n");

    let mut all_passed = true;
    for test in tests {
        debugcon_puts("  ");
        debugcon_puts(test.name);
        debugcon_puts(" ... ");
        if (test.func)() {
            debugcon_puts("ok\n");
        } else {
            debugcon_puts("FAIL\n");
            all_passed = false;
        }
    }
    all_passed
}

/// Run all selftest suites. Each suite checks its own preconditions.
pub fn run_all() -> bool {
    debugcon_puts("\n=== kernel selftests ===\n");
    let mut all_passed = true;

    all_passed &= run_suite("fd_table", &fd_table::TESTS);
    all_passed &= run_suite("syscall_io", &syscalls::io::TESTS);

    if crate::VIRTIO_FS.get().is_some() {
        all_passed &= run_suite("virtio_fs", &virtio::fs::TESTS);
        all_passed &= run_suite("syscall_fs", &syscalls::fs::TESTS);
    } else {
        debugcon_puts("[suite] virtio_fs ... skipped (no device)\n");
        debugcon_puts("[suite] syscall_fs ... skipped (no device)\n");
    }

    if all_passed {
        debugcon_puts("=== all tests passed ===\n");
    } else {
        debugcon_puts("=== SOME TESTS FAILED ===\n");
    }
    all_passed
}

// ── helpers used by submodules ──────────────────────────────────

pub(crate) fn fs() -> &'static VirtioFsClient {
    crate::VIRTIO_FS.get().unwrap()
}

pub(crate) fn syscall(nr: u64, a0: u64, a1: u64, a2: u64) -> i64 {
    syscall_dispatch(&SyscallArgs {
        nr,
        arg0: a0,
        arg1: a1,
        arg2: a2,
        arg3: 0,
        arg4: 0,
        arg5: 0,
    })
}

pub(crate) fn syscall6(nr: u64, a0: u64, a1: u64, a2: u64, a3: u64, a4: u64, a5: u64) -> i64 {
    syscall_dispatch(&SyscallArgs {
        nr,
        arg0: a0,
        arg1: a1,
        arg2: a2,
        arg3: a3,
        arg4: a4,
        arg5: a5,
    })
}
