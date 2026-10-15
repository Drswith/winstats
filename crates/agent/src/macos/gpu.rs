//! GPU load and memory from the IOAccelerator `PerformanceStatistics` that Activity Monitor
//! reads: "Device Utilization %" on Apple, Intel and AMD; "In use system memory" on Apple
//! silicon (unified memory), "vramUsedBytes" on AMD cards. Clocks, power and temperature are
//! filled in by IOReport and SMC.

use super::{float, get, int, Io};
use winstats_proto::GpuSample;

pub struct MacGpu {
    entry: Io,
    pub name: String,
    /// every accelerator found, this one among them
    pub all: Vec<winstats_proto::GpuInfo>,
    vram_total: u64,
    /// the driver counts the memory in use (Apple, AMD); Intel's iGPU only has a ceiling, which
    /// alone would read as "0 B used"
    counts_used: bool,
}

impl MacGpu {
    /// The accelerator with the most memory (a Radeon over the Intel iGPU of the same
    /// MacBook Pro); Apple silicon has one.
    pub fn find() -> Option<MacGpu> {
        let mut best: Option<MacGpu> = None;
        let mut all = Vec::new();
        for entry in Io::services("IOAccelerator") {
            let Some(stats) = entry.prop("PerformanceStatistics") else { continue };
            if get(stats.0, "Device Utilization %").is_null() {
                continue;
            }
            // unified memory: the GPU draws on all of RAM
            let unified = !get(stats.0, "In use system memory").is_null();
            let counts_used = unified || !get(stats.0, "vramUsedBytes").is_null();
            let pci = entry.parent();
            let model = entry.text("model").or_else(|| pci.as_ref().and_then(|p| p.text("model"))).unwrap_or_default();
            let cores = entry.prop("gpu-core-count").and_then(|v| int(v.0)).unwrap_or(0);
            let name = if cores > 0 { format!("{model} ({cores}-core GPU)") } else { model };
            let vram_mb = pci.as_ref().and_then(|p| p.prop("VRAM,totalMB")).and_then(|v| int(v.0)).or_else(|| entry.prop("VRAM,totalMB").and_then(|v| int(v.0))).unwrap_or(0);
            let vram_total = if vram_mb > 0 { vram_mb as u64 * 1024 * 1024 } else if unified { super::sysctl("hw.memsize").unwrap_or(0) } else { 0 };
            // PCI ids of a discrete or Intel GPU, little-endian CFData
            let id = |k: &str| pci.as_ref().and_then(|p| p.prop(k)).and_then(|v| super::data(v.0)).filter(|b| b.len() >= 2).map_or(0, |b| u16::from_le_bytes([b[0], b[1]]) as u32);
            all.push(winstats_proto::GpuInfo { name: name.clone(), vram_total: if unified { 0 } else { vram_total }, vendor_id: id("vendor-id"), device_id: id("device-id"), driver: String::new() });
            let g = MacGpu { entry, name, all: Vec::new(), vram_total, counts_used };
            if best.as_ref().map_or(true, |b| g.vram_total > b.vram_total) {
                best = Some(g);
            }
        }
        best.map(|mut b| {
            b.all = all;
            b
        })
    }

    pub fn sample(&self) -> GpuSample {
        let mut s = GpuSample { vram_total: if self.counts_used { self.vram_total } else { 0 }, ..Default::default() };
        if let Some(stats) = self.entry.prop("PerformanceStatistics") {
            let n = |k: &str| float(get(stats.0, k));
            s.load = (n("Device Utilization %").unwrap_or(0.0) / 100.0).clamp(0.0, 1.0) as f32;
            // Intel's own counters wrap around; only Apple's and AMD's are kept
            let used = n("In use system memory").or_else(|| n("vramUsedBytes")).unwrap_or(0.0);
            if used > 0.0 && used < 1e15 {
                s.vram_used = used as u64;
            }
        }
        s
    }
}
