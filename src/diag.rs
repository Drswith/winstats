//! `winstats --diag [file]`: collect every metric the popups show, as if all of them were open,
//! and write what each source returned (or that it returned nothing) to a text file, then exit.
//! For checking a machine without looking at its screen (an old Windows, a remote PC).

use crate::metrics::LocalMetrics as Metrics;
use std::fmt::Write as _;
use std::time::Duration;

pub fn run(path: &str) {
    let mut out = String::new();
    let _ = writeln!(out, "winstats {} --diag, Windows build {}", env!("CARGO_PKG_VERSION"), crate::util::windows_build());
    let mut m = Metrics::new("C:", 8, &[], &[]);
    m.gpu_details = true;
    // rates need two samples, the process lists and the sensor threads a few more
    for _ in 0..5 {
        m.update(true);
        std::thread::sleep(Duration::from_millis(1200));
    }
    m.update(true);
    let d = &m.data;

    let _ = writeln!(out, "\n[cpu]\nname: {}\ncores: {} sockets: {} names: {:?}", d.cpu_name, d.cpu_cores.len(), d.cpu_sockets, d.cpu_socket_names);
    let _ = writeln!(out, "load: {:.1}% system {:.1}% user {:.1}%", d.cpu_total * 100.0, d.cpu_system * 100.0, d.cpu_user * 100.0);
    let _ = writeln!(out, "clock per core (MHz): {:?}", d.cpu_freq);
    let _ = writeln!(out, "temperature: {:?}", d.cpu_temp);
    let _ = writeln!(out, "classes: {:?} smt: {:?}", d.cpu_classes, d.cpu_smt);
    let _ = writeln!(out, "info: {:?}", d.sys.cpu);
    let s = &d.sys;
    let _ = writeln!(
        out,
        "processes {} threads {} handles {} uptime {}s ctx_switches {:?} interrupts {:?} dpc {:?} isr {:?} perf_limit {:?}",
        s.processes, s.threads, s.handles, s.uptime_secs, s.ctx_switches, s.interrupts, s.dpc_time, s.interrupt_time, s.perf_limit
    );

    let _ = writeln!(out, "\n[memory]\ntotal {} used {} commit {}/{}", d.mem.total, d.mem.used, d.mem.commit_used, d.mem.commit_total);
    let _ = writeln!(out, "pagefile {}/{} hw_reserved {} kernel paged {} nonpaged {} cache {}", s.pagefile_used, s.pagefile_total, s.hw_reserved, d.mem_kernel_paged, d.mem_kernel_nonpaged, d.mem_cache);
    let _ = writeln!(out, "summary {:?} channels {:?}", d.mem_summary, d.mem_channels);
    for mm in &d.mem_modules {
        let _ = writeln!(out, "module: {mm:?}");
    }

    let _ = writeln!(out, "\n[gpu]\navailable (load source): {} load: {:?} name: {}", d.gpu_available, d.gpu, d.gpu_name);
    for a in &d.gpu_adapters {
        let _ = writeln!(out, "adapter: {} {:04X}:{:04X} dedicated {} driver {} luid {:x} enabled {} load {:?} mem {:?}/{:?}", a.name, a.vendor_id, a.device_id, a.dedicated, a.driver, a.luid, a.enabled, a.load, a.dedicated_used, a.shared_used);
    }
    let _ = writeln!(out, "sensors: {:?}", d.gpu_perf);
    let _ = writeln!(out, "vram (DXGI) used {} budget {} total {}", d.vram_used, d.vram_budget, d.vram_total);
    if d.gpu_adapters.iter().any(|a| a.vendor_id == 0x1002) {
        match crate::metrics::amd::Amd::new() {
            Some(a) => {
                let _ = write!(out, "ADL:\n{}", a.probe());
            }
            None => {
                let _ = writeln!(out, "ADL: not available (see winstats.log, lines starting with 'adl:')");
            }
        }
    }
    for p in d.top_gpu.iter().take(5) {
        let _ = writeln!(out, "gpu process: {} {:.1}% vram {}", p.name, p.gpu * 100.0, p.vram);
    }

    let _ = writeln!(out, "\n[disks]\nread {:.0} B/s write {:.0} B/s", d.disk_read, d.disk_write);
    for k in &d.disks {
        let _ = writeln!(out, "disk: {k:?}");
    }
    for v in &d.drives {
        let _ = writeln!(out, "volume: {v:?}");
    }

    let _ = writeln!(out, "\n[network]\nrx {:.0} tx {:.0} iface {} ip {}", d.net_rx, d.net_tx, d.net_iface, d.net_ip);
    for i in &d.net_ifaces {
        let _ = writeln!(out, "iface: {i:?}");
    }

    let _ = writeln!(out, "\n[battery]\n{:?}\n{:?}", d.battery, d.battery_detail);

    let _ = writeln!(out, "\n[processes]");
    for p in d.top_cpu.iter().take(5) {
        let _ = writeln!(out, "cpu: {} {:.1}% system {}", p.name, p.cpu * 100.0, p.system);
    }
    for p in d.top_io.iter().take(3) {
        let _ = writeln!(out, "io: {} {:.0} B/s", p.name, p.io);
    }

    let _ = std::fs::write(path, out);
}
