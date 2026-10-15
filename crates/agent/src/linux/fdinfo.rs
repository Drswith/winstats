//! GPU load from DRM fdinfo (`/proc/<pid>/fdinfo/<fd>` of every open `/dev/dri` node): each
//! client reports the busy time of each engine (`drm-engine-render: 123 ns`) or, on xe, busy and
//! total GPU cycles. The load of the device is the busiest engine over the interval, summed over
//! its clients, as nvtop and Windows Task Manager count it. Works for amdgpu, i915, xe, msm,
//! panfrost, panthor, v3d and etnaviv; other users' processes are readable only as root. The same
//! numbers per process, with the memory each one holds, make the GPU process list.

use super::{dirs, read, Root};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Which clients belong to the device: PCI GPUs by address ("0000:03:00.0"), SoC GPUs (no
/// `drm-pdev`) by DRM driver name ("panfrost").
#[derive(Clone, Debug, PartialEq)]
pub enum Key {
    Pdev(String),
    Driver(String),
}

/// One client's counters: busy ns per engine, (busy, total) cycles per engine class, the
/// number of engines of a class (`drm-engine-capacity-*`) and the memory it holds.
#[derive(Default, Debug, PartialEq)]
struct Client {
    driver: String,
    pdev: String,
    id: u64,
    /// the process it was read from (the last one, when several share it)
    pid: u32,
    /// bytes resident (`drm-resident-*`), or allocated where the driver has only `drm-memory-*`
    resident: u64,
    memory: u64,
    ns: HashMap<String, u64>,
    cycles: HashMap<String, (u64, u64)>,
    capacity: HashMap<String, u64>,
}

fn parse(text: &str) -> Option<Client> {
    let mut c = Client::default();
    let mut id = None;
    for line in text.lines() {
        let Some((k, v)) = line.split_once(':') else { continue };
        let v = v.trim();
        let num = || v.split_whitespace().next().and_then(|n| n.parse::<u64>().ok());
        if let (Some(_), Some(b)) = (k.strip_prefix("drm-resident-"), bytes(v)) {
            c.resident += b;
        } else if let (Some(_), Some(b)) = (k.strip_prefix("drm-memory-"), bytes(v)) {
            c.memory += b;
        } else if k == "drm-driver" {
            c.driver = v.to_string();
        } else if k == "drm-pdev" {
            c.pdev = v.to_string();
        } else if k == "drm-client-id" {
            id = num();
        } else if let (Some(e), Some(n)) = (k.strip_prefix("drm-engine-capacity-"), num()) {
            c.capacity.insert(e.to_string(), n);
        } else if let (Some(e), Some(n)) = (k.strip_prefix("drm-engine-"), num()) {
            c.ns.insert(e.to_string(), n);
        } else if let (Some(e), Some(n)) = (k.strip_prefix("drm-total-cycles-"), num()) {
            c.cycles.entry(e.to_string()).or_default().1 = n;
        } else if let (Some(e), Some(n)) = (k.strip_prefix("drm-cycles-"), num()) {
            // msm and panfrost report cycles without a total: no use without one
            c.cycles.entry(e.to_string()).or_default().0 = n;
        }
    }
    c.id = id?;
    (!c.driver.is_empty()).then_some(c)
}

/// "123 KiB" / "4 MiB" / "512" (bytes).
fn bytes(v: &str) -> Option<u64> {
    let mut it = v.split_whitespace();
    let n: u64 = it.next()?.parse().ok()?;
    let mult = match it.next() {
        None => 1,
        Some("KiB") => 1 << 10,
        Some("MiB") => 1 << 20,
        Some("GiB") => 1 << 30,
        Some(_) => return None,
    };
    Some(n * mult)
}

impl Key {
    fn matches(&self, c: &Client) -> bool {
        match self {
            Key::Pdev(p) => c.pdev == *p,
            Key::Driver(d) => c.pdev.is_empty() && c.driver == *d,
        }
    }
}

pub struct DrmLoad {
    root: Root,
    key: Key,
    /// fdinfo files of open DRM nodes, refreshed every `RESCAN`
    files: Vec<PathBuf>,
    scanned: Option<Instant>,
    prev: HashMap<u64, Client>,
    prev_at: Option<Instant>,
    /// (pid, busiest engine 0..1, bytes) of the last interval
    pub procs: Vec<(u32, f32, u64)>,
}

const RESCAN: Duration = Duration::from_secs(5);

impl DrmLoad {
    pub fn new(root: &Root, key: Key) -> DrmLoad {
        DrmLoad { root: root.clone(), key, files: Vec::new(), scanned: None, prev: HashMap::new(), prev_at: None, procs: Vec::new() }
    }

    /// Every fd of every process that points into /dev/dri.
    fn scan(&mut self) {
        self.files.clear();
        for proc in dirs(&self.root.path("/proc")) {
            if !proc.file_name().map_or(false, |n| n.to_string_lossy().bytes().all(|b| b.is_ascii_digit())) {
                continue;
            }
            for fd in dirs(&proc.join("fd")) {
                let to_dri = std::fs::read_link(&fd).map_or(false, |t| t.starts_with("/dev/dri/"));
                if to_dri {
                    if let Some(n) = fd.file_name() {
                        self.files.push(proc.join("fdinfo").join(n));
                    }
                }
            }
        }
        self.scanned = Some(Instant::now());
    }

    /// Busiest engine since the previous call, 0..1; 0 on the first call.
    pub fn sample(&mut self) -> f32 {
        if self.scanned.map_or(true, |t| t.elapsed() >= RESCAN) {
            self.scan();
        }
        let mut now: HashMap<u64, Client> = HashMap::new();
        for f in &self.files {
            if let Some(mut c) = read(f).and_then(|t| parse(&t)) {
                // .../proc/<pid>/fdinfo/<fd>
                c.pid = f.parent().and_then(|p| p.parent()).and_then(|p| p.file_name()).and_then(|n| n.to_string_lossy().parse().ok()).unwrap_or(0);
                if self.key.matches(&c) {
                    // one client is seen through every fd and process that shares it
                    now.insert(c.id, c);
                }
            }
        }
        let at = Instant::now();
        let load = match self.prev_at {
            Some(p) => {
                let dt = at.duration_since(p);
                self.procs = per_process(&self.prev, &now, dt);
                busiest(&self.prev, &now, dt)
            }
            None => 0.0,
        };
        self.prev = now;
        self.prev_at = Some(at);
        load
    }
}

fn busiest(prev: &HashMap<u64, Client>, now: &HashMap<u64, Client>, dt: Duration) -> f32 {
    max_engine(prev, now.iter(), dt)
}

/// Each process's busiest engine and memory; processes with neither are left out.
fn per_process(prev: &HashMap<u64, Client>, now: &HashMap<u64, Client>, dt: Duration) -> Vec<(u32, f32, u64)> {
    let mut pids: Vec<u32> = now.values().map(|c| c.pid).filter(|p| *p > 0).collect();
    pids.sort_unstable();
    pids.dedup();
    pids.into_iter()
        .map(|pid| {
            let mine = now.iter().filter(|(_, c)| c.pid == pid);
            let mem = now.values().filter(|c| c.pid == pid).map(|c| if c.resident > 0 { c.resident } else { c.memory }).sum();
            (pid, max_engine(prev, mine, dt), mem)
        })
        .filter(|(_, l, m)| *l > 0.0 || *m > 0)
        .collect()
}

/// Busiest engine of a set of clients over `dt`, 0..1.
fn max_engine<'a>(prev: &HashMap<u64, Client>, now: impl Iterator<Item = (&'a u64, &'a Client)>, dt: Duration) -> f32 {
    let dt_ns = dt.as_nanos() as f64;
    if dt_ns <= 0.0 {
        return 0.0;
    }
    let mut busy: HashMap<&str, f64> = HashMap::new();
    for (id, c) in now {
        let Some(p) = prev.get(id) else { continue };
        for (e, ns) in &c.ns {
            if let Some(old) = p.ns.get(e) {
                let cap = c.capacity.get(e).cloned().unwrap_or(1).max(1) as f64;
                *busy.entry(e.as_str()).or_default() += ns.saturating_sub(*old) as f64 / (dt_ns * cap);
            }
        }
        for (e, (cyc, total)) in &c.cycles {
            if let Some((ocyc, ototal)) = p.cycles.get(e) {
                let dtotal = total.saturating_sub(*ototal);
                if dtotal > 0 {
                    *busy.entry(e.as_str()).or_default() += cyc.saturating_sub(*ocyc) as f64 / dtotal as f64;
                }
            }
        }
    }
    busy.values().cloned().fold(0.0, f64::max).clamp(0.0, 1.0) as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    const I915: &str = "pos:\t0\nflags:\t02100002\ndrm-driver:\ti915\ndrm-pdev:\t0000:00:02.0\ndrm-client-id:\t7\ndrm-engine-render:\t1000000 ns\ndrm-engine-video:\t0 ns\ndrm-engine-capacity-video:\t2\n";
    const XE: &str = "drm-driver:\txe\ndrm-pdev:\t0000:03:00.0\ndrm-client-id:\t3\ndrm-cycles-rcs:\t500\ndrm-total-cycles-rcs:\t10000\n";

    #[test]
    fn parses_both_styles() {
        let c = parse(I915).unwrap();
        assert_eq!((c.driver.as_str(), c.pdev.as_str(), c.id), ("i915", "0000:00:02.0", 7));
        assert_eq!(c.ns.get("render"), Some(&1_000_000));
        assert_eq!(c.capacity.get("video"), Some(&2));
        let x = parse(XE).unwrap();
        assert_eq!(x.cycles.get("rcs"), Some(&(500, 10_000)));
        assert!(parse("pos: 0\n").is_none());
    }

    #[test]
    fn load_is_the_busiest_engine() {
        let one = |id: u64, render: u64| {
            let mut c = parse(I915).unwrap();
            c.id = id;
            c.ns.insert("render".into(), render);
            (id, c)
        };
        let prev: HashMap<u64, Client> = [one(1, 0), one(2, 0)].into_iter().collect();
        // two clients, 0.2 s and 0.3 s of render in one second; a new client counts from the next call
        let now: HashMap<u64, Client> = [one(1, 200_000_000), one(2, 300_000_000), one(3, 900_000_000)].into_iter().collect();
        let l = busiest(&prev, &now, Duration::from_secs(1));
        assert!((l - 0.5).abs() < 1e-6, "{l}");
        let mut xp = parse(XE).unwrap();
        let mut xn = parse(XE).unwrap();
        xp.cycles.insert("rcs".into(), (0, 0));
        xn.cycles.insert("rcs".into(), (2500, 10_000));
        let l = busiest(&[(3, xp)].into_iter().collect(), &[(3, xn)].into_iter().collect(), Duration::from_secs(1));
        assert!((l - 0.25).abs() < 1e-6, "{l}");
    }

    #[test]
    fn per_process_load_and_memory() {
        let one = |id: u64, pid: u32, render: u64, mem: &str| {
            let mut c = parse(&format!("{I915}drm-resident-system0:\t{mem}\n")).unwrap();
            c.id = id;
            c.pid = pid;
            c.ns.insert("render".into(), render);
            (id, c)
        };
        let prev: HashMap<u64, Client> = [one(1, 100, 0, "0 KiB"), one(2, 100, 0, "0 KiB"), one(3, 200, 0, "0 KiB")].into_iter().collect();
        let now: HashMap<u64, Client> = [one(1, 100, 100_000_000, "1 MiB"), one(2, 100, 300_000_000, "2048 KiB"), one(3, 200, 0, "0 KiB")].into_iter().collect();
        let p = per_process(&prev, &now, Duration::from_secs(1));
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].0, 100);
        assert!((p[0].1 - 0.4).abs() < 1e-6);
        assert_eq!(p[0].2, 3 << 20);
        assert_eq!(bytes("512"), Some(512));
        assert_eq!(bytes("3 GiB"), Some(3 << 30));
    }

    #[test]
    fn key_matching() {
        let c = parse(I915).unwrap();
        assert!(Key::Pdev("0000:00:02.0".into()).matches(&c));
        assert!(!Key::Driver("i915".into()).matches(&c));
        let p = parse("drm-driver:\tpanfrost\ndrm-client-id:\t1\ndrm-engine-fragment:\t5 ns\n").unwrap();
        assert!(Key::Driver("panfrost".into()).matches(&p));
    }
}
