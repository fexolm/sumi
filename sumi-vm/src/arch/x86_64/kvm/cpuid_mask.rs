// CPUID feature bits we mask out so glibc IFUNC resolvers do not select
// AVX/AVX2/AVX-512/FMA paths. We never enable CR4.OSXSAVE or set XCR0, so
// any AVX instruction the guest executes would #UD. Masking the CPUID bits
// forces glibc onto the SSE2 baseline. See docs/glibc-support-design.md.
const CPUID_1_ECX_FMA: u32 = 1 << 12;
const CPUID_1_ECX_XSAVE: u32 = 1 << 26;
const CPUID_1_ECX_OSXSAVE: u32 = 1 << 27;
const CPUID_1_ECX_AVX: u32 = 1 << 28;
const CPUID_1_ECX_F16C: u32 = 1 << 29;

const CPUID_7_EBX_AVX2: u32 = 1 << 5;
const CPUID_7_EBX_AVX512F: u32 = 1 << 16;
const CPUID_7_EBX_AVX512DQ: u32 = 1 << 17;
const CPUID_7_EBX_AVX512IFMA: u32 = 1 << 21;
const CPUID_7_EBX_AVX512CD: u32 = 1 << 28;
const CPUID_7_EBX_AVX512BW: u32 = 1 << 30;
const CPUID_7_EBX_AVX512VL: u32 = 1 << 31;
const CPUID_7_EBX_AVX512PF: u32 = 1 << 26;
const CPUID_7_EBX_AVX512ER: u32 = 1 << 27;

const CPUID_7_ECX_AVX512VBMI: u32 = 1 << 1;
const CPUID_7_ECX_AVX512VBMI2: u32 = 1 << 6;
const CPUID_7_ECX_VAES: u32 = 1 << 9;
const CPUID_7_ECX_VPCLMULQDQ: u32 = 1 << 10;
const CPUID_7_ECX_AVX512VNNI: u32 = 1 << 11;
const CPUID_7_ECX_AVX512BITALG: u32 = 1 << 12;
const CPUID_7_ECX_AVX512VPOPCNTDQ: u32 = 1 << 14;

const CPUID_7_EDX_AVX512_4VNNIW: u32 = 1 << 2;
const CPUID_7_EDX_AVX512_4FMAPS: u32 = 1 << 3;
const CPUID_7_EDX_AVX512_VP2INTERSECT: u32 = 1 << 8;
const CPUID_7_EDX_AVX512_FP16: u32 = 1 << 23;

// CPUID(7, 1) EAX — additional AVX-family extensions on Sapphire-Rapids+ / Zen4+.
const CPUID_7_1_EAX_AVX_VNNI: u32 = 1 << 4;
const CPUID_7_1_EAX_AVX512_BF16: u32 = 1 << 5;
const CPUID_7_1_EAX_AVX_IFMA: u32 = 1 << 23;

// CPUID(0x8000_0001) ECX — AMD AVX-family (XOP, FMA4) on Bulldozer/Piledriver.
const CPUID_8000_0001_ECX_XOP: u32 = 1 << 11;
const CPUID_8000_0001_ECX_FMA4: u32 = 1 << 16;

const CPUID_1_ECX_AVX_FAMILY_MASK: u32 =
    CPUID_1_ECX_FMA | CPUID_1_ECX_XSAVE | CPUID_1_ECX_OSXSAVE | CPUID_1_ECX_AVX | CPUID_1_ECX_F16C;

const CPUID_7_EBX_AVX_FAMILY_MASK: u32 = CPUID_7_EBX_AVX2
    | CPUID_7_EBX_AVX512F
    | CPUID_7_EBX_AVX512DQ
    | CPUID_7_EBX_AVX512IFMA
    | CPUID_7_EBX_AVX512PF
    | CPUID_7_EBX_AVX512ER
    | CPUID_7_EBX_AVX512CD
    | CPUID_7_EBX_AVX512BW
    | CPUID_7_EBX_AVX512VL;

const CPUID_7_ECX_AVX_FAMILY_MASK: u32 = CPUID_7_ECX_AVX512VBMI
    | CPUID_7_ECX_AVX512VBMI2
    | CPUID_7_ECX_VAES
    | CPUID_7_ECX_VPCLMULQDQ
    | CPUID_7_ECX_AVX512VNNI
    | CPUID_7_ECX_AVX512BITALG
    | CPUID_7_ECX_AVX512VPOPCNTDQ;

const CPUID_7_EDX_AVX_FAMILY_MASK: u32 = CPUID_7_EDX_AVX512_4VNNIW
    | CPUID_7_EDX_AVX512_4FMAPS
    | CPUID_7_EDX_AVX512_VP2INTERSECT
    | CPUID_7_EDX_AVX512_FP16;

const CPUID_7_1_EAX_AVX_FAMILY_MASK: u32 =
    CPUID_7_1_EAX_AVX_VNNI | CPUID_7_1_EAX_AVX512_BF16 | CPUID_7_1_EAX_AVX_IFMA;

const CPUID_8000_0001_ECX_AVX_FAMILY_MASK: u32 = CPUID_8000_0001_ECX_XOP | CPUID_8000_0001_ECX_FMA4;

/// Clear AVX/FMA/XSAVE/OSXSAVE bits in supported CPUID entries so glibc IFUNC
/// resolvers select SSE2-baseline implementations. Mutates in place.
pub(super) fn apply(entries: &mut [kvm_bindings::kvm_cpuid_entry2]) {
    for entry in entries.iter_mut() {
        match (entry.function, entry.index) {
            (1, 0) => {
                entry.ecx &= !CPUID_1_ECX_AVX_FAMILY_MASK;
            }
            (7, 0) => {
                entry.ebx &= !CPUID_7_EBX_AVX_FAMILY_MASK;
                entry.ecx &= !CPUID_7_ECX_AVX_FAMILY_MASK;
                entry.edx &= !CPUID_7_EDX_AVX_FAMILY_MASK;
            }
            (7, 1) => {
                entry.eax &= !CPUID_7_1_EAX_AVX_FAMILY_MASK;
            }
            (0x8000_0001, 0) => {
                entry.ecx &= !CPUID_8000_0001_ECX_AVX_FAMILY_MASK;
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod cpuid_mask_tests {
    use super::*;
    use kvm_bindings::kvm_cpuid_entry2;

    fn entry(function: u32, index: u32) -> kvm_cpuid_entry2 {
        kvm_cpuid_entry2 {
            function,
            index,
            ..Default::default()
        }
    }

    #[test]
    fn cpuid_mask_clears_avx_family_bits() {
        let mut e1 = entry(1, 0);
        e1.ecx = 0xFFFF_FFFF;
        let mut e7_0 = entry(7, 0);
        e7_0.ebx = 0xFFFF_FFFF;
        e7_0.ecx = 0xFFFF_FFFF;
        e7_0.edx = 0xFFFF_FFFF;
        let mut e7_1 = entry(7, 1);
        e7_1.eax = 0xFFFF_FFFF;
        let mut amd = entry(0x8000_0001, 0);
        amd.ecx = 0xFFFF_FFFF;
        let mut unrelated = entry(0xB, 0);
        unrelated.eax = 0xFFFF_FFFF;
        unrelated.ebx = 0xFFFF_FFFF;
        unrelated.ecx = 0xFFFF_FFFF;
        unrelated.edx = 0xFFFF_FFFF;
        let mut subleaf1 = entry(1, 1);
        subleaf1.ecx = 0xFFFF_FFFF;

        let mut entries = vec![e1, e7_0, e7_1, amd, unrelated, subleaf1];
        apply(&mut entries);

        // (1,0) ECX: AVX family cleared
        assert_eq!(entries[0].ecx & CPUID_1_ECX_AVX, 0);
        assert_eq!(entries[0].ecx & CPUID_1_ECX_FMA, 0);
        assert_eq!(entries[0].ecx & CPUID_1_ECX_XSAVE, 0);
        assert_eq!(entries[0].ecx & CPUID_1_ECX_OSXSAVE, 0);
        assert_eq!(entries[0].ecx & CPUID_1_ECX_F16C, 0);
        // (1,0) ECX: SSE3, SSSE3, CMPXCHG16B, SSE4.1, SSE4.2, POPCNT, AESNI preserved
        assert!(entries[0].ecx & (1 << 0) != 0, "SSE3 (PNI)");
        assert!(entries[0].ecx & (1 << 9) != 0, "SSSE3");
        assert!(entries[0].ecx & (1 << 13) != 0, "CMPXCHG16B");
        assert!(entries[0].ecx & (1 << 19) != 0, "SSE4.1");
        assert!(entries[0].ecx & (1 << 20) != 0, "SSE4.2");
        assert!(entries[0].ecx & (1 << 23) != 0, "POPCNT");
        assert!(entries[0].ecx & (1 << 25) != 0, "AESNI");

        // (7,0): all AVX-family bits cleared
        assert_eq!(entries[1].ebx & CPUID_7_EBX_AVX2, 0);
        assert_eq!(entries[1].ebx & CPUID_7_EBX_AVX512F, 0);
        assert_eq!(entries[1].ebx & CPUID_7_EBX_AVX512PF, 0);
        assert_eq!(entries[1].ebx & CPUID_7_EBX_AVX512ER, 0);
        assert_eq!(entries[1].ecx & CPUID_7_ECX_VAES, 0);
        assert_eq!(entries[1].ecx & CPUID_7_ECX_VPCLMULQDQ, 0);
        assert_eq!(entries[1].ecx & CPUID_7_ECX_AVX512VBMI, 0);
        assert_eq!(entries[1].edx & CPUID_7_EDX_AVX512_FP16, 0);
        // (7,0) preserved bits: SHA (EBX[29]), SMEP (EBX[7]), SMAP (EBX[20])
        assert!(entries[1].ebx & (1 << 29) != 0, "SHA");
        assert!(entries[1].ebx & (1 << 7) != 0, "SMEP");
        assert!(entries[1].ebx & (1 << 20) != 0, "SMAP");

        // (7,1) EAX: AVX_VNNI/BF16/IFMA cleared
        assert_eq!(entries[2].eax & CPUID_7_1_EAX_AVX_VNNI, 0);
        assert_eq!(entries[2].eax & CPUID_7_1_EAX_AVX512_BF16, 0);
        assert_eq!(entries[2].eax & CPUID_7_1_EAX_AVX_IFMA, 0);

        // (0x8000_0001) ECX: XOP/FMA4 cleared
        assert_eq!(entries[3].ecx & CPUID_8000_0001_ECX_XOP, 0);
        assert_eq!(entries[3].ecx & CPUID_8000_0001_ECX_FMA4, 0);

        // Unrelated leaf 0xB: untouched
        assert_eq!(entries[4].eax, 0xFFFF_FFFF);
        assert_eq!(entries[4].ebx, 0xFFFF_FFFF);
        assert_eq!(entries[4].ecx, 0xFFFF_FFFF);
        assert_eq!(entries[4].edx, 0xFFFF_FFFF);

        // Function 1 subleaf 1: untouched
        assert_eq!(entries[5].ecx, 0xFFFF_FFFF);
    }

    #[test]
    fn cpuid_mask_is_idempotent() {
        let mut e7 = entry(7, 0);
        e7.ebx = 0xFFFF_FFFF;
        e7.ecx = 0xFFFF_FFFF;
        e7.edx = 0xFFFF_FFFF;
        let mut entries_a = vec![e7];
        let mut entries_b = vec![e7];
        apply(&mut entries_a);
        apply(&mut entries_b);
        apply(&mut entries_b);
        assert_eq!(entries_a[0].ebx, entries_b[0].ebx);
        assert_eq!(entries_a[0].ecx, entries_b[0].ecx);
        assert_eq!(entries_a[0].edx, entries_b[0].edx);
    }
}
