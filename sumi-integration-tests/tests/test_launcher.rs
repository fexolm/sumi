// One #[test] function per binary in `data/`. The test set is generated
// at build time by `build.rs` and included from $OUT_DIR/generated_tests.rs.

include!(concat!(env!("OUT_DIR"), "/generated_tests.rs"));

// SMP smoke test: boot exit_zero with --vcpus 4 and verify every AP
// announces itself in kprintln output.
#[test]
fn smp_phase1_four_vcpus() {
    sumi_integration_tests::run_test_smp("syscalls/exit_zero", 4);
}

// Single-vCPU shutdown via HC_SHUTDOWN. Verifies that `exit_group(7)`
// propagates the exit code through the hypercall MMIO trap path. The
// auto-generated #[test] for exit_seven is suppressed via build.rs
// `MANUAL_SYSCALL_TESTS`.
#[test]
fn exit_seven() {
    sumi_integration_tests::run_test_expect_exit("syscalls/exit_seven", 7);
}

// Multi-vCPU shutdown via HC_SHUTDOWN. The BSP issues HC_SHUTDOWN(7)
// and the host's HypercallContext fans out SIGUSR1 to all 3 APs. The
// harness asserts both that all APs came online and that the final exit
// code is 7.
#[test]
fn smp_phase2_exit_seven() {
    sumi_integration_tests::run_test_smp_expect_exit("syscalls/exit_seven", 4, 7);
}

// exit_group() must kill every thread in the VM, not just the caller. The
// auto-generated #[test] is suppressed via build.rs `MANUAL_SYSCALL_TESTS`
// because it asserts a non-zero exit code.
#[test]
fn exit_group_kills_all() {
    sumi_integration_tests::run_test_expect_exit("syscalls/exit_group_kills_all", 7);
}

// Timer preemption test: a busy-loop thread must be preempted by the LAPIC
// timer so a worker thread can make progress. Requires >= 2 vCPUs so the
// busy-loop thread and worker contend on CPU 1 while main sleeps on CPU 0.
// Without timer preemption the worker never runs.
#[test]
fn preempt_timer() {
    sumi_integration_tests::run_test_smp("glibc/preempt_timer", 2);
}

// TCP + epoll loopback echo (Phase 1 networking, see
// docs/networking-design.md). Pinned to a single vCPU: the net stack's
// blocking/wakeup design relies on `poll_and_wake`'s poll-before-block step
// delivering loopback traffic synchronously, which only avoids an actual
// park on a single vCPU (see `net::wait::net_wait`'s doc comment). The
// auto-generated #[test] is suppressed via build.rs `MANUAL_SYSCALL_TESTS`
// because the default vCPU count comes from the host's core count, not 1.
#[test]
fn tcp_epoll_loopback() {
    sumi_integration_tests::run_test_smp("syscalls/tcp_epoll_loopback", 1);
}
