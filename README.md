# Script Manager (Rust + Iced)

## The Code

- [x] Vibe coded

- [ ] Vibe coded and manually modified

- AI model
    
    - [ ] ChatGPT
    - [x] Claude
    - [ ] Gemini
    - [ ] Qwen
    - [ ] Deepseek

- [ ] Hand typed

- [x] Tested and verified

# About

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
