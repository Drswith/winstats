//! Apple silicon clocks and power through IOReport, the private library `powermetrics` uses
//! (loaded with dlopen, no root needed): residency in each performance state of the CPU clusters
//! and the GPU, weighted with the frequency tables of the `pmgr` device, and the energy counters
//! of the CPU and GPU. Intel Macs have none of it and `IoReport::open` returns None.

use super::{get, string, CFArrayGetCount, CFArrayGetValueAtIndex, CFTypeRef, Cf, CFIndex, Io};
use std::ffi::{c_void, CString};
use std::time::Instant;

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFDictionaryGetCount(d: CFTypeRef) -> CFIndex;
    fn CFDictionaryCreateMutableCopy(alloc: CFTypeRef, cap: CFIndex, d: CFTypeRef) -> CFTypeRef;
}

type CopyChannels = unsafe extern "C" fn(CFTypeRef, CFTypeRef, u64, u64, u64) -> CFTypeRef;
type MergeChannels = unsafe extern "C" fn(CFTypeRef, CFTypeRef, CFTypeRef);
type CreateSubscription = unsafe extern "C" fn(*const c_void, CFTypeRef, *mut CFTypeRef, u64, CFTypeRef) -> CFTypeRef;
type CreateSamples = unsafe extern "C" fn(CFTypeRef, CFTypeRef, CFTypeRef) -> CFTypeRef;
type CreateDelta = unsafe extern "C" fn(CFTypeRef, CFTypeRef, CFTypeRef) -> CFTypeRef;
type GetStr = unsafe extern "C" fn(CFTypeRef) -> CFTypeRef;
type GetInt = unsafe extern "C" fn(CFTypeRef, i32) -> i64;
type GetCount = unsafe extern "C" fn(CFTypeRef) -> i32;
type GetStateName = unsafe extern "C" fn(CFTypeRef, i32) -> CFTypeRef;
type GetResidency = unsafe extern "C" fn(CFTypeRef, i32) -> i64;

struct Api {
    create_samples: CreateSamples,
    create_delta: CreateDelta,
    group: GetStr,
    subgroup: GetStr,
    name: GetStr,
    unit: GetStr,
    simple_int: GetInt,
    state_count: GetCount,
    state_name: GetStateName,
    residency: GetResidency,
}

/// What one interval measured; None where the Mac has no such channel.
#[derive(Default, Debug, Clone)]
pub struct Reading {
    /// average clock of each CPU cluster while it was running, MHz, in the order of `clusters`
    pub cluster_mhz: Vec<Option<u32>>,
    pub gpu_mhz: Option<u32>,
    pub cpu_w: Option<f32>,
    pub gpu_w: Option<f32>,
}

/// A CPU cluster: its IOReport channel ("ECPU", "PCPU1") and frequency table.
#[derive(Debug, Clone)]
pub struct Cluster {
    pub channel: String,
    /// efficiency cluster ("E…") or performance cluster
    pub efficiency: bool,
    mhz: Vec<u32>,
}

impl Cluster {
    pub fn min_mhz(&self) -> Option<u32> {
        self.mhz.iter().copied().filter(|m| *m > 0).min()
    }

    pub fn max_mhz(&self) -> Option<u32> {
        self.mhz.iter().copied().max().filter(|m| *m > 0)
    }
}

// SAFETY: the subscription and samples are only touched from the thread that holds the
// collector's lock.
unsafe impl Send for IoReport {}

pub struct IoReport {
    api: Api,
    sub: Cf,
    /// our copy of the channels, kept alive for `subbed`
    _chans: Cf,
    /// the channels the subscription accepted; owned by IOReport, never released
    subbed: CFTypeRef,
    prev: Option<(Cf, Instant)>,
    pub clusters: Vec<Cluster>,
    gpu_mhz: Vec<u32>,
    pub gpu_max_mhz: Option<u32>,
}

const GROUPS: [(&str, Option<&str>); 3] = [("Energy Model", None), ("CPU Stats", Some("CPU Complex Performance States")), ("GPU Stats", Some("GPU Performance States"))];

/// A `voltage-statesN` property: (frequency, voltage) pairs of u32; frequencies are Hz on
/// M1–M3 and the GPU, kHz on the M4 CPU clusters. Zero entries (an "off" state) are kept so
/// indices line up with the states.
fn table(pmgr: &Io, key: &str) -> Vec<u32> {
    let Some(b) = pmgr.prop(key).and_then(|v| super::data(v.0)) else { return Vec::new() };
    let raw: Vec<u32> = b.chunks_exact(8).map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect();
    let div = if raw.iter().any(|f| *f > 100_000_000) { 1_000_000 } else { 1_000 };
    raw.iter().map(|f| f / div).collect()
}

impl IoReport {
    pub fn open() -> Option<IoReport> {
        let path = CString::new("/usr/lib/libIOReport.dylib").ok()?;
        let lib = unsafe { libc::dlopen(path.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
        if lib.is_null() {
            return None;
        }
        let sym = |n: &str| -> Option<*mut c_void> {
            let c = CString::new(n).ok()?;
            let p = unsafe { libc::dlsym(lib, c.as_ptr()) };
            (!p.is_null()).then_some(p)
        };
        // SAFETY: the signatures are those of libIOReport, as used by powermetrics' callers.
        let (copy, merge, subscribe): (CopyChannels, MergeChannels, CreateSubscription) = unsafe { (std::mem::transmute(sym("IOReportCopyChannelsInGroup")?), std::mem::transmute(sym("IOReportMergeChannels")?), std::mem::transmute(sym("IOReportCreateSubscription")?)) };
        let api = unsafe {
            Api {
                create_samples: std::mem::transmute(sym("IOReportCreateSamples")?),
                create_delta: std::mem::transmute(sym("IOReportCreateSamplesDelta")?),
                group: std::mem::transmute(sym("IOReportChannelGetGroup")?),
                subgroup: std::mem::transmute(sym("IOReportChannelGetSubGroup")?),
                name: std::mem::transmute(sym("IOReportChannelGetChannelName")?),
                unit: std::mem::transmute(sym("IOReportChannelGetUnitLabel")?),
                simple_int: std::mem::transmute(sym("IOReportSimpleGetIntegerValue")?),
                state_count: std::mem::transmute(sym("IOReportStateGetCount")?),
                state_name: std::mem::transmute(sym("IOReportStateGetNameForIndex")?),
                residency: std::mem::transmute(sym("IOReportStateGetResidency")?),
            }
        };
        let mut all: Option<Cf> = None;
        for (g, sub) in GROUPS {
            let g = super::cfstr(g);
            let sub = sub.map(super::cfstr);
            let Some(c) = Cf::new(unsafe { copy(g.0, sub.as_ref().map_or(std::ptr::null(), |s| s.0), 0, 0, 0) }) else { continue };
            match &all {
                Some(a) => unsafe { merge(a.0, c.0, std::ptr::null()) },
                None => all = Some(c),
            }
        }
        let all = all?;
        let chans = Cf::new(unsafe { CFDictionaryCreateMutableCopy(std::ptr::null(), CFDictionaryGetCount(all.0), all.0) })?;
        let mut subbed: CFTypeRef = std::ptr::null();
        let sub = Cf::new(unsafe { subscribe(std::ptr::null(), chans.0, &mut subbed, 0, std::ptr::null()) })?;
        if subbed.is_null() {
            subbed = chans.0;
        }

        let pmgr = Io::services("AppleARMIODevice").into_iter().find(|d| d.text("name").as_deref() == Some("pmgr"));
        let mut r = IoReport { api, sub, _chans: chans, subbed, prev: None, clusters: Vec::new(), gpu_mhz: Vec::new(), gpu_max_mhz: None };
        if let Some(pmgr) = &pmgr {
            let t = |k: &str| {
                let v = table(pmgr, &format!("{k}-sram"));
                if v.is_empty() {
                    table(pmgr, k)
                } else {
                    v
                }
            };
            r.gpu_mhz = t("voltage-states9");
            r.gpu_max_mhz = r.gpu_mhz.iter().copied().max().filter(|m| *m > 0);
            // E cluster: voltage-states1; P clusters: 5 and, on Pro / Max chips, 13
            let mut names: Vec<String> = Vec::new();
            r.each(&r.take(), |s, item| {
                if s.str(s.api.subgroup, item).as_deref() == Some("CPU Complex Performance States") {
                    if let Some(n) = s.str(s.api.name, item).filter(|n| is_cluster(n)) {
                        if !names.contains(&n) {
                            names.push(n);
                        }
                    }
                }
            });
            let mut p = 0;
            for n in names {
                let efficiency = n.starts_with('E');
                let key = if efficiency {
                    "voltage-states1"
                } else {
                    p += 1;
                    if p == 1 {
                        "voltage-states5"
                    } else {
                        "voltage-states13"
                    }
                };
                r.clusters.push(Cluster { channel: n, efficiency, mhz: t(key) });
            }
        }
        r.prev = Some((r.take(), Instant::now()));
        Some(r)
    }

    pub fn gpu_min_mhz(&self) -> Option<u32> {
        self.gpu_mhz.iter().copied().filter(|m| *m > 0).min()
    }

    fn take(&self) -> Cf {
        Cf(unsafe { (self.api.create_samples)(self.sub.0, self.subbed, std::ptr::null()) })
    }

    fn str(&self, f: GetStr, item: CFTypeRef) -> Option<String> {
        string(unsafe { f(item) })
    }

    fn each(&self, sample: &Cf, mut f: impl FnMut(&Self, CFTypeRef)) {
        let arr = get(sample.0, "IOReportChannels");
        if arr.is_null() {
            return;
        }
        for i in 0..unsafe { CFArrayGetCount(arr) } {
            f(self, unsafe { CFArrayGetValueAtIndex(arr, i) });
        }
    }

    /// (state name, residency) of a state channel.
    fn states(&self, item: CFTypeRef) -> Vec<(String, i64)> {
        let n = unsafe { (self.api.state_count)(item) };
        (0..n).map(|i| (string(unsafe { (self.api.state_name)(item, i) }).unwrap_or_default(), unsafe { (self.api.residency)(item, i) })).collect()
    }

    pub fn sample(&mut self) -> Reading {
        let cur = self.take();
        let now = Instant::now();
        let mut out = Reading { cluster_mhz: vec![None; self.clusters.len()], ..Default::default() };
        let Some((prev, at)) = self.prev.take() else {
            self.prev = Some((cur, now));
            return out;
        };
        let secs = now.duration_since(at).as_secs_f64().max(0.05);
        let delta = Cf(unsafe { (self.api.create_delta)(prev.0, cur.0, std::ptr::null()) });
        self.prev = Some((cur, now));
        if delta.0.is_null() {
            return out;
        }
        // M4 and later report each cluster ("EACC_CPU", "PACC1_CPU"), earlier chips "CPU Energy"
        let (mut clusters_j, mut cpu_j, mut gpu_j, mut energy) = (None::<f64>, 0f64, 0f64, false);
        self.each(&delta, |s, item| {
            let group = s.str(s.api.group, item).unwrap_or_default();
            let name = s.str(s.api.name, item).unwrap_or_default();
            match group.as_str() {
                "Energy Model" => {
                    let v = unsafe { (s.api.simple_int)(item, 0) } as f64;
                    let j = match s.str(s.api.unit, item).as_deref().map(str::trim) {
                        Some("mJ") => v / 1e3,
                        Some("uJ") => v / 1e6,
                        Some("nJ") => v / 1e9,
                        _ => return,
                    };
                    energy = true;
                    if is_cluster_energy(&name) {
                        *clusters_j.get_or_insert(0.0) += j;
                    } else if name.ends_with("CPU Energy") {
                        cpu_j += j;
                    } else if name == "GPU Energy" {
                        gpu_j += j;
                    }
                }
                "CPU Stats" => {
                    if let Some(i) = s.clusters.iter().position(|c| c.channel == name) {
                        out.cluster_mhz[i] = average(&s.states(item), &s.clusters[i].mhz);
                    }
                }
                "GPU Stats" if name == "GPUPH" => out.gpu_mhz = average(&s.states(item), &s.gpu_mhz),
                _ => {}
            }
        });
        if energy {
            // macOS 27 stops these counters for unprivileged readers: no change is no reading
            let j = clusters_j.unwrap_or(cpu_j);
            out.cpu_w = (j > 0.0).then(|| (j / secs) as f32);
            out.gpu_w = Some((gpu_j / secs) as f32);
        }
        out
    }
}

/// "ECPU", "PCPU", "PCPU1": a cluster's cores; "ECPM", "PCPM_IDLE" are its uncore.
fn is_cluster(n: &str) -> bool {
    (n.starts_with("ECPU") || n.starts_with("PCPU")) && n[4..].bytes().all(|b| b.is_ascii_digit())
}

/// "EACC_CPU", "PACC0_CPU": the whole of one cluster; not "PACC0_CPU3" or "…_SRAM".
fn is_cluster_energy(n: &str) -> bool {
    n.strip_suffix("_CPU").map_or(false, |p| (p.starts_with("EACC") || p.starts_with("PACC")) && p[4..].bytes().all(|b| b.is_ascii_digit()))
}

/// Residency-weighted clock over the active states. The states are idle / off ones ("IDLE",
/// "DOWN", "OFF") followed by one per table entry; an entry of 0 MHz is a powered-off state too.
/// None when nothing ran in the interval.
fn average(states: &[(String, i64)], mhz: &[u32]) -> Option<u32> {
    let active: Vec<i64> = states.iter().filter(|(n, _)| !matches!(n.as_str(), "IDLE" | "DOWN" | "OFF")).map(|s| s.1).collect();
    // tables may carry a leading "off" entry the states do not have
    let offset = mhz.len().saturating_sub(active.len());
    let (mut sum, mut time) = (0f64, 0f64);
    for (i, r) in active.iter().enumerate() {
        let f = *mhz.get(i + offset)? as f64;
        if f > 0.0 && *r > 0 {
            sum += f * *r as f64;
            time += *r as f64;
        }
    }
    (time > 0.0).then(|| (sum / time).round() as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_names() {
        assert!(is_cluster("ECPU") && is_cluster("PCPU1") && !is_cluster("PCPM") && !is_cluster("PCPM1_IDLE"));
        assert!(is_cluster_energy("EACC_CPU") && is_cluster_energy("PACC1_CPU"));
        assert!(!is_cluster_energy("PACC0_CPU3") && !is_cluster_energy("EACC_CPU0_SRAM") && !is_cluster_energy("CPU Energy"));
    }

    #[test]
    fn residency_average() {
        let s = |v: &[(&str, i64)]| v.iter().map(|(n, r)| (n.to_string(), *r)).collect::<Vec<_>>();
        // GPU: OFF + P1..P3 against a table with a leading 0
        assert_eq!(average(&s(&[("OFF", 900), ("P1", 50), ("P2", 0), ("P3", 50)]), &[0, 338, 618, 796]), Some(567));
        // CPU: IDLE, DOWN + V0..V2
        assert_eq!(average(&s(&[("IDLE", 10), ("DOWN", 0), ("V0P5", 30), ("V1P4", 10), ("V2P3", 0)]), &[1020, 1404, 1788]), Some(1116));
        assert_eq!(average(&s(&[("IDLE", 10), ("V0P5", 0)]), &[1020]), None);
    }

    /// `cargo test -- --ignored --nocapture channels` on a Mac: what IOReport offers.
    #[test]
    #[ignore]
    fn channels() {
        let mut r = IoReport::open().expect("IOReport");
        println!("clusters: {:?}\ngpu table: {:?}", r.clusters, r.gpu_mhz);
        std::thread::sleep(std::time::Duration::from_millis(500));
        let cur = r.take();
        r.each(&cur, |s, item| {
            let g = s.str(s.api.group, item).unwrap_or_default();
            let sg = s.str(s.api.subgroup, item).unwrap_or_default();
            let n = s.str(s.api.name, item).unwrap_or_default();
            let u = s.str(s.api.unit, item).unwrap_or_default();
            let st: Vec<String> = s.states(item).into_iter().map(|(n, _)| n).collect();
            println!("{g} / {sg} / {n} [{u}] {st:?}");
        });
        std::thread::sleep(std::time::Duration::from_millis(1000));
        println!("{:?}", r.sample());
    }
}
