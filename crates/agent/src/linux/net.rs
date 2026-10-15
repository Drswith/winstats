//! Network adapters: what the hardware is (PCI name or driver) and the Wi-Fi link through `iw`
//! where it is installed.

use super::{driver, read, Root};
use std::time::{Duration, Instant};

/// "Intel Ethernet Controller I225-V" from pci.ids, else the driver ("r8152", "brcmfmac").
pub fn description(root: &Root, iface: &str) -> String {
    let dev = root.path("/sys/class/net").join(iface).join("device");
    if !dev.exists() {
        return String::new();
    }
    if let Some(name) = super::gpu::pci_device_name(root, &dev) {
        return name;
    }
    driver(&dev)
}

/// (SSID, "802.11ax, 5 GHz") of a connected Wi-Fi interface, from `iw dev <if> link`.
fn iw_link(iface: &str) -> Option<(String, String)> {
    let out = std::process::Command::new("iw").args(["dev", iface, "link"]).output().ok()?;
    parse_iw(&String::from_utf8_lossy(&out.stdout))
}

fn parse_iw(text: &str) -> Option<(String, String)> {
    if !text.starts_with("Connected") {
        return None;
    }
    let field = |k: &str| text.lines().find_map(|l| l.trim().strip_prefix(k)).map(|v| v.trim().to_string());
    let ssid = field("SSID:").unwrap_or_default();
    let mhz: f32 = field("freq:").and_then(|f| f.split_whitespace().next()?.parse().ok()).unwrap_or(0.0);
    let band = match mhz as u32 {
        2400..=2500 => "2.4 GHz",
        4900..=5900 => "5 GHz",
        5925..=7125 => "6 GHz",
        _ => "",
    };
    let rate = [field("tx bitrate:"), field("rx bitrate:")].into_iter().flatten().collect::<Vec<_>>().join(" ");
    let std = if rate.contains("EHT-") {
        "802.11be"
    } else if rate.contains("HE-") {
        "802.11ax"
    } else if rate.contains("VHT-") {
        "802.11ac"
    } else if rate.contains("MCS") {
        "802.11n"
    } else {
        ""
    };
    let link = [std, band].iter().filter(|s| !s.is_empty()).copied().collect::<Vec<_>>().join(", ");
    Some((ssid, link))
}

/// `iw` results, asked at most every 10 s per interface.
#[derive(Default)]
pub struct Wifi {
    cache: Vec<(String, Instant, Option<(String, String)>)>,
}

impl Wifi {
    pub fn link(&mut self, iface: &str) -> Option<(String, String)> {
        let now = Instant::now();
        if let Some((_, t, v)) = self.cache.iter().find(|(i, ..)| i == iface) {
            if now.duration_since(*t) < Duration::from_secs(10) {
                return v.clone();
            }
        }
        let v = iw_link(iface);
        self.cache.retain(|(i, ..)| i != iface);
        self.cache.push((iface.to_string(), now, v.clone()));
        v
    }
}

/// True when the interface is wireless (has a `wireless` or `phy80211` entry).
pub fn is_wifi(root: &Root, iface: &str) -> bool {
    let d = root.path("/sys/class/net").join(iface);
    d.join("wireless").exists() || d.join("phy80211").exists() || read(&d.join("uevent")).map_or(false, |u| u.contains("DEVTYPE=wlan"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iw() {
        let t = "Connected to 11:22:33:44:55:66 (on wlan0)\n\tSSID: home\n\tfreq: 5180.0\n\tRX: 1 bytes\n\tsignal: -50 dBm\n\trx bitrate: 866.7 MBit/s VHT-MCS 9 80MHz short GI VHT-NSS 2\n\ttx bitrate: 1200.9 MBit/s 80MHz HE-MCS 11 HE-NSS 2 HE-GI 0 HE-DCM 0\n";
        assert_eq!(parse_iw(t), Some(("home".into(), "802.11ax, 5 GHz".into())));
        assert_eq!(parse_iw("Not connected.\n"), None);
        assert_eq!(parse_iw("Connected to x\n\tSSID: a\n\tfreq: 2437\n\ttx bitrate: 72.2 MBit/s MCS 7 short GI\n").unwrap().1, "802.11n, 2.4 GHz");
    }
}
