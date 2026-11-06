# Beacon for Windows: to do

## Installer

`installer/` builds a working MSI: install, Start menu, firewall rule, clean uninstall. Still to do:

- bring ffmpeg along (or install it), since capture and encoding depend on it being on `PATH`
- offer ViGEmBus for controller passthrough, without requiring it
- code-sign the MSI and `beacon.exe`, so SmartScreen doesn't block them
- uninstall clears `%APPDATA%\Beacon` and Start with Windows only for the user who uninstalls; other
  Windows accounts that used Beacon keep theirs

## Auto-update

Needs a place where Windows releases are published (a release feed or a GitHub releases repo), then
an update check against it. Pairs naturally with the installer.

## Known limits

- Keyframes on request take ~0.7–1.6 s, because a fresh ffmpeg has to start. Driving NVENC or
  Media Foundation directly would bring it to one frame, like the Mac.
- Window mode streams the window's area of the screen, so anything covering it shows too, and
  minimising the window ends window mode. Windows.Graphics.Capture would capture the window itself.
- The video bitrate doesn't follow the UDP link's bandwidth estimate while streaming.
- UDP is offered on IPv4 only.
- Primary display only. The controller is always an Xbox 360 pad (no DualShock 4 option).

## Not yet tried

- With a real iPhone or iPad: taps landing in the right place, the tray menu, controller in a game.
