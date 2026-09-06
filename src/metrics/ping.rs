//! Round-trip time to a host, measured only while the network popup is open: the latency and
//! jitter rows. ICMP echo to the configured host first; when that gets no replies, a TCP
//! connect to the same host (ICMP is often filtered), then the local DNS servers and the default
//! gateway from the adapter settings. The primary host is retried every 30 s.

use std::collections::VecDeque;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::HANDLE;
use windows::Win32::NetworkManagement::IpHelper::{IcmpCreateFile, IcmpSendEcho, ICMP_ECHO_REPLY};

/// replies the jitter is averaged over
const WINDOW: usize = 10;
/// probes without a reply before the method / target changes
const GIVE_UP: u32 = 3;
const RETRY_PRIMARY: Duration = Duration::from_secs(30);
const TIMEOUT_MS: u32 = 2000;

struct State {
    active: bool,
    host: String,
    /// what is being measured right now, e.g. "1.1.1.1", "1.1.1.1 · TCP", "10.0.0.1"
    target: String,
    rtts: VecDeque<f32>,
    last: Option<f32>,
    /// consecutive probes without a reply
    lost: u32,
}

pub struct Ping {
    st: Arc<(Mutex<State>, Condvar)>,
}

impl Ping {
    pub fn new(host: &str) -> Self {
        let st = Arc::new((
            Mutex::new(State { active: false, host: host.to_string(), target: String::new(), rtts: VecDeque::new(), last: None, lost: 0 }),
            Condvar::new(),
        ));
        let worker = st.clone();
        let _ = std::thread::Builder::new().name("ping".into()).spawn(move || run(worker));
        Self { st }
    }

    /// Measure while `active`; `host` is the configured target, "" switches the probe off.
    pub fn set(&self, active: bool, host: &str) {
        let (m, cv) = &*self.st;
        let mut s = m.lock().unwrap();
        let on = active && !host.is_empty();
        if s.host != host || (s.active && !on) {
            s.host = host.to_string();
            s.target.clear();
            s.rtts.clear();
            s.last = None;
            s.lost = 0;
        }
        if s.active != on {
            s.active = on;
            cv.notify_all();
        }
    }

    /// (latency of the last reply, mean change between consecutive replies, target in use), ms.
    /// The values are None before the first reply or while nothing answers.
    pub fn read(&self) -> (Option<f32>, Option<f32>, String) {
        let s = self.st.0.lock().unwrap();
        if s.lost >= GIVE_UP {
            return (None, None, s.target.clone());
        }
        let jitter = if s.rtts.len() >= 2 {
            let sum: f32 = s.rtts.iter().zip(s.rtts.iter().skip(1)).map(|(a, b)| (a - b).abs()).sum();
            Some(sum / (s.rtts.len() - 1) as f32)
        } else {
            None
        };
        (s.last, jitter, s.target.clone())
    }
}

struct Target {
    label: String,
    ip: Ipv4Addr,
    /// port for the TCP fallback
    port: u16,
}

/// The configured host, then the IPv4 DNS servers and gateways of the adapters that are up.
fn targets(primary: &str) -> Vec<Target> {
    let mut v: Vec<Target> = Vec::new();
    if let Some(ip) = resolve(primary) {
        v.push(Target { label: primary.to_string(), ip, port: 443 });
    }
    let (dns, gw) = local_servers();
    for ip in dns {
        if !v.iter().any(|t| t.ip == ip) {
            v.push(Target { label: ip.to_string(), ip, port: 53 });
        }
    }
    for ip in gw {
        if !v.iter().any(|t| t.ip == ip) {
            v.push(Target { label: ip.to_string(), ip, port: 80 });
        }
    }
    v
}

fn run(st: Arc<(Mutex<State>, Condvar)>) {
    let (m, cv) = &*st;
    let mut handle: Option<HANDLE> = None;
    let mut host = String::new();
    let mut list: Vec<Target> = Vec::new();
    let mut idx = 0usize;
    let mut tcp = false;
    let mut misses = 0u32;
    let mut primary_tried = Instant::now();
    loop {
        let cur_host = {
            let mut s = m.lock().unwrap();
            while !s.active {
                s = cv.wait(s).unwrap();
            }
            s.host.clone()
        };
        if cur_host != host || list.is_empty() {
            host = cur_host;
            list = targets(&host);
            idx = 0;
            tcp = false;
            misses = 0;
        }
        if handle.is_none() {
            handle = unsafe { IcmpCreateFile() }.ok();
        }
        // while on a fallback, see now and then whether the configured host is back
        if (idx > 0 || tcp) && primary_tried.elapsed() >= RETRY_PRIMARY {
            primary_tried = Instant::now();
            if let (Some(h), Some(t)) = (handle, list.first()) {
                if echo(h, t.ip).is_some() {
                    idx = 0;
                    tcp = false;
                    misses = 0;
                    let mut s = m.lock().unwrap();
                    s.rtts.clear();
                }
            }
        }
        let rtt = match list.get(idx) {
            Some(t) if tcp => tcp_rtt(t),
            Some(t) => handle.and_then(|h| echo(h, t.ip)),
            None => None,
        };
        let label = list.get(idx).map(|t| format!("{}{}", t.label, if tcp { " \u{00B7} TCP" } else { "" })).unwrap_or_default();
        {
            let mut s = m.lock().unwrap();
            if s.active && s.host == host {
                s.target = label;
                match rtt {
                    Some(ms) => {
                        s.last = Some(ms);
                        s.lost = 0;
                        s.rtts.push_back(ms);
                        while s.rtts.len() > WINDOW {
                            s.rtts.pop_front();
                        }
                    }
                    None => s.lost += 1,
                }
            }
        }
        match rtt {
            Some(_) => misses = 0,
            None => {
                misses += 1;
                if misses >= GIVE_UP {
                    // ICMP -> TCP on the same target, then the next target; wrap around with a
                    // fresh list (the network may have changed)
                    misses = 0;
                    if !tcp && !list.is_empty() {
                        tcp = true;
                    } else {
                        tcp = false;
                        idx += 1;
                        if idx >= list.len() {
                            list = targets(&host);
                            idx = 0;
                        }
                    }
                    let mut s = m.lock().unwrap();
                    s.rtts.clear();
                    s.last = None;
                }
            }
        }
        // one probe a second; the wait is interruptible so switching off is immediate
        let s = m.lock().unwrap();
        let _ = cv.wait_timeout(s, Duration::from_secs(1)).unwrap();
    }
}

fn resolve(host: &str) -> Option<Ipv4Addr> {
    format!("{host}:0").to_socket_addrs().ok()?.find_map(|a| match a.ip() {
        IpAddr::V4(v4) => Some(v4),
        _ => None,
    })
}

/// One echo request; the time until the reply in ms, None on timeout or error.
fn echo(h: HANDLE, ip: Ipv4Addr) -> Option<f32> {
    let data = [0x61u8; 32];
    let mut reply = vec![0u8; std::mem::size_of::<ICMP_ECHO_REPLY>() + data.len() + 8];
    let dst = u32::from_ne_bytes(ip.octets());
    let t0 = Instant::now();
    let n = unsafe { IcmpSendEcho(h, dst, data.as_ptr() as *const _, data.len() as u16, None, reply.as_mut_ptr() as *mut _, reply.len() as u32, TIMEOUT_MS) };
    if n == 0 {
        return None;
    }
    let r = unsafe { &*(reply.as_ptr() as *const ICMP_ECHO_REPLY) };
    if r.Status != 0 {
        return None;
    }
    Some(t0.elapsed().as_secs_f32() * 1000.0)
}

/// Time of a TCP handshake; a refused connection still answers and counts, a timeout or an
/// unreachable network does not.
fn tcp_rtt(t: &Target) -> Option<f32> {
    let addr = SocketAddr::from((t.ip, t.port));
    let t0 = Instant::now();
    match TcpStream::connect_timeout(&addr, Duration::from_millis(TIMEOUT_MS as u64)) {
        Ok(_) => Some(t0.elapsed().as_secs_f32() * 1000.0),
        Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => Some(t0.elapsed().as_secs_f32() * 1000.0),
        Err(_) => None,
    }
}

/// (DNS servers, default gateways), IPv4, of the adapters that are up.
fn local_servers() -> (Vec<Ipv4Addr>, Vec<Ipv4Addr>) {
    use windows::Win32::NetworkManagement::IpHelper::{
        GetAdaptersAddresses, GAA_FLAG_INCLUDE_GATEWAYS, GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_MULTICAST, GAA_FLAG_SKIP_UNICAST, IP_ADAPTER_ADDRESSES_LH,
    };
    use windows::Win32::NetworkManagement::Ndis::IfOperStatusUp;
    use windows::Win32::Networking::WinSock::{AF_INET, SOCKADDR, SOCKADDR_IN};
    const IF_TYPE_SOFTWARE_LOOPBACK: u32 = 24;
    let mut dns = Vec::new();
    let mut gw = Vec::new();
    let v4 = |sa: *const SOCKADDR| -> Option<Ipv4Addr> {
        unsafe {
            if sa.is_null() || (*sa).sa_family != AF_INET {
                return None;
            }
            let b = (*(sa as *const SOCKADDR_IN)).sin_addr.S_un.S_un_b;
            let ip = Ipv4Addr::new(b.s_b1, b.s_b2, b.s_b3, b.s_b4);
            (!ip.is_loopback() && !ip.is_unspecified()).then_some(ip)
        }
    };
    unsafe {
        let flags = GAA_FLAG_INCLUDE_GATEWAYS | GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST | GAA_FLAG_SKIP_UNICAST;
        let mut size = 16 * 1024u32;
        let mut buf = vec![0u8; size as usize];
        let mut r = GetAdaptersAddresses(AF_INET.0 as u32, flags, None, Some(buf.as_mut_ptr() as *mut _), &mut size);
        if r == 111 {
            buf.resize(size as usize, 0);
            r = GetAdaptersAddresses(AF_INET.0 as u32, flags, None, Some(buf.as_mut_ptr() as *mut _), &mut size);
        }
        if r != 0 {
            return (dns, gw);
        }
        let mut p = buf.as_ptr() as *const IP_ADAPTER_ADDRESSES_LH;
        while !p.is_null() {
            let a = &*p;
            if a.OperStatus == IfOperStatusUp && a.IfType != IF_TYPE_SOFTWARE_LOOPBACK {
                let mut d = a.FirstDnsServerAddress;
                while !d.is_null() {
                    if let Some(ip) = v4((*d).Address.lpSockaddr) {
                        if !dns.contains(&ip) {
                            dns.push(ip);
                        }
                    }
                    d = (*d).Next;
                }
                let mut g = a.FirstGatewayAddress;
                while !g.is_null() {
                    if let Some(ip) = v4((*g).Address.lpSockaddr) {
                        if !gw.contains(&ip) {
                            gw.push(ip);
                        }
                    }
                    g = (*g).Next;
                }
            }
            p = a.Next;
        }
    }
    (dns, gw)
}
