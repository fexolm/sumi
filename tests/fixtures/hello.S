# Minimal hello world: write(1, "Hello, world!\n", 14) then exit_group(0)
.global _start
.text
_start:
    mov $1, %rax            # syscall: write
    mov $1, %rdi            # fd: stdout
    lea msg(%rip), %rsi     # buf
    mov $14, %rdx           # count
    syscall
    mov $231, %rax          # syscall: exit_group
    xor %rdi, %rdi          # status: 0
    syscall

.section .rodata
msg:
    .ascii "Hello, world!\n"
