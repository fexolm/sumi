# Exit with code 42 — tests exit code propagation
.global _start
.text
_start:
    mov $231, %rax          # syscall: exit_group
    mov $42, %rdi           # status: 42
    syscall
