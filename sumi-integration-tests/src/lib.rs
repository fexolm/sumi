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
/// Phase 1 SMP smoke test.
pub fn run_test_smp(name: &str, vcpus: u32) {
    if !kvm_available() {
        eprintln!("skipping {name} (smp): /dev/kvm not available");
        return;
    }
    ensure_built();

    let host_bin = bin_dir().join(name);
    assert!(host_bin.exists(), "test binary {name} not found");

    let guest_path = host_bin.to_str().expect("non-UTF8 binary path");

    let mut cmd = Command::new(vm_bin());
    cmd.arg("run")
        .arg(kernel_bin())
        .arg("--share")
        .arg("/")
        .arg("--vcpus")
        .arg(vcpus.to_string())
        .arg("--run")
        .arg(guest_path);

    let (stdout, stderr, timed_out) = run_with_timeout(cmd, TEST_TIMEOUT);

    let exit_code = find_last_exit_code(&stdout);
    let passed = !timed_out && exit_code == Some(0);

    // Every AP must have announced itself.
    let mut missing = Vec::new();
    for cpu_id in 1..vcpus {
        let needle = format!("[ap] cpu {} online", cpu_id);
        if !stdout.contains(&needle) {
            missing.push(cpu_id);
        }
    }

    if !passed || !missing.is_empty() {
        eprintln!("--- {name} stdout ---\n{stdout}");
        eprintln!("--- {name} stderr ---\n{stderr}");
        if timed_out {
            panic!("smp test {name} timed out after {TEST_TIMEOUT:?}");
        }
        if !missing.is_empty() {
            panic!("smp test {name}: missing APs online: {:?}", missing);
        }
        match exit_code {
            None => panic!("smp test {name} did not emit an [exit] code= line"),
            Some(code) => panic!("smp test {name} exited with code={code} (expected 0)"),
        }
    }
}

/// Like `run_test`, but asserts the kernel emitted `[exit] code=N`
/// where `N == expected`. Used by tests that exercise non-zero exit
/// codes (e.g. exit_seven for HC_SHUTDOWN code propagation).
pub fn run_test_expect_exit(name: &str, expected: i32) {
    if !kvm_available() {
        eprintln!("skipping {name} (expect exit {expected}): /dev/kvm not available");
        return;
    }
    ensure_built();

    let host_bin = bin_dir().join(name);
    assert!(host_bin.exists(), "test binary {name} not found");
    let guest_path = host_bin.to_str().expect("non-UTF8 binary path");

    let mut cmd = Command::new(vm_bin());
    cmd.arg("run")
        .arg(kernel_bin())
        .arg("--share")
        .arg("/")
        .arg("--run")
        .arg(guest_path);

    let (stdout, stderr, timed_out) = run_with_timeout(cmd, TEST_TIMEOUT);
    let exit_code = find_last_exit_code(&stdout);
    let passed = !timed_out && exit_code == Some(expected);

    if !passed {
        eprintln!("--- {name} stdout ---\n{stdout}");
        eprintln!("--- {name} stderr ---\n{stderr}");
        if timed_out {
            panic!("test {name} timed out after {TEST_TIMEOUT:?}");
        }
        match exit_code {
            None => panic!("test {name} did not emit an [exit] code= line"),
            Some(code) => panic!(
                "test {name} exited with code={code} (expected {expected})",
            ),
        }
    }
}

/// Like `run_test_smp`, but asserts on a specific non-zero exit code
/// in addition to the AP-online check.
pub fn run_test_smp_expect_exit(name: &str, vcpus: u32, expected: i32) {
    if !kvm_available() {
        eprintln!("skipping {name} (smp, expect exit {expected}): /dev/kvm not available");
        return;
    }
    ensure_built();

    let host_bin = bin_dir().join(name);
    assert!(host_bin.exists(), "test binary {name} not found");
    let guest_path = host_bin.to_str().expect("non-UTF8 binary path");

    let mut cmd = Command::new(vm_bin());
    cmd.arg("run")
        .arg(kernel_bin())
        .arg("--share")
        .arg("/")
        .arg("--vcpus")
        .arg(vcpus.to_string())
        .arg("--run")
        .arg(guest_path);

    let (stdout, stderr, timed_out) = run_with_timeout(cmd, TEST_TIMEOUT);
    let exit_code = find_last_exit_code(&stdout);
    let passed = !timed_out && exit_code == Some(expected);

    let mut missing = Vec::new();
    for cpu_id in 1..vcpus {
        let needle = format!("[ap] cpu {} online", cpu_id);
        if !stdout.contains(&needle) {
            missing.push(cpu_id);
        }
    }

    if !passed || !missing.is_empty() {
        eprintln!("--- {name} stdout ---\n{stdout}");
        eprintln!("--- {name} stderr ---\n{stderr}");
        if timed_out {
            panic!("smp test {name} timed out after {TEST_TIMEOUT:?}");
        }
        if !missing.is_empty() {
            panic!("smp test {name}: missing APs online: {:?}", missing);
        }
        match exit_code {
            None => panic!("smp test {name} did not emit an [exit] code= line"),
            Some(code) => panic!(
                "smp test {name} exited with code={code} (expected {expected})",
            ),
        }
    }
}

/// Run a previously-compiled test binary inside sumi-vm and assert it
/// exits cleanly. The binary's stdout is captured and printed on failure
/// to aid debugging.
pub fn run_test(name: &str) {
    if !kvm_available() {
        eprintln!("skipping {name}: /dev/kvm not available");
        return;
    }

    ensure_built();

    let host_bin = bin_dir().join(name);
    assert!(
        host_bin.exists(),
        "test binary {} not found in {}",
        name,
        bin_dir().display()
    );

    // We use `--share /` so the binary's host path is also its guest path.
    // No staging directory needed; everything resolves natively.
    let guest_path = host_bin.to_str().expect("non-UTF8 binary path");

    let mut cmd = Command::new(vm_bin());
    cmd.arg("run")
        .arg(kernel_bin())
        .arg("--share")
        .arg("/")
        .arg("--run")
        .arg(guest_path);

    let (stdout, stderr, timed_out) = run_with_timeout(cmd, TEST_TIMEOUT);

    let exit_code = find_last_exit_code(&stdout);
    let passed = !timed_out && exit_code == Some(0);

    if !passed {
        eprintln!("--- {name} stdout ---");
        eprintln!("{stdout}");
        eprintln!("--- {name} stderr ---");
        eprintln!("{stderr}");
        if timed_out {
            panic!("test {name} timed out after {TEST_TIMEOUT:?}");
        }
        match exit_code {
            None => panic!("test {name} did not emit an [exit] code= line"),
            Some(code) => panic!("test {name} exited with code={code} (expected 0)"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn last_exit_code_picks_trailing_line() {
        // A user program that prints "[exit] code=0" before the kernel's
        // real exit line must NOT fool the harness.
        let stdout = "hello\n[exit] code=0 (printed by user)\nmore output\n[exit] code=42\n";
        assert_eq!(find_last_exit_code(stdout), Some(42));
    }

    #[test]
    fn last_exit_code_picks_only_line_when_single() {
        assert_eq!(find_last_exit_code("[exit] code=0\n"), Some(0));
    }

    #[test]
    fn last_exit_code_missing_returns_none() {
        assert_eq!(find_last_exit_code("no exit marker\n"), None);
    }

    #[test]
    fn last_exit_code_without_trailing_newline() {
        assert_eq!(find_last_exit_code("[exit] code=7"), Some(7));
    }
}
