//! Linux sources `sysinfo` does not cover, each tried in turn: the GPU (NVML, amdgpu, Intel, SoC
//! devfreq and kgsl, DRM fdinfo for the rest), thermal zones, fans, power rails, the battery, disks, network
//! adapters, system counters and the CPU name on RISC-V. Every reader takes the
//! file-system root so the tests run on fake trees; on other systems the paths do not exist and
//! everything comes back empty.
// macOS samples through `crate::macos` and only looks here for nothing
#![cfg_attr(target_os = "macos", allow(dead_code))]

pub mod block;
pub mod cpu;
pub mod dmi;
pub mod fdinfo;
pub mod gpu;
pub mod hwmon;
pub mod net;
pub mod power;
#[cfg(target_os = "linux")]
mod nvml;
pub mod sys;
pub mod thermal;

use std::fs;
use std::path::{Path, PathBuf};

/// Where `/sys` and `/proc` live: "/" on the machine, a temporary tree in the tests.
#[derive(Clone, Debug)]
pub struct Root(pub PathBuf);

impl Root {
    pub fn system() -> Root {
        Root(PathBuf::from("/"))
    }

    pub fn path(&self, p: &str) -> PathBuf {
        self.0.join(p.trim_start_matches('/'))
    }
}

pub fn read(p: &Path) -> Option<String> {
    fs::read_to_string(p).ok().map(|s| s.trim().to_string())
}

pub fn read_num(p: &Path) -> Option<i64> {
    read(p)?.parse().ok()
}

/// Entries of a directory, sorted; empty when it cannot be read.
pub fn dirs(p: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = fs::read_dir(p).map(|d| d.flatten().map(|e| e.path()).collect()).unwrap_or_default();
    v.sort();
    v
}

/// Kernel driver bound to a device ("amdgpu", "i915"), "" when none.
pub fn driver(dev: &Path) -> String {
    fs::read_link(dev.join("driver")).ok().and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string())).unwrap_or_default()
}

/// Effective user is root: other users' processes (DRM fdinfo) are readable.
pub fn is_root() -> bool {
    #[cfg(unix)]
    {
        unsafe { libc::geteuid() == 0 }
    }
    #[cfg(not(unix))]
    {
        false
    }
}

/// Helpers for the tests: a fresh temporary tree and files in it.
#[cfg(test)]
pub mod fake {
    use std::path::{Path, PathBuf};

    pub fn tree(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("wsa-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    pub fn file(root: &Path, rel: &str, text: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }

    #[cfg(unix)]
    pub fn link(root: &Path, rel: &str, target: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        let _ = std::fs::remove_file(&p);
        std::os::unix::fs::symlink(target, p).unwrap();
    }
}
