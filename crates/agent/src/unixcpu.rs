//! Per-core CPU times on Linux (`/proc/stat`) and macOS (`host_processor_info`), so the
//! kernel / user split reaches winstats; sysinfo only reports the total.

/// Ticks of one core: user (with nice), system (with irq / softirq), idle (with iowait), other
/// busy time (steal).
#[derive(Clone, Copy, Default, Debug, PartialEq)]
pub struct Ticks {
    pub user: u64,
    pub sys: u64,
    pub idle: u64,
    pub other: u64,
}

pub struct UnixCpu {
    prev: Vec<Ticks>,
    pub cores: Vec<f32>,
    pub total: f32,
    pub sys: f32,
    pub user: f32,
}

impl UnixCpu {
    pub fn new(n: usize) -> Self {
        Self { prev: read(), cores: vec![0.0; n], total: 0.0, sys: 0.0, user: 0.0 }
    }

    /// False when the counters cannot be read; the caller falls back to sysinfo.
    pub fn works(&self) -> bool {
        !self.prev.is_empty()
    }

    pub fn update(&mut self) {
        let cur = read();
        self.apply(cur);
    }

    fn apply(&mut self, cur: Vec<Ticks>) {
        if cur.len() != self.prev.len() || cur.is_empty() {
            self.prev = cur;
            return;
        }
        let (mut busy_all, mut sys_all, mut user_all, mut span_all) = (0f64, 0f64, 0f64, 0f64);
        self.cores.resize(cur.len(), 0.0);
        for (i, (a, b)) in self.prev.iter().zip(cur.iter()).enumerate() {
            let user = b.user.saturating_sub(a.user) as f64;
            let sys = b.sys.saturating_sub(a.sys) as f64;
            let idle = b.idle.saturating_sub(a.idle) as f64;
            let other = b.other.saturating_sub(a.other) as f64;
            let span = user + sys + idle + other;
            let busy = user + sys + other;
            self.cores[i] = if span > 0.0 { (busy / span) as f32 } else { 0.0 };
            busy_all += busy;
            sys_all += sys;
            user_all += user;
            span_all += span;
        }
        if span_all > 0.0 {
            self.total = (busy_all / span_all) as f32;
            self.sys = (sys_all / span_all) as f32;
            self.user = (user_all / span_all) as f32;
        }
        self.prev = cur;
    }
}

/// `cpuN` lines of /proc/stat: user nice system idle iowait irq softirq steal (guest time is
/// already part of user).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn parse_stat(text: &str) -> Vec<Ticks> {
    let mut out = Vec::new();
    for line in text.lines() {
        let mut it = line.split_ascii_whitespace();
        let name = it.next().unwrap_or("");
        if !name.starts_with("cpu") || name.len() == 3 {
            continue;
        }
        let v: Vec<u64> = it.map(|x| x.parse().unwrap_or(0)).collect();
        let f = |i: usize| v.get(i).copied().unwrap_or(0);
        out.push(Ticks { user: f(0) + f(1), sys: f(2) + f(5) + f(6), idle: f(3) + f(4), other: f(7) });
    }
    out
}

#[cfg(target_os = "linux")]
fn read() -> Vec<Ticks> {
    std::fs::read_to_string("/proc/stat").map(|s| parse_stat(&s)).unwrap_or_default()
}

#[cfg(target_os = "macos")]
fn read() -> Vec<Ticks> {
    let mut count: libc::natural_t = 0;
    let mut info: libc::processor_info_array_t = std::ptr::null_mut();
    let mut len: libc::mach_msg_type_number_t = 0;
    // SAFETY: the kernel hands back `len` integers, CPU_STATE_MAX per processor, which are
    // copied out and then returned to the VM.
    unsafe {
        #[allow(deprecated)]
        let host = libc::mach_host_self();
        if libc::host_processor_info(host, libc::PROCESSOR_CPU_LOAD_INFO, &mut count, &mut info, &mut len) != 0 || info.is_null() {
            return Vec::new();
        }
        let n = libc::CPU_STATE_MAX as usize;
        let ticks = std::slice::from_raw_parts(info, len as usize);
        let out = ticks
            .chunks(n)
            .take(count as usize)
            .map(|c| {
                let t = |s: libc::c_int| c[s as usize] as u32 as u64;
                Ticks { user: t(libc::CPU_STATE_USER) + t(libc::CPU_STATE_NICE), sys: t(libc::CPU_STATE_SYSTEM), idle: t(libc::CPU_STATE_IDLE), other: 0 }
            })
            .collect();
        #[allow(deprecated)]
        let task = libc::mach_task_self();
        libc::vm_deallocate(task, info as libc::vm_address_t, len as libc::vm_size_t * std::mem::size_of::<libc::integer_t>() as libc::vm_size_t);
        out
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn read() -> Vec<Ticks> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_from_proc_stat() {
        let a = parse_stat("cpu  10 0 10 80 0 0 0 0 0 0\ncpu0 5 0 5 40 0 0 0 0 0 0\ncpu1 5 0 5 40 0 0 0 0 0 0\nintr 1 2 3\n");
        assert_eq!(a.len(), 2);
        let b = parse_stat("cpu  0 0 0 0\ncpu0 35 5 15 40 5 3 2 0\ncpu1 5 0 5 140 0 0 0 0\n");
        let mut c = UnixCpu { prev: a, cores: vec![0.0; 2], total: 0.0, sys: 0.0, user: 0.0 };
        c.apply(b);
        // core 0: user 35, sys 15, idle 5 of 55; core 1 idle
        assert!((c.cores[0] - 50.0 / 55.0).abs() < 1e-6);
        assert_eq!(c.cores[1], 0.0);
        assert!((c.user - 35.0 / 155.0).abs() < 1e-6);
        assert!((c.sys - 15.0 / 155.0).abs() < 1e-6);
        assert!((c.total - 50.0 / 155.0).abs() < 1e-6);
    }
}
