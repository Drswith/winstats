//! Facts and counters of the machine for the CPU and memory popups: the distribution and
//! kernel, caches, clocks, virtualization (once); processes, threads, open files, context
//! switches, interrupts and where the memory goes (while a popup is open).

use super::{dirs, read, read_num, Root};
use std::time::Instant;
use winstats_proto::{MemDetail, SysSample};

/// `PRETTY_NAME` of os-release: "Ubuntu 24.04.3 LTS", "Debian GNU/Linux 12 (bookworm)".
pub fn os_pretty(root: &Root) -> Option<String> {
    let text = read(&root.path("/etc/os-release")).or_else(|| read(&root.path("/usr/lib/os-release")))?;
    text.lines().find_map(|l| l.strip_prefix("PRETTY_NAME=")).map(|v| v.trim().trim_matches('"').to_string()).filter(|v| !v.is_empty())
}

/// L1 (data + instruction), L2 and L3 of the whole machine, bytes: each cache once, however
/// many cores share it.
pub fn caches(root: &Root) -> [u64; 3] {
    let mut out = [0u64; 3];
    let mut seen: Vec<(u64, String, String)> = Vec::new();
    for cpu in dirs(&root.path("/sys/devices/system/cpu")) {
        let n = cpu.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        if !n.starts_with("cpu") || !n[3..].chars().all(|c| c.is_ascii_digit()) || n.len() == 3 {
            continue;
        }
        for idx in dirs(&cpu.join("cache")) {
            let (Some(level), Some(size)) = (read_num(&idx.join("level")), read(&idx.join("size"))) else { continue };
            let kind = read(&idx.join("type")).unwrap_or_default();
            let shared = read(&idx.join("shared_cpu_list")).unwrap_or_else(|| n.clone());
            let key = (level as u64, kind, shared);
            if !(1..=3).contains(&level) || seen.contains(&key) {
                continue;
            }
            out[level as usize - 1] += size_bytes(&size);
            seen.push(key);
        }
    }
    out
}

/// "32K", "1024K", "8M" → bytes.
fn size_bytes(s: &str) -> u64 {
    let s = s.trim();
    let (num, mul) = match s.chars().last() {
        Some('K') => (&s[..s.len() - 1], 1024),
        Some('M') => (&s[..s.len() - 1], 1024 * 1024),
        Some('G') => (&s[..s.len() - 1], 1024 * 1024 * 1024),
        _ => (s, 1),
    };
    num.parse::<u64>().unwrap_or(0) * mul
}

/// (rated, highest) clock, MHz: intel_pstate / amd-pstate `base_frequency`, the highest
/// `cpuinfo_max_freq` of any core.
pub fn clocks(root: &Root) -> (Option<u32>, Option<u32>) {
    let khz = |p: &str| read_num(&root.path(p)).filter(|v| *v > 0).map(|v| (v / 1000) as u32);
    let base = khz("/sys/devices/system/cpu/cpu0/cpufreq/base_frequency").or_else(|| khz("/sys/devices/system/cpu/cpu0/cpufreq/amd_pstate_nominal_freq"));
    let max = dirs(&root.path("/sys/devices/system/cpu")).iter().filter_map(|c| read_num(&c.join("cpufreq/cpuinfo_max_freq"))).filter(|v| *v > 0).max().map(|v| (v / 1000) as u32);
    (base, max)
}

/// (virtualization usable, running under a hypervisor). x86 names VT-x / AMD-V in the CPU flags
/// (the kernel clears them when the firmware switch is off); elsewhere `/dev/kvm` tells.
pub fn virtualization(root: &Root) -> (Option<bool>, bool) {
    let cpuinfo = read(&root.path("/proc/cpuinfo")).unwrap_or_default();
    let flags: Vec<&str> = cpuinfo.lines().find(|l| l.starts_with("flags")).and_then(|l| l.split_once(':')).map(|(_, v)| v.split_ascii_whitespace().collect()).unwrap_or_default();
    let hyper = flags.contains(&"hypervisor") || root.path("/sys/firmware/devicetree/base/hypervisor").exists() || read(&root.path("/sys/hypervisor/type")).is_some();
    let kvm = root.path("/dev/kvm").exists();
    let virt = if !flags.is_empty() {
        Some(kvm || flags.contains(&"vmx") || flags.contains(&"svm"))
    } else if kvm {
        Some(true)
    } else {
        None
    };
    (virt, hyper)
}

/// The counters of /proc/stat whose rates the popup shows.
#[derive(Clone, Copy, Default, Debug, PartialEq)]
struct Stat {
    ctxt: u64,
    intr: u64,
    /// all CPU time, the irq and softirq parts
    total: u64,
    irq: u64,
    softirq: u64,
}

fn parse_stat(text: &str) -> Stat {
    let mut s = Stat::default();
    for line in text.lines() {
        let mut it = line.split_ascii_whitespace();
        match it.next() {
            Some("cpu") => {
                let v: Vec<u64> = it.map(|x| x.parse().unwrap_or(0)).collect();
                // user nice system idle iowait irq softirq steal (guest is inside user)
                s.total = v.iter().take(8).sum();
                s.irq = v.get(5).copied().unwrap_or(0);
                s.softirq = v.get(6).copied().unwrap_or(0);
            }
            Some("ctxt") => s.ctxt = it.next().and_then(|x| x.parse().ok()).unwrap_or(0),
            Some("intr") => s.intr = it.next().and_then(|x| x.parse().ok()).unwrap_or(0),
            _ => {}
        }
    }
    s
}

/// Turns /proc counters into the popup's numbers; rates need two reads.
pub struct SysCounters {
    root: Root,
    prev: Option<(Stat, Instant)>,
}

impl SysCounters {
    pub fn new(root: Root) -> Self {
        SysCounters { root, prev: None }
    }

    pub fn sample(&mut self) -> SysSample {
        let r = &self.root;
        let processes = dirs(&r.path("/proc")).iter().filter(|p| p.file_name().map_or(false, |n| n.to_string_lossy().chars().all(|c| c.is_ascii_digit()))).count() as u32;
        // "0.20 0.18 0.11 2/1093 5402": runnable / all scheduling entities, i.e. threads
        let threads = read(&r.path("/proc/loadavg")).and_then(|l| l.split_ascii_whitespace().nth(3).and_then(|f| f.split_once('/')).and_then(|(_, t)| t.parse().ok())).unwrap_or(0);
        // allocated file handles, the closest thing to Windows' handle count
        let handles = read(&r.path("/proc/sys/fs/file-nr")).and_then(|l| l.split_ascii_whitespace().next().and_then(|v| v.parse().ok())).unwrap_or(0);
        let uptime_secs = read(&r.path("/proc/uptime")).and_then(|l| l.split_ascii_whitespace().next().and_then(|v| v.parse::<f64>().ok())).map_or(0, |v| v as u64);
        let mut out = SysSample { processes, threads, handles, uptime_secs, ..Default::default() };
        let now = Instant::now();
        let cur = parse_stat(&read(&r.path("/proc/stat")).unwrap_or_default());
        if let Some((p, t)) = self.prev {
            let dt = now.duration_since(t).as_secs_f64();
            if dt > 0.05 && cur.ctxt >= p.ctxt {
                out.ctx_switches = Some((cur.ctxt - p.ctxt) as f64 / dt);
                out.interrupts = Some(cur.intr.saturating_sub(p.intr) as f64 / dt);
                let span = cur.total.saturating_sub(p.total) as f32;
                if span > 0.0 {
                    out.interrupt_time = Some(cur.irq.saturating_sub(p.irq) as f32 / span);
                    out.dpc_time = Some(cur.softirq.saturating_sub(p.softirq) as f32 / span);
                }
            }
        }
        self.prev = Some((cur, now));
        out
    }
}

/// Where the memory goes, from /proc/meminfo (kB values).
pub fn mem_detail(root: &Root) -> Option<MemDetail> {
    let text = read(&root.path("/proc/meminfo"))?;
    let kb = |k: &str| -> u64 { text.lines().find_map(|l| l.strip_prefix(k).and_then(|r| r.strip_prefix(':'))).and_then(|v| v.split_ascii_whitespace().next()?.parse::<u64>().ok()).unwrap_or(0) * 1024 };
    let swap_total = kb("SwapTotal");
    // the commit limit is enforced only in strict mode; otherwise RAM + swap is the ceiling
    let strict = read(&root.path("/proc/sys/vm/overcommit_memory")).as_deref() == Some("2");
    let commit_limit = if strict { kb("CommitLimit") } else { kb("MemTotal") + swap_total };
    // reclaimable slab (dentries, inodes) counts as available, like the file cache
    Some(MemDetail {
        processes: kb("AnonPages"),
        kernel_paged: 0,
        kernel_nonpaged: kb("SUnreclaim") + kb("KernelStack") + kb("PageTables"),
        cache: (kb("Cached") + kb("Buffers")).saturating_sub(kb("Shmem")),
        commit_used: kb("Committed_AS"),
        commit_limit,
        swap_used: swap_total.saturating_sub(kb("SwapFree")),
        swap_total,
        hw_reserved: 0,
    })
}

#[cfg(test)]
mod tests {
    use super::super::fake;
    use super::*;

    #[test]
    fn os_and_caches() {
        let t = fake::tree("sys");
        fake::file(&t, "etc/os-release", "NAME=\"Ubuntu\"\nPRETTY_NAME=\"Ubuntu 24.04.3 LTS\"\nID=ubuntu\n");
        // two cores with their own L1d / L1i / L2 and one shared L3
        for c in 0..2 {
            let d = format!("sys/devices/system/cpu/cpu{c}/cache");
            for (i, level, kind, size, shared) in [(0, 1, "Data", "32K", c.to_string()), (1, 1, "Instruction", "32K", c.to_string()), (2, 2, "Unified", "1024K", c.to_string()), (3, 3, "Unified", "8M", "0-1".to_string())] {
                fake::file(&t, &format!("{d}/index{i}/level"), &level.to_string());
                fake::file(&t, &format!("{d}/index{i}/type"), kind);
                fake::file(&t, &format!("{d}/index{i}/size"), size);
                fake::file(&t, &format!("{d}/index{i}/shared_cpu_list"), &shared);
            }
            fake::file(&t, &format!("sys/devices/system/cpu/cpu{c}/cpufreq/cpuinfo_max_freq"), &(2_400_000 + c * 100_000).to_string());
        }
        fake::file(&t, "sys/devices/system/cpu/cpu0/cpufreq/base_frequency", "2000000");
        let root = Root(t.clone());
        assert_eq!(os_pretty(&root).as_deref(), Some("Ubuntu 24.04.3 LTS"));
        assert_eq!(caches(&root), [128 * 1024, 2048 * 1024, 8 * 1024 * 1024]);
        assert_eq!(clocks(&root), (Some(2000), Some(2500)));
    }

    #[test]
    fn counters() {
        let t = fake::tree("syscnt");
        fake::file(&t, "proc/1/stat", "");
        fake::file(&t, "proc/42/stat", "");
        fake::file(&t, "proc/self/stat", "");
        fake::file(&t, "proc/loadavg", "0.20 0.18 0.11 2/1093 5402");
        fake::file(&t, "proc/sys/fs/file-nr", "4416\t0\t9223372036854775807");
        fake::file(&t, "proc/uptime", "3600.5 100.0");
        fake::file(&t, "proc/stat", "cpu  100 0 50 800 0 10 40 0 0 0\ncpu0 1 2 3\nintr 1000 1 2\nctxt 5000\n");
        let mut c = SysCounters::new(Root(t.clone()));
        let a = c.sample();
        assert_eq!((a.processes, a.threads, a.handles, a.uptime_secs), (2, 1093, 4416, 3600));
        assert_eq!(a.ctx_switches, None);
        fake::file(&t, "proc/stat", "cpu  200 0 100 1600 0 30 70 0 0 0\nintr 3000 1 2\nctxt 9000\n");
        c.prev = c.prev.map(|(s, t)| (s, t - std::time::Duration::from_secs(2)));
        let b = c.sample();
        let ctx = b.ctx_switches.unwrap();
        assert!((1900.0..2100.0).contains(&ctx), "{ctx}");
        // 20 of 1000 ticks in irq, 30 in softirq
        assert!((b.interrupt_time.unwrap() - 0.02).abs() < 1e-6);
        assert!((b.dpc_time.unwrap() - 0.03).abs() < 1e-6);
    }

    #[test]
    fn meminfo() {
        let t = fake::tree("meminfo");
        fake::file(&t, "proc/meminfo", "MemTotal:       16000 kB\nMemFree: 1000 kB\nBuffers: 100 kB\nCached: 3000 kB\nSwapTotal: 2000 kB\nSwapFree: 1500 kB\nAnonPages: 5000 kB\nShmem: 200 kB\nSReclaimable: 400 kB\nSUnreclaim: 300 kB\nKernelStack: 50 kB\nPageTables: 70 kB\nCommitted_AS: 9000 kB\nCommitLimit: 10000 kB\n");
        let m = mem_detail(&Root(t)).unwrap();
        assert_eq!(m.processes, 5000 * 1024);
        assert_eq!(m.kernel_nonpaged, 420 * 1024);
        assert_eq!(m.cache, 2900 * 1024);
        assert_eq!((m.commit_used, m.commit_limit), (9000 * 1024, 18000 * 1024));
        assert_eq!((m.swap_used, m.swap_total), (500 * 1024, 2000 * 1024));
    }
}
