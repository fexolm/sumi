//! Runtime helpers for sumi integration tests.
//!
//! Each `data/<name>.rs` file is compiled by `build.rs` into a static
//! Linux ELF binary that boots inside sumi via the `--run` flag. The
//! `tests/test_launcher.rs` test binary calls [`run_test`] for each
//! generated binary, asserting that the program exits with code 0.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

/// Maximum wall-clock time a single integration test may run inside sumi-vm.
/// A bug that hangs the kernel (e.g. futex wait on a never-written word,
/// busy-loop with no exit syscall) would otherwise block `cargo test` forever.
const TEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Exit marker emitted by the kernel's debugcon path. The test harness
/// scans for the *last* occurrence, so a user program that prints the
/// literal string to its own stdout cannot fake-pass: the kernel always
/// prints this line after the user exits, so it is always last.
const EXIT_MARKER_PREFIX: &str = "[exit] code=";

/// Workspace root directory (parent of this crate's manifest).
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crate has a workspace parent")
        .to_path_buf()
}

fn kernel_bin() -> PathBuf {
    workspace_root().join("target/x86_64-unknown-none/debug/sumi-kernel")
}

fn vm_bin() -> PathBuf {
    workspace_root().join("target/debug/sumi-vm")
}

/// Build sumi-kernel + sumi-vm exactly once per test process.
fn ensure_built() {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        let root = workspace_root();
        let status = Command::new("cargo")
            .args([
                "build",
                "-p",
                "sumi-kernel",
                "--target",
                "x86_64-unknown-none",
            ])
            .current_dir(&root)
            .status()
            .expect("failed to invoke cargo build for kernel");
        assert!(status.success(), "sumi-kernel build failed");

        let status = Command::new("cargo")
            .args(["build", "-p", "sumi-vm"])
            .current_dir(&root)
            .status()
            .expect("failed to invoke cargo build for sumi-vm");
        assert!(status.success(), "sumi-vm build failed");
    });
}

fn kvm_available() -> bool {
    Path::new("/dev/kvm").exists()
}

/// Path to the directory `build.rs` produced binaries into.
/// Provided as `OUT_DIR/bin`.
fn bin_dir() -> PathBuf {
    PathBuf::from(env!("BIN_DIR"))
}

/// Inspect captured stdout for the kernel's exit marker. Scans for the
/// *last* `[exit] code=N` occurrence (the kernel only emits one, right
/// before `halt_forever`) and returns `N`. Matching only the final
/// occurrence prevents a test program that prints the literal string to
/// its own stdout (virtio-console) from fooling the harness — the
/// kernel's debugcon line is always emitted after the user exits, so it
/// is always last in the stream.
fn find_last_exit_code(stdout: &str) -> Option<i32> {
    let idx = stdout.rfind(EXIT_MARKER_PREFIX)?;
    let rest = &stdout[idx + EXIT_MARKER_PREFIX.len()..];
    let end = rest.find(['\n', '\r']).unwrap_or(rest.len());
    rest[..end].trim().parse::<i32>().ok()
}

/// Run a child process and return `(stdout, stderr, timed_out)`, killing
/// it after `timeout` if it has not exited. Captures both streams in full
/// via parallel reader threads so neither pipe fills up and blocks the
/// child.
fn run_with_timeout(mut cmd: Command, timeout: Duration) -> (String, String, bool) {
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("failed to spawn sumi-vm");

    let stdout = child.stdout.take().expect("stdout piped");
    let stderr = child.stderr.take().expect("stderr piped");
    let stdout_handle = thread::spawn(move || {
        let mut buf = String::new();
        let mut s = stdout;
        let _ = s.read_to_string(&mut buf);
        buf
    });
    let stderr_handle = thread::spawn(move || {
        let mut buf = String::new();
        let mut s = stderr;
        let _ = s.read_to_string(&mut buf);
        buf
    });

    // Watchdog: the wait thread sends once child.wait() returns.
    let (tx, rx) = mpsc::channel::<()>();
    let child_id = child.id();
    let wait_thread = thread::spawn(move || {
        let _ = child.wait();
        let _ = tx.send(());
        child
    });

    let timed_out = rx.recv_timeout(timeout).is_err();
    if timed_out {
        // SAFETY: kill(pid, SIGKILL) with a pid we just obtained from
        // Child::id() is always a well-defined libc call.
        unsafe {
            libc::kill(child_id as i32, libc::SIGKILL);
        }
    }
    let _ = wait_thread.join();

    let stdout = stdout_handle.join().unwrap_or_default();
    let stderr = stderr_handle.join().unwrap_or_default();
    (stdout, stderr, timed_out)
}

/// Like `run_test`, but passes `--vcpus N` to sumi-vm and asserts
/// that every CPU 1..N printed `[ap] cpu <id> online`. Used by the
/// SMP smoke test.
pub fn run_test_smp(name: &str, vcpus: u32) {
    run_guest(name, Some(vcpus), 0);
}

/// Like `run_test`, but asserts the kernel emitted `[exit] code=N`
/// where `N == expected`. Used by tests that exercise non-zero exit
/// codes (e.g. exit_seven for HC_SHUTDOWN code propagation).
pub fn run_test_expect_exit(name: &str, expected: i32) {
    run_guest(name, None, expected);
}

/// Like `run_test_smp`, but asserts on a specific non-zero exit code
/// in addition to the AP-online check.
pub fn run_test_smp_expect_exit(name: &str, vcpus: u32, expected: i32) {
    run_guest(name, Some(vcpus), expected);
}

/// Run a previously-compiled test binary inside sumi-vm and assert it
/// exits cleanly. The binary's stdout is captured and printed on failure
/// to aid debugging.
pub fn run_test(name: &str) {
    run_guest(name, None, 0);
}

fn run_guest(name: &str, vcpus: Option<u32>, expected_exit: i32) {
    if !kvm_available() {
        eprintln!("skipping {name}: /dev/kvm not available");
        return;
    }
    ensure_built();

    let host_bin = bin_dir().join(name);
    assert!(host_bin.exists(), "test binary {name} not found");
    let guest_path = host_bin.to_str().expect("non-UTF8 binary path");
    let mut cmd = Command::new(vm_bin());
    cmd.arg("run").arg(kernel_bin()).arg("--share").arg("/");
    if let Some(vcpus) = vcpus {
        cmd.arg("--vcpus").arg(vcpus.to_string());
    }
    cmd.arg("--run").arg(guest_path);

    let (stdout, stderr, timed_out) = run_with_timeout(cmd, TEST_TIMEOUT);
    let exit_code = find_last_exit_code(&stdout);
    let missing = vcpus
        .map(|n| {
            (1..n)
                .filter(|cpu_id| !stdout.contains(&format!("[ap] cpu {cpu_id} online")))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    if timed_out || exit_code != Some(expected_exit) || !missing.is_empty() {
        eprintln!("--- {name} stdout ---\n{stdout}");
        eprintln!("--- {name} stderr ---\n{stderr}");
        let kind = if vcpus.is_some() { "smp test" } else { "test" };
        if timed_out {
            panic!("{kind} {name} timed out after {TEST_TIMEOUT:?}");
        }
        if !missing.is_empty() {
            panic!("smp test {name}: missing APs online: {missing:?}");
        }
        match exit_code {
            None => panic!("{kind} {name} did not emit an [exit] code= line"),
            Some(code) => {
                panic!("{kind} {name} exited with code={code} (expected {expected_exit})")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn last_exit_code_parses_kernel_marker() {
        for (stdout, expected) in [
            (
                "hello\n[exit] code=0 (printed by user)\nmore output\n[exit] code=42\n",
                Some(42),
            ),
            ("[exit] code=0\n", Some(0)),
            ("no exit marker\n", None),
            ("[exit] code=7", Some(7)),
        ] {
            assert_eq!(find_last_exit_code(stdout), expected);
        }
    }
}
