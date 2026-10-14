//! Network throughput over the physical interfaces (GetIfTable2): every hardware adapter is
//! listed for the popup with its own rate, IPv4 and, for Wi-Fi, the network it is joined to;
//! the widget and the chart sum the interfaces the user has not switched off.

use std::collections::HashMap;
use std::time::Instant;
use windows::core::GUID;
use windows::Win32::Foundation::{ERROR_SUCCESS, HANDLE, HWND, LPARAM, WPARAM};
use windows::Win32::NetworkManagement::IpHelper::*;
use windows::Win32::NetworkManagement::Ndis::{IfOperStatusNotPresent, IfOperStatusUp, MediaConnectStateUnknown, NdisPhysicalMediumBluetooth, NdisPhysicalMediumNative802_11, NdisPhysicalMediumWirelessWan};
use windows::Win32::NetworkManagement::WiFi::{
    wlan_intf_opcode_current_connection, WlanCloseHandle, WlanFreeMemory, WlanOpenHandle, WlanQueryInterface, WLAN_CONNECTION_ATTRIBUTES,
};
use windows::Win32::Networking::WinSock::{AF_INET, SOCKADDR_IN};

struct IfaceSample {
    guid: GUID,
    alias: String,
    description: String,
    kind: &'static str,
    up: bool,
    rx: u64,
    tx: u64,
    mac: String,
    /// bits/s while up
    speed: u64,
}

/// One physical adapter as shown in the popup.
#[derive(Clone, Debug)]
pub struct NetIface {
    pub luid: u64,
    pub guid: GUID,
    /// user-visible name ("Ethernet", "Wi-Fi 2")
    pub alias: String,
    /// hardware name from the driver
    pub description: String,
    /// "Ethernet", "Wi-Fi", "Mobile", "Bluetooth" or ""
    pub kind: &'static str,
    pub up: bool,
    pub rx_bps: f64,
    pub tx_bps: f64,
    pub ip: String,
    /// Wi-Fi: the network currently joined
    pub ssid: String,
    /// Wi-Fi: standard and band of the connection, "802.11ax, 5 GHz"
    pub wifi: String,
    /// physical address "AA:BB:CC:DD:EE:FF"
    pub mac: String,
    /// link speed in bits/s (0 when down or unknown)
    pub speed: u64,
    /// counted in the widget and the chart (Interfaces list in the popup toggles it)
    pub enabled: bool,
}

pub struct Net {
    prev: Option<(HashMap<u64, IfaceSample>, Instant)>,
    pub rx_bps: f64,
    pub tx_bps: f64,
    /// bytes since the app started, enabled interfaces only
    pub total_rx: u64,
    pub total_tx: u64,
    /// (luid, alias) of the enabled interface carrying the most traffic in the last sample
    busiest: Option<(u64, String)>,
    pub iface_name: String,
    pub iface_ip: String,
    /// every hardware interface, connected ones first
    pub ifaces: Vec<NetIface>,
    /// aliases the user switched off
    pub disabled: Vec<String>,
    /// IPv4 per LUID and Wi-Fi per GUID from the last background read, its time, and the read
    /// under way (the WLAN calls take ~100 ms: never on the UI thread)
    addr: (HashMap<u64, String>, HashMap<GUID, WifiInfo>),
    addr_at: Option<Instant>,
    addr_rx: Option<std::sync::mpsc::Receiver<(HashMap<u64, String>, HashMap<GUID, WifiInfo>)>>,
}

impl Net {
    pub fn new(disabled: &[String]) -> Self {
        let mut n = Self {
            prev: None,
            rx_bps: 0.0,
            tx_bps: 0.0,
            total_rx: 0,
            total_tx: 0,
            busiest: None,
            iface_name: String::new(),
            iface_ip: String::new(),
            ifaces: Vec::new(),
            disabled: disabled.to_vec(),
            addr: (HashMap::new(), HashMap::new()),
            addr_at: None,
            addr_rx: None,
        };
        n.update();
        n
    }

    pub fn set_disabled(&mut self, disabled: &[String]) {
        self.disabled = disabled.to_vec();
        for i in &mut self.ifaces {
            i.enabled = !self.disabled.contains(&i.alias);
        }
        if !self.ifaces.iter().any(|i| i.enabled) {
            for i in &mut self.ifaces {
                i.enabled = true;
            }
        }
    }

    fn sample() -> Option<HashMap<u64, IfaceSample>> {
        let mut table: *mut MIB_IF_TABLE2 = std::ptr::null_mut();
        unsafe {
            if GetIfTable2(&mut table) != ERROR_SUCCESS || table.is_null() {
                return None;
            }
            let n = (*table).NumEntries as usize;
            let rows = std::slice::from_raw_parts((*table).Table.as_ptr(), n);
            let mut out = HashMap::new();
            for r in rows {
                if r.Type == IF_TYPE_SOFTWARE_LOOPBACK || r.Type == IF_TYPE_TUNNEL {
                    continue;
                }
                // bit 0 = HardwareInterface: skip virtual adapters (Hyper-V, VPN, WSL bridges)
                if r.InterfaceAndOperStatusFlags._bitfield & 1 == 0 {
                    continue;
                }
                // an unplugged USB adapter (or a disabled one) stays in the table as "not
                // present": it is gone for the user, unlike a NIC without a cable
                if r.OperStatus == IfOperStatusNotPresent {
                    continue;
                }
                // a pulled-out USB adapter keeps a row for a while: down with the media state
                // unknown. A NIC without a cable is down too, but reports "disconnected".
                if r.OperStatus != IfOperStatusUp && r.MediaConnectState == MediaConnectStateUnknown {
                    continue;
                }
                let kind = if r.Type == IF_TYPE_IEEE80211 || r.PhysicalMediumType == NdisPhysicalMediumNative802_11 {
                    "Wi-Fi"
                } else if r.Type == IF_TYPE_WWANPP || r.Type == IF_TYPE_WWANPP2 || r.PhysicalMediumType == NdisPhysicalMediumWirelessWan {
                    "Mobile"
                } else if r.PhysicalMediumType == NdisPhysicalMediumBluetooth {
                    "Bluetooth"
                } else if r.Type == IF_TYPE_ETHERNET_CSMACD {
                    "Ethernet"
                } else {
                    ""
                };
                let alias = String::from_utf16_lossy(&r.Alias).trim_end_matches('\0').to_string();
                let description = String::from_utf16_lossy(&r.Description).trim_end_matches('\0').to_string();
                let len = (r.PhysicalAddressLength as usize).min(r.PhysicalAddress.len());
                let mac = r.PhysicalAddress[..len].iter().map(|b| format!("{b:02X}")).collect::<Vec<_>>().join(":");
                let up = r.OperStatus == IfOperStatusUp;
                let speed = if up { r.TransmitLinkSpeed } else { 0 };
                out.insert(
                    r.InterfaceLuid.Value,
                    IfaceSample { guid: r.InterfaceGuid, alias, description, kind, up, rx: r.InOctets, tx: r.OutOctets, mac, speed },
                );
            }
            FreeMibTable(table as *const _);
            Some(out)
        }
    }

    pub fn update(&mut self) {
        let Some(cur) = Self::sample() else { return };
        let now = Instant::now();
        // per-interface rates, keeping ip/ssid from the previous details pass
        let mut rates: HashMap<u64, (f64, f64, u64, u64)> = HashMap::new();
        if let Some((prev, t)) = &self.prev {
            let dt = now.duration_since(*t).as_secs_f64().max(0.001);
            for (luid, s) in &cur {
                let (prx, ptx) = prev.get(luid).map(|p| (p.rx, p.tx)).unwrap_or((s.rx, s.tx));
                let a = s.rx.saturating_sub(prx);
                let b = s.tx.saturating_sub(ptx);
                rates.insert(*luid, (a as f64 / dt, b as f64 / dt, a, b));
            }
        }
        let old: HashMap<u64, NetIface> = self.ifaces.drain(..).map(|i| (i.luid, i)).collect();
        let mut list: Vec<NetIface> = cur
            .iter()
            .map(|(luid, s)| {
                let (rx_bps, tx_bps, _, _) = rates.get(luid).cloned().unwrap_or_default();
                let o = old.get(luid);
                NetIface {
                    luid: *luid,
                    guid: s.guid,
                    alias: s.alias.clone(),
                    description: s.description.clone(),
                    kind: s.kind,
                    up: s.up,
                    rx_bps,
                    tx_bps,
                    ip: o.map(|o| o.ip.clone()).unwrap_or_default(),
                    ssid: o.map(|o| o.ssid.clone()).unwrap_or_default(),
                    wifi: o.map(|o| o.wifi.clone()).unwrap_or_default(),
                    mac: s.mac.clone(),
                    speed: s.speed,
                    enabled: !self.disabled.contains(&s.alias),
                }
            })
            .collect();
        list.sort_by(|a, b| b.up.cmp(&a.up).then_with(|| a.alias.cmp(&b.alias)));
        // a stale config may switch off everything that exists: then count all of them
        if !list.iter().any(|i| i.enabled) {
            for i in &mut list {
                i.enabled = true;
            }
        }
        // totals over the enabled interfaces
        let mut drx = 0u64;
        let mut dtx = 0u64;
        let mut best: Option<(u64, u64, String)> = None;
        for i in &list {
            if !i.enabled || !i.up {
                continue;
            }
            let (_, _, a, b) = rates.get(&i.luid).cloned().unwrap_or_default();
            drx += a;
            dtx += b;
            if best.as_ref().map(|(_, v, _)| a + b > *v).unwrap_or(true) {
                best = Some((i.luid, a + b, i.alias.clone()));
            }
        }
        if self.prev.is_some() {
            let dt = now.duration_since(self.prev.as_ref().unwrap().1).as_secs_f64().max(0.001);
            self.rx_bps = drx as f64 / dt;
            self.tx_bps = dtx as f64 / dt;
            self.total_rx += drx;
            self.total_tx += dtx;
        }
        if let Some((luid, v, alias)) = best {
            // keep the previous choice while nothing is flowing
            if v > 0 || self.busiest.as_ref().map_or(true, |(l, _)| !list.iter().any(|i| i.luid == *l && i.enabled && i.up)) {
                self.busiest = Some((luid, alias));
            }
        } else {
            self.busiest = None;
        }
        self.ifaces = list;
        self.prev = Some((cur, now));
    }

    /// Resolve IPv4 of every interface, the Wi-Fi network names, and the busiest interface for
    /// the title (slower calls; popup only).
    /// Addresses and Wi-Fi for the popup: read on a worker thread every few seconds, the last
    /// result applied meanwhile.
    pub fn update_details(&mut self) {
        if let Some(rx) = &self.addr_rx {
            match rx.try_recv() {
                Ok(v) => {
                    self.addr = v;
                    self.addr_rx = None;
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => self.addr_rx = None,
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
            }
        }
        if self.addr_rx.is_none() && self.addr_at.map_or(true, |t| t.elapsed().as_secs() >= 3) {
            self.addr_at = Some(Instant::now());
            let (tx, rx) = std::sync::mpsc::channel();
            if std::thread::Builder::new().name("net-addr".into()).spawn(move || drop(tx.send((ipv4_table(), wifi_info())))).is_ok() {
                self.addr_rx = Some(rx);
            }
        }
        let (ips, wifi) = (&self.addr.0, &self.addr.1);
        for i in &mut self.ifaces {
            i.ip = ips.get(&i.luid).cloned().unwrap_or_default();
            let w = if i.kind == "Wi-Fi" { wifi.get(&i.guid).cloned().unwrap_or_default() } else { WifiInfo::default() };
            i.ssid = w.ssid;
            // "802.11ax, 5 GHz"
            i.wifi = [w.standard, w.band].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(", ");
        }
        match &self.busiest {
            Some((luid, alias)) => {
                self.iface_name = alias.clone();
                self.iface_ip = ips.get(luid).cloned().unwrap_or_default();
            }
            None => {
                self.iface_name.clear();
                self.iface_ip.clear();
            }
        }
    }
}

/// luid -> the IPv4 addresses of every adapter, comma separated.
pub fn ipv4_table() -> HashMap<u64, String> {
    let mut out = HashMap::new();
    unsafe {
        let flags = GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST | GAA_FLAG_SKIP_DNS_SERVER;
        let mut size = 16 * 1024u32;
        let mut buf = vec![0u8; size as usize];
        let mut r = GetAdaptersAddresses(AF_INET.0 as u32, flags, None, Some(buf.as_mut_ptr() as *mut _), &mut size);
        if r == 111 {
            buf.resize(size as usize, 0);
            r = GetAdaptersAddresses(AF_INET.0 as u32, flags, None, Some(buf.as_mut_ptr() as *mut _), &mut size);
        }
        if r != ERROR_SUCCESS.0 {
            return out;
        }
        let mut p = buf.as_ptr() as *const IP_ADAPTER_ADDRESSES_LH;
        while !p.is_null() {
            let a = &*p;
            let mut u = a.FirstUnicastAddress;
            while !u.is_null() {
                let sa = (*u).Address.lpSockaddr;
                if !sa.is_null() && (*sa).sa_family == AF_INET {
                    let sin = &*(sa as *const SOCKADDR_IN);
                    let b = sin.sin_addr.S_un.S_un_b;
                    let e: &mut String = out.entry(a.Luid.Value).or_default();
                    if !e.is_empty() {
                        e.push_str(", ");
                    }
                    // with the prefix length: "10.10.24.30/24"
                    e.push_str(&format!("{}.{}.{}.{}/{}", b.s_b1, b.s_b2, b.s_b3, b.s_b4, (*u).OnLinkPrefixLength));
                }
                u = (*u).Next;
            }
            p = a.Next;
        }
    }
    out
}

/// What a Wi-Fi interface is connected to.
#[derive(Clone, Default)]
struct WifiInfo {
    ssid: String,
    /// "802.11ax" etc., from the PHY type of the association; "" when unknown
    standard: String,
    /// "2.4 GHz", "5 GHz", "6 GHz" from the channel centre frequency of the joined BSS
    band: String,
}

/// Interface GUID -> network, standard and band of the Wi-Fi it is connected to (wlanapi,
/// Windows 7+).
fn wifi_info() -> HashMap<GUID, WifiInfo> {
    use windows::Win32::NetworkManagement::WiFi::{dot11_BSS_type_any, WlanGetNetworkBssList, DOT11_PHY_TYPE};
    let mut out = HashMap::new();
    unsafe {
        let mut version = 0u32;
        let mut client = HANDLE::default();
        if WlanOpenHandle(2, None, &mut version, &mut client) != 0 {
            return out;
        }
        let mut list = std::ptr::null_mut();
        if windows::Win32::NetworkManagement::WiFi::WlanEnumInterfaces(client, None, &mut list) == 0 && !list.is_null() {
            let n = (*list).dwNumberOfItems as usize;
            let items = std::slice::from_raw_parts((*list).InterfaceInfo.as_ptr(), n);
            for it in items {
                let mut size = 0u32;
                let mut data: *mut core::ffi::c_void = std::ptr::null_mut();
                if WlanQueryInterface(client, &it.InterfaceGuid, wlan_intf_opcode_current_connection, None, &mut size, &mut data, None) == 0 && !data.is_null() {
                    let attrs = &*(data as *const WLAN_CONNECTION_ATTRIBUTES);
                    let assoc = &attrs.wlanAssociationAttributes;
                    let ssid = &assoc.dot11Ssid;
                    let len = (ssid.uSSIDLength as usize).min(ssid.ucSSID.len());
                    let name = String::from_utf8_lossy(&ssid.ucSSID[..len]).to_string();
                    let bssid = assoc.dot11Bssid;
                    let standard = phy_name(assoc.dot11PhyType);
                    WlanFreeMemory(data);
                    if name.is_empty() {
                        continue;
                    }
                    // the joined BSS tells the channel centre frequency, hence the band
                    let mut band = String::new();
                    let mut bss = std::ptr::null_mut();
                    if WlanGetNetworkBssList(client, &it.InterfaceGuid, None, dot11_BSS_type_any, false, None, &mut bss) == 0 && !bss.is_null() {
                        let m = (*bss).dwNumberOfItems as usize;
                        let entries = std::slice::from_raw_parts((*bss).wlanBssEntries.as_ptr(), m);
                        if let Some(e) = entries.iter().find(|e| e.dot11Bssid == bssid) {
                            band = band_name(e.ulChCenterFrequency);
                        }
                        WlanFreeMemory(bss as *const _);
                    }
                    out.insert(it.InterfaceGuid, WifiInfo { ssid: name, standard, band });
                }
            }
            WlanFreeMemory(list as *const _);
        }
        let _ = WlanCloseHandle(client, None);
    }
    return out;

    fn phy_name(t: DOT11_PHY_TYPE) -> String {
        match t.0 {
            4 => "802.11a",
            5 => "802.11b",
            6 => "802.11g",
            7 => "802.11n",
            8 => "802.11ac",
            9 => "802.11ad",
            10 => "802.11ax",
            11 => "802.11be",
            _ => "",
        }
        .to_string()
    }

    /// centre frequency in kHz -> band
    fn band_name(khz: u32) -> String {
        let mhz = khz / 1000;
        match mhz {
            2400..=2500 => "2.4 GHz",
            5150..=5925 => "5 GHz",
            5926..=7125 => "6 GHz",
            57000..=71000 => "60 GHz",
            _ => "",
        }
        .to_string()
    }
}

/// Posted to the hidden window on every interface change (see `watch`).
pub const WM_APP_NET: u32 = windows::Win32::UI::WindowsAndMessaging::WM_APP + 2;

static WATCH_HWND: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);

/// Subscribe to interface changes: an adapter plugged in or pulled out, a link going up or
/// down. Each change posts WM_APP_NET to `hwnd`, so the list is refreshed at once instead of
/// on the next tick. The first notification fires immediately (a self-check in the log).
pub fn watch(hwnd: HWND) {
    use std::sync::atomic::Ordering;
    use windows::Win32::Networking::WinSock::AF_UNSPEC;
    use windows::Win32::UI::WindowsAndMessaging::PostMessageW;
    unsafe extern "system" fn changed(_ctx: *const core::ffi::c_void, _row: *const MIB_IPINTERFACE_ROW, _kind: MIB_NOTIFICATION_TYPE) {
        let h = WATCH_HWND.load(Ordering::Relaxed);
        if h != 0 {
            let _ = PostMessageW(HWND(h as *mut _), WM_APP_NET, WPARAM(0), LPARAM(0));
        }
    }
    WATCH_HWND.store(hwnd.0 as isize, Ordering::Relaxed);
    let mut handle = HANDLE::default();
    let r = unsafe { NotifyIpInterfaceChange(AF_UNSPEC, Some(changed), None, true, &mut handle) };
    if r != ERROR_SUCCESS {
        crate::util::log(&format!("net: NotifyIpInterfaceChange failed: {}", r.0));
    }
}
