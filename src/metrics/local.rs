//! Local Windows handles never leave the collector thread. The UI exchanges only owned data.
use super::{builtin, LocalMetrics, MetricsData};
use crate::config::Config;
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone, PartialEq)]
struct Request {
    epoch: u64,
    disk: String,
    top_n: usize,
    net_disabled: Vec<String>,
    disks_disabled: Vec<String>,
    gpu_disabled: Vec<String>,
    group_pe: bool,
    proc_filter: (bool, bool),
    proc_interval: u32,
    interval: u32,
    details: bool,
    gpu_details: bool,
    per_disk: bool,
    begin_details: u64,
    refresh_net: u64,
}

impl Request {
    fn new(cfg: &Config, epoch: u64) -> Self {
        Self {
            epoch,
            disk: cfg.disk.clone(),
            top_n: cfg.top_processes as usize,
            net_disabled: cfg.net_disabled.clone(),
            disks_disabled: cfg.disks_disabled.clone(),
            gpu_disabled: cfg.gpu_disabled.clone(),
            group_pe: cfg.group_pe_cores,
            proc_filter: (cfg.proc_show_system, cfg.proc_show_user),
            proc_interval: cfg.proc_update_ms,
            interval: cfg.update_ms.max(250),
            details: false,
            gpu_details: false,
            per_disk: false,
            begin_details: 0,
            refresh_net: 0,
        }
    }
}

struct Snapshot {
    epoch: u64,
    data: MetricsData,
}

#[derive(Default)]
struct Mailbox {
    // Latest complete desired state, not a queue of lossy individual commands.
    request: Mutex<Option<Request>>,
    wake: Condvar,
    snapshot: Mutex<Option<Snapshot>>,
    stop: AtomicBool,
    finished: AtomicBool,
}

struct Finished(Arc<Mailbox>);
impl Drop for Finished {
    fn drop(&mut self) {
        self.0.finished.store(true, Ordering::Release);
    }
}

struct Worker(Arc<Mailbox>);
impl Worker {
    // Factory runs INSIDE the worker: Windows handles need not be Send or constructed by UI.
    fn spawn<F, C>(initial: Request, factory: F) -> Self
    where
        F: FnOnce() -> C + Send + 'static,
        C: FnMut(&Request, u64) -> MetricsData + 'static,
    {
        let shared = Arc::new(Mailbox::default());
        let thread_shared = shared.clone();
        let result = std::thread::Builder::new()
            .name("local metrics".into())
            .spawn(move || {
                let shared = thread_shared;
                let _finished = Finished(shared.clone());
                let mut collect = factory();
                let mut request = initial;
                let mut seq = 0;
                let mut next = Instant::now();
                loop {
                    let mut pending = shared.request.lock().unwrap();
                    while pending.is_none()
                        && !shared.stop.load(Ordering::Acquire)
                        && Instant::now() < next
                    {
                        pending = shared
                            .wake
                            .wait_timeout(pending, next.saturating_duration_since(Instant::now()))
                            .unwrap()
                            .0;
                    }
                    if shared.stop.load(Ordering::Acquire) {
                        break;
                    }
                    if let Some(new) = pending.take() {
                        request = new;
                    }
                    drop(pending);
                    seq += 1;
                    let started = Instant::now();
                    let data = collect(&request, seq);
                    if shared.stop.load(Ordering::Acquire) {
                        break;
                    }
                    // Build/clone outside the lock; replacement and UI consumption are constant time.
                    let old = shared.snapshot.lock().unwrap().replace(Snapshot {
                        epoch: request.epoch,
                        data,
                    });
                    drop(old);
                    // No catch-up burst if the driver took longer than the requested interval.
                    next = started + Duration::from_millis(request.interval as u64);
                    if next < Instant::now() {
                        next = Instant::now() + Duration::from_millis(request.interval as u64);
                    }
                }
            });
        if result.is_err() {
            shared.finished.store(true, Ordering::Release);
        }
        // Do not join on the window thread: an OS/driver call may never return. Stop is observed
        // after it returns; repeated reloads reconfigure this same worker, never spawn more.
        Self(shared)
    }

    fn submit(&self, request: Request) {
        let old = self.0.request.lock().unwrap().replace(request);
        drop(old);
        self.0.wake.notify_one();
    }

    fn take(&self) -> Option<Snapshot> {
        self.0.snapshot.try_lock().ok()?.take()
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        // Pair with the wait mutex to avoid a lost wake on shutdown.
        let _pending = self.0.request.lock().unwrap();
        self.0.stop.store(true, Ordering::Release);
        self.0.wake.notify_one();
    }
}

/// UI-owned snapshot and desired settings. None of these methods performs hardware I/O.
pub struct Metrics {
    pub data: MetricsData,
    worker: Worker,
    desired: Request,
    sent: Request,
    ready: bool,
    pub want_per_disk: bool,
    pub gpu_details: bool,
    pub proc_interval_ms: u32,
}

impl Deref for Metrics {
    type Target = MetricsData;
    fn deref(&self) -> &MetricsData {
        &self.data
    }
}
impl DerefMut for Metrics {
    fn deref_mut(&mut self) -> &mut MetricsData {
        &mut self.data
    }
}

impl Metrics {
    pub fn new(cfg: &Config) -> Self {
        let desired = Request::new(cfg, 0);
        let worker = Worker::spawn(desired.clone(), || {
            let mut local: Option<LocalMetrics> = None;
            let mut applied: Option<Request> = None;
            move |r: &Request, seq| {
                if applied.as_ref().map_or(true, |old| old.epoch != r.epoch) {
                    local = Some(LocalMetrics::new(
                        &r.disk,
                        r.top_n,
                        &r.net_disabled,
                        &r.disks_disabled,
                    ));
                    applied = None;
                }
                let m = local.as_mut().unwrap();
                let changed =
                    |f: fn(&Request) -> bool| applied.as_ref().map_or(true, |old| f(old) != f(r));
                if changed(|r| r.group_pe) {
                    m.set_group_pe(r.group_pe);
                }
                if applied
                    .as_ref()
                    .map_or(true, |old| old.gpu_disabled != r.gpu_disabled)
                {
                    m.set_gpu_disabled(&r.gpu_disabled);
                }
                if applied
                    .as_ref()
                    .map_or(true, |old| old.net_disabled != r.net_disabled)
                {
                    m.set_net_disabled(&r.net_disabled);
                }
                if applied
                    .as_ref()
                    .map_or(true, |old| old.disks_disabled != r.disks_disabled)
                {
                    m.set_disks_disabled(&r.disks_disabled);
                }
                if applied.as_ref().map_or(true, |old| old.top_n != r.top_n) {
                    m.set_top_n(r.top_n);
                }
                if applied
                    .as_ref()
                    .map_or(true, |old| old.proc_filter != r.proc_filter)
                {
                    m.set_proc_filter(r.proc_filter.0, r.proc_filter.1);
                }
                m.proc_interval_ms = r.proc_interval;
                m.gpu_details = r.gpu_details;
                m.want_per_disk = r.per_disk;
                if r.details
                    && applied.as_ref().map_or(true, |old| {
                        !old.details || old.begin_details != r.begin_details
                    })
                {
                    m.begin_details();
                }
                if applied
                    .as_ref()
                    .map_or(false, |old| old.refresh_net != r.refresh_net)
                {
                    m.refresh_net();
                }
                let at = winstats_metrics::unix_ms();
                m.update(r.details);
                builtin::publish(&mut m.data, seq, at, r.interval, m.pdh.is_some());
                applied = Some(r.clone());
                m.data.clone()
            }
        });
        Self::with_worker(cfg, desired, worker)
    }

    fn with_worker(cfg: &Config, desired: Request, worker: Worker) -> Self {
        let mut data = MetricsData::default();
        builtin::describe(&mut data.generic, desired.interval);
        Self {
            data,
            worker,
            sent: desired.clone(),
            desired,
            ready: false,
            want_per_disk: false,
            gpu_details: false,
            proc_interval_ms: cfg.proc_update_ms,
        }
    }

    pub fn reload(&mut self, cfg: &Config) {
        if self.worker.0.finished.load(Ordering::Acquire) {
            *self = Self::new(cfg);
            return;
        }
        self.desired = Request::new(cfg, self.desired.epoch + 1);
        self.proc_interval_ms = cfg.proc_update_ms;
        self.want_per_disk = false;
        self.gpu_details = false;
        self.ready = false;
        self.data = MetricsData::default();
        builtin::describe(&mut self.data.generic, self.desired.interval);
        self.flush();
    }

    pub fn ready(&self) -> bool {
        self.ready
    }
    fn flush(&mut self) {
        if self.sent != self.desired {
            self.worker.submit(self.desired.clone());
            self.sent = self.desired.clone();
        }
    }
    pub fn set_gpu_disabled(&mut self, v: &[String]) {
        self.desired.gpu_disabled = v.to_vec();
        self.flush();
    }
    pub fn set_net_disabled(&mut self, v: &[String]) {
        self.desired.net_disabled = v.to_vec();
        self.flush();
    }
    pub fn set_disks_disabled(&mut self, v: &[String]) {
        self.desired.disks_disabled = v.to_vec();
        self.flush();
    }
    pub fn set_proc_filter(&mut self, a: bool, b: bool) {
        self.desired.proc_filter = (a, b);
        self.flush();
    }
    pub fn set_group_pe(&mut self, v: bool) {
        self.desired.group_pe = v;
        self.flush();
    }
    pub fn set_top_n(&mut self, v: usize) {
        self.desired.top_n = v;
        self.flush();
    }
    pub fn set_update_interval(&mut self, v: u32) {
        self.desired.interval = v.max(250);
        self.flush();
    }
    pub fn begin_details(&mut self) {
        self.desired.begin_details += 1;
        self.desired.details = true;
        self.desired.gpu_details = self.gpu_details;
        self.flush();
    }
    pub fn refresh_net(&mut self) {
        self.desired.refresh_net += 1;
        self.flush();
    }

    /// At most one latest snapshot per tick. External provider state belongs to the UI and
    /// survives replacement of local data; built-in histories/freshness belong to the worker.
    pub fn update(&mut self, details: bool) -> bool {
        self.desired.details = details;
        self.desired.gpu_details = self.gpu_details;
        self.desired.per_disk = self.want_per_disk;
        self.desired.proc_interval = self.proc_interval_ms;
        self.flush();
        let mut updated = false;
        if let Some(mut snapshot) = self.worker.take().filter(|s| s.epoch == self.desired.epoch) {
            let mut registry = std::mem::take(&mut self.data.generic);
            registry
                .metrics
                .retain(|key, _| !key.starts_with("builtin/"));
            registry.metrics.append(&mut snapshot.data.generic.metrics);
            registry.sources.append(&mut snapshot.data.generic.sources);
            snapshot.data.generic = registry;
            self.data = snapshot.data;
            self.ready = true;
            updated = true;
        }
        if self.worker.0.finished.load(Ordering::Acquire) {
            self.data.offline = true;
            self.data
                .generic
                .offline("builtin", "local collector stopped");
        }
        updated
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn wait(mut f: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !f() {
            assert!(Instant::now() < deadline, "worker did not respond");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn sample(seq: u64) -> MetricsData {
        let mut data = MetricsData {
            cpu_total: 0.375,
            net_rx: seq as f64,
            ..Default::default()
        };
        data.mem.total = 1024;
        data.mem.used = 512;
        data.mem.pct = 0.5;
        builtin::publish(&mut data, seq, winstats_metrics::unix_ms(), 250, true);
        data
    }

    #[test]
    fn blocked_collection_keeps_ui_poll_settings_and_drop_responsive() {
        let cfg = Config::default();
        let request = Request::new(&cfg, 0);
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (seen_tx, seen_rx) = mpsc::channel();
        let ui_thread = std::thread::current().id();
        let worker = Worker::spawn(request.clone(), move || {
            assert_ne!(std::thread::current().id(), ui_thread);
            move |r: &Request, seq| {
                if seq == 1 {
                    entered_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                }
                seen_tx.send(r.clone()).unwrap();
                sample(seq)
            }
        });
        let mut metrics = Metrics::with_worker(&cfg, request, worker);
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let started = Instant::now();
        for n in 1..=1000 {
            metrics.set_top_n(n);
            metrics.set_net_disabled(&[format!("adapter-{n}")]);
            metrics.set_proc_filter(false, true);
            assert!(!metrics.update(false));
        }
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(!metrics.ready());
        release_tx.send(()).unwrap();
        seen_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let final_request = seen_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(final_request.top_n, 1000);
        assert_eq!(final_request.net_disabled, ["adapter-1000"]);
        assert_eq!(final_request.proc_filter, (false, true));
        wait(|| metrics.update(false));
        assert!(metrics.ready());

        // Shutdown must also stay responsive when a native call is stuck indefinitely.
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let worker = Worker::spawn(Request::new(&cfg, 0), move || {
            move |_: &Request, seq| {
                entered_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                sample(seq)
            }
        });
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let shared = worker.0.clone();
        let started = Instant::now();
        drop(worker);
        assert!(started.elapsed() < Duration::from_millis(100));
        release_tx.send(()).unwrap();
        wait(|| shared.finished.load(Ordering::Acquire));
    }

    #[test]
    fn mailbox_keeps_latest_snapshot_and_reload_rejects_previous_generation() {
        let cfg = Config {
            update_ms: 60_000,
            ..Default::default()
        };
        let request = Request::new(&cfg, 0);
        let worker = Worker::spawn(request.clone(), || |_: &Request, seq| sample(seq));
        // Let the real worker publish repeatedly without a UI consumer.
        wait(|| worker.0.snapshot.lock().unwrap().is_some());
        for n in 2..=64 {
            let mut changed = request.clone();
            changed.top_n = n;
            worker.submit(changed);
            wait(|| {
                worker
                    .0
                    .snapshot
                    .lock()
                    .unwrap()
                    .as_ref()
                    .map_or(false, |s| s.data.net_rx == n as f64)
            });
        }
        let mut metrics = Metrics::with_worker(&cfg, request, worker);
        assert!(metrics.update(false));
        assert_eq!(metrics.net_rx, 64.0);
        assert!(!metrics.update(false));
        metrics.desired.epoch = 1;
        metrics.sent = metrics.desired.clone();
        *metrics.worker.0.snapshot.lock().unwrap() = Some(Snapshot {
            epoch: 0,
            data: sample(2000),
        });
        assert!(!metrics.update(false));
        assert_eq!(metrics.net_rx, 64.0);
    }

    #[test]
    fn snapshots_preserve_external_metrics_and_do_not_refresh_old_values() {
        let cfg = Config::default();
        let request = Request::new(&cfg, 0);
        let (gate_tx, gate_rx) = mpsc::channel();
        let worker = Worker::spawn(request.clone(), move || {
            move |_: &Request, seq| {
                gate_rx.recv().unwrap();
                sample(seq)
            }
        });
        let mut metrics = Metrics::with_worker(&cfg, request, worker);
        let descriptor = metrics.generic.metrics["builtin/cpu/usage"]
            .descriptor
            .clone();
        metrics
            .generic
            .describe("external", vec![descriptor])
            .unwrap();
        gate_tx.send(()).unwrap();
        wait(|| metrics.update(false));
        let old = metrics.generic.metrics["builtin/cpu/usage"].clone();
        assert_eq!(
            old.value.as_ref().unwrap().number(),
            metrics.cpu_total as f64 * 100.0
        );
        for _ in 0..100 {
            assert!(!metrics.update(false));
        }
        let current = &metrics.generic.metrics["builtin/cpu/usage"];
        assert_eq!(current.last_success, old.last_success);
        assert_eq!(current.history.len(), old.history.len());
        assert!(metrics.generic.metrics.contains_key("external/cpu/usage"));
        assert_eq!(
            current.status(Instant::now() + Duration::from_secs(10)),
            "stale"
        );
        let shared = metrics.worker.0.clone();
        drop(metrics);
        // Release a possible second call before waiting for clean shutdown.
        let _ = gate_tx.send(());
        wait(|| shared.finished.load(Ordering::Acquire));
    }

    #[test]
    fn collector_panic_marks_builtin_offline_without_removing_last_value() {
        let cfg = Config::default();
        let request = Request::new(&cfg, 0);
        let worker = Worker::spawn(request.clone(), || {
            |_: &Request, seq| {
                if seq > 1 {
                    panic!("injected collector failure");
                }
                sample(seq)
            }
        });
        let mut metrics = Metrics::with_worker(&cfg, request, worker);
        wait(|| metrics.update(false));
        metrics.set_top_n(12); // wakes the next collection, which fails
        wait(|| metrics.worker.0.finished.load(Ordering::Acquire));
        metrics.update(false);
        assert_eq!(
            metrics.generic.metrics["builtin/cpu/usage"].status(Instant::now()),
            "offline"
        );
        assert_eq!(
            metrics.generic.metrics["builtin/cpu/usage"]
                .value
                .as_ref()
                .unwrap()
                .number(),
            37.5
        );
        assert!(metrics.offline);
    }

    #[test]
    #[ignore = "Windows hardware acceptance: run explicitly on the target machine"]
    fn real_windows_snapshot_details_and_reload() {
        let mut cfg = Config {
            update_ms: 250,
            proc_update_ms: 60_000,
            ..Default::default()
        };
        let mut metrics = Metrics::new(&cfg);
        let deadline = Instant::now() + Duration::from_secs(60);
        while !metrics.ready() {
            metrics.update(false);
            assert!(
                Instant::now() < deadline,
                "hardware initialization timed out"
            );
            std::thread::sleep(Duration::from_millis(25));
        }
        assert!(!metrics.cpu_name.is_empty());
        assert!(metrics.mem.total > 0);
        let worker = Arc::as_ptr(&metrics.worker.0);
        metrics.gpu_details = true;
        metrics.begin_details();
        let deadline = Instant::now() + Duration::from_secs(60);
        while metrics.sys.processes == 0 {
            metrics.update(true);
            assert!(Instant::now() < deadline, "details timed out");
            std::thread::sleep(Duration::from_millis(25));
        }
        for (system, user) in [(false, true), (true, false)] {
            metrics.set_proc_filter(system, user);
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                metrics.update(true);
                if !metrics.top_mem.is_empty() && metrics.top_mem.iter().all(|p| p.system == system) {
                    break;
                }
                assert!(Instant::now() < deadline, "process filter did not wake collection");
                std::thread::sleep(Duration::from_millis(25));
            }
        }
        cfg.group_pe_cores = true;
        cfg.top_processes = 5;
        metrics.reload(&cfg);
        assert_eq!(Arc::as_ptr(&metrics.worker.0), worker);
        assert!(!metrics.ready());
        let deadline = Instant::now() + Duration::from_secs(60);
        while !metrics.ready() {
            metrics.update(false);
            assert!(Instant::now() < deadline, "reload timed out");
            std::thread::sleep(Duration::from_millis(25));
        }
        assert_eq!(metrics.top_n, 5);
        assert_eq!(
            metrics.generic.metrics["builtin/memory/used"]
                .value
                .as_ref()
                .unwrap()
                .number(),
            metrics.mem.used as f64
        );
        let shared = metrics.worker.0.clone();
        drop(metrics);
        wait(|| shared.finished.load(Ordering::Acquire));
    }
}
