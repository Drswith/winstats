//! Memory modules from the SMBIOS table the firmware hands Linux (`/sys/firmware/dmi/tables/DMI`,
//! root only; x86 and some ARM servers): "DDR4 2666 MT/s, 2 × 8 GB" and the channels in use,
//! the way winstats reads them on Windows (src/metrics/smbios.rs).

use super::Root;

#[derive(Debug, Clone, PartialEq)]
struct Module {
    slot: String,
    bank: String,
    kind: &'static str,
    speed: u32,
    size: u64,
}

fn type_name(t: u8) -> &'static str {
    match t {
        0x12 => "DDR",
        0x13 => "DDR2",
        0x18 => "DDR3",
        0x1A => "DDR4",
        0x1B => "LPDDR",
        0x1C => "LPDDR2",
        0x1D => "LPDDR3",
        0x1E => "LPDDR4",
        0x20 => "HBM",
        0x21 => "HBM2",
        0x22 => "DDR5",
        0x23 => "LPDDR5",
        0x24 => "HBM3",
        _ => "",
    }
}

fn string_at(strings: &[u8], index: u8) -> String {
    if index == 0 {
        return String::new();
    }
    strings.split(|b| *b == 0).nth(index as usize - 1).map(|s| String::from_utf8_lossy(s).trim().to_string()).unwrap_or_default()
}

/// Type 17 structures of a raw SMBIOS table.
fn modules(table: &[u8]) -> Vec<Module> {
    let mut out = Vec::new();
    let mut off = 0usize;
    while off + 4 <= table.len() {
        let (kind, len) = (table[off], table[off + 1] as usize);
        if len < 4 || off + len > table.len() || kind == 127 {
            break;
        }
        let f = &table[off..off + len];
        let mut end = off + len;
        while end + 1 < table.len() && !(table[end] == 0 && table[end + 1] == 0) {
            end += 1;
        }
        let strings = &table[off + len..(end + 1).min(table.len())];
        if kind == 17 && len >= 0x15 {
            let u16_at = |o: usize| u16::from_le_bytes([f[o], f[o + 1]]);
            let raw = u16_at(0x0C);
            let mut size = match raw {
                0 | 0xFFFF => 0,
                r if r & 0x8000 != 0 => (r & 0x7FFF) as u64 * 1024,
                r => r as u64 * 1024 * 1024,
            };
            if raw == 0x7FFF && len >= 0x20 {
                size = (u32::from_le_bytes([f[0x1C], f[0x1D], f[0x1E], f[0x1F]]) & 0x7FFF_FFFF) as u64 * 1024 * 1024;
            }
            // the configured speed when the table has it, else the rated one
            let configured = if len >= 0x22 { u16_at(0x20) as u32 } else { 0 };
            let speed = if configured > 0 { configured } else if len >= 0x17 { u16_at(0x15) as u32 } else { 0 };
            if size > 0 {
                out.push(Module { slot: string_at(strings, f[0x10]), bank: string_at(strings, f[0x11]), kind: type_name(f[0x12]), speed, size });
            }
        }
        off = end + 2;
    }
    out
}

/// "ChannelA-DIMM0" → "A", "Controller0-ChannelB" → "0B".
fn channel(m: &Module) -> Option<String> {
    for s in [&m.bank, &m.slot] {
        let l = s.to_ascii_lowercase();
        if let Some(p) = l.find("channel") {
            let id: String = l[p + 7..].trim_start_matches([' ', '_', '-', '#']).chars().take_while(|c| c.is_ascii_alphanumeric()).collect();
            if id.is_empty() {
                continue;
            }
            let ctrl: String = l.find("controller").map(|q| l[q + 10..].chars().take_while(|c| c.is_ascii_digit()).collect()).unwrap_or_default();
            return Some(format!("{ctrl}{id}"));
        }
    }
    None
}

fn gb(b: u64) -> String {
    let g = b as f64 / (1u64 << 30) as f64;
    if g >= 1.0 && g.fract() == 0.0 {
        format!("{g:.0} GB")
    } else if g >= 1.0 {
        format!("{g:.1} GB")
    } else {
        format!("{} MB", b >> 20)
    }
}

/// ("DDR4 2666 MT/s, 2 × 8 GB", channels); empty / 0 without a readable table.
pub fn memory(root: &Root) -> (String, usize) {
    let Ok(table) = std::fs::read(root.path("/sys/firmware/dmi/tables/DMI")) else { return (String::new(), 0) };
    summary(&modules(&table))
}

fn summary(mods: &[Module]) -> (String, usize) {
    let Some(first) = mods.first() else { return (String::new(), 0) };
    let mut s = first.kind.to_string();
    if first.speed > 0 {
        s = format!("{s} {} MT/s", first.speed).trim().to_string();
    }
    let sizes = if mods.iter().all(|m| m.size == first.size) { format!("{} \u{00D7} {}", mods.len(), gb(first.size)) } else { mods.iter().map(|m| gb(m.size)).collect::<Vec<_>>().join(" + ") };
    let text = if s.is_empty() { sizes } else { format!("{s}, {sizes}") };
    let mut ids: Vec<String> = Vec::new();
    for m in mods {
        match channel(m) {
            Some(id) if !ids.contains(&id) => ids.push(id),
            Some(_) => {}
            None => return (text, 0),
        }
    }
    (text, ids.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn type17(size_mb: u16, kind: u8, speed: u16, locator: &str, bank: &str) -> Vec<u8> {
        let mut f = vec![0u8; 0x22];
        f[0] = 17;
        f[1] = 0x22;
        f[0x0C..0x0E].copy_from_slice(&size_mb.to_le_bytes());
        f[0x10] = 1;
        f[0x11] = 2;
        f[0x12] = kind;
        f[0x15..0x17].copy_from_slice(&speed.to_le_bytes());
        f[0x20..0x22].copy_from_slice(&speed.to_le_bytes());
        f.extend_from_slice(locator.as_bytes());
        f.push(0);
        f.extend_from_slice(bank.as_bytes());
        f.extend_from_slice(&[0, 0]);
        f
    }

    #[test]
    fn two_channels() {
        let mut t = type17(8192, 0x1A, 2666, "DIMM 0", "ChannelA-DIMM0");
        t.extend(type17(8192, 0x1A, 2666, "DIMM 0", "ChannelB-DIMM0"));
        t.extend([127, 4, 0, 0, 0, 0]);
        assert_eq!(summary(&modules(&t)), ("DDR4 2666 MT/s, 2 \u{00D7} 8 GB".to_string(), 2));
        // a VM's table: no type, no speed, no channel
        let vm = type17(4096, 0x07, 0, "RAM slot #0", "RAM slot #0");
        assert_eq!(summary(&modules(&vm)), ("1 \u{00D7} 4 GB".to_string(), 0));
    }
}
