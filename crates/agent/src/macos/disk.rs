//! The internal disk through IOKit: its model, serial, firmware and kind from the block
//! storage device, transfers and time spent from the driver's `Statistics`.

use super::{get, int, string, Io};
use std::time::Instant;

#[derive(Debug, Default, Clone)]
pub struct Hw {
    pub model: String,
    pub bus: &'static str,
    pub serial: String,
    pub firmware: String,
    pub ssd: Option<bool>,
}

pub struct Disk {
    driver: Io,
    pub hw: Hw,
    prev: Option<([u64; 4], Instant)>,
}

/// (iops, ms per transfer) since the last call.
pub type Activity = (f32, Option<f32>);

impl Disk {
    /// The internal disk the most was read from: the boot disk, not an SD card or disk image.
    pub fn find() -> Option<Disk> {
        let mut best: Option<(u64, Disk)> = None;
        for driver in Io::services("IOBlockStorageDriver") {
            let Some(dev) = driver.parent() else { continue };
            let (Some(chars), Some(proto)) = (dev.prop("Device Characteristics"), dev.prop("Protocol Characteristics")) else { continue };
            let text = |k: &str| string(get(chars.0, k)).unwrap_or_default().trim().to_string();
            let wire = string(get(proto.0, "Physical Interconnect")).unwrap_or_default();
            if string(get(proto.0, "Physical Interconnect Location")).as_deref() != Some("Internal") || wire == "Secure Digital" {
                continue;
            }
            let read = driver.prop("Statistics").and_then(|s| int(get(s.0, "Bytes (Read)"))).unwrap_or(0) as u64;
            let bus = match wire.as_str() {
                "PCI-Express" | "Apple Fabric" => "NVMe",
                "SATA" => "SATA",
                "USB" => "USB",
                _ => "",
            };
            let medium = text("Medium Type");
            let ssd = match medium.as_str() {
                "Solid State" => Some(true),
                "Rotational" => Some(false),
                _ => None,
            };
            let vendor = text("Vendor Name");
            let product = text("Product Name");
            let model = if vendor.is_empty() || product.starts_with(&vendor) { product } else { format!("{vendor} {product}") };
            let hw = Hw { model, bus, serial: text("Serial Number"), firmware: text("Product Revision Level"), ssd };
            if best.as_ref().map_or(true, |(b, _)| read > *b) {
                best = Some((read, Disk { driver, hw, prev: None }));
            }
        }
        best.map(|(_, d)| d)
    }

    pub fn activity(&mut self) -> Option<Activity> {
        let stats = self.driver.prop("Statistics")?;
        let n = |k: &str| int(get(stats.0, k)).unwrap_or(0).max(0) as u64;
        let cur = [n("Operations (Read)"), n("Operations (Write)"), n("Total Time (Read)"), n("Total Time (Write)")];
        let now = Instant::now();
        let out = self.prev.and_then(|(p, t)| {
            let dt = now.duration_since(t).as_secs_f64();
            if dt < 0.05 {
                return None;
            }
            let ops = (cur[0] + cur[1]).saturating_sub(p[0] + p[1]) as f64;
            let ns = (cur[2] + cur[3]).saturating_sub(p[2] + p[3]) as f64;
            Some(((ops / dt) as f32, (ops > 0.0).then(|| (ns / ops / 1e6) as f32)))
        });
        self.prev = Some((cur, now));
        out
    }
}
