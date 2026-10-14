//! AMD extras from ADL (atiadlxx.dll, installed with every Radeon driver back to Windows 7):
//! temperature, hot spot, board power, fan, clocks and the PCIe link through the Overdrive 5 / 6 /
//! N calls, as LibreHardwareMonitor reads them. On Windows 7 (no D3DKMT perf data) this is the
//! only GPU sensor source. Every call is optional; a failing one only leaves its value empty.

use super::gpuperf::Perf;
use windows::Win32::Foundation::HMODULE;
use windows::core::PCSTR;

type Context = *mut core::ffi::c_void;

/// AdapterInfo (Windows layout)
#[repr(C)]
struct AdapterInfo {
    size: i32,
    adapter_index: i32,
    udid: [u8; 256],
    bus_number: i32,
    device_number: i32,
    function_number: i32,
    vendor_id: i32,
    adapter_name: [u8; 256],
    display_name: [u8; 256],
    present: i32,
    exist: i32,
    driver_path: [u8; 256],
    driver_path_ext: [u8; 256],
    pnp_string: [u8; 256],
    os_display_index: i32,
}

/// ADLPMActivity
#[repr(C)]
#[derive(Default)]
struct Activity {
    size: i32,
    engine_clock: i32,
    memory_clock: i32,
    vddc: i32,
    activity_percent: i32,
    current_performance_level: i32,
    current_bus_speed: i32,
    current_bus_lanes: i32,
    maximum_bus_lanes: i32,
    reserved: i32,
}

/// ADLTemperature
#[repr(C)]
#[derive(Default)]
struct Temperature {
    size: i32,
    temperature: i32,
}

/// ADLFanSpeedValue
#[repr(C)]
#[derive(Default)]
struct FanSpeed {
    size: i32,
    speed_type: i32,
    fan_speed: i32,
    flags: i32,
}

/// ADLODNPerformanceStatus (Overdrive N: Polaris and newer)
#[repr(C)]
#[derive(Default)]
struct OdnStatus {
    core_clock: i32,
    memory_clock: i32,
    dcef_clock: i32,
    gfx_clock: i32,
    uvd_clock: i32,
    vce_clock: i32,
    activity_percent: i32,
    core_level: i32,
    memory_level: i32,
    dcef_level: i32,
    gfx_level: i32,
    uvd_level: i32,
    vce_level: i32,
    bus_speed: i32,
    bus_lanes: i32,
    max_bus_lanes: i32,
    vddc: i32,
    vddci: i32,
}

/// ADLODNFanControl
#[repr(C)]
#[derive(Default)]
struct OdnFan {
    mode: i32,
    fan_control_mode: i32,
    speed_mode: i32,
    speed: i32,
    target_speed: i32,
    target_temperature: i32,
    min_performance_clock: i32,
    min_fan_limit: i32,
}

const FAN_PERCENT: i32 = 1;
const FAN_RPM: i32 = 2;
/// ADLODNTemperatureType
const ODN_EDGE: i32 = 1;
const ODN_HOTSPOT: i32 = 7;

type MallocCb = unsafe extern "system" fn(i32) -> *mut core::ffi::c_void;
type FnCreate = unsafe extern "C" fn(MallocCb, i32, *mut Context) -> i32;
type FnCount = unsafe extern "C" fn(Context, *mut i32) -> i32;
type FnInfo = unsafe extern "C" fn(Context, *mut AdapterInfo, i32) -> i32;
type FnCaps = unsafe extern "C" fn(Context, i32, *mut i32, *mut i32, *mut i32) -> i32;
type FnTemp5 = unsafe extern "C" fn(Context, i32, i32, *mut Temperature) -> i32;
type FnActivity = unsafe extern "C" fn(Context, i32, *mut Activity) -> i32;
type FnFan5 = unsafe extern "C" fn(Context, i32, i32, *mut FanSpeed) -> i32;
type FnPower6 = unsafe extern "C" fn(Context, i32, i32, *mut i32) -> i32;
type FnTempN = unsafe extern "C" fn(Context, i32, i32, *mut i32) -> i32;
type FnStatusN = unsafe extern "C" fn(Context, i32, *mut OdnStatus) -> i32;
type FnFanN = unsafe extern "C" fn(Context, i32, *mut OdnFan) -> i32;

/// ADL hands out memory through this callback; the few small blocks are never freed.
unsafe extern "system" fn adl_malloc(size: i32) -> *mut core::ffi::c_void {
    use windows::Win32::System::Memory::{GetProcessHeap, HEAP_ZERO_MEMORY, HeapAlloc};
    match GetProcessHeap() {
        Ok(h) => HeapAlloc(h, HEAP_ZERO_MEMORY, size.max(1) as usize),
        Err(_) => std::ptr::null_mut(),
    }
}

pub struct Amd {
    ctx: Context,
    temp5: Option<FnTemp5>,
    activity: Option<FnActivity>,
    fan5: Option<FnFan5>,
    power6: Option<FnPower6>,
    temp_n: Option<FnTempN>,
    status_n: Option<FnStatusN>,
    fan_n: Option<FnFanN>,
    /// (PCI bus, ADL adapter index, Overdrive version)
    gpus: Vec<(i32, i32, i32)>,
}

unsafe fn sym<T: Copy>(lib: HMODULE, name: &[u8]) -> Option<T> {
    let f = windows::Win32::System::LibraryLoader::GetProcAddress(lib, PCSTR(name.as_ptr()))?;
    Some(std::mem::transmute_copy(&f))
}

impl Amd {
    pub fn new() -> Option<Self> {
        #[cfg(target_pointer_width = "64")]
        let lib = crate::util::load_system_dll(windows::core::w!("atiadlxx.dll"))?;
        #[cfg(not(target_pointer_width = "64"))]
        let lib = crate::util::load_system_dll(windows::core::w!("atiadlxy.dll")).or_else(|| crate::util::load_system_dll(windows::core::w!("atiadlxx.dll")))?;
        unsafe {
            let create: FnCreate = sym(lib, b"ADL2_Main_Control_Create\0")?;
            let count: FnCount = sym(lib, b"ADL2_Adapter_NumberOfAdapters_Get\0")?;
            let info: FnInfo = sym(lib, b"ADL2_Adapter_AdapterInfo_Get\0")?;
            let caps: Option<FnCaps> = sym(lib, b"ADL2_Overdrive_Caps\0");
            let mut ctx: Context = std::ptr::null_mut();
            if create(adl_malloc, 1, &mut ctx) != 0 || ctx.is_null() {
                return None;
            }
            let mut n = 0i32;
            if count(ctx, &mut n) != 0 || n <= 0 || n > 256 {
                return None;
            }
            let mut list: Vec<AdapterInfo> = (0..n).map(|_| std::mem::zeroed()).collect();
            for a in list.iter_mut() {
                a.size = std::mem::size_of::<AdapterInfo>() as i32;
            }
            if info(ctx, list.as_mut_ptr(), n * std::mem::size_of::<AdapterInfo>() as i32) != 0 {
                return None;
            }
            // one entry per display output: keep the first present AMD one per PCI bus
            let mut gpus: Vec<(i32, i32, i32)> = Vec::new();
            for a in &list {
                if a.vendor_id != 0x1002 || gpus.iter().any(|g| g.0 == a.bus_number) {
                    continue;
                }
                let mut version = 0;
                if let Some(caps) = caps {
                    let (mut supported, mut enabled) = (0, 0);
                    if caps(ctx, a.adapter_index, &mut supported, &mut enabled, &mut version) != 0 {
                        version = 0;
                    }
                }
                gpus.push((a.bus_number, a.adapter_index, version));
            }
            if gpus.is_empty() {
                return None;
            }
            Some(Self {
                ctx,
                temp5: sym(lib, b"ADL2_Overdrive5_Temperature_Get\0"),
                activity: sym(lib, b"ADL2_Overdrive5_CurrentActivity_Get\0"),
                fan5: sym(lib, b"ADL2_Overdrive5_FanSpeed_Get\0"),
                power6: sym(lib, b"ADL2_Overdrive6_CurrentPower_Get\0"),
                temp_n: sym(lib, b"ADL2_OverdriveN_Temperature_Get\0"),
                status_n: sym(lib, b"ADL2_OverdriveN_PerformanceStatus_Get\0"),
                fan_n: sym(lib, b"ADL2_OverdriveN_FanControl_Get\0"),
                gpus,
            })
        }
    }

    /// Add what ADL knows about the GPU on PCI `bus` (None: the only AMD GPU).
    pub fn fill(&self, bus: Option<u32>, p: &mut Perf) {
        let gpu = match bus {
            Some(b) => self.gpus.iter().find(|g| g.0 == b as i32),
            None if self.gpus.len() == 1 => self.gpus.first(),
            None => None,
        };
        let Some(&(_, idx, version)) = gpu else { return };
        let ctx = self.ctx;
        let celsius = |milli: i32| -> Option<f32> {
            let c = milli as f32 / 1000.0;
            // a missing sensor reads as thousands of degrees on some cards
            (c > 0.0 && c < 150.0).then_some(c)
        };
        unsafe {
            if p.temp.is_none() {
                if let Some(f) = self.temp5 {
                    let mut t = Temperature { size: std::mem::size_of::<Temperature>() as i32, ..Default::default() };
                    if f(ctx, idx, 0, &mut t) == 0 {
                        p.temp = celsius(t.temperature);
                    }
                }
            }
            if version >= 7 {
                if let Some(f) = self.temp_n {
                    let mut v = 0;
                    if p.temp.is_none() && f(ctx, idx, ODN_EDGE, &mut v) == 0 {
                        p.temp = celsius(v);
                    }
                    let mut v = 0;
                    if f(ctx, idx, ODN_HOTSPOT, &mut v) == 0 {
                        p.hotspot = celsius(v).or(p.hotspot);
                    }
                }
                // clocks and link of the Overdrive N cards, which answer the Overdrive 5
                // activity call with nothing useful
                if let Some(f) = self.status_n {
                    let mut s = OdnStatus::default();
                    if f(ctx, idx, &mut s) == 0 {
                        // 10 kHz units
                        let mhz = |v: i32| (v > 0 && v < 5_000_000).then(|| (v / 100) as u32);
                        if p.core_mhz.is_none() {
                            p.core_mhz = mhz(s.core_clock);
                        }
                        if p.mem_mhz.is_none() {
                            p.mem_mhz = mhz(s.memory_clock);
                        }
                        if p.pcie_width.is_none() && (1..=32).contains(&s.bus_lanes) {
                            p.pcie_width = Some(s.bus_lanes as u32);
                            if (1..=32).contains(&s.max_bus_lanes) {
                                p.pcie_max_width = Some(s.max_bus_lanes as u32);
                            }
                        }
                    }
                }
                if let Some(f) = self.fan_n {
                    let mut s = OdnFan::default();
                    if f(ctx, idx, &mut s) == 0 {
                        match s.speed_mode {
                            FAN_PERCENT if (0..=100).contains(&s.speed) => p.fan_pct = Some(s.speed as u32),
                            FAN_RPM if s.speed > 0 && s.speed < 20_000 => p.fan_rpm = Some(s.speed as u32),
                            _ => {}
                        }
                    }
                }
            }
            if let Some(f) = self.activity {
                let mut a = Activity { size: std::mem::size_of::<Activity>() as i32, ..Default::default() };
                if f(ctx, idx, &mut a) == 0 {
                    // clocks in units of 10 kHz
                    let mhz = |v: i32| (v > 0 && v < 5_000_000).then(|| (v / 100) as u32);
                    if p.core_mhz.is_none() {
                        p.core_mhz = mhz(a.engine_clock);
                    }
                    if p.mem_mhz.is_none() {
                        p.mem_mhz = mhz(a.memory_clock);
                    }
                    // bus speed in MT/s per lane: 2500, 5000, 8000, 16000, 32000
                    let gen = match a.current_bus_speed {
                        2500 => Some(1),
                        5000 => Some(2),
                        8000 => Some(3),
                        16000 => Some(4),
                        32000 => Some(5),
                        _ => None,
                    };
                    if let (Some(g), true) = (gen, (1..=32).contains(&a.current_bus_lanes)) {
                        p.pcie_gen = Some(g);
                        p.pcie_width = Some(a.current_bus_lanes as u32);
                        if (1..=32).contains(&a.maximum_bus_lanes) {
                            p.pcie_max_width = Some(a.maximum_bus_lanes as u32);
                        }
                    }
                }
            }
            if let Some(f) = self.fan5 {
                for kind in [FAN_PERCENT, FAN_RPM] {
                    let mut s = FanSpeed { size: std::mem::size_of::<FanSpeed>() as i32, speed_type: kind, ..Default::default() };
                    if f(ctx, idx, 0, &mut s) == 0 {
                        match kind {
                            FAN_PERCENT if (0..=100).contains(&s.fan_speed) => p.fan_pct = Some(s.fan_speed as u32),
                            FAN_RPM if s.fan_speed > 0 && s.fan_speed < 20_000 => p.fan_rpm = Some(s.fan_speed as u32),
                            _ => {}
                        }
                    }
                }
            }
            if version >= 6 {
                if let Some(f) = self.power6 {
                    // total board power in 1/256 W
                    let mut v = 0;
                    if f(ctx, idx, 0, &mut v) == 0 && v > 0 {
                        let w = (v >> 8) as f32;
                        if w > 0.0 && w < 2000.0 {
                            p.power_w = Some(w);
                        }
                    }
                }
            }
        }
    }
}
