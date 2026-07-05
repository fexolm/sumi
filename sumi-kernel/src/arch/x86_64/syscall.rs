use core::arch::asm;

const MSR_STAR: u32 = 0xC000_0081;
const MSR_LSTAR: u32 = 0xC000_0082;
const MSR_SFMASK: u32 = 0xC000_0084;

// STAR[47:32] = 0x0008 => CS=0x0008, SS=0x0010 on syscall entry
const STAR_VALUE: u64 = 0x0008u64 << 32;
// Clear IF (bit 9) and DF (bit 10)
const SFMASK_VALUE: u64 = 0x600;

pub(crate) unsafe fn rdmsr(msr: u32) -> u64 {
    let eax: u32;
    let edx: u32;
    // SAFETY: Caller ensures the MSR address is valid and we are in ring 0.
    unsafe {
        asm!(
            "rdmsr",
            in("ecx") msr,
            out("eax") eax,
            out("edx") edx,
            options(nomem, nostack, preserves_flags),
        );
    }
    ((edx as u64) << 32) | (eax as u64)
}

pub(crate) unsafe fn wrmsr(msr: u32, value: u64) {
    let eax = value as u32;
    let edx = (value >> 32) as u32;
    // SAFETY: Caller ensures the MSR address and value are valid and we are in ring 0.
    unsafe {
        asm!(
            "wrmsr",
            in("ecx") msr,
            in("eax") eax,
            in("edx") edx,
            options(nomem, nostack, preserves_flags),
        );
    }
}

unsafe extern "C" {
    fn syscall_entry();
}

/// Install `syscall`/`sysret` MSRs. Call AFTER `sched::percpu::init_for_cpu()`,
/// which has already loaded `IA32_GS_BASE` for this CPU. The `syscall_entry`
/// asm reads per-CPU scratch via `gs:<offset>`, so no per-CPU pointer is
/// written here.
pub fn init() {
    // SAFETY: We are in ring 0 during kernel boot. All MSR addresses and values
    // are correct for 64-bit long mode.
    unsafe {
        wrmsr(MSR_LSTAR, syscall_entry as *const () as u64);
        wrmsr(MSR_STAR, STAR_VALUE);
        wrmsr(MSR_SFMASK, SFMASK_VALUE);
    }
}

core::arch::global_asm!(
    ".global syscall_entry",
    "syscall_entry:",
    // At entry: rcx = return RIP (user instruction after syscall),
    // r11 = saved RFLAGS, rsp = user stack pointer.
    //
    // Switch to the per-thread kernel stack. Each user thread owns a
    // distinct 2 MB kernel stack (Thread.kernel_stack_top), so concurrent
    // syscalls from two threads cannot clobber each other's saved frames.
    //
    // After the BSP's _start completes and transitions to user mode, the
    // BSP boot stack is idle. The first syscall from user mode resets rsp
    // to KERNEL_STACK top (BSP) or the 2 MB page top (clone children).
    // Either way the stack is unoccupied at this point.
    //
    // Step 1: stash user rsp in PerCpu.saved_user_rsp (gs:[{saved_rsp_off}])
    //         so we don't lose it while we dereference the Thread pointer.
    // Step 2: load PerCpu.current_thread (gs:[{current_thread_off}]) — an
    //         AtomicPtr<Thread> which has the same layout as *mut Thread.
    // Step 3: follow that pointer and load Thread.kernel_stack_top at
    //         offset {kstack_off} (computed by offset_of! at compile time).
    "mov gs:[{saved_rsp_off}], rsp",
    "mov rsp, gs:[{current_thread_off}]",
    // Switch to the per-thread kernel stack. For the BSP main this is
    // the BSP boot stack (KERNEL_STACK), which is idle by invariant
    // (see build_current_main_thread in sched/mod.rs).
    "mov rsp, [rsp + {kstack_off}]",
    //
    // Push the 16 qwords that form the saved frame (no padding needed:
    // 16 × 8 = 128 bytes from a 16-aligned top → rsp%16==0 before call,
    // call pushes return addr → callee entry rsp%16==8 ✓ SysV ABI).
    //
    // Memory layout after all 16 pushes (rsp+0 = lowest = rbx):
    //   rsp+ 0: rbx         } callee-saved (6 qwords)
    //   rsp+ 8: rbp         }
    //   rsp+16: r12         }
    //   rsp+24: r13         }
    //   rsp+32: r14         }
    //   rsp+40: r15         }
    //   rsp+48: nr (rax)    } SyscallArgs (9 qwords, offsets 0..72)
    //   rsp+56: arg0 (rdi)  }   offset 8
    //   rsp+64: arg1 (rsi)  }   offset 16
    //   rsp+72: arg2 (rdx)  }   offset 24
    //   rsp+80: arg3 (r10)  }   offset 32
    //   rsp+88: arg4 (r8)   }   offset 40
    //   rsp+96: arg5 (r9)   }   offset 48
    //   rsp+104: caller_rip (rcx)    }   offset 56
    //   rsp+112: caller_rflags (r11) }   offset 64
    //   rsp+120: user_rsp   (highest slot)
    //
    // rcx and r11 are pushed BEFORE any other clobber so caller_rip /
    // caller_rflags are always the exact values the CPU saved at syscall.
    "push qword ptr gs:[{saved_rsp_off}]", // user_rsp   — highest slot (#1)
    "push r11",               // caller_rflags (#2)
    "push rcx",               // caller_rip    (#3)
    "push r9",                // arg5          (#4)
    "push r8",                // arg4          (#5)
    "push r10",               // arg3          (#6)
    "push rdx",               // arg2          (#7)
    "push rsi",               // arg1          (#8)
    "push rdi",               // arg0          (#9)
    "push rax",               // nr            (#10)
    "push r15",               // callee-saved  (#11)
    "push r14",               //               (#12)
    "push r13",               //               (#13)
    "push r12",               //               (#14)
    "push rbp",               //               (#15)
    "push rbx",               //               (#16)
    // rdi = &SyscallArgs.nr; 6 callee-saved × 8 = 48 bytes above rsp.
    "lea rdi, [rsp + 48]",
    "call syscall_dispatch",
    // rax holds the SyscallResult. Restore saved state in reverse push order.
    "pop rbx",
    "pop rbp",
    "pop r12",
    "pop r13",
    "pop r14",
    "pop r15",
    "add rsp, 8",  // skip nr — rax is the return value
    "pop rdi",     // restore arg0
    "pop rsi",     // restore arg1
    "pop rdx",     // restore arg2
    "pop r10",     // restore arg3
    "pop r8",      // restore arg4
    "pop r9",      // restore arg5
    "pop rcx",     // caller_rip  → rcx (used by jmp below)
    "pop r11",     // caller_rflags → r11 (used by popfq below)
    // Restore user RFLAGS while still on the kernel stack, then switch
    // rsp back to the user stack and jump to the user return address.
    //
    // IF must NOT become 1 via this `popfq`: unlike `sti`, `popfq` setting
    // IF has no interrupt-shadow — a timer tick can be recognized at the
    // very next instruction boundary, i.e. before `pop rsp` runs, which
    // would deliver the timer ISR with RSP still pointing at the (about to
    // be reused) kernel stack (confirmed empirically under KVM). So clear
    // IF in the pushed copy before `popfq`, complete the RSP switch, and
    // only then `sti` — its shadow is real and covers exactly the `jmp`
    // that follows.
    "push r11",
    "and dword ptr [rsp], 0xFFFFFDFF", // clear IF (bit 9) in the pushed copy
    "popfq",       // restore flags; IF stays 0
    "pop rsp",     // switch to user rsp — safe, IF is still 0
    "sti",         // shadow covers the next instruction (`jmp rcx`)
    "jmp rcx",     // return to user code
    kstack_off = const crate::sched::thread::KERNEL_STACK_TOP_OFFSET,
    saved_rsp_off = const crate::sched::percpu::SAVED_USER_RSP_OFFSET,
    current_thread_off = const crate::sched::percpu::CURRENT_THREAD_OFFSET,
);
