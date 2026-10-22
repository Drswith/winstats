use super::*;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc::{self, Receiver, SyncSender, TryRecvError},
    Arc, Mutex,
};
use std::thread;

enum Event {
    Catalog(Vec<Descriptor>),
    Sample(ProviderMessage),
    Offline(String),
}
struct Worker {
    config: ProviderConfig,
    stop: Arc<AtomicBool>,
    subscriptions: Arc<Mutex<Vec<Subscription>>>,
    events: Receiver<Event>,
}
impl Drop for Worker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Only reconcile/poll run on the caller. Process startup, pipe I/O and termination are workers.
#[derive(Default)]
pub struct Providers {
    workers: BTreeMap<String, Worker>,
}

impl Providers {
    pub fn reconcile(
        &mut self,
        configs: &BTreeMap<String, ProviderConfig>,
        wanted: &BTreeMap<String, Vec<Subscription>>,
        base: &Path,
        registry: &mut Registry,
    ) {
        self.workers.retain(|id, w| {
            let keep = configs
                .get(id)
                .map_or(false, |c| c.enabled && c == &w.config);
            if !keep {
                registry.offline(id, "disabled or reconfigured");
            }
            keep
        });
        for (id, config) in configs.iter().take(64) {
            if id == "builtin" {
                continue;
            }
            if !config.enabled {
                registry.offline(id, "disabled");
                continue;
            }
            if !valid_id(id) || id.contains('/') || config.command.is_empty() {
                registry.offline(id, "invalid provider configuration");
                continue;
            }
            let subscriptions = wanted.get(id).cloned().unwrap_or_default();
            if let Some(w) = self.workers.get(id) {
                *w.subscriptions.lock().unwrap() = subscriptions;
                continue;
            }
            let (tx, events) = mpsc::sync_channel(256);
            let stop = Arc::new(AtomicBool::new(false));
            let subs = Arc::new(Mutex::new(subscriptions));
            let (cfg, flag, shared, base) = (
                config.clone(),
                stop.clone(),
                subs.clone(),
                base.to_path_buf(),
            );
            let spawn = thread::Builder::new()
                .name(format!("provider {id}"))
                .spawn(move || supervise(cfg, base, flag, shared, tx));
            if let Err(e) = spawn {
                registry.offline(id, &e.to_string());
                continue;
            }
            self.workers.insert(
                id.clone(),
                Worker {
                    config: config.clone(),
                    stop,
                    subscriptions: subs,
                    events,
                },
            );
        }
    }

    pub fn poll(&mut self, registry: &mut Registry) {
        for (id, w) in &self.workers {
            // Budget work per UI tick; the queue itself applies backpressure.
            for _ in 0..256 {
                match w.events.try_recv() {
                    Ok(Event::Catalog(ds)) => {
                        if let Err(e) = registry.describe(id, ds) {
                            registry.offline(id, &e);
                        }
                    }
                    Ok(Event::Sample(m)) => {
                        if let Err(e) = registry.sample(id, m, Instant::now()) {
                            registry.offline(id, &e);
                        }
                    }
                    Ok(Event::Offline(reason)) => registry.offline(id, &reason),
                    Err(TryRecvError::Disconnected) => {
                        registry.offline(id, "provider supervisor stopped");
                        break;
                    }
                    Err(TryRecvError::Empty) => break,
                }
            }
        }
    }
}

fn emit(tx: &SyncSender<Event>, stop: &AtomicBool, mut event: Event) -> bool {
    loop {
        match tx.try_send(event) {
            Ok(()) => return true,
            Err(mpsc::TrySendError::Disconnected(_)) => return false,
            Err(mpsc::TrySendError::Full(e)) => {
                event = e;
                if stop.load(Ordering::Relaxed) {
                    return false;
                }
                thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

fn supervise(
    config: ProviderConfig,
    base: PathBuf,
    stop: Arc<AtomicBool>,
    subs: Arc<Mutex<Vec<Subscription>>>,
    tx: SyncSender<Event>,
) {
    let mut backoff = 1;
    while !stop.load(Ordering::Relaxed) {
        let started = Instant::now();
        let error = session(&config, &base, &stop, &subs, &tx)
            .err()
            .map(|e| e.to_string())
            .unwrap_or_else(|| "provider exited".into());
        if !emit(&tx, &stop, Event::Offline(error)) {
            return;
        }
        if started.elapsed() > Duration::from_secs(30) {
            backoff = 1;
        }
        let until = Instant::now() + Duration::from_secs(backoff);
        while Instant::now() < until && !stop.load(Ordering::Relaxed) {
            thread::sleep(Duration::from_millis(25));
        }
        backoff = (backoff * 2).min(30);
    }
}

fn invalid(s: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, s)
}

/// A bounded line reader also rejects unterminated frames without growing memory indefinitely.
fn read_line(reader: &mut impl BufRead) -> io::Result<Option<Vec<u8>>> {
    let mut line = Vec::new();
    let n = reader
        .take((MAX_FRAME + 1) as u64)
        .read_until(b'\n', &mut line)?;
    if n == 0 {
        return Ok(None);
    }
    if line.len() > MAX_FRAME || !line.ends_with(b"\n") {
        return Err(invalid("oversized or unterminated provider frame"));
    }
    Ok(Some(line))
}

fn session(
    config: &ProviderConfig,
    base: &Path,
    stop: &AtomicBool,
    subs: &Mutex<Vec<Subscription>>,
    tx: &SyncSender<Event>,
) -> io::Result<()> {
    let cwd = if config.cwd.is_empty() {
        base.to_path_buf()
    } else {
        base.join(&config.cwd)
    };
    let program = Path::new(&config.command);
    let program = if program.is_relative() && program.components().count() > 1 {
        cwd.join(program)
    } else {
        program.to_path_buf()
    };
    let mut command = Command::new(program);
    command
        .args(&config.args)
        .current_dir(&cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }
    let mut child = command.spawn()?;
    // Closing this job terminates the entire process tree, including scripts' child processes.
    let job = match Job::attach(&child) {
        Ok(j) => j,
        Err(e) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(e);
        }
    };
    let stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let diagnostics = Arc::new(Mutex::new(Vec::<u8>::new()));
    let tail = diagnostics.clone();
    let errors = thread::spawn(move || {
        let mut chunk = [0; 1024];
        loop {
            match stderr.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let mut tail = tail.lock().unwrap();
                    tail.extend_from_slice(&chunk[..n]);
                    if tail.len() > 2048 {
                        let excess = tail.len() - 2048;
                        tail.drain(..excess);
                    }
                }
            }
        }
    });
    let mut stdin = child.stdin.take().unwrap();
    let (lines_tx, lines) = mpsc::sync_channel(256);
    let (write_tx, writes) = mpsc::sync_channel(4);
    let reader = thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        loop {
            let line = read_line(&mut reader);
            let done = !matches!(&line, Ok(Some(_)));
            if lines_tx.send(line).is_err() || done {
                break;
            }
        }
    });
    let writer = thread::spawn(move || {
        while let Ok(msg) = writes.recv() {
            if serde_json::to_writer(&mut stdin, &msg).is_err()
                || stdin.write_all(b"\n").is_err()
                || stdin.flush().is_err()
            {
                break;
            }
        }
    });
    let result = (|| {
        write_tx
            .try_send(HostMessage::Hello { v: VERSION })
            .map_err(|_| invalid("provider pipe unavailable"))?;
        let mut hello = false;
        let mut catalog = false;
        let mut validator = Registry::default();
        let mut sent: Option<Vec<Subscription>> = None;
        let mut last_rx = Instant::now();
        let start = Instant::now();
        let mut last_seq: BTreeMap<String, (u64, u64)> = BTreeMap::new();
        loop {
            if stop.load(Ordering::Relaxed) {
                let _ = write_tx.try_send(HostMessage::Stop);
                return Ok(());
            }
            if (!catalog && start.elapsed() > Duration::from_secs(5))
                || (catalog && last_rx.elapsed()
                    > Duration::from_millis(config.heartbeat_ms.clamp(1000, 300000))
                )
            {
                return Err(invalid("provider handshake/heartbeat timed out"));
            }
            let wanted = subs.lock().unwrap().clone();
            if catalog && sent.as_ref() != Some(&wanted) {
                match write_tx.try_send(HostMessage::Subscribe {
                    metrics: wanted.clone(),
                }) {
                    Ok(()) => sent = Some(wanted),
                    Err(mpsc::TrySendError::Full(_)) => {}
                    Err(_) => return Err(invalid("provider stopped reading commands")),
                }
            }
            match lines.recv_timeout(Duration::from_millis(25)) {
                Ok(Ok(Some(line))) => {
                    let msg: ProviderMessage = serde_json::from_slice(&line)
                        .map_err(|e| invalid(&format!("invalid provider JSON: {e}")))?;
                    last_rx = Instant::now();
                    match msg {
                        ProviderMessage::Hello { v } if !hello && v == VERSION => hello = true,
                        ProviderMessage::Describe { metrics } if hello && !catalog => {
                            validator
                                .describe("provider", metrics.clone())
                                .map_err(|e| invalid(&e))?;
                            catalog = true;
                            if !emit(tx, stop, Event::Catalog(metrics)) {
                                return Ok(());
                            }
                        }
                        ProviderMessage::Sample {
                            ref metric,
                            seq,
                            sampled_at_ms,
                            ..
                        } if catalog => {
                            if sent
                                .as_ref()
                                .map_or(true, |set| !set.iter().any(|s| s.metric == *metric))
                            {
                                continue;
                            }
                            if last_seq
                                .get(metric)
                                .map_or(false, |(s, at)| seq <= *s || sampled_at_ms < *at)
                            {
                                continue;
                            }
                            validator
                                .sample("provider", msg.clone(), Instant::now())
                                .map_err(|e| invalid(&e))?;
                            last_seq.insert(metric.clone(), (seq, sampled_at_ms));
                            if !emit(tx, stop, Event::Sample(msg)) {
                                return Ok(());
                            }
                        }
                        ProviderMessage::Heartbeat if hello => {}
                        _ => return Err(invalid("provider protocol order/version mismatch")),
                    }
                }
                Ok(Ok(None)) => return Err(invalid("provider closed stdout")),
                Ok(Err(e)) => return Err(e),
                // Drain stdout before observing process exit: a short-lived provider may
                // have already written its final sample while this thread was descheduled.
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if child.try_wait()?.is_some() {
                        return Err(invalid("provider process exited"));
                    }
                }
                Err(_) => return Err(invalid("provider reader stopped")),
            }
        }
    })();
    // All waits are on this supervisor, never on the UI thread.
    drop(job);
    let _ = child.kill();
    let _ = child.wait();
    drop(write_tx);
    drop(lines);
    let _ = reader.join();
    let _ = writer.join();
    let _ = errors.join();
    result.map_err(|error| {
        let tail = diagnostics.lock().unwrap();
        if tail.is_empty() {
            error
        } else {
            invalid(&format!(
                "{error}: {}",
                String::from_utf8_lossy(&tail).trim()
            ))
        }
    })
}

#[cfg(windows)]
struct Job(windows::Win32::Foundation::HANDLE);
#[cfg(windows)]
impl Job {
    fn attach(child: &std::process::Child) -> io::Result<Self> {
        use std::os::windows::io::AsRawHandle;
        use windows::Win32::{
            Foundation::{CloseHandle, HANDLE},
            System::JobObjects::*,
        };
        unsafe {
            let handle = CreateJobObjectW(None, None).map_err(io::Error::other)?;
            let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if let Err(e) = SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                &info as *const _ as _,
                std::mem::size_of_val(&info) as u32,
            )
            .and_then(|_| AssignProcessToJobObject(handle, HANDLE(child.as_raw_handle())))
            {
                let _ = CloseHandle(handle);
                return Err(io::Error::other(e));
            }
            Ok(Self(handle))
        }
    }
}
#[cfg(windows)]
impl Drop for Job {
    fn drop(&mut self) {
        unsafe {
            let _ = windows::Win32::Foundation::CloseHandle(self.0);
        }
    }
}
#[cfg(not(windows))]
struct Job;
#[cfg(not(windows))]
impl Job {
    fn attach(_: &std::process::Child) -> io::Result<Self> {
        Ok(Self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn framing_is_bounded_and_requires_newlines() {
        assert!(read_line(&mut io::Cursor::new(vec![b'x'; MAX_FRAME + 1])).is_err());
        assert!(read_line(&mut io::Cursor::new(b"{}".to_vec())).is_err());
        assert_eq!(
            read_line(&mut io::Cursor::new(b"{}\n".to_vec())).unwrap(),
            Some(b"{}\n".to_vec())
        );
    }
}
