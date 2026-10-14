//! Light system facts for the popups, none needing elevation: CPU caches, base / boost clock,
//! instruction sets and virtualization (read once), memory reserved by the hardware (once), and
//! the page file use (read while the popup is open).

use windows::Wdk::System::SystemInformation::{NtQuerySystemInformation, SYSTEM_INFORMATION_CLASS};
use windows::Win32::System::Power::{CallNtPowerInformation, ProcessorInformation};
use windows::Win32::System::SystemInformation::{GetLogicalProcessorInformationEx, GetPhysicallyInstalledSystemMemory, RelationCache, SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX};
use windows::Win32::System::Threading::{IsProcessorFeaturePresent, PROCESSOR_FEATURE_ID};

#[derive(Clone, Debug, Default)]
pub struct CpuInfo {
    /// total L1 (data + instruction), L2 and L3 over the whole package, bytes; 0 = unknown
    pub cache: [u64; 3],
    /// rated clock, MHz
    pub base_mhz: Option<u32>,
    /// maximum boost clock where CPUID reports it (Intel, Skylake and later), MHz
    pub max_mhz: Option<u32>,
    /// "SSE4.2 · AES · AVX · AVX2 · AVX-512 · SHA": the notable extensions present
    pub features: String,
    /// every extension CPUID reports, space separated (the expanded list in the popup)
    pub all_features: String,
    /// virtualization enabled in firmware; None where Windows cannot tell (Windows 7)
    pub virtualization: Option<bool>,
    /// a hypervisor runs (Hyper-V, VBS, or this is a VM): the firmware switch is hidden then
    pub hypervisor: bool,
}

pub fn cpu_info() -> CpuInfo {
    CpuInfo { cache: caches(), base_mhz: base_clock(), max_mhz: boost_clock(), features: features(), all_features: all_features(), virtualization: virtualization(), hypervisor: hypervisor() }
}

fn caches() -> [u64; 3] {
    let mut out = [0u64; 3];
    unsafe {
        let mut len = 0u32;
        let _ = GetLogicalProcessorInformationEx(RelationCache, None, &mut len);
        if len == 0 {
            return out;
        }
        let mut buf = vec![0u8; len as usize];
        if GetLogicalProcessorInformationEx(RelationCache, Some(buf.as_mut_ptr() as *mut _), &mut len).is_err() {
            return out;
        }
        let mut off = 0usize;
        while off + 8 <= len as usize {
            let e = &*(buf.as_ptr().add(off) as *const SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX);
            if e.Size == 0 {
                break;
            }
            if e.Relationship == RelationCache {
                let c = &e.Anonymous.Cache;
                if (1..=3).contains(&c.Level) {
                    out[c.Level as usize - 1] += c.CacheSize as u64;
                }
            }
            off += e.Size as usize;
        }
    }
    out
}

/// PROCESSOR_POWER_INFORMATION
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct PowerInfo {
    number: u32,
    max_mhz: u32,
    current_mhz: u32,
    mhz_limit: u32,
    max_idle_state: u32,
    current_idle_state: u32,
}

fn base_clock() -> Option<u32> {
    let n = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1).max(1);
    let mut v = vec![PowerInfo::default(); n.max(64)];
    let size = (v.len() * std::mem::size_of::<PowerInfo>()) as u32;
    let st = unsafe { CallNtPowerInformation(ProcessorInformation, None, 0, Some(v.as_mut_ptr() as *mut _), size) };
    if st.is_err() {
        return None;
    }
    v.iter().map(|p| p.max_mhz).max().filter(|m| *m > 0)
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
fn boost_clock() -> Option<u32> {
    // leaf 0x16 (processor frequency information) exists on Intel since Skylake
    let (max_leaf, ..) = cpuid(0, 0);
    if max_leaf < 0x16 {
        return None;
    }
    let (_, max, ..) = cpuid(0x16, 0);
    let max = max & 0xFFFF;
    (max > 0).then_some(max)
}

#[cfg(not(any(target_arch = "x86", target_arch = "x86_64")))]
fn boost_clock() -> Option<u32> {
    None
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
fn features() -> String {
    let (max_leaf, ..) = cpuid(0, 0);
    let (_, _, ecx1, _) = cpuid(1, 0);
    let (_, ebx7, ..) = if max_leaf >= 7 { cpuid(7, 0) } else { (0, 0, 0, 0) };
    let list = [
        (ecx1 >> 20 & 1, "SSE4.2"),
        (ecx1 >> 25 & 1, "AES"),
        (ecx1 >> 28 & 1, "AVX"),
        (ebx7 >> 5 & 1, "AVX2"),
        (ebx7 >> 16 & 1, "AVX-512"),
        (ebx7 >> 29 & 1, "SHA"),
    ];
    list.iter().filter(|(bit, _)| *bit == 1).map(|(_, name)| *name).collect::<Vec<_>>().join(" · ")
}

/// The short line on ARM: Neon and the crypto extensions where present.
#[cfg(target_arch = "aarch64")]
fn features() -> String {
    let have = arm_features();
    [("VFP/Neon", "NEON"), ("Arm V8 Crypto", "AES/SHA")].iter().filter(|(full, _)| have.contains(full)).map(|(_, short)| *short).collect::<Vec<_>>().join(" · ")
}

#[cfg(not(any(target_arch = "x86", target_arch = "x86_64", target_arch = "aarch64")))]
fn features() -> String {
    String::new()
}

/// The instruction sets exactly as CPU-Z 2.x lists them (its own closed set and order, checked
/// against cpuz_x64.exe): "MMX, SSE (1, 2, 3, 4.1, 4.2), SSSE3, EM64T, VT-x, AES, AVX, AVX2,
/// FMA3". The 64-bit mode is EM64T on Intel and x86-64 on AMD and the rest; "(+)" marks AMD's
/// extended MMX / 3DNow!.
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
fn all_features() -> String {
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

/// ARM (Snapdragon and the like), named as CPU-Z for ARM64 lists them, from what Windows
/// reports through IsProcessorFeaturePresent; a feature an older Windows does not know reads
/// as absent.
#[cfg(target_arch = "aarch64")]
const ARM_FEATURES: &[(u32, &str)] = &[
    (19, "VFP/Neon"),
    (31, "Arm V8 CRC32"),
    (30, "Arm V8 Crypto"),
    (43, "Arm V8.2 DP"),
    (34, "Arm V8.1 Atomic"),
    (45, "Arm V8.3 LRCPC"),
    (44, "Arm V8.3 JSCVT"),
];

#[cfg(target_arch = "aarch64")]
fn arm_features() -> Vec<&'static str> {
    ARM_FEATURES.iter().filter(|(id, _)| unsafe { IsProcessorFeaturePresent(PROCESSOR_FEATURE_ID(*id)) }.as_bool()).map(|(_, n)| *n).collect()
}

#[cfg(target_arch = "aarch64")]
fn all_features() -> String {
    std::iter::once("ARM64").chain(arm_features()).collect::<Vec<_>>().join(", ")
}

#[cfg(not(any(target_arch = "x86", target_arch = "x86_64", target_arch = "aarch64")))]
fn all_features() -> String {
    String::new()
}

/// CPUID leaf 1, ECX bit 31: set by every hypervisor for the OS it runs.
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
fn hypervisor() -> bool {
    cpuid(1, 0).2 >> 31 & 1 == 1
}

#[cfg(not(any(target_arch = "x86", target_arch = "x86_64")))]
fn hypervisor() -> bool {
    false
}

fn virtualization() -> Option<bool> {
    // PF_VIRT_FIRMWARE_ENABLED exists from Windows 8; Windows 7 always answers false
    if crate::util::windows_build() < 9200 {
        return None;
    }
    Some(unsafe { IsProcessorFeaturePresent(PROCESSOR_FEATURE_ID(21)) }.as_bool())
}

/// Installed RAM the OS does not see (firmware, integrated graphics, reservations), bytes.
pub fn hardware_reserved(visible: u64) -> u64 {
    let mut kb = 0u64;
    if unsafe { GetPhysicallyInstalledSystemMemory(&mut kb) }.is_err() {
        return 0;
    }
    (kb * 1024).saturating_sub(visible)
}

/// SYSTEM_PAGEFILE_INFORMATION
#[repr(C)]
struct PageFileInfo {
    next_entry_offset: u32,
    total_size: u32,
    total_in_use: u32,
    peak_usage: u32,
    name_len: u16,
    name_max: u16,
    name: *const u16,
}

/// (in use, size) over all page files, bytes; (0, 0) without a page file.
pub fn page_file() -> (u64, u64) {
    let page = 4096u64;
    let mut buf = vec![0u8; 4096];
    let mut ret = 0u32;
    // SystemPageFileInformation = 18
    let st = unsafe { NtQuerySystemInformation(SYSTEM_INFORMATION_CLASS(18), buf.as_mut_ptr() as *mut _, buf.len() as u32, &mut ret) };
    if st.is_err() || ret == 0 {
        return (0, 0);
    }
    let (mut used, mut total) = (0u64, 0u64);
    let mut off = 0usize;
    while off + std::mem::size_of::<PageFileInfo>() <= buf.len() {
        let p = unsafe { &*(buf.as_ptr().add(off) as *const PageFileInfo) };
        used += p.total_in_use as u64 * page;
        total += p.total_size as u64 * page;
        if p.next_entry_offset == 0 {
            break;
        }
        off += p.next_entry_offset as usize;
    }
    (used, total)
}
