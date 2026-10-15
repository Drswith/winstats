//! hwmon: the machine's fans and power rails, and the sensors of a graphics card (amdgpu,
//! nouveau, Intel Arc), which belong to the GPU and not to the machine.

use super::{dirs, read, read_num, Root};
use std::path::{Path, PathBuf};
use std::time::Instant;
use winstats_proto::{FanSample, GpuSample, PowerSample};

/// hwmon chips of graphics cards: their fans and sensors go to the GPU
const CARD_CHIPS: [&str; 5] = ["amdgpu", "radeon", "nouveau", "i915", "xe"];

struct FanSrc {
    id: String,
    rpm: Option<PathBuf>,
    /// pwm value 0..255
    pwm: Option<PathBuf>,
}

/// Fans in hwmon: `fanN_input` and `pwmN` of one chip (Super I/O, laptops, Raspberry Pi 5), or
/// (Tegra) a `pwm-fan` chip with the duty and a separate tachometer chip with a bare `rpm`,
/// joined into one fan.
pub struct Fans(Vec<FanSrc>);

impl Fans {
    pub fn find(root: &Root) -> Fans {
        let mut out: Vec<FanSrc> = Vec::new();
        for chip in dirs(&root.path("/sys/class/hwmon")) {
            let name = read(&chip.join("name")).unwrap_or_default();
            if CARD_CHIPS.contains(&name.as_str()) {
                continue;
            }
            let mut found: Vec<FanSrc> = Vec::new();
            for i in 1..=8 {
                let rpm = Some(chip.join(format!("fan{i}_input"))).filter(|p| p.exists());
                let pwm = Some(chip.join(format!("pwm{i}"))).filter(|p| p.exists());
                if rpm.is_some() || pwm.is_some() {
                    found.push(FanSrc { id: format!("{name} {i}"), rpm, pwm });
                }
            }
            if found.is_empty() && chip.join("rpm").exists() {
                found.push(FanSrc { id: name.clone(), rpm: Some(chip.join("rpm")), pwm: None });
            }
            if found.len() == 1 {
                found[0].id = name.clone();
            }
            out.extend(found);
        }
        // a tachometer that reads 0 at start is an empty header (a duty-only fan stays)
        out.retain(|f| f.rpm.as_deref().map_or(true, |p| read_num(p).map_or(false, |r| r > 0)));
        // one duty-only and one tachometer-only source are the same fan
        let duty: Vec<usize> = (0..out.len()).filter(|&i| out[i].rpm.is_none()).collect();
        let tach: Vec<usize> = (0..out.len()).filter(|&i| out[i].pwm.is_none()).collect();
        if duty.len() == 1 && tach.len() == 1 {
            let rpm = out[tach[0]].rpm.take();
            out[duty[0]].rpm = rpm;
            out.remove(tach[0]);
        }
        Fans(out)
    }

    pub fn sample(&self) -> Vec<FanSample> {
        self.0
            .iter()
            .map(|f| FanSample {
                id: f.id.clone(),
                rpm: f.rpm.as_deref().and_then(read_num).map(|v| v.max(0) as u32),
                duty: f.pwm.as_deref().and_then(read_num).map(|v| (v as f32 / 255.0).clamp(0.0, 1.0)),
            })
            .collect()
    }
}

/// Board power monitors (TI INA family: Jetson's INA3221, INA219 / INA226 hats)
const POWER_CHIPS: [&str; 6] = ["ina3221", "ina2xx", "ina219", "ina226", "ina238", "ina209"];

enum RailSrc {
    /// `powerN_input`, µW
    Power(PathBuf),
    /// `inN_input` mV × `currN_input` mA
    Vi(PathBuf, PathBuf),
}

/// Power rails of a board: "VDD_IN", "VDD_CPU_GPU_CV", "VDD_SOC" on a Jetson Orin.
pub struct Rails(Vec<(String, RailSrc)>);

impl Rails {
    pub fn find(root: &Root) -> Rails {
        let mut out = Vec::new();
        for chip in dirs(&root.path("/sys/class/hwmon")) {
            let name = read(&chip.join("name")).unwrap_or_default();
            if !POWER_CHIPS.contains(&name.as_str()) {
                continue;
            }
            let mut found = Vec::new();
            for i in 1..=8 {
                let f = |k: &str| Some(chip.join(format!("{k}{i}_input"))).filter(|p| p.exists());
                // INA3221 labels the channels that are wired; a channel without one is unused
                let label = read(&chip.join(format!("in{i}_label"))).or_else(|| read(&chip.join(format!("power{i}_label"))));
                let src = match (f("power"), f("in"), f("curr")) {
                    (Some(p), _, _) => RailSrc::Power(p),
                    (None, Some(v), Some(c)) => RailSrc::Vi(v, c),
                    _ => continue,
                };
                if name == "ina3221" && label.is_none() {
                    continue;
                }
                found.push((label.unwrap_or_else(|| format!("{name} {i}")), src));
            }
            if found.len() == 1 && found[0].0.ends_with(" 1") {
                found[0].0 = name.clone();
            }
            out.extend(found);
        }
        Rails(out)
    }

    pub fn sample(&self) -> Vec<PowerSample> {
        self.0
            .iter()
            .filter_map(|(id, src)| {
                let w = match src {
                    RailSrc::Power(p) => read_num(p)? as f32 / 1e6,
                    RailSrc::Vi(v, c) => read_num(v)? as f32 * read_num(c)? as f32 / 1e6,
                };
                Some(PowerSample { id: id.clone(), w: w.max(0.0) })
            })
            .collect()
    }
}

/// The hwmon chip of a graphics card (`<device>/hwmon/hwmonN`).
pub struct CardHwmon {
    dir: PathBuf,
    /// last `energy1_input` (µJ) where the chip has no power reading (Intel Arc)
    energy: Option<(u64, Instant)>,
}

impl CardHwmon {
    pub fn of(dev: &Path) -> Option<CardHwmon> {
        dirs(&dev.join("hwmon")).into_iter().next().map(|dir| CardHwmon { dir, energy: None })
    }

    fn num(&self, f: &str) -> Option<i64> {
        read_num(&self.dir.join(f))
    }

    /// Fills what the card's chip reports and the GPU's own source left empty.
    pub fn fill(&mut self, g: &mut GpuSample) {
        let temp = |v: i64| Some(v as f32 / 1000.0).filter(|c| *c > 0.0 && *c < 150.0);
        // amdgpu labels its sensors: edge (the usual reading), junction (hot spot), mem
        let mut labelled = false;
        for i in 1..=8 {
            let Some(label) = read(&self.dir.join(format!("temp{i}_label"))) else { continue };
            labelled = true;
            let v = self.num(&format!("temp{i}_input")).and_then(temp);
            match label.as_str() {
                "edge" | "GPU core" => g.temp = g.temp.or(v),
                "junction" => g.hotspot = g.hotspot.or(v),
                _ => {}
            }
        }
        if !labelled {
            g.temp = g.temp.or(self.num("temp1_input").and_then(temp));
        }
        // µW; amdgpu has power1_average on older and power1_input on newer kernels
        let watts = |v: i64| Some(v as f32 / 1e6).filter(|w| *w > 0.0 && *w < 2000.0);
        let mut power = self.num("power1_average").or_else(|| self.num("power1_input")).and_then(watts);
        if power.is_none() {
            if let Some(uj) = self.num("energy1_input").map(|v| v.max(0) as u64) {
                let now = Instant::now();
                if let Some((prev, at)) = self.energy {
                    let dt = now.duration_since(at).as_secs_f32();
                    if dt > 0.05 && uj >= prev {
                        power = Some((uj - prev) as f32 / 1e6 / dt).filter(|w| *w < 2000.0);
                    }
                }
                self.energy = Some((uj, now));
            }
        }
        g.power_w = g.power_w.or(power);
        g.power_limit_w = g.power_limit_w.or(self.num("power1_cap").and_then(watts));
        g.fan_rpm = g.fan_rpm.or(self.num("fan1_input").filter(|r| *r > 0).map(|r| r as u32));
        g.fan_pct = g.fan_pct.or(self.num("pwm1").map(|p| ((p.clamp(0, 255) * 100 + 127) / 255) as u32));
        // amdgpu: sclk and mclk in Hz
        let mhz = |v: i64| Some((v / 1_000_000) as u32).filter(|m| *m > 0);
        g.core_mhz = g.core_mhz.or(self.num("freq1_input").and_then(mhz));
        g.mem_mhz = g.mem_mhz.or(self.num("freq2_input").and_then(mhz));
    }
}

#[cfg(test)]
mod tests {
    use super::super::fake;
    use super::*;

    #[test]
    fn ina3221_rails() {
        let t = fake::tree("rails");
        let h = "sys/class/hwmon/hwmon1";
        fake::file(&t, &format!("{h}/name"), "ina3221\n");
        for (i, label, mv, ma) in [(1, "VDD_IN", 5080, 1104), (2, "VDD_CPU_GPU_CV", 5072, 248), (3, "VDD_SOC", 5080, 280)] {
            fake::file(&t, &format!("{h}/in{i}_label"), &format!("{label}\n"));
            fake::file(&t, &format!("{h}/in{i}_input"), &format!("{mv}\n"));
            fake::file(&t, &format!("{h}/curr{i}_input"), &format!("{ma}\n"));
        }
        // the summing channel has a voltage and a current but no rail of its own
        fake::file(&t, &format!("{h}/in7_label"), "sum of shunt voltages\n");
        fake::file(&t, &format!("{h}/in4_input"), "5440\n");
        fake::file(&t, &format!("{h}/curr4_input"), "2296\n");
        fake::file(&t, "sys/class/hwmon/hwmon2/name", "ina219\n");
        fake::file(&t, "sys/class/hwmon/hwmon2/power1_input", "2500000\n");
        fake::file(&t, "sys/class/hwmon/hwmon2/in1_input", "5000\n");
        let r = Rails::find(&Root(t)).sample();
        let got: Vec<(String, f32)> = r.into_iter().map(|p| (p.id, (p.w * 100.0).round() / 100.0)).collect();
        assert_eq!(got, [("VDD_IN".to_string(), 5.61), ("VDD_CPU_GPU_CV".into(), 1.26), ("VDD_SOC".into(), 1.42), ("ina219".into(), 2.5)]);
    }

    #[test]
    fn tegra_fan_is_joined_and_cards_are_skipped() {
        let t = fake::tree("fans");
        fake::file(&t, "sys/class/hwmon/hwmon0/name", "pwmfan");
        fake::file(&t, "sys/class/hwmon/hwmon0/pwm1", "69");
        fake::file(&t, "sys/class/hwmon/hwmon2/name", "pwm_tach");
        fake::file(&t, "sys/class/hwmon/hwmon2/rpm", "1489");
        fake::file(&t, "sys/class/hwmon/hwmon3/name", "amdgpu");
        fake::file(&t, "sys/class/hwmon/hwmon3/fan1_input", "900");
        fake::file(&t, "sys/class/hwmon/hwmon4/name", "nct6798");
        fake::file(&t, "sys/class/hwmon/hwmon4/fan1_input", "0");
        fake::file(&t, "sys/class/hwmon/hwmon4/pwm1", "128");
        fake::file(&t, "sys/class/hwmon/hwmon4/fan2_input", "1100");
        fake::file(&t, "sys/class/hwmon/hwmon4/pwm2", "100");
        let s = Fans::find(&Root(t)).sample();
        assert_eq!(s.len(), 2);
        assert_eq!((s[0].id.as_str(), s[0].rpm), ("pwmfan", Some(1489)));
        assert_eq!((s[1].id.as_str(), s[1].rpm), ("nct6798 2", Some(1100)));
    }

    #[test]
    fn amdgpu_card_sensors() {
        let t = fake::tree("cardhw");
        let h = "dev/hwmon/hwmon5";
        fake::file(&t, &format!("{h}/temp1_label"), "edge");
        fake::file(&t, &format!("{h}/temp1_input"), "54000");
        fake::file(&t, &format!("{h}/temp2_label"), "junction");
        fake::file(&t, &format!("{h}/temp2_input"), "61000");
        fake::file(&t, &format!("{h}/power1_average"), "45000000");
        fake::file(&t, &format!("{h}/power1_cap"), "203000000");
        fake::file(&t, &format!("{h}/fan1_input"), "1200");
        fake::file(&t, &format!("{h}/pwm1"), "255");
        fake::file(&t, &format!("{h}/freq1_input"), "2100000000");
        let mut g = GpuSample::default();
        CardHwmon::of(&t.join("dev")).unwrap().fill(&mut g);
        assert_eq!((g.temp, g.hotspot), (Some(54.0), Some(61.0)));
        assert_eq!((g.power_w, g.power_limit_w), (Some(45.0), Some(203.0)));
        assert_eq!((g.fan_rpm, g.fan_pct, g.core_mhz), (Some(1200), Some(100), Some(2100)));
    }
}
