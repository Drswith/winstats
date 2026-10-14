//! GPU identity and VRAM usage through DXGI: every hardware adapter with its vendor/device id,
//! dedicated memory and user-mode driver version; VRAM usage for the primary one (DXGI 1.4).
//! Adapters can be switched off (popup click, config `gpu_disabled`): the load then covers the
//! others only and the primary is picked among them.

use windows::Win32::Graphics::Dxgi::*;
use windows::core::Interface;

#[derive(Clone, Debug)]
pub struct AdapterInfo {
    pub name: String,
    pub vendor_id: u32,
    pub device_id: u32,
    pub dedicated: u64,
    /// user-mode driver version as reported by DXGI, e.g. "32.0.101.6790"
    pub driver: String,
    /// adapter LUID, as in the "luid_0x..._0x..." part of the GPU Engine counter instances
    pub luid: u64,
    /// counted in the load and eligible as the primary adapter
    pub enabled: bool,
    /// busiest engine type of this adapter in 0..1; None until the counters have a sample
    pub load: Option<f32>,
    /// temperature, clocks, power; read for the primary adapter while the popup is open
    pub perf: super::gpuperf::Perf,
}

impl AdapterInfo {
    pub fn vendor(&self) -> &'static str {
        match self.vendor_id {
            0x10DE => "NVIDIA",
            0x1002 | 0x1022 => "AMD",
            0x8086 => "Intel",
            0x1414 => "Microsoft",
            0x5143 => "Qualcomm",
            0x15AD => "VMware",
            0x1AB8 => "Parallels",
            0x80EE => "VirtualBox",
            _ => "",
        }
    }
}

pub struct Gpu {
    /// DXGI handle of each entry in `adapters`, for the VRAM usage of the primary one
    handles: Vec<Option<IDXGIAdapter3>>,
    primary: Option<usize>,
    /// sensor source; None where the system has no D3DKMT perf data
    sensors: Option<super::gpuperf::GpuPerf>,
    pub name: String,
    pub vram_total: u64,
    pub vram_used: u64,
    pub vram_budget: u64,
    pub adapters: Vec<AdapterInfo>,
}

impl Gpu {
   pub fn new(disabled: &[String]) -> Self {
        let mut g = Self { handles: Vec::new(), primary: None, sensors: None, name: String::new(), vram_total: 0, vram_used: 0, vram_budget: 0, adapters: Vec::new() };
        unsafe {
            let Ok(factory) = CreateDXGIFactory1::<IDXGIFactory1>() else { return g };
            let mut i = 0;
            while let Ok(a) = factory.EnumAdapters1(i) {
                i += 1;
                let Ok(desc) = a.GetDesc1() else { continue };
                if desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0 {
                    continue;
                }
                let name = String::from_utf16_lossy(&desc.Description).trim_end_matches('\0').trim().to_string();
                let driver = a
                    .CheckInterfaceSupport(&IDXGIDevice::IID)
                    .map(|v| {
                        let v = v as u64;
                        format!("{}.{}.{}.{}", (v >> 48) & 0xFFFF, (v >> 32) & 0xFFFF, (v >> 16) & 0xFFFF, v & 0xFFFF)
                    })
                    .unwrap_or_default();
                let luid = (desc.AdapterLuid.HighPart as u32 as u64) << 32 | desc.AdapterLuid.LowPart as u64;
                g.adapters.push(AdapterInfo {
                    name,
                    vendor_id: desc.VendorId,
                    device_id: desc.DeviceId,
                    dedicated: desc.DedicatedVideoMemory as u64,
                    driver,
                    luid,
                    enabled: true,
                    load: None,
                    perf: Default::default(),
                });
                g.handles.push(a.cast::<IDXGIAdapter3>().ok());
            }
        }
        let luids: Vec<u64> = g.adapters.iter().map(|a| a.luid).collect();
        g.sensors = super::gpuperf::GpuPerf::new(&luids);
        g.set_disabled(disabled);
        g
    }

    /// Switch adapters off by name (at least one stays on) and pick the primary among the rest.
    pub fn set_disabled(&mut self, disabled: &[String]) {
        for a in self.adapters.iter_mut() {
            a.enabled = !disabled.contains(&a.name);
        }
        if !self.adapters.iter().any(|a| a.enabled) {
            self.adapters.iter_mut().for_each(|a| a.enabled = true);
        }
        // the enabled adapter with the most dedicated memory: the discrete card over the iGPU
        self.primary = self
            .adapters
            .iter()
            .enumerate()
            .filter(|(_, a)| a.enabled)
            .max_by_key(|(i, a)| (a.dedicated, std::cmp::Reverse(*i)))
            .map(|(i, _)| i);
        let p = self.primary.map(|i| &self.adapters[i]);
        self.name = p.map(|a| a.name.clone()).unwrap_or_default();
        self.vram_total = p.map(|a| a.dedicated).unwrap_or(0);
        self.vram_used = 0;
        self.vram_budget = 0;
        self.update();
    }

    /// The adapter the widgets describe.
    pub fn primary(&self) -> Option<&AdapterInfo> {
        self.primary.map(|i| &self.adapters[i])
    }

    /// Take the per-adapter loads of the GPU Engine counters (by LUID); returns the load shown:
    /// the busiest enabled adapter, or the busiest of all when no LUID matched an adapter.
    pub fn set_loads(&mut self, loads: &std::collections::HashMap<u64, f32>) -> Option<f32> {
        if loads.is_empty() {
            return None;
        }
        let mut best: Option<f32> = None;
        for a in self.adapters.iter_mut() {
            let v = loads.get(&a.luid).cloned();
            a.load = Some(v.unwrap_or(0.0));
            if let (true, Some(v)) = (a.enabled, v) {
                best = Some(best.map_or(v, |b| b.max(v)));
            }
        }
        best.or_else(|| loads.values().cloned().reduce(f32::max))
    }

    /// Sensors of the primary adapter (popup only: two kernel queries per call).
    pub fn update_perf(&mut self) {
        let (Some(t), Some(i)) = (&self.sensors, self.primary) else { return };
        let a = &mut self.adapters[i];
        a.perf = t.read(a.luid);
    }

    pub fn update(&mut self) {
        let Some(a) = self.primary.and_then(|i| self.handles[i].as_ref()) else { return };
        let mut info = DXGI_QUERY_VIDEO_MEMORY_INFO::default();
        if unsafe { a.QueryVideoMemoryInfo(0, DXGI_MEMORY_SEGMENT_GROUP_LOCAL, &mut info) }.is_ok() {
            self.vram_used = info.CurrentUsage;
            self.vram_budget = info.Budget;
        }
    }
}
