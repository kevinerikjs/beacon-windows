//! Replays the phone's `media_key` messages (phoros/docs/input.md) with SendInput.
//! Points are normalised to the streamed frame. The frame is a region of the primary display
//! (all of it, a window, or a zoomed part) scaled without letterboxing, so a point maps
//! linearly into that region and then onto Windows' 0…65535 absolute space.
//! Windows refuses injected input into elevated windows unless Beacon runs elevated too.

use serde_json::Value;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;
use std::time::Duration;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;

/// Mac modifier mask on the wire (Carbon values). ⌘ becomes Ctrl, so ⌘C copies.
const CMD: u32 = 0x0100;
const SHIFT: u32 = 0x0200;
const OPTION: u32 = 0x0800;
const CONTROL: u32 = 0x1000;

/// The mouse button held by a `pointer` drag, so a vanished client never leaves it down.
static HELD: AtomicU32 = AtomicU32::new(0);

/// The streamed region as a fraction of the display: x, y, width, height.
static REGION: Mutex<[f64; 4]> = Mutex::new([0.0, 0.0, 1.0, 1.0]);

pub fn set_region(region: [f64; 4]) {
    *REGION.lock().unwrap() = region;
}

pub fn handle(cmd: &Value) {
    // A button this host configured takes precedence over everything else in the message.
    if let Some(id) = cmd.get("controlID").and_then(Value::as_str) {
        if crate::controls::perform(id, cmd) {
            return;
        }
    }
    if !handle_fields(cmd) {
        match cmd.get("controlID").and_then(Value::as_str).or(cmd.get("key").and_then(Value::as_str)) {
            Some(k @ ("play_pause" | "next" | "previous")) => media(k),
            Some("seek_backward") => press(VK_LEFT, 0),
            Some("seek_forward") => press(VK_RIGHT, 0),
            _ => {}
        }
    }
}

/// Text, keys, clicks and pointer steps. False when the message has none of them.
pub fn handle_fields(cmd: &Value) -> bool {
    let s = |k: &str| cmd.get(k).and_then(Value::as_str);
    let mods = cmd.get("keystrokeModifiers").and_then(Value::as_u64).unwrap_or(0) as u32;
    if let Some(text) = s("text") {
        type_text(text, true);
    } else if let Some(name) = s("specialKey") {
        if let Some(vk) = special_key(name) {
            press(vk, mods);
        } // unknown keys are ignored, so a newer client never breaks this host
    } else if let Some(key) = s("keystroke") {
        keystroke(key, mods);
    } else if let Some(click) = cmd.get("click") {
        let (x, y) = point(click);
        let right = click.get("button").and_then(Value::as_str) == Some("right");
        move_to(x, y);
        let (down, up) = if right { (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP) } else { (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP) };
        // A second click inside the double-click time is a double-click on Windows, so `count` needs no work.
        mouse(0, 0, 0, down);
        mouse(0, 0, 0, up);
    } else if let Some(p) = cmd.get("pointer") {
        pointer(p);
    } else {
        return false;
    }
    true
}

pub fn media(name: &str) {
    let vk = match name {
        "play_pause" => VK_MEDIA_PLAY_PAUSE,
        "next" => VK_MEDIA_NEXT_TRACK,
        "previous" => VK_MEDIA_PREV_TRACK,
        "volume_up" => VK_VOLUME_UP,
        "volume_down" => VK_VOLUME_DOWN,
        "mute" => VK_VOLUME_MUTE,
        other => return log!("Unknown media key \"{other}\""),
    };
    press(vk, 0);
}

/// Types `text` on a background thread, one character per event so terminals keep up.
pub fn type_text(text: &str, then_return: bool) {
    let text = text.to_string();
    std::thread::spawn(move || {
        type_now(&text);
        if then_return {
            press(VK_RETURN, 0);
        }
    });
}

pub fn type_now(text: &str) {
    for ch in text.chars() {
        unicode_char(ch);
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// A shortcut with explicit modifier keys, e.g. Win+D, which the phone's mask can't express.
pub fn chord(mods: &[VIRTUAL_KEY], vk: VIRTUAL_KEY) {
    press_with(mods, vk);
}

/// Lets go of a dragged button. Call when a client goes away.
pub fn release_held() {
    let up = HELD.swap(0, Ordering::SeqCst);
    if up != 0 {
        mouse(0, 0, 0, up);
    }
}

fn point(v: &Value) -> (f64, f64) {
    let f = |k: &str| v.get(k).and_then(Value::as_f64).unwrap_or(0.5).clamp(0.0, 1.0);
    (f("x"), f("y"))
}

fn pointer(p: &Value) {
    let (x, y) = point(p);
    let right = p.get("button").and_then(Value::as_str) == Some("right");
    match p.get("phase").and_then(Value::as_str) {
        Some("down") => {
            release_held();
            move_to(x, y);
            let (down, up) = if right { (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP) } else { (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP) };
            HELD.store(up, Ordering::SeqCst);
            mouse(0, 0, 0, down);
        }
        Some("move") => move_to(x, y),
        Some("up") => {
            move_to(x, y);
            release_held();
        }
        Some("scroll") => {
            move_to(x, y);
            // Points, positive dy moves content down (wheel up). ~2 wheel units per point
            // matches a trackpad's pixel scroll in most Windows apps.
            let wheel = |d: Option<f64>| (d.unwrap_or(0.0) * 2.0).round() as i32;
            let dy = wheel(p.get("dy").and_then(Value::as_f64));
            let dx = wheel(p.get("dx").and_then(Value::as_f64));
            if dy != 0 {
                mouse(0, 0, dy, MOUSEEVENTF_WHEEL);
            }
            if dx != 0 {
                mouse(0, 0, -dx, MOUSEEVENTF_HWHEEL);
            }
        }
        _ => {} // a phase from a newer client
    }
}

fn move_to(x: f64, y: f64) {
    let [rx, ry, rw, rh] = *REGION.lock().unwrap();
    let (x, y) = (rx + x * rw, ry + y * rh);
    mouse((x * 65535.0).round() as i32, (y * 65535.0).round() as i32, 0, MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE);
}

fn mouse(dx: i32, dy: i32, data: i32, flags: MOUSE_EVENT_FLAGS) {
    let input = INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 { mi: MOUSEINPUT { dx, dy, mouseData: data as _, dwFlags: flags, time: 0, dwExtraInfo: 0 } },
    };
    send(&[input]);
}

fn key(vk: VIRTUAL_KEY, scan: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: vk, wScan: scan, dwFlags: flags, time: 0, dwExtraInfo: 0 } },
    }
}

fn send(inputs: &[INPUT]) {
    // SAFETY: a slice of fully initialised INPUT structs and their exact size.
    unsafe { SendInput(inputs.len() as u32, inputs.as_ptr(), std::mem::size_of::<INPUT>() as i32) };
}

fn modifier_keys(mods: u32) -> Vec<VIRTUAL_KEY> {
    let mut keys = Vec::new();
    if mods & (CMD | CONTROL) != 0 {
        keys.push(VK_CONTROL);
    }
    if mods & OPTION != 0 {
        keys.push(VK_MENU);
    }
    if mods & SHIFT != 0 {
        keys.push(VK_SHIFT);
    }
    keys
}

fn extended(vk: VIRTUAL_KEY) -> KEYBD_EVENT_FLAGS {
    match vk {
        VK_LEFT | VK_RIGHT | VK_UP | VK_DOWN | VK_HOME | VK_END | VK_PRIOR | VK_NEXT | VK_DELETE | VK_INSERT | VK_LWIN | VK_SNAPSHOT => KEYEVENTF_EXTENDEDKEY,
        _ => 0,
    }
}

/// Presses a key with modifiers held around it, like a physical keyboard.
fn press(vk: VIRTUAL_KEY, mods: u32) {
    press_with(&modifier_keys(mods), vk);
}

fn press_with(held: &[VIRTUAL_KEY], vk: VIRTUAL_KEY) {
    let mut seq: Vec<INPUT> = held.iter().map(|&m| key(m, 0, 0)).collect();
    seq.push(key(vk, 0, extended(vk)));
    seq.push(key(vk, 0, extended(vk) | KEYEVENTF_KEYUP));
    seq.extend(held.iter().rev().map(|&m| key(m, 0, KEYEVENTF_KEYUP)));
    send(&seq);
}

fn unicode_char(ch: char) {
    let mut units = [0u16; 2];
    for &u in ch.encode_utf16(&mut units).iter() {
        send(&[key(0, u, KEYEVENTF_UNICODE), key(0, u, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP)]);
    }
}

fn keystroke(s: &str, mods: u32) {
    match s {
        "\n" | "\r" => return press(VK_RETURN, mods),
        "\u{8}" => return press(VK_BACK, mods),
        "\t" => return press(VK_TAB, mods),
        _ => {}
    }
    let mut chars = s.chars();
    let (Some(ch), None) = (chars.next(), chars.clone().next()) else {
        return s.chars().for_each(unicode_char);
    };
    // A chord needs a real key; the layout's own key for this character supplies it.
    if mods != 0 && (ch as u32) < 0x10000 {
        // SAFETY: plain value call.
        let scan = unsafe { VkKeyScanW(ch.to_ascii_lowercase() as u16) };
        if scan != -1 {
            return press((scan & 0xFF) as VIRTUAL_KEY, mods);
        }
    }
    unicode_char(ch);
}

fn special_key(name: &str) -> Option<VIRTUAL_KEY> {
    Some(match name {
        "escape" => VK_ESCAPE,
        "tab" => VK_TAB,
        "leftArrow" => VK_LEFT,
        "upArrow" => VK_UP,
        "downArrow" => VK_DOWN,
        "rightArrow" => VK_RIGHT,
        "home" => VK_HOME,
        "end" => VK_END,
        "pageUp" => VK_PRIOR,
        "pageDown" => VK_NEXT,
        "forwardDelete" => VK_DELETE,
        f if f.starts_with('f') => {
            let n: u16 = f[1..].parse().ok().filter(|n| (1..=12).contains(n))?;
            VK_F1 + n - 1
        }
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn special_keys() {
        assert_eq!(special_key("f1"), Some(VK_F1));
        assert_eq!(special_key("f12"), Some(VK_F12));
        assert_eq!(special_key("f13"), None);
        assert_eq!(special_key("pageDown"), Some(VK_NEXT));
        assert_eq!(special_key("hyper"), None);
    }

    #[test]
    fn command_is_control() {
        assert_eq!(modifier_keys(CMD | SHIFT), vec![VK_CONTROL, VK_SHIFT]);
        assert_eq!(modifier_keys(OPTION), vec![VK_MENU]);
    }
}
