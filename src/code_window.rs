//! The pairing-code window, the Windows counterpart of Beacon's pairing panel. Laid out like a
//! Windows 11 dialog: the app icon, a title, the code in a well, and a footer with an accent
//! button, in the system's light or dark theme. Real controls carry the text, so Narrator reads it.

use crate::wide;
use std::ffi::c_void;
use std::sync::atomic::{AtomicPtr, Ordering};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Dwm::*;
use windows_sys::Win32::System::Diagnostics::Debug::MessageBeep;
use windows_sys::Win32::System::SystemServices::*;
use windows_sys::Win32::UI::Controls::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Registry::*;
use windows_sys::Win32::UI::HiDpi::*;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::SetFocus;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

static OPEN: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());

const CLASS: &str = "BeaconPairing";
const ID_TITLE: i32 = 10;
const ID_BODY: i32 = 11;
const ID_CODE: i32 = 12;

// Layout in 96-dpi pixels.
const WIDTH: i32 = 440;
const FOOTER_TOP: i32 = 208;
const HEIGHT: i32 = 288;
const WELL: (i32, i32, i32, i32) = (88, 112, 328, 72);

struct Theme {
    content: u32,
    footer: u32,
    divider: u32,
    text: u32,
    secondary: u32,
    well: u32,
    well_edge: u32,
    accent: u32,
    accent_pressed: u32,
    on_accent: u32,
}

/// 0xRRGGBB to a GDI COLORREF.
const fn rgb(hex: u32) -> u32 {
    ((hex & 0xFF) << 16) | (hex & 0xFF00) | (hex >> 16)
}

// Windows 11's ContentDialog colors, with the default accent.
const LIGHT: Theme = Theme {
    content: rgb(0xFFFFFF),
    footer: rgb(0xF3F3F3),
    divider: rgb(0xE5E5E5),
    text: rgb(0x1A1A1A),
    secondary: rgb(0x5D5D5D),
    well: rgb(0xF9F9F9),
    well_edge: rgb(0xE0E0E0),
    accent: rgb(0x005FB8),
    accent_pressed: rgb(0x3D7FC4),
    on_accent: rgb(0xFFFFFF),
};
const DARK: Theme = Theme {
    content: rgb(0x2B2B2B),
    footer: rgb(0x202020),
    divider: rgb(0x1D1D1D),
    text: rgb(0xFFFFFF),
    secondary: rgb(0xC5C5C5),
    well: rgb(0x1F1F1F),
    well_edge: rgb(0x3A3A3A),
    accent: rgb(0x60CDFF),
    accent_pressed: rgb(0x48A9D6),
    on_accent: rgb(0x000000),
};

struct Ui {
    theme: &'static Theme,
    scale: f32,
    fonts: [HFONT; 4], // title, body, code, button
    content: HBRUSH,
    well: HBRUSH,
    icon: HICON,
}

impl Ui {
    fn px(&self, v: i32) -> i32 {
        (v as f32 * self.scale).round() as i32
    }
}

/// Shows the code for `device`, replacing any window already open. Returns at once.
pub fn show(device: &str, code: &str) {
    close();
    let title = format!("Pair with {device}");
    let code = format!("{} {}", &code[..3], &code[3..]);
    std::thread::spawn(move || {
        // SAFETY: the window, its controls and GDI objects live on this thread and are freed in WM_DESTROY.
        unsafe { run(&title, &code) }
    });
}

/// Closes the window if it is open (pairing finished, failed or was replaced).
pub fn close() {
    let hwnd = OPEN.load(Ordering::Acquire);
    if !hwnd.is_null() {
        // SAFETY: posting to a stale handle fails harmlessly.
        unsafe { PostMessageW(hwnd, WM_CLOSE, 0, 0) };
    }
}

unsafe fn run(title: &str, code: &str) {
    let instance = GetModuleHandleW(std::ptr::null());
    let app_icon = std::ptr::without_provenance::<u16>(1); // beacon.rc
    let class = wide(CLASS);
    let wc = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(window_proc),
        hInstance: instance,
        hIcon: LoadIconW(instance, app_icon),
        hCursor: LoadCursorW(std::ptr::null_mut(), IDC_ARROW),
        lpszClassName: class.as_ptr(),
        ..std::mem::zeroed()
    };
    RegisterClassExW(&wc); // fails harmlessly when already registered

    let monitor = MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY);
    let (mut dpi_x, mut dpi_y) = (96u32, 96u32);
    GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y);
    let mut ui = Box::new(Ui {
        theme: if light_theme() { &LIGHT } else { &DARK },
        scale: dpi_x as f32 / 96.0,
        fonts: [std::ptr::null_mut(); 4],
        content: std::ptr::null_mut(),
        well: std::ptr::null_mut(),
        icon: std::ptr::null_mut(),
    });
    ui.fonts = [
        font(ui.px(20), "Segoe UI Variable Display Semib", "Segoe UI Semibold"),
        font(ui.px(14), "Segoe UI Variable Text", "Segoe UI"),
        font(ui.px(40), "Segoe UI Variable Display Semib", "Segoe UI Semibold"),
        font(ui.px(14), "Segoe UI Variable Text", "Segoe UI"),
    ];
    ui.content = CreateSolidBrush(ui.theme.content);
    ui.well = CreateSolidBrush(ui.theme.well);
    ui.icon = LoadImageW(instance, app_icon, IMAGE_ICON, ui.px(48), ui.px(48), LR_DEFAULTCOLOR) as HICON;

    // Client size to window size, centred on the primary display's work area.
    let style = WS_POPUP | WS_CAPTION | WS_SYSMENU;
    let ex_style = WS_EX_TOPMOST | WS_EX_DLGMODALFRAME;
    let mut frame = RECT { left: 0, top: 0, right: ui.px(WIDTH), bottom: ui.px(HEIGHT) };
    AdjustWindowRectExForDpi(&mut frame, style, 0, ex_style, dpi_x);
    let mut info: MONITORINFO = std::mem::zeroed();
    info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
    GetMonitorInfoW(monitor, &mut info);
    let work = info.rcWork;
    let (w, h) = (frame.right - frame.left, frame.bottom - frame.top);
    let x = work.left + (work.right - work.left - w) / 2;
    let y = work.top + (work.bottom - work.top - h) / 2;

    let hwnd = CreateWindowExW(ex_style, class.as_ptr(), wide("Beacon").as_ptr(), style, x, y, w, h, std::ptr::null_mut(), std::ptr::null_mut(), instance, std::ptr::null());
    if hwnd.is_null() {
        log!("Could not open the pairing window");
        return;
    }
    let dark: i32 = std::ptr::eq(ui.theme, &DARK) as i32;
    DwmSetWindowAttribute(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE as u32, &dark as *const i32 as *const c_void, 4);

    let child = |class: &str, text: &str, style: u32, id: i32, (x, y, w, h): (i32, i32, i32, i32), font: HFONT| {
        let c = CreateWindowExW(0, wide(class).as_ptr(), wide(text).as_ptr(), WS_CHILD | WS_VISIBLE | style, ui.px(x), ui.px(y), ui.px(w), ui.px(h), hwnd, id as usize as HMENU, instance, std::ptr::null());
        SendMessageW(c, WM_SETFONT, font as WPARAM, 0);
        c
    };
    // The phone names itself, so its name is shown as plain text: no & mnemonics, cut with an ellipsis.
    child("STATIC", title, SS_NOPREFIX | SS_ENDELLIPSIS, ID_TITLE, (88, 24, 328, 28), ui.fonts[0]);
    child("STATIC", "Type this code in Beam to let it see and control this PC. It expires in 5 minutes.", SS_NOPREFIX, ID_BODY, (88, 56, 328, 40), ui.fonts[1]);
    let (wx, wy, ww, wh) = WELL;
    child("STATIC", code, SS_CENTER | SS_CENTERIMAGE, ID_CODE, (wx + 1, wy + 1, ww - 2, wh - 2), ui.fonts[2]);
    // 3 px on each side of the button is room for its keyboard focus ring.
    let button = child("BUTTON", "Done", WS_TABSTOP | BS_OWNERDRAW as u32, IDOK, (WIDTH - 24 - 120 - 3, FOOTER_TOP + 24 - 3, 126, 38), ui.fonts[3]);

    SetWindowLongPtrW(hwnd, GWLP_USERDATA, Box::into_raw(ui) as isize);
    OPEN.store(hwnd, Ordering::Release);
    ShowWindow(hwnd, SW_SHOW);
    SetForegroundWindow(hwnd);
    SetFocus(button);
    // Like Windows' own dialogs: the focus ring shows once the keyboard is used.
    SendMessageW(hwnd, WM_CHANGEUISTATE, (UIS_SET | (UISF_HIDEFOCUS << 16)) as WPARAM, 0);
    MessageBeep(MB_ICONINFORMATION);

    let mut msg: MSG = std::mem::zeroed();
    while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
        // Enter and Space press Done, Esc closes.
        if IsDialogMessageW(hwnd, &msg) == 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

unsafe extern "system" fn window_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let ui = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Ui;
    if ui.is_null() {
        return DefWindowProcW(hwnd, msg, wparam, lparam);
    }
    let ui = &*ui;
    let t = ui.theme;
    match msg {
        WM_COMMAND if matches!((wparam & 0xFFFF) as i32, IDOK | IDCANCEL) => {
            DestroyWindow(hwnd);
            0
        }
        WM_ERASEBKGND => 1,
        WM_PAINT => {
            let mut ps: PAINTSTRUCT = std::mem::zeroed();
            let dc = BeginPaint(hwnd, &mut ps);
            let (w, footer, h) = (ui.px(WIDTH), ui.px(FOOTER_TOP), ui.px(HEIGHT));
            FillRect(dc, &RECT { left: 0, top: 0, right: w, bottom: footer }, ui.content);
            fill(dc, RECT { left: 0, top: footer, right: w, bottom: h }, t.footer);
            fill(dc, RECT { left: 0, top: footer, right: w, bottom: footer + ui.px(1).max(1) }, t.divider);
            let (x, y, ww, wh) = WELL;
            rounded(dc, RECT { left: ui.px(x), top: ui.px(y), right: ui.px(x + ww), bottom: ui.px(y + wh) }, ui.px(8), t.well, t.well_edge);
            DrawIconEx(dc, ui.px(24), ui.px(24), ui.icon, ui.px(48), ui.px(48), 0, std::ptr::null_mut(), DI_NORMAL);
            EndPaint(hwnd, &ps);
            0
        }
        WM_CTLCOLORSTATIC => {
            let dc = wparam as HDC;
            SetBkMode(dc, TRANSPARENT as i32);
            match GetDlgCtrlID(lparam as HWND) {
                ID_BODY => {
                    SetTextColor(dc, t.secondary);
                    ui.content as LRESULT
                }
                ID_CODE => {
                    SetTextColor(dc, t.text);
                    ui.well as LRESULT
                }
                _ => {
                    SetTextColor(dc, t.text);
                    ui.content as LRESULT
                }
            }
        }
        WM_DRAWITEM => {
            let item = &*(lparam as *const DRAWITEMSTRUCT);
            let mut r = item.rcItem;
            fill(item.hDC, r, t.footer);
            let focused = item.itemState & ODS_FOCUS != 0 && item.itemState & ODS_NOFOCUSRECT == 0;
            if focused {
                rounded(item.hDC, r, ui.px(7), t.footer, t.text);
            }
            let inset = ui.px(3);
            r = RECT { left: r.left + inset, top: r.top + inset, right: r.right - inset, bottom: r.bottom - inset };
            let face = if item.itemState & ODS_SELECTED != 0 { t.accent_pressed } else { t.accent };
            rounded(item.hDC, r, ui.px(4), face, face);
            SetBkMode(item.hDC, TRANSPARENT as i32);
            SetTextColor(item.hDC, t.on_accent);
            let old = SelectObject(item.hDC, ui.fonts[3]);
            let mut label = wide("Done");
            DrawTextW(item.hDC, label.as_mut_ptr(), -1, &mut r, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
            SelectObject(item.hDC, old);
            1
        }
        WM_DESTROY => {
            let _ = OPEN.compare_exchange(hwnd, std::ptr::null_mut(), Ordering::AcqRel, Ordering::Acquire);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            let ui = Box::from_raw(ui as *const Ui as *mut Ui);
            for f in ui.fonts {
                DeleteObject(f);
            }
            DeleteObject(ui.content);
            DeleteObject(ui.well);
            DestroyIcon(ui.icon);
            PostQuitMessage(0);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

unsafe fn fill(dc: HDC, r: RECT, color: u32) {
    let brush = CreateSolidBrush(color);
    FillRect(dc, &r, brush);
    DeleteObject(brush);
}

unsafe fn rounded(dc: HDC, r: RECT, radius: i32, face: u32, edge: u32) {
    let brush = CreateSolidBrush(face);
    let pen = CreatePen(PS_SOLID, 1, edge);
    let (old_brush, old_pen) = (SelectObject(dc, brush), SelectObject(dc, pen));
    RoundRect(dc, r.left, r.top, r.right, r.bottom, radius * 2, radius * 2);
    SelectObject(dc, old_brush);
    SelectObject(dc, old_pen);
    DeleteObject(brush);
    DeleteObject(pen);
}

/// `face` when installed (Windows 11's Segoe UI Variable), else `fallback` (Windows 10).
unsafe fn font(px: i32, face: &str, fallback: &str) -> HFONT {
    let make = |name: &str| CreateFontW(-px, 0, 0, 0, FW_NORMAL as i32, 0, 0, 0, DEFAULT_CHARSET as u32, OUT_DEFAULT_PRECIS as u32, CLIP_DEFAULT_PRECIS as u32, CLEARTYPE_QUALITY as u32, (VARIABLE_PITCH | FF_SWISS) as u32, wide(name).as_ptr());
    let f = make(face);
    let dc = GetDC(std::ptr::null_mut());
    let old = SelectObject(dc, f);
    let mut got = [0u16; 64];
    let n = GetTextFaceW(dc, got.len() as i32, got.as_mut_ptr()).max(1) as usize;
    SelectObject(dc, old);
    ReleaseDC(std::ptr::null_mut(), dc);
    if String::from_utf16_lossy(&got[..n - 1]).eq_ignore_ascii_case(face) {
        return f;
    }
    DeleteObject(f);
    make(fallback)
}

fn light_theme() -> bool {
    let mut value = 1u32;
    let mut size = 4u32;
    // SAFETY: a DWORD read into a 4-byte buffer.
    unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            wide("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize").as_ptr(),
            wide("AppsUseLightTheme").as_ptr(),
            RRF_RT_REG_DWORD,
            std::ptr::null_mut(),
            &mut value as *mut u32 as *mut c_void,
            &mut size,
        );
    }
    value != 0
}

