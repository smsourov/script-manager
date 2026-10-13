// Hide the console window in release builds on Windows.
#![cfg_attr(all(not(debug_assertions), windows), windows_subsystem = "windows")]

use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use chrono::{Datelike, Local, Timelike};
use iced::widget::{
    button, checkbox, column, container, horizontal_space, mouse_area, opaque, row, scrollable,
    stack, text, text_input,
};
use iced::{
    window, Alignment, Background, Border, Color, Element, Font, Length, Padding, Size,
    Subscription, Task, Theme,
};
use serde::{Deserialize, Serialize};
#[cfg(windows)]
use tray_icon::menu::ContextMenu;
use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

const APP_NAME: &str = "ScriptManager";
const DAY_NAMES: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];

// Segoe MDL2 Assets (Win10) / Segoe Fluent Icons (Win11) share these code points.
const ICON_EDIT: char = '\u{E70F}';
const ICON_VIEW: char = '\u{E7B3}';
const ICON_CLOCK: char = '\u{E916}';

const GREEN: Color = Color::from_rgb(0.25, 0.80, 0.35);
const RED: Color = Color::from_rgb(0.95, 0.35, 0.35);
const ORANGE: Color = Color::from_rgb(0.95, 0.60, 0.25);
const GRAY: Color = Color::from_rgb(0.60, 0.60, 0.60);

// Card colours: (background, background while editing, border)
const STARTUP_ON: (Color, Color, Color) = (
    Color::from_rgb(0.13, 0.27, 0.16),
    Color::from_rgb(0.06, 0.12, 0.07),
    Color::from_rgb(0.25, 0.65, 0.35),
);
const STARTUP_OFF: (Color, Color, Color) = (
    Color::from_rgb(0.30, 0.13, 0.13),
    Color::from_rgb(0.13, 0.06, 0.06),
    Color::from_rgb(0.75, 0.30, 0.30),
);

// ───────────────────────────── Data model ─────────────────────────────

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
struct Script {
    id: u64,
    name: String,
    /// Executable that runs the script, e.g. `py`, `pwsh`, `cmd`, `node`.
    command: String,
    /// Raw argument string, exactly as typed in a terminal.
    args: String,
    /// Working directory (PWD). Empty = inherit from the manager.
    working_dir: String,
    enabled: bool,
    run_at_start: bool,
    start_delay_secs: u64,
    /// Comma separated `HH:MM` list (24h, local time).
    times: String,
    /// Days on which `times` fire (Mon..Sun).
    days: [bool; 7],
    /// true = own visible console; false = hidden, output goes to a log file.
    show_console: bool,
}

impl Default for Script {
    fn default() -> Self {
        Self {
            id: 0,
            name: "New script".into(),
            command: String::new(),
            args: String::new(),
            working_dir: String::new(),
            enabled: true,
            run_at_start: false,
            start_delay_secs: 0,
            times: String::new(),
            days: [true; 7],
            show_console: false,
        }
    }
}

impl Script {
    fn parsed_times(&self) -> Vec<(u32, u32)> {
        self.times
            .split(',')
            .filter_map(|t| {
                let (h, m) = t.trim().split_once(':')?;
                let (h, m): (u32, u32) = (h.trim().parse().ok()?, m.trim().parse().ok()?);
                (h < 24 && m < 60).then_some((h, m))
            })
            .collect()
    }

    fn has_invalid_times(&self) -> bool {
        let given = self.times.split(',').filter(|t| !t.trim().is_empty()).count();
        given != self.parsed_times().len()
    }

    fn summary(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if !self.enabled {
            parts.push("Disabled".into());
        } else if self.run_at_start {
            parts.push(if self.start_delay_secs > 0 {
                format!("Runs at startup (+{}s)", self.start_delay_secs)
            } else {
                "Runs at startup".into()
            });
        } else {
            parts.push("Not set to run at startup".into());
        }
        if self.enabled && !self.parsed_times().is_empty() {
            parts.push(format!("Scheduled: {}", self.times.trim()));
        }
        parts.join("  |  ")
    }
}

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct Config {
    scripts: Vec<Script>,
    start_with_windows: bool,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct ExportFile {
    app: String,
    version: u32,
    scripts: Vec<Script>,
}

/// An in-progress edit. Nothing is written to the config until the user presses Save.
#[derive(Clone)]
struct Draft {
    script: Script,
    is_new: bool,
    original: Option<Script>,
}

impl Draft {
    fn is_dirty(&self) -> bool {
        self.is_new || Some(&self.script) != self.original.as_ref()
    }
}

/// Where the user wants to go next (used by the "unsaved changes" prompt).
#[derive(Debug, Clone, Copy)]
enum Nav {
    View(u64),
    Edit(u64),
    Add,
}

#[derive(Debug, Clone, Copy)]
enum Field {
    Name,
    Command,
    Args,
    WorkDir,
    Delay,
    Times,
}

fn field_id(f: Field) -> text_input::Id {
    text_input::Id::new(match f {
        Field::Name => "f_name",
        Field::Command => "f_command",
        Field::Args => "f_args",
        Field::WorkDir => "f_workdir",
        Field::Delay => "f_delay",
        Field::Times => "f_times",
    })
}

// ───────────────────────────── Persistence / import / export ─────────────────────────────

fn data_dir() -> PathBuf {
    let dir = dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(APP_NAME);
    let _ = fs::create_dir_all(&dir);
    dir
}

fn load_config(path: &Path) -> Config {
    fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn parse_import(data: &str) -> Result<Vec<Script>, String> {
    if let Ok(f) = serde_json::from_str::<ExportFile>(data) {
        return Ok(f.scripts);
    }
    serde_json::from_str::<Vec<Script>>(data).map_err(|e| format!("Not a valid export file: {e}"))
}

async fn export_dialog(file_name: String, json: String) -> Result<Option<String>, String> {
    let Some(handle) = rfd::AsyncFileDialog::new()
        .set_file_name(file_name)
        .add_filter("JSON", &["json"])
        .save_file()
        .await
    else {
        return Ok(None);
    };
    match fs::write(handle.path(), json) {
        Ok(()) => Ok(Some(handle.file_name())),
        Err(e) => Err(e.to_string()),
    }
}

async fn import_dialog() -> Result<Option<Vec<Script>>, String> {
    let Some(handle) = rfd::AsyncFileDialog::new()
        .add_filter("JSON", &["json"])
        .pick_file()
        .await
    else {
        return Ok(None);
    };
    match fs::read_to_string(handle.path()) {
        Ok(data) => parse_import(&data).map(Some),
        Err(e) => Err(e.to_string()),
    }
}

// ───────────────────────────── Executable lookup (PATH) ─────────────────────────────

/// Looks for `<cmd>.exe` / `<cmd>.com` in every directory listed in PATH.
fn find_executable(cmd: &str) -> bool {
    let cmd = cmd.trim().trim_matches('"');
    if cmd.is_empty() {
        return false;
    }
    let path = Path::new(cmd);
    let has_ext = path
        .extension()
        .map(|e| e.eq_ignore_ascii_case("exe") || e.eq_ignore_ascii_case("com"))
        .unwrap_or(false);

    let names: Vec<String> = if has_ext {
        vec![cmd.to_string()]
    } else {
        vec![format!("{cmd}.exe"), format!("{cmd}.com")]
    };

    // Explicit path (contains a separator): check it directly.
    if cmd.contains('\\') || cmd.contains('/') {
        return names.iter().any(|n| Path::new(n).is_file());
    }

    let Some(paths) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&paths).any(|dir| names.iter().any(|n| dir.join(n).is_file()))
}

// ───────────────────────────── Process handling ─────────────────────────────

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
#[cfg(windows)]
const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;

fn launch(s: &Script, log_dir: &Path) -> std::io::Result<Child> {
    if s.command.trim().is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "command is empty",
        ));
    }

    let mut cmd = Command::new(s.command.trim());

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        if !s.args.trim().is_empty() {
            cmd.raw_arg(s.args.trim());
        }
        cmd.creation_flags(if s.show_console {
            CREATE_NEW_CONSOLE
        } else {
            CREATE_NO_WINDOW
        });
    }
    #[cfg(not(windows))]
    {
        cmd.args(s.args.split_whitespace());
    }

    if !s.working_dir.trim().is_empty() {
        cmd.current_dir(s.working_dir.trim());
    }

    if !s.show_console {
        let safe: String = s
            .name
            .chars()
            .map(|c| if c.is_alphanumeric() { c } else { '_' })
            .collect();
        let log_path = log_dir.join(format!("{}_{}.log", s.id, safe));
        let file = OpenOptions::new().create(true).append(true).open(log_path)?;
        cmd.stdin(Stdio::null())
            .stdout(Stdio::from(file.try_clone()?))
            .stderr(Stdio::from(file));
    }

    cmd.spawn()
}

/// Kill the process and everything it spawned.
fn kill_tree(child: &mut Child) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let _ = Command::new("taskkill")
            .args(["/PID", &child.id().to_string(), "/T", "/F"])
            .creation_flags(CREATE_NO_WINDOW)
            .status();
    }
    let _ = child.kill();
    let _ = child.wait();
}

// ───────────────────────────── Windows helpers ─────────────────────────────

#[cfg(windows)]
fn set_autostart(enable: bool) -> Result<(), String> {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;

    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let (key, _) = hkcu
        .create_subkey(r"Software\Microsoft\Windows\CurrentVersion\Run")
        .map_err(|e| e.to_string())?;
    if enable {
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        key.set_value(APP_NAME, &format!("\"{}\" --minimized", exe.display()))
            .map_err(|e| e.to_string())
    } else {
        let _ = key.delete_value(APP_NAME);
        Ok(())
    }
}

#[cfg(not(windows))]
fn set_autostart(_enable: bool) -> Result<(), String> {
    Err("Autostart registration is only implemented for Windows".into())
}

/// Windows 11 starts at build 22000.
#[cfg(windows)]
fn is_windows_11() -> bool {
    use winreg::enums::HKEY_LOCAL_MACHINE;
    use winreg::RegKey;
    RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion")
        .ok()
        .and_then(|k| k.get_value::<String, _>("CurrentBuildNumber").ok())
        .and_then(|b| b.parse::<u32>().ok())
        .map(|b| b >= 22000)
        .unwrap_or(false)
}

#[cfg(not(windows))]
fn is_windows_11() -> bool {
    false
}

/// Injects Ctrl+<key> into the focused control (used by the Cut/Copy/Paste menu).
#[cfg(windows)]
fn send_ctrl_combo(key: u16) {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;

    fn kb(vk: u16, up: bool) -> INPUT {
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: vk,
                    wScan: 0,
                    dwFlags: if up { KEYEVENTF_KEYUP } else { 0 },
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        }
    }
    let inputs = [kb(VK_CONTROL, false), kb(key, false), kb(key, true), kb(VK_CONTROL, true)];
    unsafe {
        SendInput(
            inputs.len() as u32,
            inputs.as_ptr(),
            std::mem::size_of::<INPUT>() as i32,
        );
    }
}

#[cfg(not(windows))]
fn send_ctrl_combo(_key: u16) {}

// ───────────────────────────── Tray icon ─────────────────────────────

fn make_icon() -> Icon {
    let size = 32u32;
    let mut rgba = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let (dx, dy) = (x as f32 - 15.5, y as f32 - 15.5);
            let inside_circle = (dx * dx + dy * dy).sqrt() <= 14.5;
            let in_triangle = (11..=23).contains(&x) && (dy.abs() <= (23.0 - x as f32) * 0.8);
            let px: [u8; 4] = if inside_circle && in_triangle {
                [255, 255, 255, 255]
            } else if inside_circle {
                [40, 120, 220, 255]
            } else {
                [0, 0, 0, 0]
            };
            rgba.extend_from_slice(&px);
        }
    }
    Icon::from_rgba(rgba, size, size).expect("valid icon")
}

// ───────────────────────────── Application ─────────────────────────────

struct App {
    config: Config,
    config_path: PathBuf,
    log_dir: PathBuf,

    selected: Option<u64>,
    draft: Option<Draft>,
    prompt: Option<Nav>,
    exe_found: Option<bool>,

    running: HashMap<u64, Child>,
    pending: HashMap<u64, Instant>,
    status: HashMap<u64, String>,
    last_minute: Option<(i32, u32, u32, u32)>,

    window_id: Option<window::Id>,
    hwnd: isize,
    notice: Option<(String, bool, Instant)>,
    icon_font: Font,

    // Tray + context menu
    _tray: TrayIcon,
    show_id: MenuId,
    quit_id: MenuId,
    ctx_menu: Menu,
    cut_id: MenuId,
    copy_id: MenuId,
    paste_id: MenuId,
}

#[derive(Debug, Clone)]
enum Message {
    Tick,
    GotWindowId(Option<window::Id>),
    GotHwnd(isize),
    CloseRequested(window::Id),
    HideToTray,
    ShowWindow,
    Quit,
    ToggleAutostart(bool),

    Add,
    View(u64),
    EditBtn(u64),
    Delete(u64),
    Start(u64),
    Kill(u64),
    SaveDraft,
    DiscardDraft,
    PromptSave,
    PromptDiscard,
    PromptCancel,

    Name(String),
    Command(String),
    Args(String),
    WorkDir(String),
    BrowseDir,
    DirPicked(Option<PathBuf>),
    Enabled(bool),
    RunAtStart(bool),
    Delay(String),
    Times(String),
    Day(usize, bool),
    ShowConsole(bool),

    RightClick(Field),
    ShowCtx,

    Export,
    Exported(Result<Option<String>, String>),
    Import,
    Imported(Result<Option<Vec<Script>>, String>),
}

fn main() -> iced::Result {
    let minimized = std::env::args().any(|a| a == "--minimized");

    iced::application("Script Manager", App::update, App::view)
        .subscription(App::subscription)
        .theme(|_| Theme::Dark)
        .window(window::Settings {
            size: Size::new(1000.0, 660.0),
            exit_on_close_request: false, // close button -> hide to tray
            visible: !minimized,
            ..Default::default()
        })
        .run_with(move || App::new(minimized))
}

impl App {
    fn new(_minimized: bool) -> (Self, Task<Message>) {
        let dir = data_dir();
        let log_dir = dir.join("logs");
        let _ = fs::create_dir_all(&log_dir);
        let config_path = dir.join("config.json");
        let config = load_config(&config_path);

        // Tray icon + menu (must be created on the main/UI thread).
        let menu = Menu::new();
        let show_item = MenuItem::new("Show window", true, None);
        let quit_item = MenuItem::new("Quit (stops all scripts)", true, None);
        let _ = menu.append(&show_item);
        let _ = menu.append(&PredefinedMenuItem::separator());
        let _ = menu.append(&quit_item);
        let tray = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_menu_on_left_click(false)
            .with_tooltip("Script Manager")
            .with_icon(make_icon())
            .build()
            .expect("failed to create tray icon");

        // Right-click context menu for text fields: Cut / Copy / Paste only.
        let ctx_menu = Menu::new();
        let cut_item = MenuItem::new("Cut", true, None);
        let copy_item = MenuItem::new("Copy", true, None);
        let paste_item = MenuItem::new("Paste", true, None);
        let _ = ctx_menu.append(&cut_item);
        let _ = ctx_menu.append(&copy_item);
        let _ = ctx_menu.append(&paste_item);

        let now = Instant::now();
        let pending = config
            .scripts
            .iter()
            .filter(|s| s.enabled && s.run_at_start)
            .map(|s| (s.id, now + Duration::from_secs(s.start_delay_secs)))
            .collect();

        // Win11 ships "Segoe Fluent Icons"; Win10 ships "Segoe MDL2 Assets".
        let icon_font = if is_windows_11() {
            Font::with_name("Segoe Fluent Icons")
        } else {
            Font::with_name("Segoe MDL2 Assets")
        };

        let mut app = Self {
            selected: config.scripts.first().map(|s| s.id),
            config,
            config_path,
            log_dir,
            draft: None,
            prompt: None,
            exe_found: None,
            running: HashMap::new(),
            pending,
            status: HashMap::new(),
            last_minute: None,
            window_id: None,
            hwnd: 0,
            notice: None,
            icon_font,
            _tray: tray,
            show_id: show_item.id().clone(),
            quit_id: quit_item.id().clone(),
            ctx_menu,
            cut_id: cut_item.id().clone(),
            copy_id: copy_item.id().clone(),
            paste_id: paste_item.id().clone(),
        };
        app.refresh_exe();

        (app, window::get_oldest().map(Message::GotWindowId))
    }

    // ───────── helpers ─────────

    fn save(&self) {
        if let Ok(json) = serde_json::to_string_pretty(&self.config) {
            let _ = fs::write(&self.config_path, json);
        }
    }

    fn notify(&mut self, msg: impl Into<String>, is_error: bool) {
        self.notice = Some((msg.into(), is_error, Instant::now()));
    }

    fn next_id(&self) -> u64 {
        let a = self.config.scripts.iter().map(|s| s.id).max().unwrap_or(0);
        let b = self.draft.as_ref().map(|d| d.script.id).unwrap_or(0);
        a.max(b) + 1
    }

    /// The script currently shown in the right panel.
    fn shown_script(&self) -> Option<&Script> {
        if let Some(d) = &self.draft {
            return Some(&d.script);
        }
        self.selected
            .and_then(|id| self.config.scripts.iter().find(|s| s.id == id))
    }

    fn refresh_exe(&mut self) {
        self.exe_found = self
            .shown_script()
            .map(|s| s.command.clone())
            .filter(|c| !c.trim().is_empty())
            .map(|c| find_executable(&c));
    }

    fn edit_draft(&mut self, f: impl FnOnce(&mut Script)) {
        if let Some(d) = &mut self.draft {
            f(&mut d.script);
        }
    }

    fn commit_draft(&mut self) -> bool {
        let Some(draft) = self.draft.clone() else {
            return true;
        };
        if draft.script.name.trim().is_empty() {
            self.notify("Name cannot be empty.", true);
            return false;
        }
        let s = draft.script;
        let id = s.id;
        match self.config.scripts.iter_mut().find(|x| x.id == id) {
            Some(existing) => *existing = s,
            None => self.config.scripts.push(s),
        }
        self.draft = None;
        self.selected = Some(id);
        self.save();
        self.notify("Saved.", false);
        true
    }

    fn apply_nav(&mut self, nav: Nav) {
        match nav {
            Nav::View(id) => self.selected = Some(id),
            Nav::Edit(id) => {
                if let Some(s) = self.config.scripts.iter().find(|s| s.id == id).cloned() {
                    self.draft = Some(Draft {
                        script: s.clone(),
                        is_new: false,
                        original: Some(s),
                    });
                    self.selected = Some(id);
                }
            }
            Nav::Add => {
                let id = self.next_id();
                self.draft = Some(Draft {
                    script: Script {
                        id,
                        ..Default::default()
                    },
                    is_new: true,
                    original: None,
                });
                self.selected = Some(id);
            }
        }
    }

    fn request_nav(&mut self, nav: Nav) {
        if let Some(d) = &self.draft {
            if let Nav::View(id) = nav {
                if id == d.script.id {
                    return; // already showing this entry
                }
            }
            if d.is_dirty() {
                self.prompt = Some(nav);
                return;
            }
            self.draft = None;
        }
        self.apply_nav(nav);
    }

    fn start_script(&mut self, id: u64) {
        if self.running.contains_key(&id) {
            return;
        }
        let Some(script) = self.config.scripts.iter().find(|s| s.id == id) else {
            return;
        };
        let stamp = Local::now().format("%H:%M:%S");
        match launch(script, &self.log_dir) {
            Ok(child) => {
                self.status
                    .insert(id, format!("Started {stamp} (PID {})", child.id()));
                self.running.insert(id, child);
            }
            Err(e) => {
                self.status.insert(id, format!("Failed to start {stamp}: {e}"));
            }
        }
    }

    fn kill_script(&mut self, id: u64) {
        if let Some(mut child) = self.running.remove(&id) {
            kill_tree(&mut child);
            self.status
                .insert(id, format!("Killed {}", Local::now().format("%H:%M:%S")));
        }
        self.pending.remove(&id);
    }

    fn kill_all(&mut self) {
        let ids: Vec<u64> = self.running.keys().copied().collect();
        for id in ids {
            self.kill_script(id);
        }
    }

    fn hide_window(&self) -> Task<Message> {
        match self.window_id {
            Some(id) => window::change_mode(id, window::Mode::Hidden),
            None => Task::none(),
        }
    }

    fn show_window(&self) -> Task<Message> {
        match self.window_id {
            Some(id) => Task::batch([
                window::change_mode(id, window::Mode::Windowed),
                window::minimize(id, false),
                window::gain_focus(id),
            ]),
            None => Task::none(),
        }
    }

    // ───────── update ─────────

    fn update(&mut self, message: Message) -> Task<Message> {
        let is_tick = matches!(message, Message::Tick);
        let mut task = Task::none();

        match message {
            Message::Tick => task = self.on_tick(),
            Message::GotWindowId(id) => {
                self.window_id = id;
                if let Some(id) = id {
                    task = window::run_with_handle(id, |handle| match handle.as_raw() {
                        raw_window_handle::RawWindowHandle::Win32(h) => h.hwnd.get(),
                        _ => 0isize,
                    })
                    .map(Message::GotHwnd);
                }
            }
            Message::GotHwnd(h) => {
                self.hwnd = h;
                #[cfg(windows)]
                if h != 0 {
                    // Lets the popup menu deliver its click events for this window.
                    unsafe {
                        self.ctx_menu.attach_menu_subclass_for_hwnd(h);
                    }
                }
            }
            Message::CloseRequested(_) | Message::HideToTray => task = self.hide_window(),
            Message::ShowWindow => task = self.show_window(),
            Message::Quit => {
                self.kill_all();
                return iced::exit();
            }
            Message::ToggleAutostart(on) => match set_autostart(on) {
                Ok(()) => {
                    self.config.start_with_windows = on;
                    self.save();
                }
                Err(e) => self.notify(format!("Autostart: {e}"), true),
            },

            // ── navigation / entries ──
            Message::Add => self.request_nav(Nav::Add),
            Message::View(id) => self.request_nav(Nav::View(id)),
            Message::EditBtn(id) => self.request_nav(Nav::Edit(id)),
            Message::SaveDraft => {
                self.commit_draft();
            }
            Message::DiscardDraft => {
                self.draft = None;
                if self
                    .selected
                    .map(|id| !self.config.scripts.iter().any(|s| s.id == id))
                    .unwrap_or(true)
                {
                    self.selected = self.config.scripts.first().map(|s| s.id);
                }
            }
            Message::PromptSave => {
                let nav = self.prompt.take();
                if self.commit_draft() {
                    if let Some(nav) = nav {
                        self.apply_nav(nav);
                    }
                }
            }
            Message::PromptDiscard => {
                self.draft = None;
                if let Some(nav) = self.prompt.take() {
                    self.apply_nav(nav);
                }
            }
            Message::PromptCancel => self.prompt = None,
            Message::Delete(id) => {
                self.kill_script(id);
                self.config.scripts.retain(|s| s.id != id);
                self.status.remove(&id);
                if self.draft.as_ref().is_some_and(|d| d.script.id == id) {
                    self.draft = None;
                }
                if self.selected == Some(id) {
                    self.selected = self.config.scripts.first().map(|s| s.id);
                }
                self.save();
            }
            Message::Start(id) => {
                self.pending.remove(&id);
                self.start_script(id);
            }
            Message::Kill(id) => self.kill_script(id),

            // ── field edits (draft only) ──
            Message::Name(v) => self.edit_draft(|s| s.name = v),
            Message::Command(v) => self.edit_draft(|s| s.command = v),
            Message::Args(v) => self.edit_draft(|s| s.args = v),
            Message::WorkDir(v) => self.edit_draft(|s| s.working_dir = v),
            Message::BrowseDir => {
                task = Task::perform(
                    async {
                        rfd::AsyncFileDialog::new()
                            .pick_folder()
                            .await
                            .map(|h| h.path().to_path_buf())
                    },
                    Message::DirPicked,
                );
            }
            Message::DirPicked(p) => {
                if let Some(p) = p {
                    self.edit_draft(|s| s.working_dir = p.display().to_string());
                }
            }
            Message::Enabled(v) => self.edit_draft(|s| s.enabled = v),
            Message::RunAtStart(v) => self.edit_draft(|s| s.run_at_start = v),
            Message::Delay(v) => {
                let v = v.trim();
                if v.is_empty() {
                    self.edit_draft(|s| s.start_delay_secs = 0);
                } else if let Ok(n) = v.parse::<u64>() {
                    self.edit_draft(|s| s.start_delay_secs = n);
                }
            }
            Message::Times(v) => self.edit_draft(|s| s.times = v),
            Message::Day(i, v) => self.edit_draft(|s| s.days[i] = v),
            Message::ShowConsole(v) => self.edit_draft(|s| s.show_console = v),

            // ── context menu ──
            Message::RightClick(f) => {
                // Focus the field and select its content, then show the menu.
                let id = field_id(f);
                task = text_input::focus(id.clone())
                    .chain(text_input::select_all(id))
                    .chain(Task::done(Message::ShowCtx));
            }
            Message::ShowCtx => {
                #[cfg(windows)]
                if self.hwnd != 0 {
                    // Blocks until the popup closes; the click arrives as a MenuEvent.
                    unsafe {
                        self.ctx_menu.show_context_menu_for_hwnd(self.hwnd, None);
                    }
                }
            }

            // ── import / export ──
            Message::Export => {
                let payload = ExportFile {
                    app: APP_NAME.into(),
                    version: 1,
                    scripts: self.config.scripts.clone(),
                };
                match serde_json::to_string_pretty(&payload) {
                    Ok(json) => {
                        let name = format!(
                            "Script-Manager_{}.json",
                            Local::now().format("%Y_%m_%d_%H_%M_%S")
                        );
                        task = Task::perform(export_dialog(name, json), Message::Exported);
                    }
                    Err(e) => self.notify(format!("Export failed: {e}"), true),
                }
            }
            Message::Exported(res) => match res {
                Ok(Some(name)) => self.notify(format!("Exported to {name}"), false),
                Ok(None) => {}
                Err(e) => self.notify(format!("Export failed: {e}"), true),
            },
            Message::Import => task = Task::perform(import_dialog(), Message::Imported),
            Message::Imported(res) => match res {
                Ok(Some(list)) => {
                    let count = list.len();
                    let mut id = self.next_id();
                    for mut s in list {
                        s.id = id;
                        id += 1;
                        self.config.scripts.push(s);
                    }
                    self.save();
                    self.notify(format!("Imported {count} script(s)."), false);
                }
                Ok(None) => {}
                Err(e) => self.notify(format!("Import failed: {e}"), true),
            },
        }

        if !is_tick {
            self.refresh_exe();
        }
        task
    }

    fn on_tick(&mut self) -> Task<Message> {
        let mut task = Task::none();

        // 0. Expire notices.
        if let Some((_, _, t)) = &self.notice {
            if t.elapsed() > Duration::from_secs(8) {
                self.notice = None;
            }
        }

        // 1. Tray + context-menu events.
        while let Ok(ev) = MenuEvent::receiver().try_recv() {
            if ev.id == self.show_id {
                task = Task::batch([task, self.show_window()]);
            } else if ev.id == self.quit_id {
                self.kill_all();
                return iced::exit();
            } else if ev.id == self.cut_id {
                send_ctrl_combo(b'X' as u16);
            } else if ev.id == self.copy_id {
                send_ctrl_combo(b'C' as u16);
            } else if ev.id == self.paste_id {
                send_ctrl_combo(b'V' as u16);
            }
        }
        while let Ok(ev) = TrayIconEvent::receiver().try_recv() {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = ev
            {
                task = Task::batch([task, self.show_window()]);
            }
        }

        // 2. Reap finished processes.
        let mut finished = Vec::new();
        for (id, child) in self.running.iter_mut() {
            let stamp = Local::now().format("%H:%M:%S");
            match child.try_wait() {
                Ok(Some(st)) => finished.push((*id, format!("Exited at {stamp} ({st})"))),
                Ok(None) => {}
                Err(e) => finished.push((*id, format!("Lost track of process: {e}"))),
            }
        }
        for (id, msg) in finished {
            self.running.remove(&id);
            self.status.insert(id, msg);
        }

        // 3. Delayed startup launches.
        let now_i = Instant::now();
        let ready: Vec<u64> = self
            .pending
            .iter()
            .filter(|(_, t)| **t <= now_i)
            .map(|(id, _)| *id)
            .collect();
        for id in ready {
            self.pending.remove(&id);
            self.start_script(id);
        }

        // 4. Time-of-day schedule (evaluated once per minute, uses saved config).
        let now = Local::now();
        let key = (now.year(), now.ordinal(), now.hour(), now.minute());
        if self.last_minute != Some(key) {
            self.last_minute = Some(key);
            let wd = now.weekday().num_days_from_monday() as usize;
            let due: Vec<u64> = self
                .config
                .scripts
                .iter()
                .filter(|s| {
                    s.enabled
                        && s.days[wd]
                        && s.parsed_times().contains(&(now.hour(), now.minute()))
                })
                .map(|s| s.id)
                .collect();
            for id in due {
                self.start_script(id);
            }
        }

        task
    }

    fn subscription(&self) -> Subscription<Message> {
        Subscription::batch([
            iced::time::every(Duration::from_millis(100)).map(|_| Message::Tick),
            window::close_requests().map(Message::CloseRequested),
        ])
    }

    // ───────── view ─────────

    fn glyph<'a>(&self, code: char) -> iced::widget::Text<'a> {
        text(code.to_string()).font(self.icon_font)
    }

    fn view(&self) -> Element<'_, Message> {
        let top = row![
            text("Script Manager").size(24),
            horizontal_space(),
            button("Import").on_press(Message::Import),
            button("Export").on_press(Message::Export),
            button("Hide to tray").on_press(Message::HideToTray),
            button("Quit").style(button::danger).on_press(Message::Quit),
        ]
        .spacing(8)
        .align_y(Alignment::Center);

        let second = row![
            button("+ Add script").on_press(Message::Add),
            horizontal_space(),
            checkbox("Start with Windows", self.config.start_with_windows)
                .on_toggle(Message::ToggleAutostart),
        ]
        .align_y(Alignment::Center);

        // Left: list of entries.
        let mut list = column![].spacing(8);
        if let Some(d) = &self.draft {
            if d.is_new {
                list = list.push(self.script_card(&d.script));
            }
        }
        for s in &self.config.scripts {
            let shown = match &self.draft {
                Some(d) if d.script.id == s.id => &d.script,
                _ => s,
            };
            list = list.push(self.script_card(shown));
        }
        let scroll_pad = Padding {
            right: 14.0,
            ..Padding::ZERO
        };
        let left = scrollable(container(list).padding(scroll_pad))
            .height(Length::Fill)
            .width(Length::FillPortion(5));

        // Right: editor (draft) or read-only details.
        let right_content: Element<Message> = match (&self.draft, self.shown_script()) {
            (Some(d), _) => self.editor(&d.script, true),
            (None, Some(s)) => self.editor(s, false),
            _ => text("Select an entry, or press “+ Add script”.").into(),
        };
        let right = container(scrollable(
            container(right_content).padding(scroll_pad).width(Length::Fill),
        ))
        .padding(10)
        .width(Length::FillPortion(6))
        .height(Length::Fill)
        .style(container::bordered_box);

        let mut page = column![top, second].spacing(10).padding(14);
        if let Some((msg, is_err, _)) = &self.notice {
            page = page.push(text(msg).size(13).color(if *is_err { RED } else { GREEN }));
        }
        let page = page.push(row![left, right].spacing(14).height(Length::Fill));

        if self.prompt.is_some() {
            stack![page, self.prompt_view()].into()
        } else {
            page.into()
        }
    }

    fn prompt_view(&self) -> Element<'_, Message> {
        let name = self
            .draft
            .as_ref()
            .map(|d| d.script.name.as_str())
            .unwrap_or("This entry");
        let dialog = container(
            column![
                text("Unsaved changes").size(20),
                text(format!("“{name}” has changes that are not saved.")),
                row![
                    button("Save")
                        .style(button::success)
                        .on_press(Message::PromptSave),
                    button("Discard")
                        .style(button::danger)
                        .on_press(Message::PromptDiscard),
                    button("Cancel")
                        .style(button::secondary)
                        .on_press(Message::PromptCancel),
                ]
                .spacing(8),
            ]
            .spacing(14),
        )
        .padding(20)
        .width(440)
        .style(container::rounded_box);

        opaque(
            container(dialog)
                .center_x(Length::Fill)
                .center_y(Length::Fill)
                .style(|_t: &Theme| container::Style {
                    background: Some(Background::Color(Color {
                        a: 0.6,
                        ..Color::BLACK
                    })),
                    ..container::Style::default()
                }),
        )
    }

    fn script_card<'a>(&'a self, s: &'a Script) -> Element<'a, Message> {
        let running = self.running.contains_key(&s.id);
        let editing = self.draft.as_ref().is_some_and(|d| d.script.id == s.id);
        let unsaved = editing && self.draft.as_ref().is_some_and(|d| d.is_new);
        let viewing = self.selected == Some(s.id) && !editing;

        let (bg_normal, bg_edit, border_col) = if s.enabled && s.run_at_start {
            STARTUP_ON
        } else {
            STARTUP_OFF
        };
        let bg = if editing { bg_edit } else { bg_normal };
        let border_w = if editing || viewing { 2.5 } else { 1.0 };

        let marker: Element<Message> = if editing {
            self.glyph(ICON_EDIT).into()
        } else if viewing {
            self.glyph(ICON_VIEW).into()
        } else {
            text("").into()
        };

        let state: Element<Message> = if unsaved {
            text("unsaved").color(ORANGE).into()
        } else if running {
            text("● Running").color(GREEN).into()
        } else if let Some(t) = self.pending.get(&s.id) {
            let left = t.saturating_duration_since(Instant::now()).as_secs();
            row![self.glyph(ICON_CLOCK), text(format!(" starts in {left}s"))].into()
        } else {
            text("○ Stopped").color(GRAY).into()
        };

        let status = self.status.get(&s.id).map(String::as_str).unwrap_or("");

        let delete_msg = if unsaved {
            Message::DiscardDraft
        } else {
            Message::Delete(s.id)
        };

        let content = column![
            row![
                container(marker).width(26),
                text(&s.name).size(18),
                horizontal_space(),
                state
            ]
            .align_y(Alignment::Center),
            text(s.summary()).size(12),
            text(status).size(12).color(GRAY),
            row![
                button("Start")
                    .style(button::success)
                    .on_press_maybe((!running && !unsaved).then_some(Message::Start(s.id))),
                button("Kill")
                    .style(button::danger)
                    .on_press_maybe(running.then_some(Message::Kill(s.id))),
                button("Edit")
                    .style(button::secondary)
                    .on_press_maybe((!editing).then_some(Message::EditBtn(s.id))),
                horizontal_space(),
                button(if unsaved { "Discard" } else { "Delete" })
                    .style(button::secondary)
                    .on_press(delete_msg),
            ]
            .spacing(6),
        ]
        .spacing(6);

        let card = container(content)
            .padding(10)
            .width(Length::Fill)
            .style(move |_t: &Theme| container::Style {
                background: Some(Background::Color(bg)),
                border: Border {
                    color: border_col,
                    width: border_w,
                    radius: 6.0.into(),
                },
                ..container::Style::default()
            });

        // Clicking the card body (not its buttons) shows the entry read-only.
        mouse_area(card)
            .on_press(Message::View(s.id))
            .interaction(iced::mouse::Interaction::Pointer)
            .into()
    }

    fn editor<'a>(&'a self, s: &'a Script, editable: bool) -> Element<'a, Message> {
        let title = if editable {
            if self.draft.as_ref().is_some_and(|d| d.is_new) {
                "New script"
            } else {
                "Edit script"
            }
        } else {
            "Script details (read-only)"
        };

        // Name (+ Save / Discard while editing)
        let name_input = tinput(Field::Name, "My nightly job", &s.name, editable, Message::Name);
        let name_row: Element<Message> = if editable {
            row![
                name_input,
                button("Save")
                    .style(button::success)
                    .on_press(Message::SaveDraft),
                button("Discard")
                    .style(button::secondary)
                    .on_press(Message::DiscardDraft),
            ]
            .spacing(6)
            .into()
        } else {
            name_input
        };

        // Command label reflects whether the executable was found in PATH.
        let cmd_label: Element<Message> = match self.exe_found {
            Some(true) => row![
                text("Command (").size(13),
                text("found").size(13).color(GREEN),
                text(")").size(13),
            ]
            .into(),
            Some(false) => row![
                text("Command (").size(13),
                text("not found").size(13).color(RED),
                text(")").size(13),
            ]
            .into(),
            None => text("Command (executable)").size(13).into(),
        };

        let workdir_row: Element<Message> = {
            let input = tinput(Field::WorkDir, r"C:\scripts", &s.working_dir, editable, Message::WorkDir);
            if editable {
                row![input, button("Browse…").on_press(Message::BrowseDir)]
                    .spacing(6)
                    .into()
            } else {
                input
            }
        };

        let days = DAY_NAMES
            .iter()
            .enumerate()
            .fold(row![].spacing(10), |r, (i, n)| {
                r.push(
                    checkbox(*n, s.days[i])
                        .on_toggle_maybe(editable.then_some(move |b| Message::Day(i, b))),
                )
            });

        let times_hint = if s.has_invalid_times() {
            "⚠ Some entries are not valid HH:MM (24h) and will be ignored"
        } else {
            "Comma separated, 24h local time, e.g. 08:30, 17:00"
        };

        column![
            text(title).size(20),
            checkbox("Enabled (allows automatic launches)", s.enabled)
                .on_toggle_maybe(editable.then_some(Message::Enabled)),
            field("Name", name_row),
            field_el(
                cmd_label,
                tinput(
                    Field::Command,
                    "py   |   pwsh   |   cmd   |   node",
                    &s.command,
                    editable,
                    Message::Command
                )
            ),
            field(
                "Arguments",
                tinput(
                    Field::Args,
                    r#"-u "C:\scripts\job.py" --verbose"#,
                    &s.args,
                    editable,
                    Message::Args
                )
            ),
            field("Start in (working directory)", workdir_row),
            checkbox(
                "Show console window (otherwise hidden, output → log file)",
                s.show_console
            )
            .on_toggle_maybe(editable.then_some(Message::ShowConsole)),
            text("Triggers").size(20),
            checkbox("Launch when the manager starts", s.run_at_start)
                .on_toggle_maybe(editable.then_some(Message::RunAtStart)),
            field(
                "Startup delay (seconds)",
                tinput(
                    Field::Delay,
                    "0",
                    &s.start_delay_secs.to_string(),
                    editable,
                    Message::Delay
                )
            ),
            field(
                "Launch at specific times",
                column![
                    tinput(Field::Times, "08:30, 17:00", &s.times, editable, Message::Times),
                    text(times_hint).size(12),
                ]
                .spacing(4)
            ),
            field("On these days", days),
            text(format!("Logs: {}", self.log_dir.display())).size(12),
        ]
        .spacing(10)
        .into()
    }
}

/// Text input that is read-only when `editable` is false. Editable inputs get a
/// right-click handler that opens the Cut / Copy / Paste menu.
fn tinput<'a>(
    f: Field,
    placeholder: &str,
    value: &str,
    editable: bool,
    on_input: fn(String) -> Message,
) -> Element<'a, Message> {
    let input = text_input(placeholder, value)
        .id(field_id(f))
        .on_input_maybe(editable.then_some(on_input));
    if editable {
        mouse_area(input)
            .on_right_press(Message::RightClick(f))
            .into()
    } else {
        input.into()
    }
}

fn field<'a>(label: &'a str, input: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    field_el(text(label).size(13), input)
}

fn field_el<'a>(
    label: impl Into<Element<'a, Message>>,
    input: impl Into<Element<'a, Message>>,
) -> Element<'a, Message> {
    column![label.into(), input.into()].spacing(4).into()
}
