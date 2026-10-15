//! NVIDIA GPUs through NVML (`libnvidia-ml.so.1`, installed with the proprietary driver), loaded
//! at run time: load, memory, temperature, clocks, power and fan, and the same per process. A
//! missing library, entry point or a failing call only leaves values empty. Jetson ships the
//! library but answers nothing.

use std::ffi::{c_char, c_void, CStr};
use winstats_proto::GpuSample;

type Device = *mut c_void;

#[repr(C)]
struct Utilization {
    gpu: u32,
    memory: u32,
}

#[repr(C)]
struct Memory {
    total: u64,
    free: u64,
    used: u64,
}

/// nvmlProcessUtilizationSample_t
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct ProcUtil {
    pid: u32,
    timestamp: u64,
    sm: u32,
    mem: u32,
    enc: u32,
    dec: u32,
}

/// nvmlProcessInfo_v2_t
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct ProcInfo {
    pid: u32,
    used: u64,
    gpu_instance: u32,
    compute_instance: u32,
}

/// nvmlPciInfo_t
#[repr(C)]
struct PciInfo {
    bus_id_legacy: [u8; 16],
    domain: u32,
    bus: u32,
    device: u32,
    pci_device_id: u32,
    pci_sub_system_id: u32,
    bus_id: [u8; 32],
}

type Fn0 = unsafe extern "C" fn() -> i32;
type FnCount = unsafe extern "C" fn(*mut u32) -> i32;
type FnHandle = unsafe extern "C" fn(u32, *mut Device) -> i32;
type FnName = unsafe extern "C" fn(Device, *mut c_char, u32) -> i32;
type FnUtil = unsafe extern "C" fn(Device, *mut Utilization) -> i32;
type FnMem = unsafe extern "C" fn(Device, *mut Memory) -> i32;
type FnPci = unsafe extern "C" fn(Device, *mut PciInfo) -> i32;
type FnU32 = unsafe extern "C" fn(Device, *mut u32) -> i32;
type FnKindU32 = unsafe extern "C" fn(Device, u32, *mut u32) -> i32;
type FnProcUtil = unsafe extern "C" fn(Device, *mut ProcUtil, *mut u32, u64) -> i32;
type FnProcs = unsafe extern "C" fn(Device, *mut u32, *mut ProcInfo) -> i32;

const NVML_ERROR_INSUFFICIENT_SIZE: i32 = 7;
/// "N/A" in a memory field (Windows WDDM, MIG)
const NVML_VALUE_NOT_AVAILABLE: u64 = u64::MAX;

const NVML_TEMPERATURE_GPU: u32 = 0;
const NVML_CLOCK_GRAPHICS: u32 = 0;
const NVML_CLOCK_MEM: u32 = 2;

pub struct Nvml {
    util: FnUtil,
    mem: Option<FnMem>,
    power: Option<FnU32>,
    limit: Option<FnU32>,
    fan: Option<FnU32>,
    temp: Option<FnKindU32>,
    clock: Option<FnKindU32>,
    max_clock: Option<FnKindU32>,
    proc_util: Option<FnProcUtil>,
    compute_procs: Option<FnProcs>,
    graphics_procs: Option<FnProcs>,
}

/// One GPU: its handle, name, PCI address ("0000:01:00.0") and memory size.
pub struct Dev {
    pub handle: Device,
    pub name: String,
    pub pci: String,
    pub vram_total: u64,
    /// timestamp of the newest per-process sample seen, µs
    last_seen: std::cell::Cell<u64>,
}

// the handles are plain ids into the library, used from the sampler thread only
unsafe impl Send for Nvml {}
unsafe impl Sync for Nvml {}
unsafe impl Send for Dev {}

unsafe fn sym<T: Copy>(lib: *mut c_void, name: &[u8]) -> Option<T> {
    let f = libc::dlsym(lib, name.as_ptr() as *const c_char);
    if f.is_null() {
        None
    } else {
        Some(std::mem::transmute_copy(&f))
    }
}

impl Nvml {
    /// The library and every GPU that answers a load query.
    pub fn open() -> Option<(Nvml, Vec<Dev>)> {
        unsafe {
            let lib = libc::dlopen(b"libnvidia-ml.so.1\0".as_ptr() as *const c_char, libc::RTLD_NOW);
            if lib.is_null() {
                return None;
            }
            let init: Fn0 = sym(lib, b"nvmlInit_v2\0")?;
            if init() != 0 {
                return None;
            }
            let count: FnCount = sym(lib, b"nvmlDeviceGetCount_v2\0")?;
            let handle: FnHandle = sym(lib, b"nvmlDeviceGetHandleByIndex_v2\0")?;
            let name: Option<FnName> = sym(lib, b"nvmlDeviceGetName\0");
            let pci: Option<FnPci> = sym(lib, b"nvmlDeviceGetPciInfo_v3\0").or_else(|| sym(lib, b"nvmlDeviceGetPciInfo_v2\0"));
            let nv = Nvml {
                util: sym(lib, b"nvmlDeviceGetUtilizationRates\0")?,
                mem: sym(lib, b"nvmlDeviceGetMemoryInfo\0"),
                power: sym(lib, b"nvmlDeviceGetPowerUsage\0"),
                limit: sym(lib, b"nvmlDeviceGetEnforcedPowerLimit\0"),
                fan: sym(lib, b"nvmlDeviceGetFanSpeed\0"),
                temp: sym(lib, b"nvmlDeviceGetTemperature\0"),
                clock: sym(lib, b"nvmlDeviceGetClockInfo\0"),
                max_clock: sym(lib, b"nvmlDeviceGetMaxClockInfo\0"),
                proc_util: sym(lib, b"nvmlDeviceGetProcessUtilization\0"),
                compute_procs: sym(lib, b"nvmlDeviceGetComputeRunningProcesses_v2\0"),
                graphics_procs: sym(lib, b"nvmlDeviceGetGraphicsRunningProcesses_v2\0"),
            };
            let mut n = 0u32;
            if count(&mut n) != 0 {
                return None;
            }
            let mut devs = Vec::new();
            for i in 0..n.min(16) {
                let mut d: Device = std::ptr::null_mut();
                if handle(i, &mut d) != 0 {
                    continue;
                }
                let mut u = Utilization { gpu: 0, memory: 0 };
                if (nv.util)(d, &mut u) != 0 {
                    continue;
                }
                let mut buf = [0 as c_char; 96];
                let model = match name {
                    Some(f) if f(d, buf.as_mut_ptr(), buf.len() as u32) == 0 => CStr::from_ptr(buf.as_ptr()).to_string_lossy().to_string(),
                    _ => "NVIDIA GPU".to_string(),
                };
                let mut info: PciInfo = std::mem::zeroed();
                let addr = match pci {
                    Some(f) if f(d, &mut info) == 0 => {
                        // "00000000:01:00.0" -> "0000:01:00.0", the sysfs spelling
                        let s = CStr::from_bytes_until_nul(&info.bus_id).map(|c| c.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
                        s.strip_prefix("0000").filter(|r| r.len() == 12).map(|r| r.to_string()).unwrap_or(s)
                    }
                    _ => String::new(),
                };
                let mut m = Memory { total: 0, free: 0, used: 0 };
                let vram_total = match nv.mem {
                    Some(f) if f(d, &mut m) == 0 => m.total,
                    _ => 0,
                };
                devs.push(Dev { handle: d, name: model, pci: addr, vram_total, last_seen: std::cell::Cell::new(0) });
            }
            Some((nv, devs))
        }
    }

    pub fn sample(&self, d: &Dev) -> GpuSample {
        let dev = d.handle;
        let get = |f: Option<FnU32>| -> Option<u32> {
            let f = f?;
            let mut v = 0u32;
            (unsafe { f(dev, &mut v) } == 0).then_some(v)
        };
        let kind = |f: Option<FnKindU32>, k: u32| -> Option<u32> {
            let f = f?;
            let mut v = 0u32;
            (unsafe { f(dev, k, &mut v) } == 0).then_some(v)
        };
        let mut u = Utilization { gpu: 0, memory: 0 };
        let load = if unsafe { (self.util)(dev, &mut u) } == 0 { u.gpu.min(100) as f32 / 100.0 } else { 0.0 };
        let mut m = Memory { total: 0, free: 0, used: 0 };
        let mem_ok = self.mem.map_or(false, |f| unsafe { f(dev, &mut m) } == 0);
        let watts = |mw: u32| Some(mw).filter(|mw| *mw > 0 && *mw < 2_000_000).map(|mw| mw as f32 / 1000.0);
        let valid = |m: &u32| *m > 0 && *m < 50_000;
        GpuSample {
            load,
            vram_used: if mem_ok { m.used } else { 0 },
            vram_total: if mem_ok { m.total } else { d.vram_total },
            temp: kind(self.temp, NVML_TEMPERATURE_GPU).filter(|c| *c > 0 && *c < 150).map(|c| c as f32),
            core_mhz: kind(self.clock, NVML_CLOCK_GRAPHICS).filter(valid),
            core_max_mhz: kind(self.max_clock, NVML_CLOCK_GRAPHICS).filter(valid),
            mem_mhz: kind(self.clock, NVML_CLOCK_MEM).filter(valid),
            mem_max_mhz: kind(self.max_clock, NVML_CLOCK_MEM).filter(valid),
            power_w: get(self.power).and_then(watts),
            power_limit_w: get(self.limit).and_then(watts),
            fan_pct: get(self.fan).filter(|f| *f <= 100),
            ..Default::default()
        }
    }

    /// (pid, SM load 0..1, bytes) of the processes on the GPU, since the previous call.
    pub fn procs(&self, d: &Dev) -> Vec<(u32, f32, u64)> {
        let mut out: Vec<(u32, f32, u64)> = Vec::new();
        if let Some(f) = self.proc_util {
            let mut n = 0u32;
            let first = unsafe { f(d.handle, std::ptr::null_mut(), &mut n, d.last_seen.get()) };
            if first == NVML_ERROR_INSUFFICIENT_SIZE && n > 0 {
                // room to spare in case processes start between the two calls
                let mut buf = vec![ProcUtil::default(); n as usize + 16];
                let mut got = buf.len() as u32;
                if unsafe { f(d.handle, buf.as_mut_ptr(), &mut got, d.last_seen.get()) } == 0 {
                    for s in &buf[..(got as usize).min(buf.len())] {
                        d.last_seen.set(d.last_seen.get().max(s.timestamp));
                        let load = s.sm.min(100) as f32 / 100.0;
                        match out.iter_mut().find(|p| p.0 == s.pid) {
                            Some(p) => p.1 = p.1.max(load),
                            None => out.push((s.pid, load, 0)),
                        }
                    }
                }
            }
        }
        for f in [self.compute_procs, self.graphics_procs].into_iter().flatten() {
            let mut buf = vec![ProcInfo::default(); 128];
            let mut n = buf.len() as u32;
            if unsafe { f(d.handle, &mut n, buf.as_mut_ptr()) } != 0 {
                continue;
            }
            for p in &buf[..(n as usize).min(buf.len())] {
                let used = if p.used == NVML_VALUE_NOT_AVAILABLE { 0 } else { p.used };
                match out.iter_mut().find(|x| x.0 == p.pid) {
                    // a process can be on both lists with the same memory
                    Some(x) => x.2 = x.2.max(used),
                    None => out.push((p.pid, 0.0, used)),
                }
            }
        }
        out
    }
}
