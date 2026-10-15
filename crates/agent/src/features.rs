//! Instruction-set extensions of the CPU in the two forms the winstats CPU popup shows: a short
//! line ("AVX2 · AVX · VT-x · EM64T") and the full list ("MMX, SSE (1, 2, 3, 4.1, 4.2), ..."),
//! named as winstats names them for its own CPU (src/metrics/sysinfo.rs). x86 asks CPUID; ARM
//! and RISC-V Linux read `/proc/cpuinfo`, Apple silicon its `hw.optional` sysctls and Windows on
//! ARM `IsProcessorFeaturePresent`.

/// (short, full); both empty when unknown.
pub fn read() -> (String, String) {
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    return (x86_short(), x86_full());
    #[cfg(all(target_os = "linux", not(any(target_arch = "x86", target_arch = "x86_64"))))]
    return from_cpuinfo(&std::fs::read_to_string("/proc/cpuinfo").unwrap_or_default());
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    return arm(|f| !f.apple.is_empty() && crate::macos::sysctl(f.apple).unwrap_or(0) != 0);
    #[cfg(all(windows, target_arch = "aarch64"))]
    return arm(|f| f.windows != 0 && unsafe { IsProcessorFeaturePresent(f.windows) } != 0);
    #[allow(unreachable_code)]
    (String::new(), String::new())
}

/// (rated, highest) clock from CPUID leaf 0x16 (Intel, Skylake and later), MHz; VMs and AMD
/// leave it empty.
pub fn x86_clocks() -> (Option<u32>, Option<u32>) {
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    {
        if cpuid(0, 0).0 >= 0x16 {
            let (a, b, ..) = cpuid(0x16, 0);
            let mhz = |v: u32| Some(v & 0xFFFF).filter(|m| *m > 0);
            return (mhz(a), mhz(b));
        }
    }
    (None, None)
}

/// CPUID leaf 1, ECX bit 31: set by every hypervisor for the OS it runs.
pub fn x86_hypervisor() -> bool {
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    return cpuid(1, 0).2 >> 31 & 1 == 1;
    #[allow(unreachable_code)]
    false
}

#[cfg(all(windows, target_arch = "aarch64"))]
#[link(name = "kernel32")]
extern "system" {
    fn IsProcessorFeaturePresent(feature: u32) -> i32;
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
fn cpuid(leaf: u32, sub: u32) -> (u32, u32, u32, u32) {
    #[cfg(target_arch = "x86")]
    use std::arch::x86::__cpuid_count;
    #[cfg(target_arch = "x86_64")]
    use std::arch::x86_64::__cpuid_count;
    let r = unsafe { __cpuid_count(leaf, sub) };
    (r.eax, r.ebx, r.ecx, r.edx)
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
fn x86_short() -> String {
    // four at most: the two newest vector extensions, virtualization, the 64-bit mode, then
    // AES when there is room
    let (max_leaf, vb, vc, vd) = cpuid(0, 0);
    let intel = [vb, vd, vc].iter().flat_map(|r| r.to_le_bytes()).collect::<Vec<u8>>() == b"GenuineIntel";
    let (max_ext, ..) = cpuid(0x8000_0000, 0);
    let (_, _, c1, d1) = cpuid(1, 0);
    let (_, b7, ..) = if max_leaf >= 7 { cpuid(7, 0) } else { (0, 0, 0, 0) };
    let (_, _, ce, de) = if max_ext >= 0x8000_0001 { cpuid(0x8000_0001, 0) } else { (0, 0, 0, 0) };
    let has = |reg: u32, bit: u32| reg >> bit & 1 == 1;
    let vector = [(b7, 16, "AVX-512"), (b7, 5, "AVX2"), (c1, 28, "AVX"), (c1, 20, "SSE4.2"), (c1, 19, "SSE4.1"), (c1, 9, "SSSE3"), (c1, 0, "SSE3"), (d1, 26, "SSE2"), (d1, 25, "SSE")];
    let mut v: Vec<&str> = vector.iter().filter(|(r, b, _)| has(*r, *b)).map(|(_, _, n)| *n).take(2).collect();
    if has(c1, 5) {
        v.push("VT-x");
    } else if has(ce, 2) {
        v.push("AMD-V");
    }
    if has(de, 29) {
        v.push(if intel { "EM64T" } else { "x86-64" });
    }
    if has(c1, 25) {
        v.push("AES");
    }
    v.truncate(4);
    v.join(" · ")
}

/// CPU-Z's list: "MMX, SSE (1, 2, 3, 4.1, 4.2), SSSE3, EM64T, VT-x, AES, AVX, AVX2, FMA3".
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
fn x86_full() -> String {
    let (max_leaf, vb, vc, vd) = cpuid(0, 0);
    let intel = [vb, vd, vc].iter().flat_map(|r| r.to_le_bytes()).collect::<Vec<u8>>() == b"GenuineIntel";
    let (max_ext, ..) = cpuid(0x8000_0000, 0);
    let (_, _, c1, d1) = cpuid(1, 0);
    let (_, b7, _, d7) = if max_leaf >= 7 { cpuid(7, 0) } else { (0, 0, 0, 0) };
    let (a71, _, _, d71) = if max_leaf >= 7 { cpuid(7, 1) } else { (0, 0, 0, 0) };
    let (_, _, ce, de) = if max_ext >= 0x8000_0001 { cpuid(0x8000_0001, 0) } else { (0, 0, 0, 0) };
    let has = |reg: u32, bit: u32| reg >> bit & 1 == 1;
    let mut out: Vec<String> = Vec::new();
    if has(d1, 23) {
        out.push(if has(de, 22) && !intel { "MMX(+)" } else { "MMX" }.into());
    }
    if has(de, 31) {
        out.push(if has(de, 30) { "3DNow!(+)" } else { "3DNow!" }.into());
    }
    let sse: Vec<&str> = [(d1, 25, "1"), (d1, 26, "2"), (c1, 0, "3"), (c1, 19, "4.1"), (c1, 20, "4.2"), (ce, 6, "4A")]
        .iter()
        .filter(|(r, b, _)| has(*r, *b))
        .map(|(_, _, n)| *n)
        .collect();
    if !sse.is_empty() {
        out.push(format!("SSE ({})", sse.join(", ")));
    }
    // TSX: restricted transactional memory or hardware lock elision
    let tsx = (b7 >> 11 | b7 >> 4) & 1;
    let rest: &[(u32, u32, &str)] = &[
        (c1, 9, "SSSE3"),
        (de, 29, if intel { "EM64T" } else { "x86-64" }),
        (c1, 5, "VT-x"),
        (ce, 2, "AMD-V"),
        (c1, 25, "AES"),
        (c1, 28, "AVX"),
        (b7, 5, "AVX2"),
        (a71, 4, "AVX-VNNI"),
        (b7, 16, "AVX512"),
        (d71, 19, "AVX10"),
        (ce, 11, "XOP"),
        (c1, 12, "FMA3"),
        (ce, 16, "FMA4"),
        (tsx, 0, "TSX"),
        (b7, 29, "SHA"),
        (d7, 24, "AMX"),
        (a71, 1, "SM3"),
        (a71, 2, "SM4"),
    ];
    out.extend(rest.iter().filter(|(r, b, _)| has(*r, *b)).map(|(_, _, n)| n.to_string()));
    out.join(", ")
}

/// An ARM extension: its CPU-Z name and where each system reports it.
#[allow(dead_code)]
struct ArmFeature {
    name: &'static str,
    /// flag in the `Features` line of /proc/cpuinfo
    linux: &'static str,
    /// `hw.optional` sysctl on macOS
    apple: &'static str,
    /// `PF_ARM_*` id for IsProcessorFeaturePresent; 0 = Windows does not report it
    windows: u32,
}

const ARM: &[ArmFeature] = &[
    ArmFeature { name: "VFP/Neon", linux: "asimd", apple: "hw.optional.arm.AdvSIMD", windows: 19 },
    ArmFeature { name: "Arm V8 CRC32", linux: "crc32", apple: "hw.optional.arm.FEAT_CRC32", windows: 31 },
    ArmFeature { name: "Arm V8 Crypto", linux: "aes", apple: "hw.optional.arm.FEAT_AES", windows: 30 },
    ArmFeature { name: "Arm V8.2 DP", linux: "asimddp", apple: "hw.optional.arm.FEAT_DotProd", windows: 43 },
    ArmFeature { name: "Arm V8.1 Atomic", linux: "atomics", apple: "hw.optional.arm.FEAT_LSE", windows: 34 },
    ArmFeature { name: "Arm V8.3 LRCPC", linux: "lrcpc", apple: "hw.optional.arm.FEAT_LRCPC", windows: 45 },
    ArmFeature { name: "Arm V8.3 JSCVT", linux: "jscvt", apple: "hw.optional.arm.FEAT_JSCVT", windows: 44 },
    ArmFeature { name: "I8MM", linux: "i8mm", apple: "hw.optional.arm.FEAT_I8MM", windows: 0 },
    ArmFeature { name: "BF16", linux: "bf16", apple: "hw.optional.arm.FEAT_BF16", windows: 0 },
    ArmFeature { name: "SVE", linux: "sve", apple: "", windows: 0 },
    ArmFeature { name: "SVE2", linux: "sve2", apple: "", windows: 0 },
    ArmFeature { name: "SME", linux: "sme", apple: "hw.optional.arm.FEAT_SME", windows: 0 },
    ArmFeature { name: "SME2", linux: "sme2", apple: "hw.optional.arm.FEAT_SME2", windows: 0 },
];

/// Short line "NEON · AES/SHA · SME2" and full list "ARM64, VFP/Neon, Arm V8 CRC32, ...".
#[allow(dead_code)]
fn arm(has: impl Fn(&ArmFeature) -> bool) -> (String, String) {
    let have: Vec<&str> = ARM.iter().filter(|f| has(f)).map(|f| f.name).collect();
    if have.is_empty() {
        return (String::new(), String::new());
    }
    let mut short: Vec<&str> = [("VFP/Neon", "NEON"), ("Arm V8 Crypto", "AES/SHA")].iter().filter(|(n, _)| have.contains(n)).map(|(_, s)| *s).collect();
    // the newest vector extension
    if let Some(v) = ["SME2", "SME", "SVE2", "SVE"].iter().find(|v| have.contains(v)) {
        short.push(v);
    }
    let base = if cfg!(target_arch = "arm") { "ARM" } else { "ARM64" };
    (short.join(" · "), std::iter::once(base).chain(have).collect::<Vec<_>>().join(", "))
}

/// The first `isa` (RISC-V) or `Features` (ARM) line of /proc/cpuinfo.
#[allow(dead_code)]
fn from_cpuinfo(text: &str) -> (String, String) {
    let field = |key: &str| text.lines().find_map(|l| l.split_once(':').filter(|(k, _)| k.trim() == key).map(|(_, v)| v.trim().to_string()));
    if let Some(isa) = field("isa") {
        return riscv(&isa);
    }
    if let Some(flags) = field("Features") {
        let flags: Vec<&str> = flags.split_ascii_whitespace().collect();
        // 32-bit ARM names Neon "neon"
        return arm(|f| flags.contains(&f.linux) || (f.linux == "asimd" && flags.contains(&"neon")));
    }
    (String::new(), String::new())
}

/// "rv64imafdc_zicntr_zicsr_zba_zbb" → ("RV64GC · Zba · Zbb", "RV64IMAFDC, Zicntr, Zicsr, Zba, Zbb").
#[allow(dead_code)]
fn riscv(isa: &str) -> (String, String) {
    let isa = isa.trim().to_ascii_lowercase();
    let Some(rest) = isa.strip_prefix("rv") else { return (String::new(), String::new()) };
    let bits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    let mut parts = rest[bits.len()..].split('_');
    let first = parts.next().unwrap_or("");
    // single letters up to the first multi-letter extension ("z…", "s…", "x…")
    let cut = first.find(['z', 's', 'x']).unwrap_or(first.len());
    let mut letters: String = first[..cut].replace('g', "imafd");
    letters.retain(|c| c.is_ascii_alphabetic());
    let mut multi: Vec<String> = Vec::new();
    // legacy kernels end the letters with the privilege modes, "rv64imafdcsu"
    if !first[cut..].chars().all(|c| c == 's' || c == 'u') {
        multi.push(first[cut..].to_string());
    }
    multi.extend(parts.filter(|p| !p.is_empty()).map(String::from));
    let cap = |s: &str| s[..1].to_ascii_uppercase() + &s[1..];
    let full = std::iter::once(format!("RV{bits}{}", letters.to_ascii_uppercase())).chain(multi.iter().map(|m| cap(m))).collect::<Vec<_>>().join(", ");
    let base = if "imafd".chars().all(|c| letters.contains(c)) {
        format!("G{}", letters.chars().filter(|c| !"imafd".contains(*c)).collect::<String>())
    } else {
        letters.clone()
    };
    let mut short = vec![format!("RV{bits}{}", base.to_ascii_uppercase())];
    short.extend(["zba", "zbb", "zbs", "zbc", "zfh", "zvfh", "zvbb"].iter().filter(|z| multi.iter().any(|m| m == *z)).take(3).map(|z| cap(z)));
    (short.join(" · "), full)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn risc_v() {
        let (s, f) = riscv("rv64imafdc_zicntr_zicsr_zifencei_zihpm_zba_zbb");
        assert_eq!(s, "RV64GC · Zba · Zbb");
        assert_eq!(f, "RV64IMAFDC, Zicntr, Zicsr, Zifencei, Zihpm, Zba, Zbb");
        assert_eq!(riscv("rv64imafdcsu"), ("RV64GC".into(), "RV64IMAFDC".into()));
        assert_eq!(riscv("rv64gcv_zba").0, "RV64GCV · Zba");
        assert_eq!(riscv("rv32imac").0, "RV32IMAC");
    }

    #[test]
    fn arm_from_cpuinfo() {
        // Jetson Orin (Cortex-A78AE)
        let t = "processor\t: 0\nBogoMIPS\t: 62.50\nFeatures\t: fp asimd evtstrm aes pmull sha1 sha2 crc32 atomics fphp asimdhp cpuid asimdrdm lrcpc dcpop asimddp uscat ilrcpc flagm paca pacg\n";
        let (s, f) = from_cpuinfo(t);
        assert_eq!(s, "NEON · AES/SHA");
        assert_eq!(f, format!("{}, VFP/Neon, Arm V8 CRC32, Arm V8 Crypto, Arm V8.2 DP, Arm V8.1 Atomic, Arm V8.3 LRCPC", if cfg!(target_arch = "arm") { "ARM" } else { "ARM64" }));
        assert_eq!(from_cpuinfo("isa\t\t: rv64imafdc_zba\n").0, "RV64GC · Zba");
        assert_eq!(from_cpuinfo("model name : x\n"), (String::new(), String::new()));
    }
}
