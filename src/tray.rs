//! The notification-area icon and its menu, the Windows counterpart of Beacon's menu bar
//! item, plus Start with Windows (the per-user Run key). Runs the message loop on the main thread.

use crate::{host, wide};
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Registry::*;
use windows_sys::Win32::UI::Shell::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

const WM_TRAY: u32 = WM_APP + 1;
const ID_LOGIN: usize = 2;
const ID_BUTTONS: usize = 3;
const ID_LOG: usize = 4;
const ID_FORGET: usize = 5;
const ID_QUIT: usize = 6;
const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
const RUN_VALUE: &str = "Beacon";

static mut TASKBAR_CREATED: u32 = 0;

pub fn run(name: &str) -> ! {
    // SAFETY: standard window-class registration and a message loop on this thread.
    unsafe {
        let instance = GetModuleHandleW(std::ptr::null());
        let class = wide("BeaconTray");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            hInstance: instance,
            lpszClassName: class.as_ptr(),
            ..std::mem::zeroed()
        };
        RegisterClassW(&wc);
        let hwnd = CreateWindowExW(0, class.as_ptr(), wide("Beacon").as_ptr(), WS_OVERLAPPED, 0, 0, 0, 0, std::ptr::null_mut(), std::ptr::null_mut(), instance, std::ptr::null());
        TASKBAR_CREATED = RegisterWindowMessageW(wide("TaskbarCreated").as_ptr());
        add_icon(hwnd, name);
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    std::process::exit(0)
}

fn icon_data(hwnd: HWND) -> NOTIFYICONDATAW {
    // SAFETY: plain data; the fields set below are the ones the shell reads.
    let mut data: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
    data.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    data.hWnd = hwnd;
    data.uID = 1;
    data
}

unsafe fn add_icon(hwnd: HWND, name: &str) {
    let mut data = icon_data(hwnd);
    data.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
    data.uCallbackMessage = WM_TRAY;
    // The app icon from beacon.rc, at the small-icon size for this display's scale.
    let (w, h) = (GetSystemMetrics(SM_CXSMICON), GetSystemMetrics(SM_CYSMICON));
    data.hIcon = LoadImageW(GetModuleHandleW(std::ptr::null()), std::ptr::without_provenance::<u16>(1), IMAGE_ICON, w, h, LR_DEFAULTCOLOR) as HICON;
    let tip = wide(&format!("Beacon: {name}"));
    let n = tip.len().min(data.szTip.len() - 1);
    data.szTip[..n].copy_from_slice(&tip[..n]);
    if Shell_NotifyIconW(NIM_ADD, &data) == 0 {
        log!("Could not add the notification-area icon");
    }
}

unsafe extern "system" fn window_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg == TASKBAR_CREATED && msg != 0 {
        add_icon(hwnd, &host().name); // Explorer restarted
        return 0;
    }
    if msg == WM_CLOSE {
        // The installer closing Beacon to update or remove it.
        Shell_NotifyIconW(NIM_DELETE, &icon_data(hwnd));
        std::process::exit(0);
    }
    if msg == WM_TRAY && matches!(lparam as u32, WM_RBUTTONUP | WM_LBUTTONUP | WM_CONTEXTMENU) {
        show_menu(hwnd);
        return 0;
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

unsafe fn show_menu(hwnd: HWND) {
    let menu = CreatePopupMenu();
    let names: Vec<String> = host()
        .authenticated()
        .iter()
        .filter_map(|s| s.state.lock().unwrap().device_id.clone())
        .map(|id| crate::store::name(&id).unwrap_or(id))
        .collect();
    let status = if names.is_empty() { "Waiting for Beam".to_string() } else { format!("Streaming to {}", names.join(", ")) };
    let item = |flags: u32, id: usize, text: &str| {
        AppendMenuW(menu, flags, id, wide(text).as_ptr());
    };
    item(MF_STRING | MF_GRAYED, 1, &status);
    item(MF_SEPARATOR, 0, "");
    item(MF_STRING | if login_enabled() { MF_CHECKED } else { 0 }, ID_LOGIN, "Start with Windows");
    item(MF_STRING, ID_BUTTONS, "Edit phone buttons…");
    item(MF_STRING, ID_LOG, "Open log");
    item(MF_STRING, ID_FORGET, "Forget paired phones…");
    item(MF_SEPARATOR, 0, "");
    item(MF_STRING, ID_QUIT, "Quit Beacon");
    let mut at = POINT { x: 0, y: 0 };
    GetCursorPos(&mut at);
    SetForegroundWindow(hwnd); // otherwise the menu doesn't close when clicking elsewhere
    let chosen = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON, at.x, at.y, 0, hwnd, std::ptr::null()) as usize;
    DestroyMenu(menu);
    match chosen {
        ID_LOGIN => set_login(!login_enabled()),
        ID_BUTTONS => {
            let _ = crate::controls::load(); // makes sure the file exists
            open_in_notepad(&crate::controls::path());
        }
        ID_LOG => open_in_notepad(&crate::log_path()),
        ID_FORGET => {
            let answer = MessageBoxW(hwnd, wide("Forget every paired iPhone and iPad? Each one pairs again with a new code.").as_ptr(), wide("Beacon").as_ptr(), MB_YESNO | MB_ICONQUESTION);
            if answer == IDYES {
                host().forget_all();
            }
        }
        ID_QUIT => {
            Shell_NotifyIconW(NIM_DELETE, &icon_data(hwnd));
            std::process::exit(0);
        }
        _ => {}
    }
}

fn open_in_notepad(path: &std::path::Path) {
    let file = wide(&format!("\"{}\"", path.display()));
    // SAFETY: valid NUL-terminated strings for the call.
    unsafe { ShellExecuteW(std::ptr::null_mut(), wide("open").as_ptr(), wide("notepad.exe").as_ptr(), file.as_ptr(), std::ptr::null(), SW_SHOWNORMAL) };
}

pub fn login_enabled() -> bool {
    let mut size = 0u32;
    // SAFETY: a size query; no buffer is written.
    unsafe { RegGetValueW(HKEY_CURRENT_USER, wide(RUN_KEY).as_ptr(), wide(RUN_VALUE).as_ptr(), RRF_RT_REG_SZ, std::ptr::null_mut(), std::ptr::null_mut(), &mut size) == 0 }
}

pub fn set_login(enabled: bool) {
    // SAFETY: valid strings and a data buffer with its byte length.
    let status = unsafe {
        if enabled {
            let exe = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_default();
            let value = wide(&format!("\"{exe}\""));
            RegSetKeyValueW(HKEY_CURRENT_USER, wide(RUN_KEY).as_ptr(), wide(RUN_VALUE).as_ptr(), REG_SZ, value.as_ptr() as *const _, (value.len() * 2) as u32)
        } else {
            RegDeleteKeyValueW(HKEY_CURRENT_USER, wide(RUN_KEY).as_ptr(), wide(RUN_VALUE).as_ptr())
        }
    };
    log!("Start with Windows {}{}", if enabled { "on" } else { "off" }, if status == 0 { "" } else { " (failed)" });
}
