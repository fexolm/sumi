//! Runtime helpers for sumi integration tests.
//!
//! Each `data/<name>.rs` file is compiled by `build.rs` into a static
//! Linux ELF binary that boots inside sumi via the `--run` flag. The
//! `tests/test_launcher.rs` test binary calls [`run_test`] for each
//! generated binary, asserting that the program exits with code 0.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

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

    let output = Command::new(vm_bin())
        .arg("run")
        .arg(kernel_bin())
        .arg("--share")
        .arg("/")
        .arg("--run")
        .arg(guest_path)
        .output()
        .expect("failed to spawn sumi-vm");

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    let exit_marker = "[exit] code=0";
    let passed = stdout.contains(exit_marker);

    if !passed {
        eprintln!("--- {name} stdout ---");
        eprintln!("{stdout}");
        eprintln!("--- {name} stderr ---");
        eprintln!("{stderr}");
        panic!("test {name} did not produce '{exit_marker}'");
    }
}
