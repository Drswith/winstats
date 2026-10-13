//! Blurred translucent window background drawn by DWM: the Windows 11 system backdrop
//! (the acrylic of the system flyouts, with their shadow and rounded corners), or on Windows 10
//! the undocumented accent policy. Windows without either keep a self-drawn layered window.

use windows::Win32::Foundation::{BOOL, HWND};
use windows::Win32::Graphics::Dwm::*;
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows::Win32::UI::Controls::MARGINS;
use windows::core::{s, w};

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Mode {
    /// per-pixel alpha through UpdateLayeredWindow; the app draws its own card and shadow
    Layered,
    /// Windows 10 1803+: SetWindowCompositionAttribute with ACCENT_ENABLE_ACRYLICBLURBEHIND
    Accent,
    /// Windows 11 22H2+: DWMWA_SYSTEMBACKDROP_TYPE = transient window (flyout acrylic)
    System,
}

/// Popup background opacity picked in the menu (config `popup_opacity`).
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Opacity {
    /// thinner than the system material: the accent policy with a light tint, Windows 11 too
    More,
    /// the system material as is
    System,
    /// the system material under a half-opaque fill
    Less,
    /// nearly opaque fill
    Solid,
}

impl Opacity {
    pub fn from_config(s: &str) -> Self {
        match s {
            "more" => Opacity::More,
            "less" => Opacity::Less,
            "solid" => Opacity::Solid,
            _ => Opacity::System,
        }
    }

    /// Alpha of the theme fill painted over a DWM system backdrop.
    pub fn overlay(self) -> f32 {
        match self {
            Opacity::More | Opacity::System => 0.0,
            Opacity::Less => 0.5,
            Opacity::Solid => 0.88,
        }
    }

    /// Alpha of the self-drawn card of a layered popup.
    pub fn card_alpha(self) -> u8 {
        match self {
            Opacity::More => 215,
            Opacity::System => 244,
            Opacity::Less => 250,
            Opacity::Solid => 255,
        }
    }

    /// Alpha of the accent policy tint.
    fn tint_alpha(self) -> u32 {
        match self {
            Opacity::More => 0x66,
            Opacity::System => 0xCC,
            Opacity::Less => 0xE6,
            Opacity::Solid => 0xF8,
        }
    }
}

/// The mode for `opacity` on a Windows whose best is `native`: "more transparent" swaps the
/// fixed Windows 11 material for the accent policy, whose tint can be thinned.
pub fn mode_for(native: Mode, opacity: Opacity) -> Mode {
    if native == Mode::System && opacity == Opacity::More && set_accent_fn().is_some() {
        Mode::Accent
    } else {
        native
    }
}

/// What this Windows supports; WINSTATS_BACKDROP=layered|accent|system overrides it.
pub fn mode() -> Mode {
    match std::env::var("WINSTATS_BACKDROP").as_deref() {
        Ok("layered") => return Mode::Layered,
        Ok("accent") => return Mode::Accent,
        Ok("system") => return Mode::System,
        _ => {}
    }
    let build = crate::util::windows_build();
    if build >= 22621 {
        Mode::System
    } else if build >= 17134 && set_accent_fn().is_some() {
        Mode::Accent
    } else {
        Mode::Layered
    }
}

/// The framed window would get the heavy shadow of an application window from DWM; the popup
/// draws its own soft one (a layered window underneath, see popup.rs), like the system flyouts.
unsafe fn no_dwm_shadow(hwnd: HWND) {
    let policy = DWMNCRP_DISABLED;
    let _ = unsafe { DwmSetWindowAttribute(hwnd, DWMWA_NCRENDERING_POLICY, &policy as *const _ as _, 4) };
}

/// Turn the backdrop on for `hwnd` (a non-layered popup) in the given theme; false when DWM
/// refused, and the caller falls back to the layered window.
/// A window that has had the system backdrop does not take the accent policy any more (it stays
/// an opaque fill), so a switch between the two needs a new window.
pub fn apply(hwnd: HWND, mode: Mode, dark: bool, opacity: Opacity) -> bool {
    unsafe {
        let dark_attr = BOOL::from(dark);
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE, &dark_attr as *const _ as _, 4);
        // the whole client area is "frame", so its alpha lets the backdrop through
        let m = MARGINS { cxLeftWidth: -1, cxRightWidth: -1, cyTopHeight: -1, cyBottomHeight: -1 };
        // Windows 11 rounds the window like the flyouts; refused (and harmless) on Windows 10
        let corner = DWMWCP_ROUND;
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_WINDOW_CORNER_PREFERENCE, &corner as *const _ as _, 4);
        match mode {
            Mode::Layered => false,
            Mode::System => {
                let backdrop = DWMSBT_TRANSIENTWINDOW;
                let ok = DwmExtendFrameIntoClientArea(hwnd, &m).is_ok()
                    && DwmSetWindowAttribute(hwnd, DWMWA_SYSTEMBACKDROP_TYPE, &backdrop as *const _ as _, 4).is_ok();
                no_dwm_shadow(hwnd);
                ok
            }
            Mode::Accent => {
                // tint as AABBGGRR over the blur, close to the flyouts
                let color: u32 = if dark { 0x20_2020 } else { 0xF3_F3F3 };
                no_dwm_shadow(hwnd);
                set_accent(hwnd, ACCENT_ENABLE_ACRYLICBLURBEHIND, opacity.tint_alpha() << 24 | color)
            }
        }
    }
}

fn set_accent(hwnd: HWND, state: u32, gradient: u32) -> bool {
    let Some(set) = set_accent_fn() else { return false };
    let mut accent = AccentPolicy { state, flags: 0, gradient, animation: 0 };
    let mut data = WinCompAttrData { attrib: WCA_ACCENT_POLICY, data: &mut accent as *mut _ as _, size: std::mem::size_of::<AccentPolicy>() as u32 };
    unsafe { set(hwnd, &mut data).as_bool() }
}

const WCA_ACCENT_POLICY: u32 = 19;
const ACCENT_ENABLE_ACRYLICBLURBEHIND: u32 = 4;

#[repr(C)]
struct AccentPolicy {
    state: u32,
    flags: u32,
    gradient: u32,
    animation: u32,
}

#[repr(C)]
struct WinCompAttrData {
    attrib: u32,
    data: *mut core::ffi::c_void,
    size: u32,
}

type SetWindowCompositionAttribute = unsafe extern "system" fn(HWND, *mut WinCompAttrData) -> BOOL;

/// user32!SetWindowCompositionAttribute: exported but undocumented.
fn set_accent_fn() -> Option<SetWindowCompositionAttribute> {
    unsafe {
        let user32 = GetModuleHandleW(w!("user32.dll")).ok()?;
        let f = GetProcAddress(user32, s!("SetWindowCompositionAttribute"))?;
        Some(std::mem::transmute::<unsafe extern "system" fn() -> isize, SetWindowCompositionAttribute>(f))
    }
}
