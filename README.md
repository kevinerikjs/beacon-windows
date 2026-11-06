# Beacon for Windows

**The Windows half of Beam.** Streams your PC's screen (or one window) and sound to the
[Beam](https://apps.apple.com/us/app/beam-stream-your-screen/id6760154962) app on your iPhone or iPad,
and plays the phone's taps, drags, scrolling, keys, buttons and game controller back on the PC.

It speaks the same wire protocol as Beacon for Mac ([Phoros](../phoros/docs/wire-format.md)), so the
existing Beam app connects to it unchanged. It shows up in Beam's host list like a Mac does.

## Requirements

- Windows 10 or 11
- A GPU video encoder: NVIDIA (NVENC), AMD (AMF), Intel (Quick Sync), or one reachable through
  Media Foundation. As on the Mac, there is no software fallback.
- [ffmpeg](https://www.gyan.dev/ffmpeg/builds/) 6.1 or later on `PATH`, a build with `ddagrab`
  (`winget install Gyan.FFmpeg`)
- Beam 3.6 or later. This host only accepts encrypted connections, so older Beam builds are refused
  with a message asking to update.
- For controller passthrough: [ViGEmBus](https://github.com/nefarius/ViGEmBus/releases) (Sunshine and
  many emulators install it). Without it Beacon doesn't offer controller input.

## Install

`installer\build.ps1` builds `beacon.exe` and packs it into `target\installer\Beacon-<version>.msi`
(needs the .NET SDK; WiX v5 comes from NuGet). The MSI:

- installs to `C:\Program Files\Beacon`, adds **Beacon** to the Start menu, and starts it
- adds a firewall rule for `beacon.exe` on every network type, because Windows marks new networks
  (home ones too) as Public. Pairing still needs the code shown on the PC, and every connection is
  encrypted
- closes a running Beacon before it updates or removes it. Updates keep your paired phones

Uninstalling (Settings → Apps) removes everything Beacon added: its files and shortcut, every firewall
rule for it (including any Windows made when it asked about network access), **Start with Windows**,
and `%APPDATA%\Beacon` (paired phones, phone buttons, log) for the user who uninstalls.

ffmpeg and ViGEmBus are not included; install them as listed above.

## Run from source

```powershell
cargo build --release
.\target\release\beacon.exe
```

Beacon lives in the notification area (the tray). Run from source, Windows asks on first run whether
`beacon.exe` may use the network: allow it, for TCP 7979, UDP 7981–7990 and Bonjour. Beam finds the
PC over Bonjour as its computer name. Pick it, and Beacon shows a 6-digit code in a window (light or
dark, following Windows). Type it on the phone. Later connections sign in by themselves.

Right-click the tray icon for: who's streaming, **Start with Windows**, **Edit phone buttons…**,
**Open log**, **Forget paired phones…** and **Quit**. Started from a terminal, Beacon also prints its
log there. `beacon.exe --forget-all` forgets every phone from the command line.

Everything lives in `%APPDATA%\Beacon`: `paired-devices.json` (each secret sealed with DPAPI for your
Windows account), `controls.json` and `beacon.log`.

## What it does

| | |
| --- | --- |
| Capture | Desktop Duplication (ffmpeg `ddagrab`) of the primary display, with the cursor. Window mode and zoom capture a region of it |
| Encode | Hardware H.264 High, or HEVC Main when every connected phone decodes it. Constant bitrate, no B-frames, low-latency presets, a keyframe every 5 s |
| Keyframes on demand | A replacement encoder starts and takes over at its first frame, with no gap in the picture. Used for joins, resume, loss and every change of quality, window or zoom |
| Audio | WASAPI loopback of the default output device, at 44.1 or 48 kHz (96/192 kHz devices are downsampled). AAC-LC from Windows' own encoder for phones that list it, Float32 PCM otherwise |
| Media transport | UDP (Phoros rtc2: ICE, DTLS, SRTP, NACK/RTX, XOR FEC) through `phoros-core`, the same Rust core Beam uses. Falls back to the TCP connection if the phone doesn't take the offer, or if frames go unacknowledged for 700 ms |
| Discovery | Bonjour `_beam._tcp` on port 7979, TXT `enc=1` |
| Security | X25519 + HKDF-SHA256 + AES-256-GCM per connection, keyed with the pairing secret (Phoros `secure/1`); DTLS on UDP; 5 wrong pairing codes end the attempt |
| Input | `SendInput`: clicks, drags, scroll, live keyboard, special keys, text, media keys. ⌘ and ⌃ become Ctrl, ⌥ becomes Alt |
| Controller | A virtual Xbox 360 controller through ViGEmBus, plugged in while the phone's controller is connected |
| Window mode | Beam's window list shows your open windows; picking one brings it forward and streams just that window, following it as it moves |
| Zoom | Beam's viewport lock streams the zoomed part at full resolution |
| Quality | Every preset from 360p30 to native60. `auto` moves between 360p and 1080p from Beam's playback feedback (the Mac's ladder). 60 fps presets rise to the phone's refresh rate when the display supports it |
| Button bar | Up to eight buttons in `controls.json`: keys and shortcuts, media keys, macros, a text box, the keyboard, click modes and sticky modifiers. Beacon's default layout to start |
| Remote | Advertises Tailscale addresses (100.64.0.0/10) so Beam can connect from outside the home network |

### Phone buttons

**Edit phone buttons…** opens `controls.json`. Changes apply when the phone next connects. Symbols are
[SF Symbols](https://developer.apple.com/sf-symbols/) names, which the phone draws.

```json
{ "buttons": [
  { "label": "Play/Pause", "symbol": "playpause.fill", "prominent": true, "action": "media", "key": "play_pause" },
  { "label": "Desktop", "symbol": "macwindow", "action": "key", "keys": "win+d" },
  { "label": "Copy line", "symbol": "doc.on.doc", "action": "macro",
    "steps": [ { "keys": "home" }, { "keys": "shift+end" }, { "wait_ms": 50 }, { "keys": "ctrl+c" } ] },
  { "label": "Search", "symbol": "magnifyingglass", "action": "text", "prompt": "Search for…", "send_return": true },
  { "label": "Keyboard", "symbol": "keyboard", "action": "keyboard" },
  { "label": "Click", "symbol": "cursorarrow.click", "action": "click", "button": "left" },
  { "label": "Ctrl", "symbol": "control", "action": "modifier", "modifier": "ctrl" }
] }
```

Actions: `key` (`keys` like `ctrl+shift+t`, `alt+f4`, `left`, `f5`), `media` (`play_pause`, `next`,
`previous`, `volume_up`, `volume_down`, `mute`), `macro` (`keys`, `text` and `wait_ms` steps), `text`,
`keyboard`, `click` (`left`, `right` or `choose`), `modifier` (`cmd`, `ctrl`, `alt`, `shift`) and `none`.

## Limits

- **Keyframes on demand take ~0.7–1.5 s** (starting a replacement ffmpeg), where the Mac takes one
  frame. Driving NVENC or Media Foundation directly instead of through ffmpeg would fix it.
- **Window mode streams the window's area of the screen**, so whatever covers the window shows too, and a
  minimised window ends window mode. The Mac captures the window on its own.
- **The encoder bitrate doesn't follow the UDP link's bandwidth estimate** live, because ffmpeg can't
  change it mid-stream. Quality comes from the preset, `auto`, and Beam's bitrate cap.
- **UDP is offered on IPv4 only.** A phone that reached the PC over IPv6 stays on TCP.
- **Primary display only**, and the controller is always an Xbox 360 pad (no DualShock 4 option).
- **No auto-update**: there's no published Windows release feed to update from yet.

Input can't reach windows running as administrator unless Beacon runs as administrator too. Windows
blocks that on purpose.

## Testing without a phone

`examples/fake_phone.rs` acts as Beam. It pairs (reading the code from stdin) and signs in. It takes the
UDP offer with a `phoros-core` client peer and acknowledges frames like Beam. Then it drives ping, clock
sync, a controller, the window list, pause and resume, a zoom and a quality change. Finally it stops
acknowledging, so the host has to fall back to TCP. It saves the video it receives on both transports.

```powershell
.\target\release\beacon.exe            # in one terminal
cargo run --release --example fake_phone -- 127.0.0.1:7979 out.h264         # type the code shown
cargo run --release --example fake_phone -- 127.0.0.1:7979 out.h265 hevc
ffprobe -count_frames out.h264         # every received frame should decode
```

The fake phone plugs in a virtual Xbox controller for a moment, so Windows may chime.

`cargo test` covers the key schedule against the pinned Phoros vector, framing, the FLV reader, the AAC
encoder's timing (it encodes a click and finds it after decoding with ffmpeg), DPAPI, the quality
ladder, controller mapping, shortcut parsing, the button file and window clipping.

## Building on `phoros-core`

The UDP transport links `../phoros/Core/phoros-core` directly. That crate picks its crypto per platform:
CryptoKit on Apple, pure-Rust crypto elsewhere. Its own tests pass on Windows (`cargo test` in
`phoros/Core/phoros-core`), including the two-peer loopback.

## Layout

```
src/
├── main.rs      # Host state, Bonjour, listeners, presets, capture lifecycle, window and zoom, auto ladder
├── session.rs   # One phone: encrypted handshake, pairing, auth, control messages, TCP/UDP media routing
├── rtc.rs       # The UDP peer (phoros-core): offer, lanes, frame acks, stall detection
├── proto.rs     # Phoros wire format and secure channel
├── video.rs     # Encoder probe, ffmpeg capture+encode, FLV → parameter sets and Annex B
├── audio.rs     # WASAPI loopback
├── aac.rs       # Media Foundation AAC encoder
├── input.rs     # SendInput replay
├── controls.rs  # The phone's button bar (controls.json)
├── gamepad.rs   # Virtual Xbox 360 controller (ViGEmBus)
├── desktop.rs   # Window list and geometry
├── quality.rs   # The auto quality ladder
├── tray.rs      # Notification-area icon and menu, Start with Windows
├── code_window.rs # The pairing-code window
installer/         # Beacon.msi (WiX v5): Beacon.wxs, build.ps1
└── store.rs     # Paired devices, DPAPI
```
