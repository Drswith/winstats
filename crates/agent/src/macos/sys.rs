//! The Mac for the CPU and memory popups: macOS version and build, caches, virtualization,
//! memory modules (once); tasks, threads, open files and where the memory goes (while a popup
//! is open).

use super::sysctl;
use std::ffi::{c_void, CString};
use winstats_proto::{MemDetail, SysSample};

extern "C" {
    fn host_statistics64(host: u32, flavor: i32, info: *mut i32, count: *mut u32) -> i32;
    fn processor_set_default(host: u32, pset: *mut u32) -> i32;
    fn processor_set_statistics(pset: u32, flavor: i32, info: *mut i32, count: *mut u32) -> i32;
}

const HOST_VM_INFO64: i32 = 4;
const PROCESSOR_SET_LOAD_INFO: i32 = 4;

/// `vm_statistics64` of <mach/vm_statistics.h>.
#[repr(C)]
#[derive(Default)]
struct VmStats {
    free_count: u32,
    active_count: u32,
    inactive_count: u32,
    wire_count: u32,
    zero_fill_count: u64,
    reactivations: u64,
    pageins: u64,
    pageouts: u64,
    faults: u64,
    cow_faults: u64,
    lookups: u64,
    hits: u64,
    purges: u64,
    purgeable_count: u32,
    speculative_count: u32,
    decompressions: u64,
    compressions: u64,
    swapins: u64,
    swapouts: u64,
    compressor_page_count: u32,
    throttled_count: u32,
    external_page_count: u32,
    internal_page_count: u32,
    total_uncompressed_pages_in_compressor: u64,
}

const _: () = assert!(std::mem::size_of::<VmStats>() == 152);

/// A string sysctl ("kern.osproductversion").
pub fn sysctl_str(name: &str) -> Option<String> {
    let c = CString::new(name).ok()?;
    let mut buf = vec![0u8; 256];
    let mut len = buf.len();
    let r = unsafe { libc::sysctlbyname(c.as_ptr(), buf.as_mut_ptr() as *mut c_void, &mut len, std::ptr::null_mut(), 0) };
    (r == 0).then(|| String::from_utf8_lossy(&buf[..len]).trim_end_matches('\0').trim().to_string()).filter(|s| !s.is_empty())
}

/// ("macOS 15.8.1 (24H32)", "Darwin 24.6.0").
pub fn os() -> (String, String) {
    let v = sysctl_str("kern.osproductversion").unwrap_or_default();
    let os = match sysctl_str("kern.osversion") {
        Some(b) => format!("macOS {v} ({b})"),
        None => format!("macOS {v}"),
    };
    (os, sysctl_str("kern.osrelease").map(|r| format!("Darwin {r}")).unwrap_or_default())
}

/// L1 (data + instruction), L2, L3 over the whole machine, bytes: per core kind, one L2 for
/// each `cpusperl2` cores (Apple silicon) or per core (Intel), one L3.
pub fn caches() -> [u64; 3] {
    let n = |k: &str| sysctl(k).unwrap_or(0);
    let mut out = [0u64; 3];
    if n("hw.nperflevels") > 0 {
        for lvl in 0..n("hw.nperflevels") {
            let p = format!("hw.perflevel{lvl}");
            let cores = n(&format!("{p}.physicalcpu"));
            out[0] += cores * (n(&format!("{p}.l1icachesize")) + n(&format!("{p}.l1dcachesize")));
            let per = n(&format!("{p}.cpusperl2")).max(1);
            out[1] += cores.div_ceil(per) * n(&format!("{p}.l2cachesize"));
        }
        // Intel Macs list one perflevel too, and have an L3
        out[2] = n("hw.l3cachesize");
    } else {
        let cores = n("hw.physicalcpu");
        out = [cores * (n("hw.l1icachesize") + n("hw.l1dcachesize")), cores * n("hw.l2cachesize"), n("hw.l3cachesize")];
    }
    out
}

/// (Hypervisor.framework usable, this Mac is a VM).
pub fn virtualization() -> (Option<bool>, bool) {
    (sysctl("kern.hv_support").map(|v| v == 1), sysctl("kern.hv_vmm_present") == Some(1))
}

/// Rated clock of an Intel Mac, MHz.
pub fn base_mhz() -> Option<u32> {
    sysctl("hw.cpufrequency").filter(|h| *h > 0).map(|h| (h / 1_000_000) as u32)
}

/// Tasks, threads (the kernel's own counts, no root needed), open files.
pub fn counters() -> SysSample {
    let mut s = SysSample { handles: sysctl("kern.num_files").unwrap_or(0) as u32, ..Default::default() };
    // SAFETY: plain Mach calls into a 4-integer buffer
    unsafe {
        #[allow(deprecated)]
        let host = libc::mach_host_self();
        let mut pset = 0u32;
        let mut info = [0i32; 4];
        let mut count = 4u32;
        if processor_set_default(host, &mut pset) == 0 && processor_set_statistics(pset, PROCESSOR_SET_LOAD_INFO, info.as_mut_ptr(), &mut count) == 0 {
            s.processes = info[0].max(0) as u32;
            s.threads = info[1].max(0) as u32;
        }
    }
    s
}

/// Activity Monitor's split: app memory (anonymous less purgeable), wired, cached files.
pub fn mem_detail() -> Option<MemDetail> {
    let mut v = VmStats::default();
    let mut count = (std::mem::size_of::<VmStats>() / 4) as u32;
    // SAFETY: the buffer is the size the count announces
    let r = unsafe {
        #[allow(deprecated)]
        let host = libc::mach_host_self();
        host_statistics64(host, HOST_VM_INFO64, &mut v as *mut VmStats as *mut i32, &mut count)
    };
    if r != 0 {
        return None;
    }
    let page = sysctl("hw.pagesize").unwrap_or(4096);
    let b = |p: u32| p as u64 * page;
    Some(MemDetail {
        processes: b(v.internal_page_count.saturating_sub(v.purgeable_count)),
        kernel_nonpaged: b(v.wire_count),
        cache: b(v.external_page_count + v.purgeable_count),
        ..Default::default()
    })
}

/// "LPDDR5", "DDR4 2667 MT/s, 2 × 32 GB" and the channels in use, from System Information
/// (half a second, read once).
pub fn memory() -> (String, usize) {
    let Ok(out) = std::process::Command::new("system_profiler").args(["SPMemoryDataType", "-json"]).output() else { return (String::new(), 0) };
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(&out.stdout) else { return (String::new(), 0) };
    let Some(top) = v.get("SPMemoryDataType").and_then(|a| a.get(0)) else { return (String::new(), 0) };
    let text = |o: &serde_json::Value, k: &str| o.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
    let items: Vec<&serde_json::Value> = top.get("_items").and_then(|i| i.as_array()).map(|a| a.iter().filter(|m| text(m, "dimm_size") != "empty").collect()).unwrap_or_default();
    // Apple silicon: one entry for the package, "LPDDR5"
    let Some(first) = items.first() else { return (text(top, "dimm_type"), 0) };
    let mut s = text(first, "dimm_type");
    if let Some(mhz) = text(first, "dimm_speed").split_whitespace().next().filter(|m| m.parse::<u32>().is_ok()) {
        s = format!("{s} {mhz} MT/s").trim().to_string();
    }
    let size = text(first, "dimm_size");
    let sizes = if items.iter().all(|m| text(m, "dimm_size") == size) { format!("{} \u{00D7} {size}", items.len()) } else { items.iter().map(|m| text(m, "dimm_size")).collect::<Vec<_>>().join(" + ") };
    // "BANK 0/ChannelA-DIMM0"
    let mut ids: Vec<String> = Vec::new();
    for m in &items {
        let name = text(m, "_name").to_ascii_lowercase();
        let Some(p) = name.find("channel") else { return (format!("{s}, {sizes}"), 0) };
        let id: String = name[p + 7..].chars().take_while(|c| c.is_ascii_alphanumeric()).collect();
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    (format!("{s}, {sizes}"), ids.len())
}
