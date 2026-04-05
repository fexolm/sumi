# Test brk: get current brk, grow it, write to new memory, then exit
.global _start
.text
_start:
    # brk(0) — get current break
    mov $12, %rax           # syscall: brk
    xor %rdi, %rdi          # arg: 0 (query)
    syscall
    mov %rax, %r12          # save current brk

    # brk(current + 4096) — grow by one page
    lea 4096(%rax), %rdi
    mov $12, %rax           # syscall: brk
    syscall
    # rax = new brk (should be >= r12 + 4096)
    cmp %r12, %rax
    jle fail                # if brk didn't grow, fail

    # Write a byte to the new memory
    movb $0x42, (%r12)

    # Print "brk ok\n"
    mov $1, %rax
    mov $1, %rdi
    lea ok_msg(%rip), %rsi
    mov $7, %rdx
    syscall

    # exit_group(0)
    mov $231, %rax
    xor %rdi, %rdi
    syscall

fail:
    mov $1, %rax
    mov $1, %rdi
    lea fail_msg(%rip), %rsi
    mov $9, %rdx
    syscall
    mov $231, %rax
    mov $1, %rdi
    syscall

.section .rodata
ok_msg:
    .ascii "brk ok\n"
fail_msg:
    .ascii "brk FAIL\n"
