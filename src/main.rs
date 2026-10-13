// Hide the console window in release builds on Windows.
#![cfg_attr(all(not(debug_assertions), windows), windows_subsystem = "windows")]

use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use chrono::{Datelike, Local, Timelike};
use iced::widget::{
    button, checkbox, column, container, horizontal_space, row, scrollable, text, text_input,
};
use iced::{window, Alignment, Color, Element, Length, Size, Subscription, Task, Theme};
use serde::{Deserialize, Serialize};
use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

const APP_NAME: &str = "ScriptManager";
const DAY_NAMES: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];

// ───────────────────────────── Data model ─────────────────────────────

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
struct Script {
    id: u64,
    name: String,
    /// Executable that runs the script, e.g. `py`, `pwsh`, `cmd`, `node`.
    command: String,
    /// Raw argument string, exactly as you would type it in a terminal,
    /// e.g. `-u "C:\tools\job.py" --flag`.
    args: String,
    /// Working directory (PWD). Empty = inherit from the manager.
    working_dir: String,
    enabled: bool,
    run_at_start: bool,
    start_delay_secs: u64,
    /// Comma separated `HH:MM` list (24h, local time), e.g. `08:30, 17:00`.
    times: String,
    /// Days on which the `times` fire (Mon..Sun).
    days: [bool; 7],
    /// true = give the script its own visible console; false = hidden, output goes to a log file.
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
}

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct Config {
    scripts: Vec<Script>,
    start_with_windows: bool,
}

// ───────────────────────────── Persistence ─────────────────────────────

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
            // Pass arguments verbatim, so quoting works like in cmd/PowerShell.
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

// ───────────────────────────── Windows autostart ─────────────────────────────

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

// ───────────────────────────── Tray icon ─────────────────────────────

fn make_icon() -> Icon {
    let size = 32u32;
    let mut rgba = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let (dx, dy) = (x as f32 - 15.5, y as f32 - 15.5);
            let inside_circle = (dx * dx + dy * dy).sqrt() <= 14.5;
            // White "play" triangle in the middle.
            let in_triangle = (11..=23).contains(&x)
                && (dy.abs() <= (23.0 - x as f32) * 0.8);
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
    running: HashMap<u64, Child>,
    pending: HashMap<u64, Instant>,
    status: HashMap<u64, String>,
    last_minute: Option<(i32, u32, u32, u32)>,
    window_id: Option<window::Id>,
    notice: Option<String>,
    // Tray
    _tray: TrayIcon,
    show_id: MenuId,
    quit_id: MenuId,
}

#[derive(Debug, Clone)]
enum Message {
    Tick,
    GotWindowId(Option<window::Id>),
    CloseRequested(window::Id),
    HideToTray,
    ShowWindow,
    Quit,
    ToggleAutostart(bool),

    Add,
    Select(u64),
    Delete(u64),
    Start(u64),
    Kill(u64),

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
}

fn main() -> iced::Result {
    let minimized = std::env::args().any(|a| a == "--minimized");

    iced::application("Script Manager", App::update, App::view)
        .subscription(App::subscription)
        .theme(|_| Theme::Dark)
        .window(window::Settings {
            size: Size::new(980.0, 640.0),
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

        // Schedule "run at manager start" scripts (with their delay).
        let now = Instant::now();
        let pending = config
            .scripts
            .iter()
            .filter(|s| s.enabled && s.run_at_start)
            .map(|s| (s.id, now + Duration::from_secs(s.start_delay_secs)))
            .collect();

        let app = Self {
            selected: config.scripts.first().map(|s| s.id),
            config,
            config_path,
            log_dir,
            running: HashMap::new(),
            pending,
            status: HashMap::new(),
            last_minute: None,
            window_id: None,
            notice: None,
            _tray: tray,
            show_id: show_item.id().clone(),
            quit_id: quit_item.id().clone(),
        };

        (app, window::get_oldest().map(Message::GotWindowId))
    }

    fn save(&self) {
        if let Ok(json) = serde_json::to_string_pretty(&self.config) {
            let _ = fs::write(&self.config_path, json);
        }
    }

    fn selected_mut(&mut self) -> Option<&mut Script> {
        let id = self.selected?;
        self.config.scripts.iter_mut().find(|s| s.id == id)
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
        let mut dirty = true;
        let mut task = Task::none();

        match message {
            Message::Tick => {
                dirty = false;
                task = self.on_tick();
            }
            Message::GotWindowId(id) => {
                dirty = false;
                self.window_id = id;
            }
            Message::CloseRequested(_) | Message::HideToTray => {
                dirty = false;
                task = self.hide_window();
            }
            Message::ShowWindow => {
                dirty = false;
                task = self.show_window();
            }
            Message::Quit => {
                self.kill_all();
                return iced::exit();
            }
            Message::ToggleAutostart(on) => match set_autostart(on) {
                Ok(()) => {
                    self.config.start_with_windows = on;
                    self.notice = None;
                }
                Err(e) => self.notice = Some(format!("Autostart: {e}")),
            },

            Message::Add => {
                let id = self.config.scripts.iter().map(|s| s.id).max().unwrap_or(0) + 1;
                self.config.scripts.push(Script {
                    id,
                    ..Default::default()
                });
                self.selected = Some(id);
            }
            Message::Select(id) => {
                dirty = false;
                self.selected = Some(id);
            }
            Message::Delete(id) => {
                self.kill_script(id);
                self.config.scripts.retain(|s| s.id != id);
                self.status.remove(&id);
                if self.selected == Some(id) {
                    self.selected = self.config.scripts.first().map(|s| s.id);
                }
            }
            Message::Start(id) => {
                dirty = false;
                self.pending.remove(&id);
                self.start_script(id);
            }
            Message::Kill(id) => {
                dirty = false;
                self.kill_script(id);
            }

            Message::Name(v) => self.edit(|s| s.name = v),
            Message::Command(v) => self.edit(|s| s.command = v),
            Message::Args(v) => self.edit(|s| s.args = v),
            Message::WorkDir(v) => self.edit(|s| s.working_dir = v),
            Message::BrowseDir => {
                dirty = false;
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
                    self.edit(|s| s.working_dir = p.display().to_string());
                }
            }
            Message::Enabled(v) => self.edit(|s| s.enabled = v),
            Message::RunAtStart(v) => self.edit(|s| s.run_at_start = v),
            Message::Delay(v) => {
                let v = v.trim();
                if v.is_empty() {
                    self.edit(|s| s.start_delay_secs = 0);
                } else if let Ok(n) = v.parse::<u64>() {
                    self.edit(|s| s.start_delay_secs = n);
                }
            }
            Message::Times(v) => self.edit(|s| s.times = v),
            Message::Day(i, v) => self.edit(|s| s.days[i] = v),
            Message::ShowConsole(v) => self.edit(|s| s.show_console = v),
        }

        if dirty {
            self.save();
        }
        task
    }

    fn edit(&mut self, f: impl FnOnce(&mut Script)) {
        if let Some(s) = self.selected_mut() {
            f(s);
        }
    }

    fn on_tick(&mut self) -> Task<Message> {
        let mut task = Task::none();

        // 1. Tray events.
        while let Ok(ev) = MenuEvent::receiver().try_recv() {
            if ev.id == self.show_id {
                task = Task::batch([task, self.show_window()]);
            } else if ev.id == self.quit_id {
                self.kill_all();
                return iced::exit();
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

        // 4. Time-of-day schedule (evaluated once per minute).
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
            iced::time::every(Duration::from_millis(250)).map(|_| Message::Tick),
            window::close_requests().map(Message::CloseRequested),
        ])
    }

    // ───────── view ─────────

    fn view(&self) -> Element<'_, Message> {
        let top = row![
            text("Script Manager").size(24),
            horizontal_space(),
            checkbox("Start with Windows", self.config.start_with_windows)
                .on_toggle(Message::ToggleAutostart),
            button("Hide to tray").on_press(Message::HideToTray),
            button("Quit").style(button::danger).on_press(Message::Quit),
        ]
        .spacing(12)
        .align_y(Alignment::Center);

        let mut list = column![].spacing(8);
        for s in &self.config.scripts {
            list = list.push(self.script_card(s));
        }
        let left = column![
            button("+ Add script").on_press(Message::Add),
            scrollable(list).height(Length::Fill),
        ]
        .spacing(10)
        .width(Length::FillPortion(5));

        let right: Element<Message> = match self
            .selected
            .and_then(|id| self.config.scripts.iter().find(|s| s.id == id))
        {
            Some(s) => self.editor(s),
            None => text("Add or select a script to edit it.").into(),
        };
        let right = container(scrollable(right))
            .width(Length::FillPortion(6))
            .padding(10)
            .style(container::bordered_box);

        let mut page = column![top].spacing(12).padding(14);
        if let Some(n) = &self.notice {
            page = page.push(text(n).color(Color::from_rgb(0.95, 0.5, 0.3)));
        }
        page.push(row![left, right].spacing(14).height(Length::Fill))
            .into()
    }

    fn script_card<'a>(&'a self, s: &'a Script) -> Element<'a, Message> {
        let running = self.running.contains_key(&s.id);
        let state: Element<Message> = if running {
            text("● Running")
                .color(Color::from_rgb(0.25, 0.8, 0.35))
                .into()
        } else if let Some(t) = self.pending.get(&s.id) {
            let left = t.saturating_duration_since(Instant::now()).as_secs();
            text(format!("⏳ starts in {left}s")).into()
        } else {
            text("○ Stopped")
                .color(Color::from_rgb(0.6, 0.6, 0.6))
                .into()
        };
        let marker = if self.selected == Some(s.id) { "▶ " } else { "" };
        let status = self.status.get(&s.id).map(String::as_str).unwrap_or("");
        let disabled = if s.enabled { "" } else { "  (disabled)" };

        container(
            column![
                row![
                    text(format!("{marker}{}{disabled}", s.name)).size(18),
                    horizontal_space(),
                    state
                ]
                .align_y(Alignment::Center),
                text(status).size(12),
                row![
                    button("Start")
                        .style(button::success)
                        .on_press_maybe((!running).then_some(Message::Start(s.id))),
                    button("Kill")
                        .style(button::danger)
                        .on_press_maybe(running.then_some(Message::Kill(s.id))),
                    button("Edit")
                        .style(button::secondary)
                        .on_press(Message::Select(s.id)),
                    horizontal_space(),
                    button("Delete")
                        .style(button::secondary)
                        .on_press(Message::Delete(s.id)),
                ]
                .spacing(6),
            ]
            .spacing(6),
        )
        .padding(10)
        .width(Length::Fill)
        .style(container::bordered_box)
        .into()
    }

    fn editor<'a>(&'a self, s: &'a Script) -> Element<'a, Message> {
        let days = DAY_NAMES.iter().enumerate().fold(row![].spacing(10), |r, (i, n)| {
            r.push(checkbox(*n, s.days[i]).on_toggle(move |b| Message::Day(i, b)))
        });

        let times_hint = if s.has_invalid_times() {
            "⚠ Some entries are not valid HH:MM (24h) and will be ignored"
        } else {
            "Comma separated, 24h local time, e.g. 08:30, 17:00"
        };

        column![
            text("Script settings").size(20),
            checkbox("Enabled (allows automatic launches)", s.enabled).on_toggle(Message::Enabled),
            field(
                "Name",
                text_input("My nightly job", &s.name).on_input(Message::Name)
            ),
            field(
                "Command (executable)",
                text_input("py   |   pwsh   |   cmd   |   node", &s.command)
                    .on_input(Message::Command)
            ),
            field(
                "Arguments",
                text_input(r#"-u "C:\scripts\job.py" --verbose"#, &s.args).on_input(Message::Args)
            ),
            field(
                "Start in (working directory)",
                row![
                    text_input(r"C:\scripts", &s.working_dir).on_input(Message::WorkDir),
                    button("Browse…").on_press(Message::BrowseDir),
                ]
                .spacing(6)
            ),
            checkbox("Show console window (otherwise hidden, output → log file)", s.show_console)
                .on_toggle(Message::ShowConsole),
            text("Triggers").size(20),
            checkbox("Launch when the manager starts", s.run_at_start)
                .on_toggle(Message::RunAtStart),
            field(
                "Startup delay (seconds)",
                text_input("0", &s.start_delay_secs.to_string()).on_input(Message::Delay)
            ),
            field(
                "Launch at specific times",
                column![
                    text_input("08:30, 17:00", &s.times).on_input(Message::Times),
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

fn field<'a>(label: &'a str, input: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    column![text(label).size(13), input.into()].spacing(4).into()
}