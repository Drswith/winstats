//! Thermal zones (`/sys/class/thermal`): where Jetson and many ARM / RISC-V boards keep their
//! sensors, and where x86 adds `x86_pkg_temp` and `acpitz` next to hwmon.

use super::{dirs, read, read_num, Root};
use std::path::{Path, PathBuf};
use winstats_proto::TempSample;

/// (zone type, path of its `temp`), e.g. ("cpu-thermal", .../thermal_zone0/temp).
pub fn zones(root: &Root) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    for z in dirs(&root.path("/sys/class/thermal")) {
        if !z.file_name().map_or(false, |n| n.to_string_lossy().starts_with("thermal_zone")) {
            continue;
        }
        if let Some(kind) = read(&z.join("type")) {
            out.push((kind, z.join("temp")));
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// The zones worth showing next to hwmon: on device-tree boards a zone that reads a sensor which
/// also has its own hwmon chip ("cpu-thermal" over "sfctemp temp1" on JH7110) is dropped. The
/// match is the zone's `thermal-sensors` phandle against the hwmon device's `of_node`.
pub fn shown(root: &Root, zones: &[(String, PathBuf)]) -> Vec<(String, PathBuf)> {
    let mut chips = Vec::new();
    for h in dirs(&root.path("/sys/class/hwmon")) {
        let temps = dirs(&h).iter().any(|f| f.file_name().map_or(false, |n| n.to_string_lossy().starts_with("temp") && n.to_string_lossy().ends_with("_input")));
        if let (true, Some(p)) = (temps, phandle(&h.join("device/of_node/phandle"))) {
            chips.push(p);
        }
    }
    let dt = root.path("/sys/firmware/devicetree/base/thermal-zones");
    zones.iter().filter(|(kind, _)| !phandle(&dt.join(kind).join("thermal-sensors")).map_or(false, |p| chips.contains(&p))).cloned().collect()
}

/// First cell of a device-tree property: a big-endian u32.
fn phandle(p: &Path) -> Option<u32> {
    let b = std::fs::read(p).ok()?;
    Some(u32::from_be_bytes(b.get(..4)?.try_into().ok()?))
}

/// Millidegrees to °C; a powered-off zone reads 0 or a large negative value.
pub fn read_temp(p: &Path) -> Option<f32> {
    let c = read_num(p)? as f32 / 1000.0;
    (c > 0.0 && c < 150.0).then_some(c)
}

/// "soc-thermal" and the hwmon copy of it, "soc_thermal temp1", are one sensor: "soc thermal".
fn key(s: &str) -> String {
    let s = s.to_ascii_lowercase().replace(['-', '_'], " ");
    s.strip_suffix(" temp1").unwrap_or(&s).trim().to_string()
}

/// Adds the zones that hwmon (`sysinfo`) does not already report.
pub fn merge(temps: &mut Vec<TempSample>, zones: &[(String, PathBuf)]) {
    let have: Vec<String> = temps.iter().map(|t| key(&t.id)).collect();
    for (kind, path) in zones {
        if have.contains(&key(kind)) {
            continue;
        }
        if let Some(c) = read_temp(path) {
            temps.push(TempSample { id: kind.clone(), c });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::fake;
    use super::*;

    #[test]
    fn merge_skips_hwmon_copies() {
        let t = fake::tree("thermal");
        fake::file(&t, "sys/class/thermal/thermal_zone0/type", "soc-thermal\n");
        fake::file(&t, "sys/class/thermal/thermal_zone0/temp", "44384\n");
        fake::file(&t, "sys/class/thermal/thermal_zone1/type", "x86_pkg_temp\n");
        fake::file(&t, "sys/class/thermal/thermal_zone1/temp", "51000\n");
        fake::file(&t, "sys/class/thermal/thermal_zone2/type", "cv0-thermal\n");
        fake::file(&t, "sys/class/thermal/thermal_zone2/temp", "-256000\n");
        let z = zones(&Root(t));
        assert_eq!(z.len(), 3);
        let mut temps = vec![TempSample { id: "soc_thermal temp1".into(), c: 44.0 }];
        merge(&mut temps, &z);
        let ids: Vec<&str> = temps.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, ["soc_thermal temp1", "x86_pkg_temp"]);
    }

    #[cfg(unix)]
    #[test]
    fn shown_skips_zones_of_hwmon_sensors() {
        let t = fake::tree("thermal-dt");
        fake::file(&t, "sys/class/thermal/thermal_zone0/type", "cpu-thermal\n");
        fake::file(&t, "sys/class/thermal/thermal_zone0/temp", "56155\n");
        fake::file(&t, "sys/class/thermal/thermal_zone1/type", "gpu-thermal\n");
        fake::file(&t, "sys/class/thermal/thermal_zone1/temp", "50000\n");
        fake::file(&t, "sys/firmware/devicetree/base/thermal-zones/cpu-thermal/thermal-sensors", "\0\0\0\x0a");
        fake::file(&t, "sys/firmware/devicetree/base/thermal-zones/gpu-thermal/thermal-sensors", "\0\0\0\x0b\0\0\0\x01");
        fake::file(&t, "sys/firmware/devicetree/base/soc/temperature-sensor@120e0000/phandle", "\0\0\0\x0a");
        fake::file(&t, "sys/class/hwmon/hwmon0/temp1_input", "56155\n");
        // the zone's own hwmon copy has no of_node and keeps nothing out
        fake::file(&t, "sys/class/hwmon/hwmon1/temp1_input", "50000\n");
        fake::link(&t, "sys/class/hwmon/hwmon0/device", "../../../firmware/devicetree/base/soc");
        fake::link(&t, "sys/firmware/devicetree/base/soc/of_node", "temperature-sensor@120e0000");
        let root = Root(t);
        let z = zones(&root);
        let ids: Vec<String> = shown(&root, &z).into_iter().map(|z| z.0).collect();
        assert_eq!(ids, ["gpu-thermal"]);
    }
}
