# Xbox Connect

A tiny Windows tray app that keeps Xbox Wireless Controllers connected over Bluetooth, and shows a live view of the controller: battery, firmware and every button you press.

![Xbox Connect](docs/screenshot.png)

## Why

On some Bluetooth adapters (for example the Realtek RTL8852BE found on many motherboards), a paired Xbox Series controller never reconnects after it's switched off and on, so you have to re-pair it every time. Windows waits for the controller to reconnect, and with these adapters that reconnect never succeeds.

Xbox Connect asks Windows to **actively hold the connection** (`GattSession.MaintainConnection`) for every paired Xbox controller. Turn the controller on, tap the Xbox button, and it connects.

## Features

- Keeps **all** paired Xbox controllers connected; survives sleep/resume and re-pairing.
- Battery percentage straight from the controller (Bluetooth GATT), plus firmware version.
- Live input view: sticks, triggers (analog), bumpers, D-pad, ABXY, View/Menu/Share, Guide, and Elite paddles.
- System tray icon (cyan when connected) with a battery tooltip. Closing the window keeps it running in the tray.
- Start with Windows (optional).
- One small `.exe`, per-user install, no admin, no installer framework.

## Install

In PowerShell:

```powershell
irm https://raw.githubusercontent.com/ic3man5/xbox-connect/main/install.ps1 | iex
```

This downloads the latest release to `%LOCALAPPDATA%\Programs\XboxConnect`, verifies its SHA-256 checksum, adds a Start Menu shortcut, enables start-with-Windows and launches it.

Prefer to look before you run? Download [`install.ps1`](install.ps1), read it, then run it. To pin a version, set `$env:XBOX_CONNECT_VERSION = 'v0.1.0'` first. Release binaries carry [GitHub build attestations](https://docs.github.com/actions/security-guides/using-artifact-attestations-to-establish-provenance-for-builds); verify with `gh attestation verify xbox-connect.exe -R ic3man5/xbox-connect`.

> The checksum is published in the same release, so it protects against corrupted downloads, not a compromised repository. The attestation ties the binary to this repo's CI build.

The `.exe` isn't code-signed yet. If you download it from the Releases page in a browser, SmartScreen may say "Windows protected your PC"; choose **More info → Run anyway**.

## Uninstall

```powershell
irm https://raw.githubusercontent.com/ic3man5/xbox-connect/main/uninstall.ps1 | iex
```

Or use **Settings → Apps → Installed apps → Xbox Connect** (runs the local copy of the uninstaller saved at install time).

## Usage

1. Pair the controller once in **Settings → Bluetooth & devices** (hold the pair button on top until the Xbox button flashes fast).
2. Run Xbox Connect. From then on, tap the Xbox button to reconnect.

Command-line flags: `--minimized` starts hidden in the tray (used for start-with-Windows); `--demo` animates fake input, for screenshots.

## Build

```powershell
cargo build --release
```

Requires Rust (stable) on Windows. Output: `target\release\xbox-connect.exe`.

## Notes

- The Guide button is read through an undocumented XInput export (`xinput1_4.dll` ordinal 100); everything else uses documented Windows APIs.
- With several controllers, the input view follows whichever controller you used most recently.
- Controller drawing: "Gamepad Line Art Pack" by gidedin (https://gidedin.itch.io/gamepad-line-art-pack), CC0 1.0. See [assets/LICENSE-gamepad-line-art.txt](assets/LICENSE-gamepad-line-art.txt).
- This project is not affiliated with or endorsed by Microsoft. Xbox is a trademark of Microsoft Corporation.

## License

MIT
