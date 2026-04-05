use std::path::{Path, PathBuf};
use std::process::Command;

fn project_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf()
}

fn kernel_bin() -> PathBuf {
    project_root().join("target/x86_64-unknown-none/debug/sumi-kernel")
}

fn vm_bin() -> PathBuf {
    project_root().join("target/debug/sumi-vm")
}

/// Build kernel + VM if not already built.
fn ensure_built() {
    let root = project_root();

    let status = Command::new("cargo")
        .args(["build", "-p", "sumi-kernel", "--target", "x86_64-unknown-none"])
        .current_dir(&root)
        .status()
        .expect("failed to run cargo build for kernel");
    assert!(status.success(), "kernel build failed");

    let status = Command::new("cargo")
        .args(["build", "-p", "sumi-vm"])
        .current_dir(&root)
        .status()
        .expect("failed to run cargo build for VM");
    assert!(status.success(), "VM build failed");
}

/// Build a Rust no_std fixture crate into a static ELF.
fn build_rust_fixture(crate_name: &str, out_dir: &Path) -> PathBuf {
    let crate_dir = project_root().join("tests/fixtures").join(crate_name);
    let status = Command::new("cargo")
        .args(["build", "--target", "x86_64-unknown-linux-gnu"])
        .current_dir(&crate_dir)
        .status()
        .expect("failed to build rust fixture");
    assert!(status.success(), "building {crate_name} failed");

    let bin = crate_dir
        .join("target/x86_64-unknown-linux-gnu/debug")
        .join(crate_name);
    let dest = out_dir.join(crate_name);
    std::fs::copy(&bin, &dest).expect("failed to copy rust fixture binary");
    dest
}

/// Assemble a static ELF from an assembly source file.
fn assemble_fixture(src: &str, out_dir: &Path) -> PathBuf {
    let stem = Path::new(src).file_stem().unwrap().to_str().unwrap();
    let out = out_dir.join(stem);
    let fixture_path = project_root().join("tests/fixtures").join(src);

    let status = Command::new("gcc")
        .args([
            "-nostdlib",
            "-static",
            "-no-pie",
            "-o",
            out.to_str().unwrap(),
            fixture_path.to_str().unwrap(),
        ])
        .status()
        .expect("failed to run gcc — is gcc installed?");
    assert!(status.success(), "assembling {src} failed");

    out
}

/// Run a user program under sumi-vm and return the captured output.
fn run_program(binary_name: &str, share_dir: &Path) -> String {
    let output = Command::new(vm_bin())
        .arg("run")
        .arg(kernel_bin())
        .arg("--share")
        .arg(share_dir)
        .arg("--run")
        .arg(format!("/{binary_name}"))
        .output()
        .expect("failed to run sumi-vm");

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    if !stderr.is_empty() {
        eprintln!("--- stderr ---\n{stderr}");
    }

    stdout
}

fn kvm_available() -> bool {
    Path::new("/dev/kvm").exists()
}

#[test]
fn hello_world() {
    if !kvm_available() {
        eprintln!("skipping: /dev/kvm not available");
        return;
    }

    ensure_built();
    let tmp = TempDir::new();
    assemble_fixture("hello.S", tmp.path());
    let output = run_program("hello", tmp.path());

    assert!(
        output.contains("Hello, world!"),
        "expected 'Hello, world!' in output, got:\n{output}"
    );
    assert!(
        output.contains("[exit] code=0"),
        "expected clean exit in output, got:\n{output}"
    );
}

#[test]
fn exit_code_42() {
    if !kvm_available() {
        eprintln!("skipping: /dev/kvm not available");
        return;
    }

    ensure_built();
    let tmp = TempDir::new();
    assemble_fixture("exit42.S", tmp.path());
    let output = run_program("exit42", tmp.path());

    assert!(
        output.contains("[exit] code=42"),
        "expected '[exit] code=42' in output, got:\n{output}"
    );
}

#[test]
fn brk_allocation() {
    if !kvm_available() {
        eprintln!("skipping: /dev/kvm not available");
        return;
    }

    ensure_built();
    let tmp = TempDir::new();
    assemble_fixture("brk_test.S", tmp.path());
    let output = run_program("brk_test", tmp.path());

    assert!(
        output.contains("brk ok"),
        "expected 'brk ok' in output, got:\n{output}"
    );
}

#[test]
fn mmap_anonymous() {
    if !kvm_available() {
        eprintln!("skipping: /dev/kvm not available");
        return;
    }

    ensure_built();
    let tmp = TempDir::new();
    assemble_fixture("mmap_test.S", tmp.path());
    let output = run_program("mmap_test", tmp.path());

    assert!(
        output.contains("mmap ok"),
        "expected 'mmap ok' in output, got:\n{output}"
    );
}

#[test]
fn rust_hello() {
    if !kvm_available() {
        eprintln!("skipping: /dev/kvm not available");
        return;
    }

    ensure_built();
    let tmp = TempDir::new();
    build_rust_fixture("rust-hello", tmp.path());
    let output = run_program("rust-hello", tmp.path());

    assert!(
        output.contains("Hello from Rust!"),
        "expected 'Hello from Rust!' in output, got:\n{output}"
    );
    assert!(
        output.contains("[exit] code=0"),
        "expected clean exit in output, got:\n{output}"
    );
}

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "sumi-inttest-{}-{}",
            std::process::id(),
            id
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
