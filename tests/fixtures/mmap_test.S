# Test anonymous mmap: allocate a page, write to it, exit
.global _start
.text
_start:
    # mmap(NULL, 4096, PROT_READ|PROT_WRITE, MAP_PRIVATE|MAP_ANONYMOUS, -1, 0)
    mov $9, %rax            # syscall: mmap
    xor %rdi, %rdi          # addr: NULL
    mov $4096, %rsi         # length: 4096
    mov $3, %rdx            # prot: PROT_READ|PROT_WRITE
    mov $0x22, %r10         # flags: MAP_PRIVATE|MAP_ANONYMOUS
    mov $-1, %r8            # fd: -1
    xor %r9, %r9            # offset: 0
    syscall
    # rax = mapped address (or negative error)
    test %rax, %rax
    js fail                 # negative = error

    # Write a byte to the mapped page
    movb $0x55, (%rax)

    # Print "mmap ok\n"
    mov $1, %rax
    mov $1, %rdi
    lea ok_msg(%rip), %rsi
    mov $8, %rdx
    syscall

    # exit_group(0)
    mov $231, %rax
    xor %rdi, %rdi
    syscall

fail:
    mov $1, %rax
    mov $1, %rdi
    lea fail_msg(%rip), %rsi
    mov $10, %rdx
    syscall
    mov $231, %rax
    mov $1, %rdi
    syscall

.section .rodata
ok_msg:
    .ascii "mmap ok\n"
fail_msg:
    .ascii "mmap FAIL\n"
