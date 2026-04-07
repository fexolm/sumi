use std::path::{Path, PathBuf};
use std::process::Command;

fn project_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf()
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
        .args([
            "build",
            "-p",
            "sumi-kernel",
            "--target",
            "x86_64-unknown-none",
        ])
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

fn timeout_cmd_available() -> bool {
    Command::new("timeout")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
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

fn musl_gcc_available() -> bool {
    Command::new("musl-gcc").arg("--version").output().is_ok()
}

fn gcc_available() -> bool {
    Command::new("gcc").arg("--version").output().is_ok()
}

/// Compile a C fixture with musl-gcc, dynamically linked (the default for musl-gcc).
fn compile_musl_dynamic(src: &str, out_dir: &Path) -> PathBuf {
    let stem = Path::new(src).file_stem().unwrap().to_str().unwrap();
    let out = out_dir.join("bin").join(stem);
    std::fs::create_dir_all(out.parent().unwrap()).unwrap();
    let fixture_path = project_root().join("tests/fixtures").join(src);

    let status = Command::new("musl-gcc")
        .args(["-o", out.to_str().unwrap(), fixture_path.to_str().unwrap()])
        .status()
        .expect("failed to run musl-gcc");
    assert!(status.success(), "compiling {src} with musl-gcc failed");

    out
}

fn compile_glibc_dynamic(src: &str, out_dir: &Path) -> PathBuf {
    let stem = Path::new(src).file_stem().unwrap().to_str().unwrap();
    let out = out_dir.join("bin").join(stem);
    std::fs::create_dir_all(out.parent().unwrap()).unwrap();
    let fixture_path = project_root().join("tests/fixtures").join(src);

    // Pin -march to the x86-64-v2 baseline. Anything higher (v3 / native) makes
    // gcc emit unconditional VEX-encoded instructions in the user binary itself,
    // which #UD inside sumi because we deliberately leave CR4.OSXSAVE clear (see
    // sumi-vm/src/arch/x86_64/kvm/cpuid_mask.rs). The CPUID mask only redirects
    // glibc's IFUNC resolvers; precompiled user code is on its own.
    let proc = Command::new("gcc")
        .args([
            "-O2",
            "-march=x86-64-v2",
            "-o",
            out.to_str().unwrap(),
            fixture_path.to_str().unwrap(),
        ])
        .output()
        .expect("failed to run gcc");
    assert!(
        proc.status.success(),
        "compiling {src} with gcc failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&proc.stdout),
        String::from_utf8_lossy(&proc.stderr),
    );

    out
}

/// Set up the shared directory with musl libc for dynamic linking tests.
/// Copies ld-musl-x86_64.so.1 (and libc.so if separate) into lib/.
fn setup_musl_libs(share_dir: &Path) {
    let lib_dir = share_dir.join("lib");
    std::fs::create_dir_all(&lib_dir).unwrap();

    // Find the musl dynamic linker on the host system.
    let interp_paths = [
        "/lib/ld-musl-x86_64.so.1",
        "/usr/lib/x86_64-linux-musl/libc.so",
        "/lib/x86_64-linux-musl/libc.so",
    ];

    // The interpreter is typically a symlink to libc.so — copy the real file
    // and create the expected symlink.
    let ld_musl = interp_paths
        .iter()
        .find(|p| Path::new(p).exists())
        .expect("could not find musl dynamic linker on host");

    let real_path = std::fs::canonicalize(ld_musl).unwrap();
    let dest_libc = lib_dir.join("libc.so");
    std::fs::copy(&real_path, &dest_libc).expect("failed to copy libc.so");

    // Create the ld-musl symlink that PT_INTERP references.
    let dest_ld = lib_dir.join("ld-musl-x86_64.so.1");
    if !dest_ld.exists() {
        #[cfg(unix)]
        std::os::unix::fs::symlink("libc.so", &dest_ld).expect("failed to create ld-musl symlink");
    }
}

/// Run a user binary located on the host filesystem under sumi-vm using
/// `--share /`. The binary's PT_INTERP and DT_NEEDED libraries resolve
/// natively against the host (no staging dir, no library copies). This is
/// the canonical way to run a glibc-linked binary.
fn run_host_program_with_timeout(host_path: &Path, timeout_secs: u32) -> String {
    ensure_built();
    let kernel = kernel_bin();
    let vm = vm_bin();
    let proc = Command::new("timeout")
        .arg(format!("{timeout_secs}"))
        .arg(&vm)
        .args([
            "run",
            kernel.to_str().unwrap(),
            "--share",
            "/",
            "--run",
            host_path.to_str().unwrap(),
        ])
        .output()
        .expect("failed to run sumi-vm under timeout");
    if proc.status.code() == Some(124) {
        let stdout = String::from_utf8_lossy(&proc.stdout);
        let stderr = String::from_utf8_lossy(&proc.stderr);
        panic!(
            "sumi-vm hit {timeout_secs}s timeout running {}\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
            host_path.display()
        );
    }
    let stdout = String::from_utf8_lossy(&proc.stdout).to_string();
    let stderr = String::from_utf8_lossy(&proc.stderr).to_string();
    if !stderr.is_empty() {
        eprintln!("--- stderr ---\n{stderr}");
    }
    stdout
}

#[test]
fn dynamic_hello_musl() {
    if !kvm_available() {
        eprintln!("skipping: /dev/kvm not available");
        return;
    }
    if !musl_gcc_available() {
        eprintln!("skipping: musl-gcc not available (install musl-tools)");
        return;
    }

    ensure_built();
    let tmp = TempDir::new();
    compile_musl_dynamic("dynamic_hello.c", tmp.path());
    setup_musl_libs(tmp.path());
    let output = run_program("bin/dynamic_hello", tmp.path());

    assert!(
        output.contains("Hello from dynamic linking!"),
        "expected 'Hello from dynamic linking!' in output, got:\n{output}"
    );
    assert!(
        output.contains("[exit] code=0"),
        "expected clean exit in output, got:\n{output}"
    );
}

#[test]
fn dynamic_hello_glibc() {
    if !kvm_available() {
        eprintln!("skipping: /dev/kvm not available");
        return;
    }
    if !gcc_available() {
        eprintln!("skipping: gcc not available");
        return;
    }
    if !timeout_cmd_available() {
        eprintln!("skipping: timeout(1) not available");
        return;
    }

    ensure_built();
    let tmp = TempDir::new();
    let host_bin = compile_glibc_dynamic("dynamic_hello_glibc.c", tmp.path());
    // Run via `--share /`: the binary lives in tmp on the host filesystem,
    // its PT_INTERP and DT_NEEDED libraries resolve natively against the host
    // root. No staging of /lib64 or /lib/x86_64-linux-gnu required.
    let output = run_host_program_with_timeout(&host_bin, 30);

    assert!(
        output.contains("Hello from glibc!"),
        "expected 'Hello from glibc!' in output, got:\n{output}"
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
        let dir = std::env::temp_dir().join(format!("sumi-inttest-{}-{}", std::process::id(), id));
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
