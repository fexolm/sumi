ENTRY(_start)

MEMORY
{
    phys (rx) : ORIGIN = 0x0, LENGTH = 1M
    virt (rw) : ORIGIN = 0xffffffff80000000, LENGTH = 1M
}

PHDRS
{
    text PT_LOAD FLAGS(5);
    data PT_LOAD FLAGS(6);
}

SECTIONS
{
    .text : ALIGN(4K) {
        *(.text .text.*)
    } > virt AT > phys :text

    .rodata : ALIGN(4K) {
        *(.rodata .rodata.*)
    } > virt AT > phys :text

    .data : ALIGN(4K) {
        *(.data .data.*)
    } > virt AT > phys :data

    .bss : ALIGN(4K) {
        *(.bss .bss.*)
        *(COMMON)
    } > virt :data
}
