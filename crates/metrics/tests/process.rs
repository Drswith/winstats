#![cfg(windows)]
use std::{
    collections::BTreeMap,
    path::Path,
    thread,
    time::{Duration, Instant},
};
use winstats_metrics::*;

fn start(
    mode: &str,
) -> (
    Providers,
    Registry,
    BTreeMap<String, ProviderConfig>,
    BTreeMap<String, Vec<Subscription>>,
) {
    let cfg = ProviderConfig {
        command: "powershell.exe".into(),
        args: vec![
            "-NoProfile".into(),
            "-NonInteractive".into(),
            "-ExecutionPolicy".into(),
            "RemoteSigned".into(),
            "-File".into(),
            format!("{}/tests/fixtures/provider.ps1", env!("CARGO_MANIFEST_DIR")),
            "-Mode".into(),
            mode.into(),
        ],
        heartbeat_ms: 1500,
        ..Default::default()
    };
    let configs = BTreeMap::from([("test".into(), cfg)]);
    let wanted = BTreeMap::from([(
        "test".into(),
        ["fast", "slow", "pid"]
            .iter()
            .map(|id| Subscription {
                metric: (*id).into(),
                interval_ms: 250,
            })
            .collect(),
    )]);
    let mut manager = Providers::default();
    let mut registry = Registry::default();
    manager.reconcile(
        &configs,
        &wanted,
        Path::new(env!("CARGO_MANIFEST_DIR")),
        &mut registry,
    );
    (manager, registry, configs, wanted)
}

fn until(manager: &mut Providers, registry: &mut Registry, predicate: impl Fn(&Registry) -> bool) {
    // Parallel PowerShell cold starts can consume the five-second handshake window;
    // leave room for a retry and a second session in the restart case.
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        manager.poll(registry);
        if predicate(registry) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out: sources={:?}, metrics={:?}",
            registry.sources,
            registry.metrics
        );
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn slow_metric_does_not_block_fast_metric_or_poll() {
    let (mut manager, mut registry, _, _) = start("async");
    until(&mut manager, &mut registry, |r| {
        r.metrics
            .get("test/fast")
            .map_or(false, |m| m.value.is_some())
    });
    assert!(registry.metrics["test/slow"].value.is_none());
    let start = Instant::now();
    for _ in 0..100 {
        manager.poll(&mut registry);
    }
    assert!(start.elapsed() < Duration::from_millis(200));
    until(&mut manager, &mut registry, |r| {
        r.metrics["test/slow"].value.is_some()
    });
}

#[test]
fn invalid_output_and_exit_are_reported_without_blocking() {
    for mode in ["bad", "oversize", "crash", "silent"] {
        let (mut manager, mut registry, _, _) = start(mode);
        until(&mut manager, &mut registry, |r| {
            r.sources.get("test").map_or(false, |s| s != "online")
        });
        let reason = &registry.sources["test"];
        assert!(
            reason.contains("JSON")
                || reason.contains("frame")
                || reason.contains("exit")
                || reason.contains("closed stdout")
                || reason.contains("timed out"),
            "{mode}: {reason}"
        );
    }
}

#[test]
fn restart_accepts_a_new_session_sequence_without_losing_history() {
    let (mut manager, mut registry, _, _) = start("restart");
    until(&mut manager, &mut registry, |r| {
        r.metrics.get("test/fast").map_or(false, |m| {
            m.history.iter().filter(|p| p.value.is_some()).count() >= 2
        })
    });
    assert_eq!(registry.metrics["test/fast"].text(), "42.0");
    assert!(registry.metrics["test/fast"]
        .history
        .iter()
        .any(|p| p.value.is_none()));
}

#[test]
fn disabling_a_provider_terminates_its_child_process_tree() {
    use windows::Win32::{
        Foundation::{CloseHandle, WAIT_OBJECT_0},
        System::Threading::{OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE},
    };
    let (mut manager, mut registry, mut configs, wanted) = start("tree");
    until(&mut manager, &mut registry, |r| {
        r.metrics
            .get("test/pid")
            .map_or(false, |m| m.value.is_some())
    });
    let pid = registry.metrics["test/pid"]
        .value
        .as_ref()
        .unwrap()
        .number() as u32;
    let process = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid).unwrap() };
    configs.get_mut("test").unwrap().enabled = false;
    manager.reconcile(
        &configs,
        &wanted,
        Path::new(env!("CARGO_MANIFEST_DIR")),
        &mut registry,
    );
    assert_eq!(
        registry.metrics["test/pid"].status(Instant::now()),
        "offline"
    );
    unsafe {
        assert_eq!(WaitForSingleObject(process, 5000), WAIT_OBJECT_0);
        let _ = CloseHandle(process);
    }
}

#[test]
fn real_http_provider_handles_fast_response_and_concurrent_timeout() {
    use std::{
        io::{Read, Write},
        net::TcpListener,
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        },
    };
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    let server = thread::spawn(move || {
        while !flag.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    thread::spawn(move || {
                        stream
                            .set_read_timeout(Some(Duration::from_secs(2)))
                            .unwrap();
                        let mut bytes = [0; 2048];
                        let n = stream.read(&mut bytes).unwrap_or(0);
                        if String::from_utf8_lossy(&bytes[..n]).contains("/slow") {
                            thread::sleep(Duration::from_millis(900));
                        }
                        let _ = stream.write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nOK",
                        );
                    });
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(10))
                }
                Err(_) => break,
            }
        }
    });
    struct Cleanup {
        stop: Arc<AtomicBool>,
        file: std::path::PathBuf,
    }
    impl Drop for Cleanup {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            let _ = std::fs::remove_file(&self.file);
        }
    }
    let file =
        std::env::temp_dir().join(format!("winstats-http-targets-{}.json", std::process::id()));
    let cleanup = Cleanup {
        stop,
        file: file.clone(),
    };
    std::fs::write(&file, format!(r#"[{{"id":"slow","name":"Slow","url":"http://{address}/slow"}},{{"id":"fast","name":"Fast","url":"http://{address}/fast"}}]"#)).unwrap();
    let cfg = ProviderConfig {
        command: "powershell.exe".into(),
        args: vec![
            "-NoProfile".into(),
            "-NonInteractive".into(),
            "-ExecutionPolicy".into(),
            "RemoteSigned".into(),
            "-File".into(),
            format!(
                "{}/../../examples/providers/http.ps1",
                env!("CARGO_MANIFEST_DIR")
            ),
            "-TargetsFile".into(),
            file.to_string_lossy().into_owned(),
            "-TimeoutMs".into(),
            "500".into(),
        ],
        ..Default::default()
    };
    let configs = BTreeMap::from([("http".into(), cfg)]);
    let mut wanted = BTreeMap::from([(
        "http".into(),
        ["fast/latency", "slow/latency"]
            .iter()
            .map(|id| Subscription {
                metric: (*id).into(),
                interval_ms: 250,
            })
            .collect(),
    )]);
    let (mut manager, mut registry) = (Providers::default(), Registry::default());
    manager.reconcile(
        &configs,
        &wanted,
        Path::new(env!("CARGO_MANIFEST_DIR")),
        &mut registry,
    );
    until(&mut manager, &mut registry, |r| {
        r.metrics
            .get("http/fast/latency")
            .map_or(false, |m| m.value.is_some())
    });
    assert!(registry.metrics["http/slow/latency"].value.is_none());
    until(&mut manager, &mut registry, |r| {
        r.metrics["http/slow/latency"].error.is_some()
    });
    assert_eq!(
        registry.metrics["http/fast/latency"].status(Instant::now()),
        "ok"
    );
    assert_eq!(
        registry.metrics["http/slow/latency"].status(Instant::now()),
        "error"
    );
    wanted.insert("http".into(), Vec::new());
    manager.reconcile(
        &configs,
        &wanted,
        Path::new(env!("CARGO_MANIFEST_DIR")),
        &mut registry,
    );
    thread::sleep(Duration::from_millis(150));
    manager.poll(&mut registry);
    let history_size = registry.metrics["http/fast/latency"].history.len();
    thread::sleep(Duration::from_millis(600));
    manager.poll(&mut registry);
    assert_eq!(
        registry.metrics["http/fast/latency"].history.len(),
        history_size
    );
    wanted.insert(
        "http".into(),
        vec![Subscription {
            metric: "fast/latency".into(),
            interval_ms: 250,
        }],
    );
    manager.reconcile(
        &configs,
        &wanted,
        Path::new(env!("CARGO_MANIFEST_DIR")),
        &mut registry,
    );
    until(&mut manager, &mut registry, |r| {
        r.metrics["http/fast/latency"].history.len() > history_size
    });
    drop(manager);
    drop(cleanup);
    server.join().unwrap();
}
