# Script Manager (Rust + Iced)

Runs scripts at manager start (with optional delay) or at specific times, lives in the
system tray, and gives every script a Kill button.

## Build

```powershell
cargo run                # debug
cargo build --release    # target\release\script-manager.exe
```

## Usage

| Field | Example |
|---|---|
| Command | `py`, `pwsh`, `cmd`, `node` |
| Arguments | `-u "C:\scripts\job.py" --flag` (typed exactly like a terminal) |
| Start in | `C:\scripts` |

- For `.bat` files use command `cmd`, arguments `/c nothing.bat`.
- For PowerShell use `pwsh` (or `powershell`), arguments `-NoProfile -File "C:\x\a.ps1"`.
- For Python use `py -u script.py`; `-u` makes output appear in the log immediately.
- **Start with Windows** writes `HKCU\...\Run` -> `script-manager.exe --minimized`,
  so the manager starts hidden in the tray.
- Closing the window hides it to the tray. Use the tray menu (or the Quit button) to exit.
  Quitting kills all running scripts.
- Config: `%APPDATA%\ScriptManager\config.json`. Logs: `%APPDATA%\ScriptManager\logs\`.