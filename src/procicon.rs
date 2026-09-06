//! Icons of processes for the popup lists: the executable's shell icon (what Explorer shows),
//! converted to premultiplied BGRA once per executable and kept in a cache.

use std::collections::HashMap;

use windows::core::PWSTR;
use windows::Win32::Foundation::{CloseHandle, BOOL, HWND};
use windows::Win32::Graphics::Gdi::{
    DeleteObject, GetDC, GetDIBits, ReleaseDC, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS,
};
use windows::Win32::Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES;
use windows::Win32::System::Threading::{IsWow64Process, OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE};
use windows::Win32::UI::Shell::{SHGetFileInfoW, SHFILEINFOW, SHGFI_ICON, SHGFI_LARGEICON, SHGFI_SMALLICON};
use windows::Win32::UI::WindowsAndMessaging::{DestroyIcon, GetIconInfo, ICONINFO};

use crate::util::pcwstr;

/// Premultiplied BGRA pixels, top-down.
#[derive(Clone)]
pub struct Image {
    pub w: i32,
    pub h: i32,
    pub px: Vec<u32>,
}

#[derive(Default)]
pub struct IconCache {
    /// executable path -> icon (None: no icon could be read; remembered so it is not retried)
    by_path: HashMap<String, Option<Image>>,
    /// pid -> executable path, so the handle dance happens once per process
    by_pid: HashMap<u32, Option<String>>,
    /// pid -> runs under WOW64 (a 32-bit process on 64-bit Windows)
    wow64: HashMap<u32, bool>,
    /// pid -> a handle with terminate rights could be opened
    killable: HashMap<u32, bool>,
}

impl IconCache {
    /// The icon of `pid`'s executable, read on first use. `want` is the pixel size the caller
    /// will draw at; the shell's 32 px icon is taken when the small one would be upscaled.
    pub fn get(&mut self, pid: u32, want: i32) -> Option<&Image> {
        let path = self.by_pid.entry(pid).or_insert_with(|| exe_path(pid)).clone()?;
        let key = format!("{path}|{}", if want > 16 { 32 } else { 16 });
        if !self.by_path.contains_key(&key) {
            let img = load_icon(&path, want > 16);
            self.by_path.insert(key.clone(), img);
        }
        self.by_path.get(&key).and_then(|i| i.as_ref())
    }

    /// True for a 32-bit process on 64-bit Windows.
    pub fn is_32bit(&mut self, pid: u32) -> bool {
        *self.wow64.entry(pid).or_insert_with(|| wow64(pid))
    }

    /// True when this user may terminate the process (system and protected processes refuse
    /// a handle with PROCESS_TERMINATE, so no kill button is offered for them).
    pub fn can_kill(&mut self, pid: u32) -> bool {
        *self.killable.entry(pid).or_insert_with(|| unsafe {
            match OpenProcess(PROCESS_TERMINATE, false, pid) {
                Ok(h) => {
                    let _ = CloseHandle(h);
                    true
                }
                Err(_) => false,
            }
        })
    }

    /// Forget processes that are no longer listed, so pids reused later get a fresh lookup.
    pub fn retain_pids(&mut self, alive: &[u32]) {
        self.by_pid.retain(|pid, _| alive.contains(pid));
        self.wow64.retain(|pid, _| alive.contains(pid));
        self.killable.retain(|pid, _| alive.contains(pid));
    }
}

fn exe_path(pid: u32) -> Option<String> {
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len).is_ok();
        let _ = CloseHandle(h);
        if ok && len > 0 {
            Some(String::from_utf16_lossy(&buf[..len as usize]))
        } else {
            None
        }
    }
}

fn wow64(pid: u32) -> bool {
    unsafe {
        let Ok(h) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else { return false };
        let mut w = BOOL(0);
        let ok = IsWow64Process(h, &mut w).is_ok();
        let _ = CloseHandle(h);
        ok && w.as_bool()
    }
}

fn load_icon(path: &str, large: bool) -> Option<Image> {
    let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        let mut info = SHFILEINFOW::default();
        let flags = SHGFI_ICON | if large { SHGFI_LARGEICON } else { SHGFI_SMALLICON };
        let r = SHGetFileInfoW(pcwstr(&wide), FILE_FLAGS_AND_ATTRIBUTES(0), Some(&mut info), std::mem::size_of::<SHFILEINFOW>() as u32, flags);
        // SHFILEINFOW is packed on 32-bit targets: copy the handle out before using it
        let hicon = info.hIcon;
        if r == 0 || hicon.is_invalid() {
            return None;
        }
        let img = icon_to_image(hicon);
        let _ = DestroyIcon(hicon);
        img
    }
}

/// Pixels of an HICON: the colour bitmap's alpha when it has one, else the 1-bit mask.
unsafe fn icon_to_image(hicon: windows::Win32::UI::WindowsAndMessaging::HICON) -> Option<Image> {
    let mut ii = ICONINFO::default();
    GetIconInfo(hicon, &mut ii).ok()?;
    let hdc = GetDC(HWND::default());
    let read = |hbm: windows::Win32::Graphics::Gdi::HBITMAP| -> Option<(i32, i32, Vec<u32>)> {
        if hbm.is_invalid() {
            return None;
        }
        let mut bi = BITMAPINFO { bmiHeader: BITMAPINFOHEADER { biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32, ..Default::default() }, ..Default::default() };
        // first call: dimensions
        if GetDIBits(hdc, hbm, 0, 0, None, &mut bi, DIB_RGB_COLORS) == 0 {
            return None;
        }
        let w = bi.bmiHeader.biWidth;
        let h = bi.bmiHeader.biHeight.abs();
        if w <= 0 || h <= 0 || w > 512 || h > 512 {
            return None;
        }
        bi.bmiHeader.biHeight = -h; // top-down
        bi.bmiHeader.biPlanes = 1;
        bi.bmiHeader.biBitCount = 32;
        bi.bmiHeader.biCompression = BI_RGB.0;
        bi.bmiHeader.biSizeImage = 0;
        let mut px = vec![0u32; (w * h) as usize];
        if GetDIBits(hdc, hbm, 0, h as u32, Some(px.as_mut_ptr() as *mut _), &mut bi, DIB_RGB_COLORS) == 0 {
            return None;
        }
        Some((w, h, px))
    };
    let color = read(ii.hbmColor);
    let mask = read(ii.hbmMask);
    let _ = ReleaseDC(HWND::default(), hdc);
    if !ii.hbmColor.is_invalid() {
        let _ = DeleteObject(ii.hbmColor);
    }
    if !ii.hbmMask.is_invalid() {
        let _ = DeleteObject(ii.hbmMask);
    }
    let (w, h, mut px) = color?;
    let has_alpha = px.iter().any(|p| p >> 24 != 0);
    if has_alpha {
        // straight -> premultiplied
        for p in px.iter_mut() {
            let a = *p >> 24;
            if a == 255 {
                continue;
            }
            let ch = |shift: u32| ((*p >> shift) & 0xFF) * a / 255;
            *p = a << 24 | ch(16) << 16 | ch(8) << 8 | ch(0);
        }
    } else if let Some((mw, mh, mpx)) = mask {
        // the mask's first h rows: 1 = transparent
        if mw == w && mh >= h {
            for (i, p) in px.iter_mut().enumerate() {
                let transparent = mpx[i] & 0xFF_FFFF != 0;
                *p = if transparent { 0 } else { 0xFF00_0000 | (*p & 0xFF_FFFF) };
            }
        } else {
            for p in px.iter_mut() {
                *p |= 0xFF00_0000;
            }
        }
    } else {
        for p in px.iter_mut() {
            *p |= 0xFF00_0000;
        }
    }
    Some(Image { w, h, px })
}
