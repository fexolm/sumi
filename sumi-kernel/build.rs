use std::env;
use std::fs::File;
use std::io::Write;
use std::path::PathBuf;

use sumi_abi::layout::{KERNEL_CODE_PHYS, KERNEL_CODE_VIRT};

fn gen_linker_script(linker_script_path: &PathBuf) {
    let linker_script_content = format!(
        r#"
        ENTRY(_start)
        MEMORY
        {{
            phys (rx) : ORIGIN = {phys:#x}, LENGTH = 1M
            virt (rw) : ORIGIN = {virt:#x}, LENGTH = 1M
        }}

        PHDRS
        {{
            text PT_LOAD FLAGS(5);    /* RX - Read + Execute */
            data PT_LOAD FLAGS(6);    /* RW - Read + Write */
        }}

        SECTIONS {{
            .text : ALIGN(4K) {{
                *(.text .text.*)
            }} > virt AT > phys :text

            .rodata : ALIGN(4K) {{
                *(.rodata .rodata.*) 
            }} > virt AT > phys :text

                .data : ALIGN(4K) {{
                    *(.data .data.*) 
            }} > virt AT > phys :data

                .bss : ALIGN(4K) {{
                    *(.bss .bss.*) 
                    *(COMMON)
            }} > virt :data
        }}
        "#,
        virt = KERNEL_CODE_VIRT.as_u64(),
        phys = KERNEL_CODE_PHYS.as_u64(),
    );

    let mut f = File::create(linker_script_path).unwrap();
    f.write_all(linker_script_content.as_bytes()).unwrap();
}

fn main() {
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let linker_script_path = out_dir.join("linker.ld");
    let target_name = env::var("CARGO_PKG_NAME").unwrap();

    gen_linker_script(&linker_script_path);

    println!(
        "cargo:rustc-link-arg-bin={}=-T{}",
        target_name,
        linker_script_path.display()
    );

    println!("cargo:rerun-if-changed=.");
}
