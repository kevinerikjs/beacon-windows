//! The phone's button bar, the Windows counterpart of Beacon's Controls preferences: up to
//! eight buttons in `%APPDATA%\Beacon\controls.json`, written with Beacon's default layout on
//! first run and read again on every connection, so edits apply when the phone reconnects.

use crate::input;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::time::Duration;

#[derive(Serialize, Deserialize, Clone)]
pub struct Button {
    pub label: String,
    /// SF Symbols name the phone draws, e.g. "playpause.fill", "keyboard", "command".
    pub symbol: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub prominent: bool,
    #[serde(flatten)]
    pub action: Action,
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Action {
    /// A key or shortcut, e.g. "left", "ctrl+shift+t", "alt+f4", "win+d".
    Key { keys: String },
    /// play_pause, next, previous, volume_up, volume_down, mute.
    Media { key: String },
    /// Steps in order: {"keys": "ctrl+c"}, {"text": "hello"}, {"wait_ms": 200}.
    Macro { steps: Vec<Step> },
    /// The phone asks for a line of text and Beacon types it.
    Text {
        #[serde(default)]
        prompt: String,
        #[serde(default = "yes")]
        send_return: bool,
    },
    /// The phone's live keyboard.
    Keyboard,
    /// Click mode: taps on the picture click. `button`: left, right, or choose.
    Click { button: String },
    /// A sticky modifier for the next key: cmd, ctrl, alt, shift.
    Modifier { modifier: String },
    None,
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(untagged)]
pub enum Step {
    Keys { keys: String },
    Text { text: String },
    Wait { wait_ms: u64 },
}

fn yes() -> bool {
    true
}

pub fn path() -> std::path::PathBuf {
    crate::store::path().with_file_name("controls.json")
}

fn default_layout() -> Vec<Button> {
    let b = |label: &str, symbol: &str, action: Action| Button { label: label.into(), symbol: symbol.into(), prominent: false, action };
    let key = |k: &str| Action::Key { keys: k.into() };
    let media = |k: &str| Action::Media { key: k.into() };
    let mut buttons = vec![
        b("Left Click", "cursorarrow.click", Action::Click { button: "left".into() }),
        b("Seek Back", "arrow.counterclockwise", key("left")),
        b("Previous", "backward.fill", media("previous")),
        b("Play/Pause", "playpause.fill", media("play_pause")),
        b("Next", "forward.fill", media("next")),
        b("Seek Forward", "arrow.clockwise", key("right")),
        b("Keyboard", "keyboard", Action::Keyboard),
        b("Right Click", "cursorarrow.click.2", Action::Click { button: "right".into() }),
    ];
    buttons[3].prominent = true;
    buttons
}

/// The layout from disk, or the default (written out so there's a file to edit).
pub fn load() -> Vec<Button> {
    let p = path();
    match std::fs::read(&p) {
        Ok(bytes) => match serde_json::from_slice::<Value>(&bytes).map(|v| serde_json::from_value::<Vec<Button>>(v["buttons"].clone())) {
            Ok(Ok(mut buttons)) => {
                buttons.truncate(8);
                buttons
            }
            Ok(Err(e)) | Err(e) => {
                log!("{} is not valid ({e}); using the default buttons", p.display());
                default_layout()
            }
        },
        Err(_) => {
            let buttons = default_layout();
            let _ = std::fs::create_dir_all(p.parent().expect("has a parent"));
            let _ = std::fs::write(&p, serde_json::to_vec_pretty(&json!({"buttons": buttons})).unwrap_or_default());
            buttons
        }
    }
}

/// What `phoneControls` carries.
pub fn wire(buttons: &[Button]) -> Value {
    Value::Array(
        buttons
            .iter()
            .enumerate()
            .map(|(i, b)| {
                let mut c = json!({"id": format!("button-{i}"), "symbol": b.symbol, "label": b.label});
                if b.prominent {
                    c["prominent"] = json!(true);
                }
                c["mode"] = json!(match &b.action {
                    Action::Text { prompt, .. } => {
                        c["promptsForText"] = json!(true);
                        if !prompt.is_empty() {
                            c["textPrompt"] = json!(prompt);
                        }
                        "text"
                    }
                    Action::Keyboard => "keyboard",
                    Action::Click { button } if button == "left" => "click_left",
                    Action::Click { button } if button == "right" => "click_right",
                    Action::Click { .. } => "click",
                    Action::Modifier { modifier } => {
                        c["modifier"] = json!(modifier);
                        "modifier"
                    }
                    _ => "tap",
                });
                c
            })
            .collect(),
    )
}

/// Runs the configured action for `button-N`. False for an id this layout doesn't have,
/// so the caller falls back to the generic fields (keystroke, click, `key`).
pub fn perform(id: &str, command: &Value) -> bool {
    let Some(button) = id.strip_prefix("button-").and_then(|n| n.parse::<usize>().ok()).and_then(|n| load().into_iter().nth(n)) else {
        return false;
    };
    match button.action {
        Action::Key { keys } => press(&keys),
        Action::Media { key } => input::media(&key),
        Action::Macro { steps } => {
            std::thread::spawn(move || {
                for step in steps {
                    match step {
                        Step::Keys { keys } => press(&keys),
                        Step::Text { text } => input::type_now(&text),
                        Step::Wait { wait_ms } => std::thread::sleep(Duration::from_millis(wait_ms.min(10_000))),
                    }
                }
            });
        }
        Action::Text { send_return, .. } => {
            if let Some(text) = command["text"].as_str() {
                input::type_text(text, send_return);
            }
        }
        // Mode buttons: the phone sends clicks and keystrokes itself; a bare press does nothing.
        Action::Keyboard | Action::Click { .. } | Action::Modifier { .. } | Action::None => {
            input::handle_fields(command);
        }
    }
    true
}

fn press(keys: &str) {
    match parse_keys(keys) {
        Some((mods, vk)) => input::chord(&mods, vk),
        None => log!("Button keys \"{keys}\" not understood"),
    }
}

/// "ctrl+shift+t" → ([Ctrl, Shift], T).
pub fn parse_keys(spec: &str) -> Option<(Vec<u16>, u16)> {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
    let mut mods = Vec::new();
    let mut key = None;
    for part in spec.split('+').map(|p| p.trim().to_ascii_lowercase()) {
        let vk = match part.as_str() {
            "ctrl" | "control" | "cmd" => { mods.push(VK_CONTROL); continue }
            "shift" => { mods.push(VK_SHIFT); continue }
            "alt" | "option" => { mods.push(VK_MENU); continue }
            "win" | "windows" => { mods.push(VK_LWIN); continue }
            "enter" | "return" => VK_RETURN,
            "esc" | "escape" => VK_ESCAPE,
            "tab" => VK_TAB,
            "space" => VK_SPACE,
            "backspace" => VK_BACK,
            "delete" | "del" => VK_DELETE,
            "insert" => VK_INSERT,
            "home" => VK_HOME,
            "end" => VK_END,
            "pageup" => VK_PRIOR,
            "pagedown" => VK_NEXT,
            "left" => VK_LEFT,
            "right" => VK_RIGHT,
            "up" => VK_UP,
            "down" => VK_DOWN,
            "printscreen" => VK_SNAPSHOT,
            f if f.len() > 1 && f.starts_with('f') && f[1..].chars().all(|c| c.is_ascii_digit()) => {
                let n: u16 = f[1..].parse().ok().filter(|n| (1..=24).contains(n))?;
                VK_F1 + n - 1
            }
            c if c.chars().count() == 1 => {
                let ch = c.chars().next()?;
                if ch.is_ascii_alphanumeric() {
                    ch.to_ascii_uppercase() as u16 // VK codes for A–Z and 0–9 are their ASCII
                } else {
                    // SAFETY: plain value call.
                    let scan = unsafe { VkKeyScanW(ch as u16) };
                    if scan == -1 { return None; }
                    (scan & 0xFF) as u16
                }
            }
            _ => return None,
        };
        key = Some(vk);
    }
    key.map(|k| (mods, k))
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;

    #[test]
    fn parses_shortcuts() {
        assert_eq!(parse_keys("ctrl+shift+t"), Some((vec![VK_CONTROL, VK_SHIFT], b'T' as u16)));
        assert_eq!(parse_keys("Alt + F4"), Some((vec![VK_MENU], VK_F4)));
        assert_eq!(parse_keys("win+d"), Some((vec![VK_LWIN], b'D' as u16)));
        assert_eq!(parse_keys("left"), Some((vec![], VK_LEFT)));
        assert_eq!(parse_keys("ctrl"), None);
        assert_eq!(parse_keys("ctrl+banana"), None);
    }

    #[test]
    fn default_layout_round_trips_and_matches_beacon() {
        let text = serde_json::to_string(&json!({"buttons": default_layout()})).unwrap();
        let back: Vec<Button> = serde_json::from_value(serde_json::from_str::<Value>(&text).unwrap()["buttons"].clone()).unwrap();
        let wire = wire(&back);
        let modes: Vec<&str> = wire.as_array().unwrap().iter().map(|c| c["mode"].as_str().unwrap()).collect();
        assert_eq!(modes, ["click_left", "tap", "tap", "tap", "tap", "tap", "keyboard", "click_right"]);
        assert_eq!(wire[3]["prominent"], true);
        let macro_json = r#"{"label":"Copy","symbol":"doc.on.doc","action":"macro","steps":[{"keys":"ctrl+c"},{"wait_ms":50},{"text":"hi"}]}"#;
        assert!(matches!(serde_json::from_str::<Button>(macro_json).unwrap().action, Action::Macro { ref steps } if steps.len() == 3));
    }
}
