/// Kernel selftests — run inside the actual kernel under KVM.
///
/// Each suite runs if its preconditions are met. Suites are independent.
use crate::{kprint, kprintln};
use crate::fs::virtio_fs::VirtioFsClient;
use crate::syscall::{SyscallArgs, syscall_dispatch};

mod fd_table;
mod syscalls;
mod virtio;

pub(crate) struct SelfTest {
    pub name: &'static str,
    pub func: fn() -> bool,
}

fn run_suite(name: &str, tests: &[SelfTest]) -> bool {
    kprintln!("[suite] {}", name);

    let mut all_passed = true;
    for test in tests {
        kprint!("  {} ... ", test.name);
        if (test.func)() {
            kprintln!("ok");
        } else {
            kprintln!("FAIL");
            all_passed = false;
        }
    }
    all_passed
}

/// Run all selftest suites. Each suite checks its own preconditions.
pub fn run_all() -> bool {
    kprintln!("\n=== kernel selftests ===");
    let mut all_passed = true;

    all_passed &= run_suite("fd_table", &fd_table::TESTS);
    all_passed &= run_suite("syscall_io", &syscalls::io::TESTS);

    if crate::VIRTIO_FS.get().is_some() {
        all_passed &= run_suite("virtio_fs", &virtio::fs::TESTS);
        all_passed &= run_suite("syscall_fs", &syscalls::fs::TESTS);
    } else {
        kprintln!("[suite] virtio_fs ... skipped (no device)");
        kprintln!("[suite] syscall_fs ... skipped (no device)");
    }

    if all_passed {
        kprintln!("=== all tests passed ===");
    } else {
        kprintln!("=== SOME TESTS FAILED ===");
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
