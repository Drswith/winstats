//! CPU name where `sysinfo` has none (RISC-V): the `uarch` lines of `/proc/cpuinfo`, else the
//! `compatible` of the enabled cores in the device tree. "sifive,u74-mc" becomes "SiFive U74-MC".

use super::{dirs, read, Root};

pub fn name(root: &Root) -> String {
    let mut found: Vec<String> = Vec::new();
    let cpuinfo = read(&root.path("/proc/cpuinfo")).unwrap_or_default();
    for line in cpuinfo.lines() {
        if let Some((k, v)) = line.split_once(':') {
            if k.trim() == "uarch" {
                push(&mut found, v.trim());
            }
        }
    }
    if found.is_empty() {
        // the device tree also lists cores Linux does not run on, e.g. JH7110's S7 monitor core
        for c in dirs(&root.path("/sys/firmware/devicetree/base/cpus")) {
            let node = c.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            let status = read(&c.join("status")).unwrap_or_default();
            if !node.starts_with("cpu@") || status.trim_end_matches('\0') == "disabled" {
                continue;
            }
            if let Some(compat) = read(&c.join("compatible")) {
                push(&mut found, compat.split('\0').next().unwrap_or(""));
            }
        }
    }
    let names: Vec<String> = found.iter().map(|c| pretty(c)).collect();
    names.join(" + ")
}

fn push(found: &mut Vec<String>, v: &str) {
    if !v.is_empty() && !found.iter().any(|f| f == v) {
        found.push(v.to_string());
    }
}

/// "sifive,u74-mc" → "SiFive U74-MC", "arm,cortex-a55" → "Cortex-A55" (as sysinfo names ARM cores).
fn pretty(compat: &str) -> String {
    let (vendor, model) = compat.split_once(',').unwrap_or(("", compat));
    let model: Vec<String> = model
        .split('-')
        .map(|p| {
            if p.len() > 2 && p.chars().all(|c| c.is_ascii_alphabetic()) {
                cap(p)
            } else {
                p.to_ascii_uppercase()
            }
        })
        .collect();
    let model = model.join("-");
    let vendor = match vendor {
        "" | "arm" => return model,
        "sifive" => "SiFive".to_string(),
        "starfive" => "StarFive".into(),
        "thead" => "T-Head".into(),
        "spacemit" => "SpacemiT".into(),
        "andestech" => "Andes".into(),
        v => cap(v),
    };
    format!("{vendor} {model}")
}

fn cap(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::super::fake;
    use super::*;

    #[test]
    fn names() {
        assert_eq!(pretty("sifive,u74-mc"), "SiFive U74-MC");
        assert_eq!(pretty("thead,c910"), "T-Head C910");
        assert_eq!(pretty("spacemit,x60"), "SpacemiT X60");
        assert_eq!(pretty("arm,cortex-a55"), "Cortex-A55");
        assert_eq!(pretty("riscv"), "Riscv");
    }

    #[test]
    fn uarch_then_device_tree() {
        let t = fake::tree("cpu");
        fake::file(&t, "proc/cpuinfo", "processor\t: 0\nhart\t\t: 1\nisa\t\t: rv64imafdc\nuarch\t\t: sifive,u74-mc\n\nprocessor\t: 1\nuarch\t\t: sifive,u74-mc\n");
        fake::file(&t, "sys/firmware/devicetree/base/cpus/cpu@0/compatible", "sifive,s7\0riscv\0");
        fake::file(&t, "sys/firmware/devicetree/base/cpus/cpu@0/status", "disabled\0");
        fake::file(&t, "sys/firmware/devicetree/base/cpus/cpu@1/compatible", "sifive,u74-mc\0riscv\0");
        fake::file(&t, "sys/firmware/devicetree/base/cpus/cpu@1/status", "okay\0");
        fake::file(&t, "sys/firmware/devicetree/base/cpus/cpu@2/compatible", "sifive,u74-mc\0riscv\0");
        let root = Root(t.clone());
        assert_eq!(name(&root), "SiFive U74-MC");
        std::fs::remove_file(t.join("proc/cpuinfo")).unwrap();
        assert_eq!(name(&root), "SiFive U74-MC");
    }
}
