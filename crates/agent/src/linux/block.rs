//! Disks behind the mounted file systems: the whole device a partition or LVM volume lives on,
//! its model, bus, serial, firmware, SSD / TRIM and temperature from sysfs, and the activity
//! of the mounted device from /proc/diskstats.

use super::{dirs, read, read_num, Root};
use std::collections::HashMap;
use std::path::Path;
use std::time::Instant;

/// (block device of the mount, the whole disk under it): "/dev/nvme0n1p2" → ("nvme0n1p2",
/// "nvme0n1"); "/dev/mapper/vg-root" → ("dm-0", "sda"); "/dev/root" (Raspberry Pi) through the
/// device number of the mount.
pub fn resolve(root: &Root, dev: &str, mount: &str) -> Option<(String, String)> {
    let class = root.path("/sys/class/block");
    let name = std::fs::canonicalize(root.path(dev)).ok().and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string())).or_else(|| Path::new(dev).file_name().map(|n| n.to_string_lossy().to_string()))?;
    let name = if class.join(&name).exists() { name } else { by_mount(root, mount)? };
    if !class.join(&name).exists() {
        return None;
    }
    let mut whole = name.clone();
    // device mapper / md: down to the first member, then from a partition up to its disk
    for _ in 0..4 {
        match dirs(&class.join(&whole).join("slaves")).first() {
            Some(s) => whole = s.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or(whole),
            None => break,
        }
    }
    if class.join(&whole).join("partition").exists() {
        if let Some(parent) = std::fs::canonicalize(class.join(&whole)).ok().and_then(|p| p.parent().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().to_string())) {
            whole = parent;
        }
    }
    Some((name, whole))
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Hw {
    pub model: String,
    pub bus: &'static str,
    pub serial: String,
    pub firmware: String,
    pub ssd: Option<bool>,
    pub trim: Option<bool>,
}

/// What sysfs tells about a whole disk ("nvme0n1", "sda", "mmcblk0").
pub fn hw(root: &Root, whole: &str) -> Hw {
    let blk = root.path("/sys/block").join(whole);
    let dev = blk.join("device");
    let real = std::fs::canonicalize(&blk).map(|p| p.to_string_lossy().to_string()).unwrap_or_default();
    // SCSI pads the vendor to 8 characters: "VMware, ", "ATA     "
    let vendor = read(&dev.join("vendor")).unwrap_or_default().trim_end_matches([',', ' ']).to_string();
    let mmc_type = read(&dev.join("type")).unwrap_or_default();
    let bus = if whole.starts_with("nvme") {
        "NVMe"
    } else if whole.starts_with("mmcblk") {
        if mmc_type == "SD" { "SD" } else { "MMC" }
    } else if real.contains("/usb") {
        "USB"
    } else if whole.starts_with("vd") || whole.starts_with("xvd") || real.contains("virtio") || ["VMware", "QEMU", "Msft", "VBOX"].iter().any(|v| vendor.starts_with(v)) {
        "Virtual"
    } else if whole.starts_with("sd") {
        "SATA"
    } else {
        ""
    };
    let model = [read(&dev.join("model")), read(&dev.join("name"))].into_iter().flatten().find(|m| !m.is_empty()).unwrap_or_default();
    let model = if !vendor.is_empty() && bus != "NVMe" && !model.starts_with(&vendor) && vendor != "ATA" { format!("{vendor} {model}") } else { model };
    let serial = read(&dev.join("serial")).or_else(|| vpd_serial(&dev.join("vpd_pg80"))).unwrap_or_default();
    let firmware = [read(&dev.join("firmware_rev")), read(&dev.join("fwrev")), read(&dev.join("rev"))].into_iter().flatten().find(|m| !m.is_empty()).unwrap_or_default();
    // virtual disks claim to spin; nothing to tell there
    let ssd = if bus == "Virtual" { None } else { read_num(&blk.join("queue/rotational")).map(|r| r == 0) };
    let trim = read_num(&blk.join("queue/discard_max_bytes")).map(|b| b > 0);
    Hw { model: model.split_whitespace().collect::<Vec<_>>().join(" "), bus, serial: serial.trim().to_string(), firmware, ssd, trim }
}

/// The block device of a mount point: "179:2" in mountinfo, then /sys/dev/block/179:2.
fn by_mount(root: &Root, mount: &str) -> Option<String> {
    let info = read(&root.path("/proc/self/mountinfo"))?;
    let devno = info.lines().find_map(|l| {
        let f: Vec<&str> = l.split_whitespace().collect();
        (f.get(4) == Some(&mount)).then(|| f.get(2).map(|s| s.to_string())).flatten()
    })?;
    std::fs::canonicalize(root.path("/sys/dev/block").join(devno)).ok()?.file_name().map(|n| n.to_string_lossy().to_string())
}

/// Unit serial number page of a SCSI / SATA disk: 4 header bytes, then ASCII.
fn vpd_serial(p: &Path) -> Option<String> {
    let b = std::fs::read(p).ok()?;
    (b.len() > 4).then(|| String::from_utf8_lossy(&b[4..]).trim_matches(|c: char| c == '\0' || c.is_whitespace()).to_string()).filter(|s| !s.is_empty())
}

/// °C of the disk: NVMe's own hwmon, `drivetemp` for SATA.
pub fn temp(root: &Root, whole: &str) -> Option<f32> {
    let dev = root.path("/sys/block").join(whole).join("device");
    let mut hw = dirs(&dev.join("hwmon"));
    hw.extend(dirs(&dev).into_iter().filter(|d| d.file_name().map_or(false, |n| n.to_string_lossy().starts_with("hwmon"))));
    hw.iter().find_map(|h| read_num(&h.join("temp1_input"))).map(|m| m as f32 / 1000.0).filter(|c| *c > 0.0 && *c < 150.0)
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Activity {
    pub active: f32,
    pub queue: f32,
    pub latency_ms: Option<f32>,
    pub iops: f32,
}

/// Rates of the /proc/diskstats counters between two reads.
#[derive(Default)]
pub struct DiskStats {
    prev: HashMap<String, ([u64; 11], Instant)>,
    now: HashMap<String, [u64; 11]>,
    at: Option<Instant>,
}

impl DiskStats {
    /// Reads /proc/diskstats once for this interval.
    pub fn refresh(&mut self, root: &Root) {
        let at = Instant::now();
        if let Some(t) = self.at {
            for (k, v) in self.now.drain() {
                self.prev.insert(k, (v, t));
            }
        }
        self.now = parse(&read(&root.path("/proc/diskstats")).unwrap_or_default());
        self.at = Some(at);
    }

    pub fn activity(&self, name: &str) -> Option<Activity> {
        let (cur, at) = (self.now.get(name)?, self.at?);
        let (prev, t) = self.prev.get(name)?;
        let dt = at.duration_since(*t).as_secs_f64();
        if dt < 0.05 {
            return None;
        }
        let d = |i: usize| cur[i].saturating_sub(prev[i]) as f64;
        let ios = d(0) + d(4);
        Some(Activity {
            active: (d(9) / (dt * 1000.0)).clamp(0.0, 1.0) as f32,
            queue: (d(10) / (dt * 1000.0)) as f32,
            latency_ms: (ios > 0.0).then(|| ((d(3) + d(7)) / ios) as f32),
            iops: (ios / dt) as f32,
        })
    }
}

/// "major minor name" and then reads, merged, sectors, ms, writes, merged, sectors, ms,
/// in flight, io ms, weighted io ms (and discard / flush counters on newer kernels).
fn parse(text: &str) -> HashMap<String, [u64; 11]> {
    let mut out = HashMap::new();
    for line in text.lines() {
        let f: Vec<&str> = line.split_ascii_whitespace().collect();
        if f.len() < 14 {
            continue;
        }
        let mut v = [0u64; 11];
        for (i, x) in f[3..14].iter().enumerate() {
            v[i] = x.parse().unwrap_or(0);
        }
        out.insert(f[2].to_string(), v);
    }
    out
}

#[cfg(all(test, unix))]
mod tests {
    use super::super::fake;
    use super::*;

    #[test]
    fn nvme_partition_and_activity() {
        let t = fake::tree("block");
        let d = "sys/devices/pci0000:00/0000:00:1d.0/0000:3d:00.0/nvme/nvme0/nvme0n1";
        fake::file(&t, &format!("{d}/nvme0n1p2/partition"), "2");
        fake::file(&t, &format!("{d}/queue/rotational"), "0");
        fake::file(&t, &format!("{d}/queue/discard_max_bytes"), "2199023255040");
        fake::file(&t, "sys/devices/pci0000:00/0000:00:1d.0/0000:3d:00.0/nvme/nvme0/model", "Samsung SSD 980 1TB                     ");
        fake::file(&t, "sys/devices/pci0000:00/0000:00:1d.0/0000:3d:00.0/nvme/nvme0/serial", "S649NX0R123456   ");
        fake::file(&t, "sys/devices/pci0000:00/0000:00:1d.0/0000:3d:00.0/nvme/nvme0/firmware_rev", "1B4QFXO7");
        fake::file(&t, "sys/devices/pci0000:00/0000:00:1d.0/0000:3d:00.0/nvme/nvme0/hwmon1/temp1_input", "38850");
        fake::link(&t, &format!("{d}/device"), "../../nvme0");
        let rel = d.trim_start_matches("sys/");
        fake::link(&t, "sys/class/block/nvme0n1p2", &format!("../../{rel}/nvme0n1p2"));
        fake::link(&t, "sys/class/block/nvme0n1", &format!("../../{rel}"));
        fake::link(&t, "sys/block/nvme0n1", &format!("../{rel}"));
        let root = Root(t.clone());
        assert_eq!(resolve(&root, "/dev/nvme0n1p2", "/"), Some(("nvme0n1p2".into(), "nvme0n1".into())));
        // /dev/root: no such node, the mount's device number tells
        fake::file(&t, "proc/self/mountinfo", "22 1 259:2 / / rw,noatime shared:1 - ext4 /dev/root rw\n");
        fake::link(&t, "sys/dev/block/259:2", &format!("../../{rel}/nvme0n1p2"));
        assert_eq!(resolve(&root, "/dev/root", "/"), Some(("nvme0n1p2".into(), "nvme0n1".into())));
        let h = hw(&root, "nvme0n1");
        assert_eq!(h, Hw { model: "Samsung SSD 980 1TB".into(), bus: "NVMe", serial: "S649NX0R123456".into(), firmware: "1B4QFXO7".into(), ssd: Some(true), trim: Some(true) });
        assert_eq!(temp(&root, "nvme0n1"), Some(38.85));

        let mut s = DiskStats::default();
        fake::file(&t, "proc/diskstats", " 259 2 nvme0n1p2 100 0 800 50 200 0 1600 150 0 1000 2000 0 0 0 0\n");
        s.refresh(&root);
        assert_eq!(s.activity("nvme0n1p2"), None);
        s.at = s.at.map(|a| a - std::time::Duration::from_secs(1));
        fake::file(&t, "proc/diskstats", " 259 2 nvme0n1p2 150 0 900 100 250 0 1700 250 1 1250 2400 0 0 0 0\n");
        s.refresh(&root);
        let a = s.activity("nvme0n1p2").unwrap();
        assert!((a.active - 0.25).abs() < 0.01 && (a.queue - 0.4).abs() < 0.01 && (a.iops - 100.0).abs() < 1.0);
        assert_eq!(a.latency_ms, Some(1.5));
    }
}
