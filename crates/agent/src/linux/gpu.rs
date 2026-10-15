//! The machine's GPU, from the first source that gives its load:
//! - NVML for NVIDIA cards with the proprietary driver;
//! - amdgpu sysfs (`gpu_busy_percent`, VRAM, the card's hwmon);
//! - SoC devfreq with a load file (Tegra nvgpu, Mali on Rockchip kernels) and Adreno kgsl;
//! - DRM fdinfo for every other DRM GPU (Intel i915 / xe, msm, panfrost, panthor, v3d, etnaviv),
//!   only as root, with the clock from the driver's sysfs or devfreq.
//! Of several, the one with the most video memory is reported (the discrete card of a laptop).

use super::fdinfo::{DrmLoad, Key};
use super::hwmon::CardHwmon;
use super::{dirs, driver, read, read_num, Root};
use std::path::{Path, PathBuf};
use winstats_proto::GpuSample;

/// DRM drivers that run a GPU and report engine time in fdinfo; display-only and NPU drivers
/// (rockchip-drm, RKNPU, vc4, tegra host1x, nvidia-drm) are not GPUs to measure
const FDINFO_DRIVERS: [&str; 8] = ["i915", "xe", "amdgpu", "panfrost", "panthor", "v3d", "etnaviv", "lima"];

enum Src {
    #[cfg(target_os = "linux")]
    Nvml(std::sync::Arc<super::nvml::Nvml>, super::nvml::Dev),
    /// amdgpu device directory
    Amd(PathBuf),
    /// `load` 0..1000 (Tegra) or "N@Hz" (Rockchip Mali)
    DevfreqLoad(PathBuf),
    /// Adreno on downstream kernels: `gpu_busy_percentage` "12 %"
    Kgsl(PathBuf),
    Drm(DrmLoad),
}

pub struct Gpu {
    pub name: String,
    /// every candidate found, this one among them
    pub all: Vec<winstats_proto::GpuInfo>,
    vram_total: u64,
    src: Src,
    hwmon: Option<CardHwmon>,
    /// current clock: devfreq `cur_freq` (Hz) or a driver file in MHz
    clock: Option<Clock>,
    max_mhz: Option<u32>,
    /// memory clock where the driver does not report it: Tegra's EMC, through BPMP debugfs (root)
    mem_clock: Option<Clock>,
    mem_max_mhz: Option<u32>,
    /// thermal zone of the GPU (SoCs)
    temp: Option<PathBuf>,
    /// the device in sysfs: PCI ids, driver and PCIe link
    dev: Option<PathBuf>,
}

enum Clock {
    Hz(PathBuf),
    Mhz(PathBuf),
}

impl Gpu {
    /// Candidates in order of preference, the reported one picked by video memory.
    pub fn find(root: &Root, zones: &[(String, PathBuf)]) -> Option<Gpu> {
        let mut all = candidates(root, zones);
        // the biggest memory wins; among equals the earlier source (a dGPU's hwmon breaks ties)
        let mut best = 0;
        for i in 1..all.len() {
            let k = |g: &Gpu| (g.vram_total, g.hwmon.is_some());
            if k(&all[i]) > k(&all[best]) {
                best = i;
            }
        }
        let list: Vec<winstats_proto::GpuInfo> = all.iter().map(|g| g.info(root)).collect();
        (!all.is_empty()).then(|| {
            let mut g = all.swap_remove(best);
            g.all = list;
            g
        })
    }

    /// Name, memory, PCI ids and driver ("nvidia 580.95.05", "nvgpu (L4T R36.4.3)").
    fn info(&self, root: &Root) -> winstats_proto::GpuInfo {
        let mut g = winstats_proto::GpuInfo { name: self.name.clone(), vram_total: self.vram_total, ..Default::default() };
        let Some(dev) = &self.dev else { return g };
        if let Some((v, d)) = pci_ids(dev) {
            (g.vendor_id, g.device_id) = (v, d);
        }
        let drv = driver(dev);
        let version = read(&root.path(&format!("/sys/module/{drv}/version"))).unwrap_or_default();
        g.driver = format!("{drv} {version}").trim().to_string();
        // Jetson: the nvgpu driver comes with the L4T release
        if let Some(rel) = read(&root.path("/etc/nv_tegra_release")).and_then(|t| l4t(&t)) {
            if g.vendor_id == 0 {
                g.driver = format!("{} ({rel})", g.driver).trim().to_string();
            }
        }
        g
    }

    /// (pid, busiest engine 0..1, bytes) of the processes on the GPU, from the last sample;
    /// empty where the source cannot tell (devfreq, kgsl, fdinfo without root).
    pub fn procs(&self) -> Vec<(u32, f32, u64)> {
        match &self.src {
            #[cfg(target_os = "linux")]
            Src::Nvml(nv, d) => nv.procs(d),
            Src::Drm(d) => d.procs.clone(),
            _ => Vec::new(),
        }
    }

    pub fn sample(&mut self) -> GpuSample {
        let mut g = match &mut self.src {
            #[cfg(target_os = "linux")]
            Src::Nvml(nv, d) => nv.sample(d),
            Src::Amd(dev) => {
                let mut g = GpuSample { load: read_num(&dev.join("gpu_busy_percent")).map_or(0.0, |v| v as f32 / 100.0), ..Default::default() };
                g.vram_used = read_num(&dev.join("mem_info_vram_used")).map_or(0, |v| v.max(0) as u64);
                g.vram_total = self.vram_total;
                g
            }
            Src::DevfreqLoad(p) => {
                let load = read(p).and_then(|s| match s.split_once('@') {
                    Some((n, _)) => n.trim().parse::<f32>().ok().map(|v| v / 100.0),
                    None => s.parse::<f32>().ok().map(|v| v / 1000.0),
                });
                GpuSample { load: load.unwrap_or(0.0), ..Default::default() }
            }
            Src::Kgsl(dir) => {
                let load = read(&dir.join("gpu_busy_percentage")).and_then(|s| s.trim_end_matches('%').trim().parse::<f32>().ok());
                GpuSample { load: load.map_or(0.0, |v| v / 100.0), ..Default::default() }
            }
            Src::Drm(d) => GpuSample { load: d.sample(), ..Default::default() },
        };
        g.load = g.load.clamp(0.0, 1.0);
        if let Some(h) = &mut self.hwmon {
            h.fill(&mut g);
        }
        g.core_mhz = g.core_mhz.or_else(|| match &self.clock {
            Some(Clock::Hz(p)) => read_num(p).map(|h| (h / 1_000_000) as u32),
            Some(Clock::Mhz(p)) => read_num(p).map(|m| m as u32),
            None => None,
        });
        g.core_max_mhz = g.core_max_mhz.or(self.max_mhz);
        g.mem_mhz = g.mem_mhz.or_else(|| match &self.mem_clock {
            Some(Clock::Hz(p)) => read_num(p).map(|h| (h / 1_000_000) as u32),
            Some(Clock::Mhz(p)) => read_num(p).map(|m| m as u32),
            None => None,
        });
        g.mem_max_mhz = g.mem_max_mhz.or(self.mem_max_mhz);
        g.temp = g.temp.or_else(|| self.temp.as_deref().and_then(super::thermal::read_temp));
        // PCIe link of a card: "8.0 GT/s PCIe" and the lane count, now and at most
        if let Some(dev) = self.dev.as_ref().filter(|d| d.join("current_link_speed").exists()) {
            let gen = |f: &str| read(&dev.join(f)).and_then(|s| pcie_gen(&s));
            let lanes = |f: &str| read_num(&dev.join(f)).filter(|w| *w > 0).map(|w| w as u32);
            g.pcie_gen = g.pcie_gen.or_else(|| gen("current_link_speed"));
            g.pcie_width = g.pcie_width.or_else(|| lanes("current_link_width"));
            g.pcie_max_gen = g.pcie_max_gen.or_else(|| gen("max_link_speed"));
            g.pcie_max_width = g.pcie_max_width.or_else(|| lanes("max_link_width"));
        }
        g
    }
}

fn candidates(root: &Root, zones: &[(String, PathBuf)]) -> Vec<Gpu> {
    let mut out: Vec<Gpu> = Vec::new();
    let gpu_zone = zones.iter().find(|(k, _)| k.to_ascii_lowercase().starts_with("gpu")).map(|(_, p)| p.clone());

    nvml_candidates(root, &mut out);

    // SoC GPUs with their own load file; a board has one GPU, so DRM below is skipped then
    for df in dirs(&root.path("/sys/class/devfreq")) {
        let dev = df.join("device");
        let compat = compatible(&dev);
        let node = df.file_name().map(|n| n.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
        if !node.ends_with(".gpu") && !compat.contains("mali") {
            continue;
        }
        let load = if df.join("load").exists() {
            df.join("load")
        } else if dev.join("load").exists() {
            dev.join("load")
        } else {
            continue;
        };
        let name = read(&dev.join("gpuinfo")).map(|s| mali_name(&s)).filter(|s| !s.is_empty()).unwrap_or_else(|| soc_name(&compat));
        // Tegra: the memory controller clock, readable by root only
        let emc = root.path("/sys/kernel/debug/bpmp/debug/clk/emc");
        let (mem_clock, mem_max_mhz) = if compat.contains("nvidia,") && read_num(&emc.join("rate")).is_some() {
            (Some(Clock::Hz(emc.join("rate"))), read_num(&emc.join("max_rate")).map(|h| (h / 1_000_000) as u32))
        } else {
            (None, None)
        };
        out.push(Gpu { name, all: Vec::new(), vram_total: 0, src: Src::DevfreqLoad(load), hwmon: None, clock: Some(Clock::Hz(df.join("cur_freq"))), max_mhz: devfreq_max(&df), mem_clock, mem_max_mhz, temp: gpu_zone.clone(), dev: Some(dev.clone()) });
    }
    let kgsl = root.path("/sys/class/kgsl/kgsl-3d0");
    if kgsl.join("gpu_busy_percentage").exists() {
        let name = read(&kgsl.join("gpu_model")).map(|m| adreno_name(&m)).unwrap_or_else(|| "Adreno".into());
        let max_mhz = read_num(&kgsl.join("max_gpuclk")).map(|h| (h / 1_000_000) as u32);
        out.push(Gpu { name, all: Vec::new(), vram_total: 0, src: Src::Kgsl(kgsl.clone()), hwmon: None, clock: Some(Clock::Hz(kgsl.join("gpuclk"))), max_mhz, mem_clock: None, mem_max_mhz: None, temp: gpu_zone.clone(), dev: Some(kgsl.join("device")) });
    }
    let soc_found = out.iter().any(|g| matches!(g.src, Src::DevfreqLoad(_) | Src::Kgsl(_)));

    let nvml_pci: Vec<String> = out.iter().filter_map(nvml_pci).collect();
    let fdinfo_ok = super::is_root() || root.0.as_path() != Path::new("/");
    for card in dirs(&root.path("/sys/class/drm")) {
        let cname = card.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        // card0, not card0-HDMI-A-1 (connectors) or renderD128
        if !cname.starts_with("card") || cname.contains('-') {
            continue;
        }
        let dev = card.join("device");
        let drv = driver(&dev);
        let pci = std::fs::canonicalize(&dev).ok().and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string())).filter(|a| is_pci_addr(a));
        if pci.as_ref().map_or(false, |a| nvml_pci.contains(a)) {
            continue;
        }
        let name = pci_name(root, &dev).unwrap_or_else(|| drv.clone());
        let hwmon = CardHwmon::of(&dev);
        if drv == "amdgpu" && dev.join("gpu_busy_percent").exists() {
            let vram_total = read_num(&dev.join("mem_info_vram_total")).map_or(0, |v| v.max(0) as u64);
            out.push(Gpu { name, all: Vec::new(), vram_total, src: Src::Amd(dev.clone()), hwmon, clock: None, max_mhz: dpm_max(&dev.join("pp_dpm_sclk")), mem_clock: None, mem_max_mhz: dpm_max(&dev.join("pp_dpm_mclk")), temp: None, dev: Some(dev.clone()) });
            continue;
        }
        let drm_name = if drv.starts_with("msm") { "msm".to_string() } else { drv.clone() };
        if !fdinfo_ok || !(FDINFO_DRIVERS.contains(&drv.as_str()) || drm_name == "msm") {
            continue;
        }
        if pci.is_none() && soc_found {
            continue;
        }
        // panfrost counts engine time only with profiling switched on
        if read(&dev.join("profiling")).as_deref() == Some("0") {
            continue;
        }
        let key = match &pci {
            Some(a) => Key::Pdev(a.clone()),
            None => Key::Driver(drm_name),
        };
        let (clock, max_mhz) = intel_clock(&card, &dev).unwrap_or_else(|| {
            // a SoC GPU's devfreq sits under its device, or (msm: the card is the display) is the one `*.gpu` node
            let df = dirs(&dev.join("devfreq")).into_iter().next().or_else(|| dirs(&root.path("/sys/class/devfreq")).into_iter().find(|d| d.to_string_lossy().ends_with(".gpu")));
            match df {
                Some(df) => (Some(Clock::Hz(df.join("cur_freq"))), devfreq_max(&df)),
                None => (None, None),
            }
        });
        let name = if pci.is_none() { soc_name(&compatible(&dev)).trim().to_string() } else { name };
        let name = if name.is_empty() { drv.clone() } else { name };
        let temp = if pci.is_none() { gpu_zone.clone() } else { None };
        out.push(Gpu { name, all: Vec::new(), vram_total: 0, src: Src::Drm(DrmLoad::new(root, key)), hwmon, clock, max_mhz, mem_clock: None, mem_max_mhz: None, temp, dev: Some(dev.clone()) });
    }
    out
}

/// NVIDIA cards that answer NVML; not in the tests (a fake root) and not for a sleeping card.
#[cfg(target_os = "linux")]
fn nvml_candidates(root: &Root, out: &mut Vec<Gpu>) {
    if root.0.as_path() != Path::new("/") || nvidia_asleep(root) {
        return;
    }
    if let Some((nv, devs)) = super::nvml::Nvml::open() {
        let nv = std::sync::Arc::new(nv);
        for d in devs {
            let dev = (!d.pci.is_empty()).then(|| root.path("/sys/bus/pci/devices").join(&d.pci));
            out.push(Gpu { name: d.name.clone(), all: Vec::new(), vram_total: d.vram_total, src: Src::Nvml(nv.clone(), d), hwmon: None, clock: None, max_mhz: None, mem_clock: None, mem_max_mhz: None, temp: None, dev });
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn nvml_candidates(_: &Root, _: &mut Vec<Gpu>) {}

fn nvml_pci(g: &Gpu) -> Option<String> {
    match &g.src {
        #[cfg(target_os = "linux")]
        Src::Nvml(_, d) => Some(d.pci.clone()),
        _ => None,
    }
}

/// An NVIDIA card in runtime suspend (Optimus laptops): NVML would wake it and keep it awake.
#[cfg(target_os = "linux")]
fn nvidia_asleep(root: &Root) -> bool {
    dirs(&root.path("/sys/bus/pci/devices")).iter().any(|d| {
        read(&d.join("vendor")).as_deref() == Some("0x10de")
            && read(&d.join("class")).map_or(false, |c| c.starts_with("0x03"))
            && read(&d.join("power/runtime_status")).as_deref() == Some("suspended")
    })
}

fn is_pci_addr(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 12 && b[4] == b':' && b[7] == b':' && b[10] == b'.'
}

/// Intel: i915 `gt_act_freq_mhz` / `gt_RP0_freq_mhz` on the card, xe `tile0/gt0/freq0`.
fn intel_clock(card: &Path, dev: &Path) -> Option<(Option<Clock>, Option<u32>)> {
    let max = |p: PathBuf| read_num(&p).map(|m| m as u32).filter(|m| *m > 0);
    for (cur, top) in [("gt_act_freq_mhz", "gt_RP0_freq_mhz"), ("gt_cur_freq_mhz", "gt_max_freq_mhz")] {
        if card.join(cur).exists() {
            return Some((Some(Clock::Mhz(card.join(cur))), max(card.join(top))));
        }
    }
    let xe = dev.join("tile0/gt0/freq0");
    if xe.join("act_freq").exists() {
        return Some((Some(Clock::Mhz(xe.join("act_freq"))), max(xe.join("rp0_freq"))));
    }
    None
}

/// The hardware top step of a devfreq node, MHz; `max_freq` may be capped by a power mode.
fn devfreq_max(df: &Path) -> Option<u32> {
    read(&df.join("available_frequencies"))
        .and_then(|s| s.split_whitespace().filter_map(|f| f.parse::<u64>().ok()).max())
        .or_else(|| read_num(&df.join("max_freq")).map(|v| v as u64))
        .map(|h| (h / 1_000_000) as u32)
}

/// Highest level of an amdgpu `pp_dpm_*` table ("2: 2500Mhz *"), MHz.
fn dpm_max(p: &Path) -> Option<u32> {
    read(p)?
        .lines()
        .filter_map(|l| {
            let v = l.split_once(':')?.1.trim();
            let digits: String = v.chars().take_while(|c| c.is_ascii_digit()).collect();
            digits.parse::<u32>().ok()
        })
        .max()
}

/// First entry of a device-tree `compatible` list ("nvidia,ga10b"), "" when there is none.
fn compatible(dev: &Path) -> String {
    std::fs::read(dev.join("of_node/compatible")).map(|b| String::from_utf8_lossy(&b).split('\0').next().unwrap_or("").to_string()).unwrap_or_default()
}

/// Mali `gpuinfo` "Mali-G610 4 cores r0p0 0xA867" -> "Mali-G610 MP4".
fn mali_name(s: &str) -> String {
    let w: Vec<&str> = s.split_whitespace().collect();
    match w.as_slice() {
        [m, n, c, ..] if c.starts_with("core") && n.parse::<u32>().is_ok() => format!("{m} MP{n}"),
        [m, ..] => m.to_string(),
        [] => String::new(),
    }
}

/// kgsl `gpu_model` "Adreno650v2" -> "Adreno 650".
fn adreno_name(s: &str) -> String {
    let n: String = s.trim_start_matches("Adreno").chars().take_while(|c| c.is_ascii_digit()).collect();
    if n.is_empty() {
        "Adreno".into()
    } else {
        format!("Adreno {n}")
    }
}

/// "nvidia,ga10b" -> "NVIDIA Ampere (GA10B)", "arm,mali-valhall-csf" -> "Arm mali-valhall-csf".
fn soc_name(compat: &str) -> String {
    let (vendor, model) = compat.split_once(',').unwrap_or(("", compat));
    match vendor {
        "nvidia" => {
            let arch = match model {
                "gm20b" => "Maxwell ",
                "gp10b" => "Pascal ",
                "gv11b" => "Volta ",
                "ga10b" => "Ampere ",
                _ => "",
            };
            if arch.is_empty() {
                format!("NVIDIA {}", model.to_ascii_uppercase())
            } else {
                format!("NVIDIA {arch}({})", model.to_ascii_uppercase())
            }
        }
        "arm" => format!("Arm {model}"),
        "qcom" => format!("Qualcomm {model}"),
        "brcm" => format!("Broadcom {model}"),
        "img" => format!("Imagination {model}"),
        "" => model.to_string(),
        v => format!("{v} {model}"),
    }
}

/// (vendor, device) of a PCI device directory.
fn pci_ids(dev: &Path) -> Option<(u32, u32)> {
    let hex = |f: &str| read(&dev.join(f)).and_then(|s| u32::from_str_radix(s.trim_start_matches("0x"), 16).ok());
    Some((hex("vendor")?, hex("device")?))
}

/// "8.0 GT/s PCIe" → 3.
fn pcie_gen(s: &str) -> Option<u32> {
    let gts: f32 = s.split_whitespace().next()?.parse().ok()?;
    Some(match gts as u32 {
        0..=2 => 1,
        3..=5 => 2,
        6..=8 => 3,
        9..=16 => 4,
        17..=32 => 5,
        _ => 6,
    })
}

/// "R36 (release), REVISION: 4.3, GCID: ..." → "L4T R36.4.3".
fn l4t(text: &str) -> Option<String> {
    let line = text.lines().next()?.trim_start_matches('#').trim();
    let major = line.split_whitespace().next().filter(|m| m.starts_with('R'))?;
    let rev = line.split("REVISION:").nth(1)?.split(',').next()?.trim();
    Some(format!("L4T {major}.{rev}"))
}

/// Any PCI device by `pci.ids`: "Intel Ethernet Controller I225-V"; None when the list lacks it.
pub fn pci_device_name(root: &Root, dev: &Path) -> Option<String> {
    let (vendor, device) = pci_ids(dev)?;
    let ids = ["/usr/share/hwdata/pci.ids", "/usr/share/misc/pci.ids", "/usr/share/pci.ids"].iter().find_map(|p| std::fs::read_to_string(root.path(p)).ok())?;
    let model = pci_lookup(&ids, vendor, device)?;
    let vname = ids.lines().find_map(|l| l.strip_prefix(&format!("{vendor:04x}  "))).and_then(|v| v.split_whitespace().next()).unwrap_or("").trim_end_matches(',');
    Some(if vname.is_empty() || model.starts_with(vname) { model } else { format!("{vname} {model}") })
}

/// "AMD Radeon RX 6800/6800 XT / 6900 XT" from the PCI ids and `pci.ids`; the bracketed
/// marketing name when there is one.
fn pci_name(root: &Root, dev: &Path) -> Option<String> {
    let (vendor, device) = pci_ids(dev)?;
    let short = match vendor {
        0x10de => "NVIDIA",
        0x1002 => "AMD",
        0x8086 => "Intel",
        0x1a03 => "ASPEED",
        0x1af4 => "virtio",
        0x15ad => "VMware",
        _ => "",
    };
    let text = ["/usr/share/hwdata/pci.ids", "/usr/share/misc/pci.ids", "/usr/share/pci.ids"]
        .iter()
        .find_map(|p| std::fs::read_to_string(root.path(p)).ok())
        .and_then(|ids| pci_lookup(&ids, vendor, device));
    let name = match text {
        Some(t) => {
            let model = match (t.rfind('['), t.rfind(']')) {
                (Some(a), Some(b)) if b > a + 1 => t[a + 1..b].to_string(),
                _ => t,
            };
            if short.is_empty() || model.starts_with(short) {
                model
            } else {
                format!("{short} {model}")
            }
        }
        None => format!("{} GPU {device:04X}", if short.is_empty() { "PCI" } else { short }),
    };
    Some(name)
}

/// The device line of `pci.ids` ("\t73bf  Navi 21 [Radeon RX 6800/6800 XT / 6900 XT]").
fn pci_lookup(ids: &str, vendor: u32, device: u32) -> Option<String> {
    let v = format!("{vendor:04x}  ");
    let d = format!("\t{device:04x}  ");
    let mut lines = ids.lines().skip_while(|l| !l.starts_with(&v));
    lines.next()?;
    for l in lines {
        if !l.starts_with('\t') && !l.starts_with('#') && !l.is_empty() {
            break;
        }
        if let Some(rest) = l.strip_prefix(&d) {
            return Some(rest.trim().to_string());
        }
    }
    None
}

#[cfg(all(test, unix))]
mod tests {
    use super::super::fake;
    use super::*;

    const IDS: &str = "# comment\n1002  Advanced Micro Devices, Inc. [AMD/ATI]\n\t73bf  Navi 21 [Radeon RX 6800/6800 XT / 6900 XT]\n\t\t1002 0e3a  Radeon RX 6900 XT\n8086  Intel Corporation\n\t46a6  Alder Lake-P GT2 [Iris Xe Graphics]\n";

    #[test]
    fn names() {
        assert_eq!(pci_lookup(IDS, 0x1002, 0x73bf).as_deref(), Some("Navi 21 [Radeon RX 6800/6800 XT / 6900 XT]"));
        assert_eq!(pci_lookup(IDS, 0x8086, 0x73bf), None);
        assert_eq!(mali_name("Mali-G610 4 cores r0p0 0xA867"), "Mali-G610 MP4");
        assert_eq!(soc_name("nvidia,ga10b"), "NVIDIA Ampere (GA10B)");
        assert_eq!(adreno_name("Adreno650v2"), "Adreno 650");
        assert_eq!(l4t("# R36 (release), REVISION: 4.3, GCID: 38968081, BOARD: generic, EABI: aarch64, DATE: Wed Jan  8 01:49:37 UTC 2025\n").as_deref(), Some("L4T R36.4.3"));
        assert_eq!(pcie_gen("8.0 GT/s PCIe"), Some(3));
        assert_eq!(pcie_gen("16.0 GT/s PCIe"), Some(4));
        assert_eq!(pcie_gen("2.5 GT/s"), Some(1));
    }

    /// A laptop: Intel iGPU (fdinfo) and an AMD dGPU (gpu_busy_percent); the dGPU is reported.
    #[test]
    fn amd_card_wins_over_intel_igpu() {
        let t = fake::tree("gpus");
        fake::file(&t, "usr/share/hwdata/pci.ids", IDS);
        // card0: i915
        fake::file(&t, "sys/devices/pci0000:00/0000:00:02.0/vendor", "0x8086");
        fake::file(&t, "sys/devices/pci0000:00/0000:00:02.0/device", "0x46a6");
        fake::link(&t, "sys/devices/pci0000:00/0000:00:02.0/driver", "../../../bus/pci/drivers/i915");
        fake::link(&t, "sys/class/drm/card0", "../../devices/pci0000:00/0000:00:02.0/drm/card0");
        fake::link(&t, "sys/devices/pci0000:00/0000:00:02.0/drm/card0/device", "../../../0000:00:02.0");
        fake::file(&t, "sys/devices/pci0000:00/0000:00:02.0/drm/card0/gt_act_freq_mhz", "350");
        fake::file(&t, "sys/devices/pci0000:00/0000:00:02.0/drm/card0/gt_RP0_freq_mhz", "1400");
        // card1: amdgpu
        let a_rel = "devices/pci0000:00/0000:00:01.0/0000:03:00.0";
        let a = format!("sys/{a_rel}");
        fake::file(&t, &format!("{a}/vendor"), "0x1002");
        fake::file(&t, &format!("{a}/device"), "0x73bf");
        fake::link(&t, &format!("{a}/driver"), "../../../../bus/pci/drivers/amdgpu");
        fake::file(&t, &format!("{a}/gpu_busy_percent"), "37");
        fake::file(&t, &format!("{a}/mem_info_vram_total"), "17163091968");
        fake::file(&t, &format!("{a}/mem_info_vram_used"), "1073741824");
        fake::file(&t, &format!("{a}/pp_dpm_sclk"), "0: 500Mhz\n1: 1500Mhz *\n2: 2475Mhz\n");
        fake::file(&t, &format!("{a}/hwmon/hwmon3/temp1_input"), "52000");
        fake::link(&t, "sys/class/drm/card1", &format!("../../{a_rel}/drm/card1"));
        fake::link(&t, &format!("{a}/drm/card1/device"), "../../../0000:03:00.0");
        let root = Root(t);
        let all = candidates(&root, &[]);
        assert_eq!(all.len(), 2, "{:?}", all.iter().map(|g| g.name.clone()).collect::<Vec<_>>());
        assert_eq!(all[0].name, "Intel Iris Xe Graphics");
        let mut g = Gpu::find(&root, &[]).unwrap();
        assert_eq!(g.name, "AMD Radeon RX 6800/6800 XT / 6900 XT");
        let s = g.sample();
        assert!((s.load - 0.37).abs() < 1e-6);
        assert_eq!((s.vram_used, s.temp, s.core_max_mhz), (1073741824, Some(52.0), Some(2475)));
    }

    /// Jetson: devfreq load in the device directory, thermal zone for the temperature.
    #[test]
    fn tegra_devfreq() {
        let t = fake::tree("tegra");
        let d_rel = "devices/platform/bus@0/17000000.gpu";
        let d = format!("sys/{d_rel}");
        fake::file(&t, &format!("{d}/load"), "250");
        fake::file(&t, &format!("{d}/of_node/compatible"), "nvidia,ga10b\0");
        fake::link(&t, "sys/class/devfreq/17000000.gpu", &format!("../../{d_rel}/devfreq/17000000.gpu"));
        fake::link(&t, &format!("{d}/devfreq/17000000.gpu/device"), "../..");
        fake::file(&t, &format!("{d}/devfreq/17000000.gpu/cur_freq"), "612000000");
        fake::file(&t, &format!("{d}/devfreq/17000000.gpu/available_frequencies"), "306000000 765000000");
        fake::file(&t, "zone/temp", "47500");
        fake::file(&t, "sys/kernel/debug/bpmp/debug/clk/emc/rate", "2133000000");
        fake::file(&t, "sys/kernel/debug/bpmp/debug/clk/emc/max_rate", "3199000000");
        let mut g = Gpu::find(&Root(t.clone()), &[("gpu-thermal".into(), t.join("zone/temp"))]).unwrap();
        assert_eq!(g.name, "NVIDIA Ampere (GA10B)");
        let s = g.sample();
        assert!((s.load - 0.25).abs() < 1e-6);
        assert_eq!((s.core_mhz, s.core_max_mhz, s.temp), (Some(612), Some(765), Some(47.5)));
        assert_eq!((s.mem_mhz, s.mem_max_mhz), (Some(2133), Some(3199)));
    }
}
