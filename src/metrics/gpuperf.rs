//! GPU sensors the way Task Manager reads them: D3DKMTQueryAdapterInfo on the adapter opened by
//! its LUID, ADAPTERPERFDATA (temperature, memory clock, power) and NODEPERFDATA (engine clock).
//! Vendor neutral (NVIDIA, AMD, Intel), no SDK, no elevation; needs a WDDM 2.4+ driver
//! (Windows 10 1803) that fills the fields, which integrated GPUs often do not. Anything missing
//! (Windows 7, old driver, no sensor) is just None.

use windows::Win32::Foundation::LUID;
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::core::{s, w};

/// KMTQUERYADAPTERINFOTYPE values
const KMTQAITYPE_NODEPERFDATA: i32 = 61;
const KMTQAITYPE_ADAPTERPERFDATA: i32 = 62;

/// One reading; every field is None when the driver does not report it.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Perf {
    /// °C
    pub temp: Option<f32>,
    /// engine clock of the 3D node, MHz
    pub core_mhz: Option<u32>,
    /// memory clock, MHz
    pub mem_mhz: Option<u32>,
    /// share of the power budget, 0..1
    pub power: Option<f32>,
}

#[repr(C)]
struct OpenAdapterFromLuid {
    luid: LUID,
    adapter: u32,
}

#[repr(C)]
struct QueryAdapterInfo {
    adapter: u32,
    kind: i32,
    data: *mut core::ffi::c_void,
    size: u32,
}

#[repr(C)]
struct CloseAdapter {
    adapter: u32,
}

/// D3DKMT_ADAPTER_PERFDATA
#[repr(C)]
#[derive(Default)]
struct AdapterPerfData {
    physical_adapter_index: u32,
    /// Hz
    memory_frequency: u64,
    max_memory_frequency: u64,
    max_memory_frequency_oc: u64,
    memory_bandwidth: u64,
    pcie_bandwidth: u64,
    fan_rpm: u32,
    /// tenths of a percent of the power budget
    power: u32,
    /// tenths of a degree Celsius
    temperature: u32,
    power_state_override: u8,
}

/// D3DKMT_NODE_PERFDATA
#[repr(C)]
#[derive(Default)]
struct NodePerfData {
    node_ordinal: u32,
    physical_adapter_index: u32,
    /// Hz
    frequency: u64,
    max_frequency: u64,
    max_frequency_oc: u64,
    voltage: u32,
    voltage_max: u32,
    voltage_max_oc: u32,
    max_transition_latency: u64,
}

type OpenFn = unsafe extern "system" fn(*mut OpenAdapterFromLuid) -> i32;
type QueryFn = unsafe extern "system" fn(*const QueryAdapterInfo) -> i32;
type CloseFn = unsafe extern "system" fn(*const CloseAdapter) -> i32;

pub struct GpuPerf {
    query: QueryFn,
    close: CloseFn,
    /// open kernel adapter handle per LUID
    handles: Vec<(u64, u32)>,
}

impl GpuPerf {
    /// None when gdi32 lacks the D3DKMT entry points (Windows 7) or no adapter opens.
    pub fn new(luids: &[u64]) -> Option<Self> {
        unsafe {
            let gdi = LoadLibraryW(w!("gdi32.dll")).ok()?;
            let open: OpenFn = std::mem::transmute(GetProcAddress(gdi, s!("D3DKMTOpenAdapterFromLuid"))?);
            let query: QueryFn = std::mem::transmute(GetProcAddress(gdi, s!("D3DKMTQueryAdapterInfo"))?);
            let close: CloseFn = std::mem::transmute(GetProcAddress(gdi, s!("D3DKMTCloseAdapter"))?);
            let mut handles = Vec::new();
            for &luid in luids {
                let mut o = OpenAdapterFromLuid { luid: LUID { LowPart: luid as u32, HighPart: (luid >> 32) as i32 }, adapter: 0 };
                if open(&mut o) == 0 {
                    handles.push((luid, o.adapter));
                }
            }
            if handles.is_empty() {
                return None;
            }
            Some(Self { query, close, handles })
        }
    }

    fn query<T>(&self, adapter: u32, kind: i32, data: &mut T) -> bool {
        let q = QueryAdapterInfo { adapter, kind, data: data as *mut T as _, size: std::mem::size_of::<T>() as u32 };
        unsafe { (self.query)(&q) == 0 }
    }

    /// Sensors of the adapter with this LUID; implausible values (no sensor reads as 0) are None.
    pub fn read(&self, luid: u64) -> Perf {
        let Some(&(_, adapter)) = self.handles.iter().find(|h| h.0 == luid) else { return Perf::default() };
        let mut perf = Perf::default();
        let mut a = AdapterPerfData::default();
        if self.query(adapter, KMTQAITYPE_ADAPTERPERFDATA, &mut a) {
            let c = a.temperature as f32 / 10.0;
            perf.temp = (c > 0.0 && c < 150.0).then_some(c);
            perf.mem_mhz = mhz(a.memory_frequency);
            perf.power = (a.power > 0 && a.power <= 2000).then_some(a.power as f32 / 1000.0);
        }
        // node 0 is the 3D engine
        let mut n = NodePerfData::default();
        if self.query(adapter, KMTQAITYPE_NODEPERFDATA, &mut n) {
            perf.core_mhz = mhz(n.frequency);
        }
        perf
    }
}

/// Hz to MHz; None for 0 and for anything past 50 GHz.
fn mhz(hz: u64) -> Option<u32> {
    let m = (hz / 1_000_000) as u32;
    (m > 0 && m < 50_000).then_some(m)
}

impl Drop for GpuPerf {
    fn drop(&mut self) {
        for &(_, adapter) in &self.handles {
            unsafe {
                (self.close)(&CloseAdapter { adapter });
            }
        }
    }
}
