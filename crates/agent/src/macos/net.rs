//! What each BSD interface is: on a Mac "en0" may be Wi-Fi or Ethernet, the name does not say.

use super::{string, CFArrayGetCount, CFArrayGetValueAtIndex, CFTypeRef, Cf};

#[link(name = "SystemConfiguration", kind = "framework")]
extern "C" {
    fn SCNetworkInterfaceCopyAll() -> CFTypeRef;
    fn SCNetworkInterfaceGetBSDName(i: CFTypeRef) -> CFTypeRef;
    fn SCNetworkInterfaceGetInterfaceType(i: CFTypeRef) -> CFTypeRef;
    fn SCNetworkInterfaceGetLocalizedDisplayName(i: CFTypeRef) -> CFTypeRef;
}

/// (BSD name, kind as in `IfaceInfo::kind`, what System Settings calls it: "Wi-Fi",
/// "Thunderbolt Ethernet Slot 0") of every hardware interface.
pub fn kinds() -> Vec<(String, &'static str, String)> {
    let mut out = Vec::new();
    let Some(all) = Cf::new(unsafe { SCNetworkInterfaceCopyAll() }) else { return out };
    for i in 0..unsafe { CFArrayGetCount(all.0) } {
        let iface = unsafe { CFArrayGetValueAtIndex(all.0, i) };
        let (Some(name), Some(kind)) = (string(unsafe { SCNetworkInterfaceGetBSDName(iface) }), string(unsafe { SCNetworkInterfaceGetInterfaceType(iface) })) else { continue };
        let kind = match kind.as_str() {
            "IEEE80211" => "Wi-Fi",
            "Ethernet" => "Ethernet",
            "Bluetooth" => "Bluetooth",
            "WWAN" => "Mobile",
            _ => "",
        };
        let display = string(unsafe { SCNetworkInterfaceGetLocalizedDisplayName(iface) }).unwrap_or_default();
        out.push((name, kind, display));
    }
    out
}
