//! Windows on the desktop, for window mode: the list the phone picks from, where a window
//! is (physical pixels, so the process is per-monitor DPI aware), and bringing it forward.

use windows_sys::Win32::Foundation::{BOOL, CloseHandle, HWND, LPARAM, RECT};
use windows_sys::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS};
use windows_sys::Win32::System::Threading::{OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION};
use windows_sys::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

pub fn dpi_aware() {
    // SAFETY: process-wide setting, called once before any window or display query.
    unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
}

fn hwnd(id: u32) -> HWND {
    id as usize as HWND // window handles fit in 32 bits, by Windows' own interop rule
}

pub struct Window {
    pub id: u32,
    pub title: String,
    pub app: String,
}

/// Visible, titled, top-level application windows, front to back.
pub fn list() -> Vec<Window> {
    unsafe extern "system" fn each(h: HWND, out: LPARAM) -> BOOL {
        let out = &mut *(out as *mut Vec<Window>);
        if IsWindowVisible(h) == 0 || IsIconic(h) != 0 || !GetWindow(h, GW_OWNER).is_null() {
            return 1;
        }
        if GetWindowLongW(h, GWL_EXSTYLE) as u32 & WS_EX_TOOLWINDOW != 0 {
            return 1;
        }
        let mut cloaked = 0u32;
        DwmGetWindowAttribute(h, DWMWA_CLOAKED as u32, &mut cloaked as *mut _ as *mut _, 4);
        let title = title(h);
        if cloaked != 0 || title.is_empty() || rect(h as usize as u32).is_none_or(|r| r.w < 50 || r.h < 50) {
            return 1;
        }
        out.push(Window { id: h as usize as u32, title, app: app(h) });
        1
    }
    let mut out: Vec<Window> = Vec::new();
    // SAFETY: the callback only runs during the call, with a pointer to `out`.
    unsafe { EnumWindows(Some(each), &mut out as *mut _ as LPARAM) };
    out.retain(|w| w.app != "beacon"); // never offer our own pairing popup
    out
}

pub fn info(id: u32) -> Option<Window> {
    let h = hwnd(id);
    // SAFETY: plain query on a handle that may be stale; IsWindow checks it.
    (unsafe { IsWindow(h) } != 0).then(|| Window { id, title: title(h), app: app(h) })
}

fn title(h: HWND) -> String {
    let mut buf = [0u16; 512];
    // SAFETY: buffer and its length.
    let n = unsafe { GetWindowTextW(h, buf.as_mut_ptr(), buf.len() as i32) };
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

fn app(h: HWND) -> String {
    let mut pid = 0u32;
    // SAFETY: plain queries; the process handle is closed below.
    unsafe {
        GetWindowThreadProcessId(h, &mut pid);
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if process.is_null() {
            return String::new();
        }
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(process, 0, buf.as_mut_ptr(), &mut len);
        CloseHandle(process);
        if ok == 0 {
            return String::new();
        }
        let path = String::from_utf16_lossy(&buf[..len as usize]);
        std::path::Path::new(&path).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
    }
}

/// Where a window is on the desktop, without its invisible resize border. `None` when it is
/// gone or minimised.
pub fn rect(id: u32) -> Option<Rect> {
    let h = hwnd(id);
    let mut r = RECT { left: 0, top: 0, right: 0, bottom: 0 };
    // SAFETY: out-pointer of the right size.
    unsafe {
        if IsWindow(h) == 0 || IsIconic(h) != 0 {
            return None;
        }
        if DwmGetWindowAttribute(h, DWMWA_EXTENDED_FRAME_BOUNDS as u32, &mut r as *mut _ as *mut _, std::mem::size_of::<RECT>() as u32) != 0 {
            GetWindowRect(h, &mut r);
        }
    }
    Some(Rect { x: r.left, y: r.top, w: r.right - r.left, h: r.bottom - r.top })
}

pub fn focus(id: u32) {
    let h = hwnd(id);
    // SAFETY: plain window calls. Windows may refuse the foreground change; the capture works either way.
    unsafe {
        ShowWindow(h, SW_RESTORE);
        SetForegroundWindow(h);
    }
}

/// `r` clipped to the display at the origin with `w`×`h` pixels, even sizes. `None` if off it.
pub fn clip(r: Rect, w: i32, h: i32) -> Option<Rect> {
    let (x0, y0) = (r.x.max(0), r.y.max(0));
    let (x1, y1) = ((r.x + r.w).min(w), (r.y + r.h).min(h));
    let (cw, ch) = ((x1 - x0) & !1, (y1 - y0) & !1);
    (cw >= 64 && ch >= 64).then_some(Rect { x: x0, y: y0, w: cw, h: ch })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clip_to_display() {
        let r = |x, y, w, h| Rect { x, y, w, h };
        assert_eq!(clip(r(-10, 5, 501, 300), 1920, 1080), Some(r(0, 5, 490, 300)));
        assert_eq!(clip(r(1900, 0, 400, 400), 1920, 1080), None);
        assert_eq!(clip(r(0, 0, 1920, 1080), 1920, 1080), Some(r(0, 0, 1920, 1080)));
    }

    #[test]
    fn lists_windows() {
        dpi_aware();
        for w in list() {
            assert!(!w.title.is_empty() && rect(w.id).is_some());
        }
    }
}
