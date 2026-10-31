//! The main window: the colour panels, and the link that keeps them in sync with CSP.

use std::collections::VecDeque;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use eframe::egui::text_selection::LabelSelectionState;
use eframe::egui::{self, Color32, RichText};

use crate::color::ColorState;
use crate::csp::auth::{parse_qr_url, Pairing};
use crate::csp::client::CspSlots;
use crate::csp::worker::{Command, Event, Worker};
use crate::csp::{Rgb, Slot};
use crate::frame::{self, CORNER_RADIUS, TITLE_BAR_HEIGHT, TITLE_BUTTON_WIDTH};
use crate::picker::{app_icon_pixels, switch, IconButton, Picker};
use crate::platform::{self, click_csp_title_bar, pen_or_mouse_down, PaletteWindow};
use crate::session::{CloseAction, SavedColors, SavedLayout, SavedSettings};
use crate::tray::{self, Tray};
use crate::update::{self, Updater};
use crate::{qr, session};

/// A read-back matching a colour sent this recently is our own echo, not an edit
/// made in CSP, so it must not make the sliders jump back.
const ECHO_WINDOW: Duration = Duration::from_millis(1500);
/// While the user is moving a slider, CSP's colour never overrides it.
const EDIT_HOLD: Duration = Duration::from_millis(250);
const SCAN_INTERVAL: Duration = Duration::from_millis(700);
const LOG_LINES: usize = 200;
/// Longer than the default double-click time, so that real clicks on CSP's title
/// bar can never pair up and un-maximise it.
const RESTORE_MIN_INTERVAL: Duration = Duration::from_millis(700);
/// A click that lands as a stroke begins draws a straight line from the title
/// bar, so once the pen is seen down, wait for this long a pause in the strokes.
const RESTORE_QUIET: Duration = Duration::from_millis(400);
/// Short enough not to miss the quick taps of hatching between two checks.
const RESTORE_POLL: Duration = Duration::from_millis(30);
const WARNING: Color32 = Color32::from_rgb(220, 160, 60);
/// After choosing a slot here, CSP's report may lag; do not let it undo the choice.
const SLOT_HOLD: Duration = Duration::from_millis(300);
/// Relative widths of the foreground, background and transparent slots.
const SLOT_WIDTHS: [f32; 3] = [5.0, 2.0, 2.0];
const MIN_SLOT_CHIP_HEIGHT: f32 = 18.0;
/// Below the colour slots, the row with the HEX field.
const HEX_ROW_ROOM: f32 = 25.0;
const SLOT_CHIP_GAP: f32 = 6.0;
/// The active slot's outline sits around the colour, with this much room for it.
const SLOT_OUTLINE_ROOM: f32 = 3.0;
const SLOT_CORNER: f32 = 5.0;
const HEX_WIDTH: f32 = 76.0;
const STATUS_DOT_SIZE: f32 = 9.0;
const CONNECTED: Color32 = Color32::from_rgb(70, 180, 100);
const WAITING: Color32 = Color32::from_rgb(205, 180, 40);
const DISCONNECTED: Color32 = Color32::from_gray(150);
const SETTINGS_TITLE: &str = "Percepta 设置";
const SETTINGS_SIZE: [f32; 2] = [320.0, 520.0];
const SETTINGS_MIN_SIZE: [f32; 2] = [240.0, 300.0];
/// A hidden window only needs to keep up with CSP's reports now and then.
const HIDDEN_REPAINT_INTERVAL: Duration = Duration::from_secs(1);
const TITLE_BAR_FILL: Color32 = Color32::from_gray(232);
const TITLE_TEXT: Color32 = Color32::from_gray(90);
const TITLE_FONT_SIZE: f32 = 12.0;
const WINDOW_EDGE: Color32 = Color32::from_gray(150);
const WINDOW_EDGE_WIDTH: f32 = 1.0;
const CONTENT_MARGIN: f32 = 8.0;
const PANEL_PADDING: f32 = 3.0;
/// How far from the line between two panels it can be grabbed.
const SPLITTER_GRAB: f32 = 4.0;
const SPLITTER_ACTIVE: Color32 = Color32::from_gray(120);
const PANEL_COUNT: usize = 3;
/// The colour slots, the wheel and the sliders.
const MIN_PANEL_HEIGHTS: [f32; PANEL_COUNT] = [60.0, 200.0, 150.0];
const DEFAULT_PANEL_HEIGHTS: [f32; PANEL_COUNT] = [72.0, 340.0, 160.0];
pub const DEFAULT_WINDOW_HEIGHT: f32 =
    TITLE_BAR_HEIGHT + DEFAULT_PANEL_HEIGHTS[0] + DEFAULT_PANEL_HEIGHTS[1] + DEFAULT_PANEL_HEIGHTS[2];
pub const MIN_WINDOW_HEIGHT: f32 =
    TITLE_BAR_HEIGHT + MIN_PANEL_HEIGHTS[0] + MIN_PANEL_HEIGHTS[1] + MIN_PANEL_HEIGHTS[2];
/// Room beside the address field for its button.
const CONNECT_BUTTON_ROOM: f32 = 52.0;
const LOG_HEIGHT: f32 = 160.0;
const CLOSE_DIALOG_WIDTH: f32 = 220.0;
const CHECKER_SIZE: f32 = 6.0;
const CHECKER_LIGHT: Color32 = Color32::from_gray(235);
const CHECKER_DARK: Color32 = Color32::from_gray(190);
const ACTIVE_SLOT_WIDTH: f32 = 2.0;
/// The mark on the settings button when an update is waiting.
const UPDATE_DOT_RADIUS: f32 = 3.0;

/// Things are saved once they stop changing, not on every step of a drag.
const SAVE_DELAY: Duration = Duration::from_secs(1);
const SETTINGS_ICON: &str = include_str!("../assets/settings.svg");
const CLOSE_ICON: &str = include_str!("../assets/close.svg");

/// A value that is written to disk some time after it last changed.
struct Saved<T> {
    on_disk: T,
    due: Option<Instant>,
}

impl<T: Copy + PartialEq> Saved<T> {
    fn new(on_disk: T) -> Self {
        Self { on_disk, due: None }
    }

    /// Returns whether `current` should be written now.
    fn is_due(&mut self, current: T, ctx: &egui::Context) -> bool {
        if current == self.on_disk {
            self.due = None;
            return false;
        }
        let now = Instant::now();
        let at = *self.due.get_or_insert(now + SAVE_DELAY);
        if now < at {
            ctx.request_repaint_after(at - now);
            return false;
        }
        true
    }

    fn written(&mut self, value: T) {
        self.on_disk = value;
        self.due = None;
    }
}

/// The colour slots as shown here, which work the same with or without CSP.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Slots {
    active: Slot,
    /// Main or Sub: where a colour edit goes while transparent is selected.
    last_color: Slot,
    main: Rgb,
    sub: Rgb,
}

impl From<SavedColors> for Slots {
    fn from(saved: SavedColors) -> Self {
        // Only Main or Sub can hold a colour edit, whatever the file says.
        let last_color = if saved.last_color == Slot::Sub { Slot::Sub } else { Slot::Main };
        Self { active: saved.active, last_color, main: saved.main, sub: saved.sub }
    }
}

impl From<Slots> for SavedColors {
    fn from(slots: Slots) -> Self {
        Self { active: slots.active, last_color: slots.last_color, main: slots.main, sub: slots.sub }
    }
}

impl Slots {
    fn color(&self, slot: Slot) -> Option<Rgb> {
        match slot {
            Slot::Main => Some(self.main),
            Slot::Sub => Some(self.sub),
            Slot::Transparent => None,
        }
    }

    fn select(&mut self, slot: Slot) {
        self.active = slot;
        if slot != Slot::Transparent {
            self.last_color = slot;
        }
    }

    /// A colour edit always lands in a colour slot, as transparent has no value.
    fn store_edit(&mut self, rgb: Rgb) {
        self.select(self.last_color);
        match self.last_color {
            Slot::Sub => self.sub = rgb,
            _ => self.main = rgb,
        }
    }
}

/// Enough of the common CJK range for this UI; first match wins.
const FONT_CANDIDATES: [&str; 3] = [
    "C:/Windows/Fonts/msyh.ttc",
    "C:/Windows/Fonts/simhei.ttf",
    "C:/Windows/Fonts/simsun.ttc",
];

#[derive(PartialEq, Eq)]
enum Link {
    Idle,
    Scanning,
    Connecting,
    Connected,
    Reconnecting,
}

enum ScanResult {
    Found(Pairing),
    NotFound,
    Failed(String),
}

/// A window's turn with the keyboard, which it otherwise leaves to CSP.
#[derive(Default)]
struct LentKeyboard {
    typing: bool,
    /// The window has actually become active since typing started.
    active: bool,
    /// The user went off to another program, where their click has done the restoring.
    left: bool,
    /// The user pressed on the other window, which can't take the keyboard by that alone.
    end_requested: bool,
}

pub struct App {
    window: Option<PaletteWindow>,
    keyboard: LentKeyboard,
    settings_keyboard: LentKeyboard,
    /// Typing ended without the user leaving, so CSP needs the click that makes it resume.
    restore_after_typing: bool,
    settings: SavedSettings,
    saved_settings: Saved<SavedSettings>,
    updater: Updater,
    restore_at: Option<Instant>,
    last_restore: Option<Instant>,

    link: Link,
    worker: Option<Worker>,
    events: Receiver<Event>,
    event_tx: Sender<Event>,
    scan_rx: Option<Receiver<ScanResult>>,

    color: ColorState,
    picker: Picker,
    /// What CSP should currently hold, so drags within one byte send nothing.
    last_sent: Rgb,
    hex: String,
    last_edit: Option<Instant>,
    recent_sent: VecDeque<(Rgb, Instant)>,
    slots: Slots,
    last_slot_choice: Option<Instant>,
    saved_slots: Saved<Slots>,
    saved_layout: Saved<SavedLayout>,
    settings_open: bool,
    /// The settings window, once it has been found and given its frame.
    settings_hwnd: Option<isize>,
    settings_position: Option<[i32; 2]>,
    settings_offset: Option<[i32; 2]>,
    settings_size: Option<[i32; 2]>,
    settings_icon: IconButton,
    /// One for each window, as they can sit on monitors of different scales.
    close_icon: IconButton,
    settings_close_icon: IconButton,
    tray: Option<Tray>,
    close_dialog_open: bool,
    /// Set once the app is really going, so that closing no longer goes to the tray.
    quitting: bool,
    confirm_update: bool,
    panel_heights: [f32; PANEL_COUNT],
    dragging_splitter: bool,

    blocked: bool,
    csp_in_front: bool,
    generation: String,
    reads_per_second: u32,
    read_ms: f32,
    read_max_ms: f32,
    write_ms: f32,
    writes: u32,
    rejected: u32,

    url_input: String,
    log: VecDeque<String>,
    saved: Option<Pairing>,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        install_cjk_font(&cc.egui_ctx);
        // Labels here are controls' captions, not text anyone reads to copy. Selecting
        // them only gets in the way of a pen. Text fields are not affected.
        cc.egui_ctx.all_styles_mut(|style| style.interaction.selectable_labels = false);
        let (event_tx, events) = mpsc::channel();
        let slots = session::load_colors().map_or(
            Slots { active: Slot::Main, last_color: Slot::Main, main: [18, 52, 86], sub: [0, 0, 0] },
            Slots::from,
        );
        let rgb = slots.color(slots.last_color).unwrap_or(slots.main);
        let window = PaletteWindow::new(cc);
        let mut picker = Picker::default();
        let (wheel, slider) = picker.modes();
        let layout =
            session::load_layout().unwrap_or(SavedLayout {
                position: None,
                size: None,
                wheel,
                slider,
                panels: None,
                settings_offset: None,
                settings_size: None,
            });
        picker.set_modes(layout.wheel, layout.slider);
        let panel_heights = layout.panels.map_or(DEFAULT_PANEL_HEIGHTS, |heights| heights.map(|height| height as f32));
        let tray = Tray::new(&cc.egui_ctx, app_icon_pixels);
        if let Some(window) = &window {
            if let Some(position) = layout.position {
                window.restore_placement(position, layout.size);
            }
            window.follow_csp();
        }
        let settings = session::load_settings();
        let mut updater = Updater::new();
        if settings.check_updates {
            updater.check(&cc.egui_ctx);
        }
        let mut app = Self {
            window,
            keyboard: LentKeyboard::default(),
            settings_keyboard: LentKeyboard::default(),
            restore_after_typing: false,
            settings,
            saved_settings: Saved::new(settings),
            updater,
            restore_at: None,
            last_restore: None,
            link: Link::Idle,
            worker: None,
            events,
            event_tx,
            scan_rx: None,
            color: ColorState::from_rgb8(rgb),
            picker,
            last_sent: rgb,
            hex: to_hex(rgb),
            last_edit: None,
            recent_sent: VecDeque::new(),
            slots,
            last_slot_choice: None,
            saved_slots: Saved::new(slots),
            saved_layout: Saved::new(layout),
            settings_open: false,
            settings_hwnd: None,
            settings_position: None,
            settings_offset: layout.settings_offset,
            settings_size: layout.settings_size,
            settings_icon: IconButton::new(SETTINGS_ICON),
            close_icon: IconButton::new(CLOSE_ICON),
            settings_close_icon: IconButton::new(CLOSE_ICON),
            tray,
            close_dialog_open: false,
            quitting: false,
            confirm_update: false,
            panel_heights,
            dragging_splitter: false,
            blocked: false,
            csp_in_front: true,
            generation: String::new(),
            reads_per_second: 0,
            read_ms: 0.0,
            read_max_ms: 0.0,
            write_ms: 0.0,
            writes: 0,
            rejected: 0,
            url_input: String::new(),
            log: VecDeque::new(),
            saved: session::load(),
        };
        // The saved colours show until CSP's first report replaces them.
        if let Some(saved) = app.saved.clone() {
            app.connect(&cc.egui_ctx, saved, true);
        }
        app
    }

    fn sync_saved_state(&mut self, ctx: &egui::Context) {
        if self.saved_slots.is_due(self.slots, ctx) {
            self.save_colors();
        }
        let layout = self.current_layout();
        if self.saved_layout.is_due(layout, ctx) {
            self.save_layout(layout);
        }
        if self.saved_settings.is_due(self.settings, ctx) {
            self.save_settings();
        }
    }

    fn current_layout(&self) -> SavedLayout {
        let (wheel, slider) = self.picker.modes();
        // Keep the last good placement while the window has none to report.
        let saved = &self.saved_layout.on_disk;
        let (position, size) = match self.window.as_ref().and_then(PaletteWindow::placement) {
            Some((position, size)) => (Some(position), Some(size)),
            None => (saved.position, saved.size),
        };
        let panels = self.panel_heights.map(|height| height.round() as i32);
        SavedLayout {
            position,
            size,
            wheel,
            slider,
            panels: Some(panels),
            settings_offset: self.settings_offset,
            settings_size: self.settings_size,
        }
    }

    fn save_colors(&mut self) {
        self.saved_slots.written(self.slots);
        if let Err(e) = session::save_colors(&self.slots.into()) {
            self.log(format!("保存颜色失败：{e}"));
        }
    }

    fn save_layout(&mut self, layout: SavedLayout) {
        self.saved_layout.written(layout);
        if let Err(e) = session::save_layout(&layout) {
            self.log(format!("保存窗口布局失败：{e}"));
        }
    }

    fn save_settings(&mut self) {
        self.saved_settings.written(self.settings);
        if let Err(e) = session::save_settings(&self.settings) {
            self.log(format!("保存设置失败：{e}"));
        }
    }

    fn log(&mut self, line: impl Into<String>) {
        if self.log.len() == LOG_LINES {
            self.log.pop_front();
        }
        self.log.push_back(line.into());
    }

    fn connect(&mut self, ctx: &egui::Context, pairing: Pairing, reconnect: bool) {
        self.log(format!("连接 {:?}:{} ...", pairing.ips, pairing.port));
        self.link = Link::Connecting;
        let ctx = ctx.clone();
        self.worker = Some(Worker::spawn(pairing, reconnect, self.event_tx.clone(), move || {
            if platform::is_cloaked() {
                ctx.request_repaint_after(HIDDEN_REPAINT_INTERVAL);
            } else {
                ctx.request_repaint();
            }
        }));
    }

    fn start_scan(&mut self, ctx: &egui::Context) {
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        thread::spawn(move || {
            thread::sleep(SCAN_INTERVAL);
            let result = match qr::scan_screens() {
                Ok(Some(pairing)) => ScanResult::Found(pairing),
                Ok(None) => ScanResult::NotFound,
                Err(e) => ScanResult::Failed(e),
            };
            let _ = tx.send(result);
            ctx.request_repaint();
        });
        self.scan_rx = Some(rx);
    }

    fn poll_scan(&mut self, ctx: &egui::Context) {
        let Some(rx) = &self.scan_rx else { return };
        let Ok(result) = rx.try_recv() else { return };
        self.scan_rx = None;
        if self.link != Link::Scanning {
            return;
        }
        match result {
            ScanResult::Found(pairing) => {
                self.log("在屏幕上找到了二维码");
                self.connect(ctx, pairing, false);
            }
            ScanResult::NotFound => self.start_scan(ctx),
            ScanResult::Failed(e) => {
                self.log(format!("截屏失败：{e}"));
                self.link = Link::Idle;
            }
        }
    }

    fn poll_events(&mut self) {
        while let Ok(event) = self.events.try_recv() {
            match event {
                Event::Connected { generation, saved } => {
                    self.log(format!("已连接，协议版本 {generation}"));
                    session::log_connection("connected");
                    self.generation = generation;
                    self.link = Link::Connected;
                    if let Err(e) = session::save(&saved) {
                        self.log(format!("保存会话失败：{e}"));
                    }
                    self.saved = Some(saved);
                }
                Event::SessionInvalid => {
                    self.log("保存的密码已失效，请重新扫描二维码");
                    session::clear();
                    self.saved = None;
                    self.reset_link();
                }
                Event::Failed(reason) => {
                    self.log(format!("连接失败：{reason}"));
                    self.reset_link();
                }
                Event::Disconnected => {
                    self.log("已断开");
                    session::log_connection("disconnected by user");
                    self.reset_link();
                }
                Event::Reconnecting(reason) => {
                    // Only the change of state: retries can go on for as long as CSP is closed.
                    if self.link != Link::Reconnecting {
                        self.log(format!("正在重连：{reason}"));
                        session::log_connection(&format!("dropped: {reason}"));
                    }
                    self.link = Link::Reconnecting;
                }
                Event::Slots(csp) => self.sync_from_csp(&csp),
                Event::Stats { reads_per_second, read_average, read_max } => {
                    self.reads_per_second = reads_per_second;
                    self.read_ms = millis(read_average);
                    self.read_max_ms = millis(read_max);
                }
                Event::Sent { rgb, accepted, round_trip } => {
                    self.write_ms = millis(round_trip);
                    self.writes += 1;
                    if !accepted {
                        self.rejected += 1;
                        self.log(format!("CSP 拒绝了 {}", to_hex(rgb)));
                    }
                }
                Event::Blocked(blocked) => {
                    self.blocked = blocked;
                    self.log(if blocked {
                        "CSP 暂不接受改色（正在画或有弹窗）"
                    } else {
                        "CSP 恢复接受改色"
                    });
                }
                Event::Foreground(in_front) => self.csp_in_front = in_front,
                Event::Log(line) => self.log(line),
            }
        }
    }

    fn sync_from_csp(&mut self, csp: &CspSlots) {
        let choosing = self.last_slot_choice.is_some_and(|t| t.elapsed() < SLOT_HOLD);
        let switched = !choosing && csp.active != self.slots.active;
        if !choosing {
            self.slots.active = csp.active;
            self.slots.last_color = if csp.active == Slot::Transparent { csp.previous } else { csp.active };
        }

        self.recent_sent.retain(|(_, sent)| sent.elapsed() < ECHO_WINDOW);
        let editing = self.last_edit.is_some_and(|t| t.elapsed() < EDIT_HOLD);
        let last_color = self.slots.last_color;
        let recent_sent = &self.recent_sent;
        let is_echo = |rgb: Rgb| recent_sent.iter().any(|(sent, _)| *sent == rgb);
        let keep_own = |slot: Slot, rgb: Rgb| slot == last_color && (editing || is_echo(rgb));
        let new_main = csp.main.map(|c| c.rgb).filter(|&rgb| !keep_own(Slot::Main, rgb));
        let new_sub = csp.sub.map(|c| c.rgb).filter(|&rgb| !keep_own(Slot::Sub, rgb));
        if let Some(rgb) = new_main {
            self.slots.main = rgb;
        }
        if let Some(rgb) = new_sub {
            self.slots.sub = rgb;
        }

        // Comparing bytes keeps the sliders' finer value when CSP merely reports the
        // rounded version of it. Transparent has no value, so the sliders keep showing
        // the slot that a colour edit would go to.
        let Some(rgb) = self.slots.color(last_color) else { return };
        if self.color.rgb8() != rgb && (switched || !editing) {
            self.color.set_rgb8(rgb);
            self.last_sent = rgb;
        }
    }

    fn choose_slot(&mut self, slot: Slot) {
        self.last_slot_choice = Some(Instant::now());
        self.slots.select(slot);
        if let Some(rgb) = self.slots.color(slot) {
            self.color.set_rgb8(rgb);
            self.last_sent = rgb;
        }
        if let Some(worker) = &self.worker {
            worker.send(Command::SelectSlot(slot));
        }
    }

    /// After the pen lifts from this window, hands CSP and the tablet driver
    /// the title bar click that makes them resume.
    fn sync_csp_restore(&mut self, ctx: &egui::Context) {
        let finished = frame::take_drag_end() | std::mem::take(&mut self.restore_after_typing);
        if !self.settings.restore_csp || self.keyboard.typing || self.settings_keyboard.typing {
            self.restore_at = None;
            return;
        }
        let (released, down) = ctx.input(|i| (i.pointer.any_released() || finished, i.pointer.any_down()));
        if down {
            self.restore_at = None;
        } else if released {
            // Click at once: the sooner it happens, the less likely a new stroke
            // has started by then.
            let now = Instant::now();
            let earliest = self.last_restore.map(|t| t + RESTORE_MIN_INTERVAL);
            self.restore_at = Some(earliest.map_or(now, |t| t.max(now)));
        }
        let Some(mut at) = self.restore_at else { return };
        if pen_or_mouse_down() {
            at = at.max(Instant::now() + RESTORE_QUIET);
            self.restore_at = Some(at);
        }
        let remaining = at.saturating_duration_since(Instant::now());
        if !remaining.is_zero() {
            ctx.request_repaint_after(remaining.min(RESTORE_POLL));
            return;
        }
        self.restore_at = None;
        match click_csp_title_bar() {
            Ok(()) => self.last_restore = Some(Instant::now()),
            Err(platform::CSP_NOT_IN_FRONT) => {}
            Err(reason) => self.log(format!("没有恢复 CSP：{reason}")),
        }
    }

    /// Lends a window the keyboard only while a text field has focus, or in the settings
    /// window, while text is selected in the log to be copied. `settings` is the settings
    /// window if that is the one being asked about, or else it is the main window.
    fn sync_keyboard(&mut self, ctx: &egui::Context, settings: Option<isize>) {
        let selecting = settings.is_some() && ctx.plugin::<LabelSelectionState>().lock().has_selection();
        let wants_keyboard = ctx.egui_wants_keyboard_input() || selecting;
        let (lent, other) = if settings.is_some() {
            (&mut self.settings_keyboard, &mut self.keyboard)
        } else {
            (&mut self.keyboard, &mut self.settings_keyboard)
        };
        if other.typing && ctx.input(|i| i.pointer.any_pressed()) {
            other.end_requested = true;
            ctx.request_repaint();
        }
        let changed = wants_keyboard != lent.typing;
        if changed {
            lent.typing = wants_keyboard;
            lent.active = false;
            let left = std::mem::take(&mut lent.left);
            self.restore_after_typing |= !wants_keyboard && !left;
        }
        let window_active = ctx.input(|i| i.focused);
        if lent.typing && window_active {
            lent.active = true;
        }
        let left = lent.typing && !window_active && lent.active;
        lent.left |= left;
        let end = left || std::mem::take(&mut lent.end_requested);
        if changed {
            let hwnd = settings.map(|hwnd| hwnd as _);
            let granted = self.window.as_mut().is_none_or(|w| w.set_typing(hwnd, wants_keyboard));
            if !granted {
                self.log("Windows 拒绝切换键盘焦点，请再点一次输入框");
            }
        }
        if end {
            // The user went elsewhere; end the edit so the next click on
            // a slider does not activate the window again.
            ctx.memory_mut(|memory| {
                if let Some(id) = memory.focused() {
                    memory.surrender_focus(id);
                }
            });
            if selecting {
                ctx.plugin::<LabelSelectionState>().lock().clear_selection();
            }
            ctx.request_repaint();
        }
    }

    fn hide_to_tray(&mut self) {
        if let Some(window) = &self.window {
            window.hide();
        }
    }

    /// The close button, or anything else that asks the window to close.
    fn close_requested(&mut self, ctx: &egui::Context) {
        match self.settings.close_action {
            None => self.close_dialog_open = true,
            Some(CloseAction::MinimizeToTray) if self.tray.is_some() => self.hide_to_tray(),
            Some(_) => self.quit(ctx),
        }
    }

    fn quit(&mut self, ctx: &egui::Context) {
        self.quitting = true;
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }

    fn poll_tray(&mut self, ctx: &egui::Context) {
        match tray::take_request() {
            Some(tray::Request::Show) => {
                if let Some(window) = &self.window {
                    window.show();
                }
            }
            Some(tray::Request::Settings) => self.settings_open = true,
            Some(tray::Request::Quit) => self.quit(ctx),
            None => {}
        }
    }

    /// Asked once, the first time the window is closed.
    fn close_dialog_ui(&mut self, ctx: &egui::Context, ui: &egui::Ui) {
        let modal = egui::Modal::new(ui.scope_id().with("close")).show(ctx, |ui| {
            ui.set_max_width(CLOSE_DIALOG_WIDTH);
            ui.label(RichText::new("关闭 Percepta").strong());
            ui.label("要最小化到托盘，还是直接退出？以后可以在设置里改。");
            ui.label(RichText::new("托盘图标在任务栏右下角，可能收在小箭头里。").small());
            ui.add_space(4.0);
            let mut chosen = None;
            ui.horizontal_wrapped(|ui| {
                if ui.button("最小化到托盘").clicked() {
                    chosen = Some(CloseAction::MinimizeToTray);
                }
                if ui.button("直接退出").clicked() {
                    chosen = Some(CloseAction::Quit);
                }
                if ui.button("取消").clicked() {
                    self.close_dialog_open = false;
                }
            });
            chosen
        });
        if modal.should_close() {
            self.close_dialog_open = false;
        }
        if let Some(action) = modal.inner {
            self.close_dialog_open = false;
            self.settings.close_action = Some(action);
            self.close_requested(ctx);
        }
    }

    /// The panels fill the window, with a line between them that resizes the one above it.
    fn panels_ui(&mut self, ui: &mut egui::Ui) {
        let area = ui.max_rect();
        if !self.dragging_splitter {
            self.panel_heights = fit_panels(self.panel_heights, area.height());
        }
        let mut changed = false;
        let mut top = area.top();
        let mut grow = 0.0;
        for panel in 0..PANEL_COUNT {
            let height = self.panel_heights[panel];
            let rect = egui::Rect::from_min_max(
                egui::pos2(area.left(), top),
                egui::pos2(area.right(), top + height),
            );
            let content = rect.shrink2(egui::vec2(CONTENT_MARGIN, PANEL_PADDING));
            let mut child = ui.new_child(egui::UiBuilder::new().max_rect(content));
            child.set_clip_rect(rect.intersect(ui.clip_rect()));
            match panel {
                0 => self.color_header_ui(&mut child),
                1 => changed |= self.picker.wheel_panel(&mut child, &mut self.color),
                _ => changed |= self.picker.slider_panel(&mut child, &mut self.color),
            }
            top += height;
            if panel + 1 < PANEL_COUNT {
                grow += self.splitter_ui(ui, panel, top);
                top = area.top() + self.panel_heights[..=panel].iter().sum::<f32>();
            }
        }
        if changed {
            self.color_edited();
        }
        if grow > 0.0 {
            let total: f32 = self.panel_heights.iter().sum();
            let size = egui::vec2(area.width(), TITLE_BAR_HEIGHT + total);
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::InnerSize(size));
        }
    }

    /// Returns by how much the window should grow.
    fn splitter_ui(&mut self, ui: &mut egui::Ui, panel: usize, y: f32) -> f32 {
        let area = ui.max_rect();
        let grab = egui::Rect::from_min_max(
            egui::pos2(area.left(), y - SPLITTER_GRAB),
            egui::pos2(area.right(), y + SPLITTER_GRAB),
        );
        let response = ui
            .interact(grab, ui.scope_id().with(("splitter", panel)), egui::Sense::drag())
            .on_hover_cursor(egui::CursorIcon::ResizeVertical);
        let color = if response.hovered() || response.dragged() {
            SPLITTER_ACTIVE
        } else {
            ui.visuals().widgets.noninteractive.bg_stroke.color
        };
        let line_x = egui::Rangef::new(area.left() + CONTENT_MARGIN, area.right() - CONTENT_MARGIN);
        ui.painter().hline(line_x, y, egui::Stroke::new(1.0, color));
        if response.drag_started() {
            self.dragging_splitter = true;
        }
        if response.drag_stopped() {
            self.dragging_splitter = false;
        }
        if !response.dragged() {
            return 0.0;
        }
        let (heights, grow) = drag_panel(self.panel_heights, panel, response.drag_delta().y);
        self.panel_heights = heights;
        grow
    }

    fn reset_link(&mut self) {
        self.link = Link::Idle;
        self.worker = None;
        self.blocked = false;
    }

    fn color_edited(&mut self) {
        let now = Instant::now();
        self.last_edit = Some(now);
        let rgb = self.color.rgb8();
        let leaving_transparent = self.slots.active == Slot::Transparent;
        self.slots.store_edit(rgb);
        if leaving_transparent {
            self.last_slot_choice = Some(now);
        }
        // A drag within one byte sends nothing, unless it must leave transparent.
        if rgb == self.last_sent && !leaving_transparent {
            return;
        }
        self.last_sent = rgb;
        self.recent_sent.retain(|(_, sent)| sent.elapsed() < ECHO_WINDOW);
        self.recent_sent.push_back((rgb, now));
        if let Some(worker) = &self.worker {
            worker.send(Command::SetRgb(rgb));
        }
    }

    fn connection_ui(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        match self.link {
            Link::Idle => {
                ui.label("在 CSP 打开“文件 > 连接智能手机”后扫码。");
                ui.horizontal(|ui| {
                    if ui.button("扫描屏幕二维码").clicked() {
                        self.link = Link::Scanning;
                        self.start_scan(&ctx);
                    }
                    if let Some(saved) = self.saved.clone() {
                        if ui.button("免扫码重连").clicked() {
                            self.connect(&ctx, saved, true);
                        }
                    }
                });
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.url_input)
                            .hint_text("或粘贴二维码里的网址")
                            .desired_width(ui.available_width() - CONNECT_BUTTON_ROOM),
                    );
                    if ui.button("连接").clicked() {
                        match parse_qr_url(&self.url_input) {
                            Some(pairing) => {
                                self.url_input.clear();
                                self.connect(&ctx, pairing, false);
                            }
                            None => self.log("这个网址不是 CSP 的配对地址"),
                        }
                    }
                });
            }
            Link::Scanning => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("正在屏幕上查找二维码…");
                    if ui.button("取消").clicked() {
                        self.link = Link::Idle;
                    }
                });
            }
            Link::Connecting => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("正在连接 CSP…");
                });
            }
            Link::Reconnecting => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("正在等待 CSP，会自动重连…");
                    if ui.button("取消").clicked() {
                        if let Some(worker) = &self.worker {
                            worker.send(Command::Disconnect);
                        }
                    }
                });
            }
            Link::Connected => {
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new("已连接").color(CONNECTED));
                    ui.label(&self.generation);
                    ui.label(if self.csp_in_front { "CSP 在前台" } else { "CSP 在后台（低频同步）" });
                    if ui.button("断开").clicked() {
                        if let Some(worker) = &self.worker {
                            worker.send(Command::Disconnect);
                        }
                    }
                });
            }
        }
    }

    fn color_header_ui(&mut self, ui: &mut egui::Ui) {
        let rgb = self.color.rgb8();
        // While the field is being typed in, the user's text must stay as it is.
        if !self.keyboard.typing {
            self.hex = to_hex(rgb);
        }
        let mut chosen = None;
        let chip_height = (ui.available_height() - HEX_ROW_ROOM).max(MIN_SLOT_CHIP_HEIGHT);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = SLOT_CHIP_GAP;
            let unit = (ui.available_width() - SLOT_CHIP_GAP * 2.0) / SLOT_WIDTHS.iter().sum::<f32>();
            let slots = [(Slot::Main, "前景色"), (Slot::Sub, "背景色"), (Slot::Transparent, "透明色")];
            for ((slot, name), width) in slots.into_iter().zip(SLOT_WIDTHS) {
                let size = egui::vec2(unit * width, chip_height);
                if slot_chip(ui, size, self.slots.color(slot), self.slots.active == slot, name) {
                    chosen = Some(slot);
                }
            }
        });
        if let Some(slot) = chosen {
            self.choose_slot(slot);
        }
        ui.horizontal(|ui| {
            ui.label("HEX");
            let edit = ui.add(egui::TextEdit::singleline(&mut self.hex).desired_width(HEX_WIDTH));
            if edit.changed() {
                if let Some(rgb) = from_hex(&self.hex) {
                    self.color.set_rgb8(rgb);
                    self.color_edited();
                }
            }
            let (text, color) = self.status();
            let (dot, _) = ui.allocate_exact_size(egui::Vec2::splat(STATUS_DOT_SIZE), egui::Sense::hover());
            ui.painter().circle_filled(dot.center(), STATUS_DOT_SIZE / 2.0, color);
            ui.label(RichText::new(text).color(color));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let update_waiting = matches!(self.updater.state, update::State::Available(_));
                let hover_text = if update_waiting { "设置（有新版本）" } else { "设置" };
                if self.settings_icon.show(ui, self.settings_open, hover_text) {
                    if self.settings_open {
                        self.close_settings();
                    } else {
                        self.settings_open = true;
                    }
                }
                if update_waiting {
                    ui.painter().circle_filled(ui.min_rect().right_top(), UPDATE_DOT_RADIUS, CONNECTED);
                }
                if self.color.gamut_mapped {
                    ui.label(RichText::new("已映射").color(WARNING)).on_hover_text("超出 sRGB，已映射到色域内");
                }
            });
        });
    }

    /// Every text is the same length, so that the line never shifts.
    fn status(&self) -> (&'static str, Color32) {
        match self.link {
            Link::Connected if self.blocked => ("已暂停", WARNING),
            Link::Connected => ("已连接", CONNECTED),
            Link::Reconnecting => ("重连中", WAITING),
            Link::Connecting | Link::Scanning => ("连接中", WAITING),
            Link::Idle => ("未连接", DISCONNECTED),
        }
    }

    fn settings_ui(&mut self, ui: &mut egui::Ui) {
        ui.label(RichText::new("连接 CSP").strong());
        self.connection_ui(ui);
        switch(ui, &mut self.settings.restore_csp, "抬笔后自动点击 CSP 标题栏")
            .on_hover_text("修复用笔操作取色器后，数位板按键映射错误和笔刷光标卡住的问题");
        ui.separator();

        ui.label(RichText::new("关闭按钮").strong());
        let mut action = self.settings.close_action;
        ui.horizontal_wrapped(|ui| {
            ui.radio_value(&mut action, Some(CloseAction::MinimizeToTray), "最小化到托盘");
            ui.radio_value(&mut action, Some(CloseAction::Quit), "直接退出");
        });
        self.settings.close_action = action;
        ui.separator();

        ui.label(RichText::new("CMYK").strong());
        match self.picker.cmyk_profile_name() {
            Some(name) => ui.label(format!("配置文件：{name}")),
            None => ui.label(RichText::new("读不到 CSP 的配置文件，正在用通用公式换算").color(WARNING)),
        };
        if ui.button("重新读取").on_hover_text("在 CSP 里改了配置文件后使用").clicked() {
            self.picker.reload_cmyk_profile();
        }
        ui.separator();

        ui.label(RichText::new("更新").strong());
        self.update_ui(ui);
        ui.separator();

        egui::CollapsingHeader::new("诊断").show(ui, |ui| {
            self.stats_ui(ui);
            egui::ScrollArea::vertical().max_height(LOG_HEIGHT).show(ui, |ui| {
                // The exception: error messages are worth copying.
                ui.style_mut().interaction.selectable_labels = true;
                for line in &self.log {
                    ui.label(RichText::new(line).small());
                }
            });
        });
    }

    /// Places the settings window when it first appears, and from then on remembers where
    /// the user moves it to. Moving the main window alone changes nothing.
    fn sync_settings_placement(&mut self, ctx: &egui::Context) {
        let Some(window) = &self.window else { return };
        let Some(hwnd) = self.settings_hwnd else {
            if let Some(hwnd) = platform::find_own_window(SETTINGS_TITLE) {
                platform::adopt_settings(hwnd);
                // Only ever active because the user went to it, so its shadow has no conditions.
                frame::install(hwnd, true, || true);
                window.place_settings(hwnd, self.settings_offset, self.settings_size);
                self.settings_hwnd = Some(hwnd as isize);
                self.settings_position = window.settings_placement(hwnd).map(|(position, ..)| position);
                ctx.request_repaint_of(egui::ViewportId::ROOT);
            }
            return;
        };
        self.remember_settings_placement(hwnd);
    }

    fn remember_settings_placement(&mut self, hwnd: isize) {
        let Some(window) = &self.window else { return };
        let Some((position, offset, size)) = window.settings_placement(hwnd as _) else { return };
        if self.settings_position != Some(position) {
            self.settings_position = Some(position);
            self.settings_offset = Some(offset);
        }
        self.settings_size = Some(size);
    }

    /// Every way of closing the window comes through here. The next one to open is a new
    /// window, which must be found and given its frame all over again.
    fn close_settings(&mut self) {
        if let Some(hwnd) = self.settings_hwnd.take() {
            self.remember_settings_placement(hwnd);
            if std::mem::take(&mut self.settings_keyboard).typing {
                if let Some(window) = &mut self.window {
                    window.set_typing(Some(hwnd as _), false);
                }
                self.restore_after_typing = true;
            }
            platform::forget_settings();
        }
        self.settings_position = None;
        self.settings_open = false;
    }

    /// A window of its own, so that the tray can open it while the main window is hidden.
    fn settings_window(&mut self, ctx: &egui::Context) {
        let builder = egui::ViewportBuilder::default()
            .with_title(SETTINGS_TITLE)
            .with_inner_size(SETTINGS_SIZE)
            .with_min_inner_size(SETTINGS_MIN_SIZE)
            .with_decorations(false)
            .with_maximize_button(false)
            .with_taskbar(false)
            .with_always_on_top()
            // Hidden until it has been placed, or it would flash wherever Windows first puts it.
            .with_visible(self.settings_hwnd.is_some());
        let id = egui::ViewportId::from_hash_of("settings");
        ctx.show_viewport_immediate(id, builder, |ui, _class| {
            self.sync_settings_placement(ui.ctx());
            if let Some(hwnd) = self.settings_hwnd {
                self.sync_keyboard(ui.ctx(), Some(hwnd));
            }
            self.sync_csp_restore(ui.ctx());
            let frame = egui::Frame::central_panel(ui.style()).inner_margin(0.0);
            egui::CentralPanel::default().frame(frame).show(ui, |ui| {
                let closed = title_bar(ui, "设置", &mut self.settings_close_icon);
                let content = ui.available_rect_before_wrap().shrink(CONTENT_MARGIN);
                let mut child = ui.new_child(egui::UiBuilder::new().max_rect(content));
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(&mut child, |ui| {
                    self.settings_ui(ui);
                });
                window_edge(ui);
                if closed || ui.input(|input| input.viewport().close_requested()) {
                    self.close_settings();
                }
            });
        });
    }

    fn update_ui(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        ui.label(format!("当前版本：{}", update::CURRENT_VERSION));
        let mut install = None;
        let mut check = false;
        let mut ask_update = false;
        let mut cancel_update = false;
        match &self.updater.state {
            update::State::Idle => check = ui.button("检查更新").clicked(),
            update::State::Checking => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("正在检查…");
                });
            }
            update::State::UpToDate => {
                ui.label("已经是最新版本");
                check = ui.button("检查更新").clicked();
            }
            update::State::Available(release) => {
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new(format!("有新版本 {}", release.version)).color(CONNECTED));
                    ui.hyperlink_to("更新内容", &release.page);
                });
                if self.confirm_update {
                    ui.label("更新需要退出 Percepta，装好后会自动重新打开。");
                    ui.horizontal_wrapped(|ui| {
                        if ui.button("退出并更新").clicked() {
                            install = Some(release.clone());
                        }
                        if ui.button("取消").clicked() {
                            cancel_update = true;
                        }
                    });
                } else if ui.button("下载并安装").clicked() {
                    ask_update = true;
                }
            }
            update::State::Downloading(fraction) => {
                ui.add(egui::ProgressBar::new(*fraction).text("正在下载…"));
            }
            update::State::Installing => {
                ui.label("正在安装…");
            }
            update::State::Failed(reason) => {
                ui.label(RichText::new(reason).color(WARNING));
                check = ui.button("重新检查").clicked();
            }
        }
        if check {
            self.updater.check(&ctx);
        }
        self.confirm_update = (self.confirm_update || ask_update) && !cancel_update && install.is_none();
        if let Some(release) = install {
            self.updater.install(&ctx, release);
        }
        switch(ui, &mut self.settings.check_updates, "启动时检查更新").on_hover_text("只会联网询问 GitHub 有没有新版本");
    }

    fn stats_ui(&self, ui: &mut egui::Ui) {
        ui.label(format!(
            "回读 {} 次/秒 · 平均 {:.1} ms · 最慢 {:.1} ms",
            self.reads_per_second, self.read_ms, self.read_max_ms
        ));
        ui.label(format!(
            "写入 {:.1} ms · 已写入 {} 次 · 被拒 {} 次",
            self.write_ms, self.writes, self.rejected
        ));
    }
}

impl Drop for App {
    /// Catches a change made less than `SAVE_DELAY` before closing.
    fn drop(&mut self) {
        if self.slots != self.saved_slots.on_disk {
            self.save_colors();
        }
        let layout = self.current_layout();
        if layout != self.saved_layout.on_disk {
            self.save_layout(layout);
        }
        if self.settings != self.saved_settings.on_disk {
            self.save_settings();
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.poll_events();
        self.poll_scan(&ctx);
        self.sync_keyboard(&ctx, None);
        self.sync_csp_restore(&ctx);
        self.sync_saved_state(&ctx);
        self.updater.poll();
        self.poll_tray(&ctx);
        if matches!(self.updater.state, update::State::Installing) && !self.quitting {
            self.quit(&ctx);
        }
        if ctx.input(|input| input.viewport().close_requested()) && !self.quitting {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.close_requested(&ctx);
        }

        let frame = egui::Frame::central_panel(ui.style()).inner_margin(0.0);
        egui::CentralPanel::default().frame(frame).show(ui, |ui| {
            if title_bar(ui, "Percepta", &mut self.close_icon) {
                self.close_requested(&ctx);
            }
            let area = ui.available_rect_before_wrap();
            let mut panels = ui.new_child(egui::UiBuilder::new().max_rect(area));
            self.panels_ui(&mut panels);
            window_edge(ui);
        });

        if self.close_dialog_open {
            self.close_dialog_ui(&ctx, ui);
        }
        if self.settings_open {
            self.settings_window(&ctx);
        }
    }
}

/// Paints the title bar and reports a click on its close button. Windows does the
/// dragging, in `frame`, which leaves that button's corner to the UI.
fn title_bar(ui: &mut egui::Ui, title: &str, close_icon: &mut IconButton) -> bool {
    let (bar, _) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), TITLE_BAR_HEIGHT),
        egui::Sense::hover(),
    );
    let painter = ui.painter_at(bar);
    painter.rect_filled(bar, 0.0, TITLE_BAR_FILL);
    painter.text(
        bar.center(),
        egui::Align2::CENTER_CENTER,
        title,
        egui::FontId::proportional(TITLE_FONT_SIZE),
        TITLE_TEXT,
    );
    let close = egui::Rect::from_min_max(egui::pos2(bar.right() - TITLE_BUTTON_WIDTH, bar.top()), bar.max);
    close_icon.show_in(ui, close, ui.scope_id().with("title close"))
}

/// The thin line around the whole window, over everything else in it.
fn window_edge(ui: &egui::Ui) {
    let layer = egui::LayerId::new(egui::Order::Foreground, ui.scope_id().with("window edge"));
    ui.ctx().layer_painter(layer).rect_stroke(
        ui.ctx().content_rect(),
        CORNER_RADIUS,
        egui::Stroke::new(WINDOW_EDGE_WIDTH, WINDOW_EDGE),
        egui::StrokeKind::Inside,
    );
}

/// Makes the panels fill `total`. Spare room goes to the last one, and a shortage comes
/// from the last one first, as far as the minimum heights allow.
fn fit_panels(mut heights: [f32; PANEL_COUNT], total: f32) -> [f32; PANEL_COUNT] {
    for (height, min) in heights.iter_mut().zip(MIN_PANEL_HEIGHTS) {
        *height = height.max(min);
    }
    let mut spare = total - heights.iter().sum::<f32>();
    if spare >= 0.0 {
        heights[PANEL_COUNT - 1] += spare;
        return heights;
    }
    for (height, min) in heights.iter_mut().zip(MIN_PANEL_HEIGHTS).rev() {
        let taken = (*height - min).min(-spare);
        *height -= taken;
        spare += taken;
    }
    heights
}

/// Applies dragging the line below `panel` by `delta`. Returns the new heights, and by how
/// much the window should grow. A panel that grows makes the window taller, while one that
/// shrinks hands its room to the last panel and leaves the window as it is.
fn drag_panel(mut heights: [f32; PANEL_COUNT], panel: usize, delta: f32) -> ([f32; PANEL_COUNT], f32) {
    let new_height = (heights[panel] + delta).max(MIN_PANEL_HEIGHTS[panel]);
    let change = new_height - heights[panel];
    heights[panel] = new_height;
    if change < 0.0 {
        heights[PANEL_COUNT - 1] -= change;
        (heights, 0.0)
    } else {
        (heights, change)
    }
}

/// A colour slot's button: its colour, or a checkerboard for transparent, outlined in
/// the accent colour while active. Returns whether it was clicked.
fn slot_chip(ui: &mut egui::Ui, size: egui::Vec2, rgb: Option<Rgb>, active: bool, name: &str) -> bool {
    let (outer, response) = ui.allocate_exact_size(size, egui::Sense::click());
    let rect = outer.shrink(SLOT_OUTLINE_ROOM);
    let painter = ui.painter_at(outer);
    match rgb {
        Some([r, g, b]) => {
            painter.rect_filled(rect, SLOT_CORNER, Color32::from_rgb(r, g, b));
        }
        None => {
            painter.rect_filled(rect, SLOT_CORNER, CHECKER_LIGHT);
            let checks = painter.with_clip_rect(rect.shrink(1.0));
            let columns = (rect.width() / CHECKER_SIZE).ceil() as usize;
            let rows = (rect.height() / CHECKER_SIZE).ceil() as usize;
            for row in 0..rows {
                for column in (0..columns).filter(|column| (column + row) % 2 == 1) {
                    let min = rect.min + egui::vec2(column as f32, row as f32) * CHECKER_SIZE;
                    let square = egui::Rect::from_min_size(min, egui::Vec2::splat(CHECKER_SIZE));
                    checks.rect_filled(square, 0.0, CHECKER_DARK);
                }
            }
        }
    }
    let edge = ui.visuals().widgets.noninteractive.bg_stroke;
    painter.rect_stroke(rect, SLOT_CORNER, edge, egui::StrokeKind::Outside);
    if active {
        let outline = egui::Stroke::new(ACTIVE_SLOT_WIDTH, ui.visuals().selection.stroke.color);
        painter.rect_stroke(outer, SLOT_CORNER + SLOT_OUTLINE_ROOM, outline, egui::StrokeKind::Inside);
    }
    response.on_hover_text(name).clicked()
}

fn install_cjk_font(ctx: &egui::Context) {
    let Some(data) = FONT_CANDIDATES.iter().find_map(|p| std::fs::read(p).ok()) else {
        return;
    };
    let mut fonts = egui::FontDefinitions::default();
    fonts
        .font_data
        .insert("cjk".into(), Arc::new(egui::FontData::from_owned(data)));
    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        fonts.families.entry(family).or_default().push("cjk".into());
    }
    ctx.set_fonts(fonts);
}

fn millis(duration: Duration) -> f32 {
    duration.as_secs_f32() * 1000.0
}

fn to_hex([r, g, b]: Rgb) -> String {
    format!("#{r:02X}{g:02X}{b:02X}")
}

fn from_hex(text: &str) -> Option<Rgb> {
    let digits = text.trim().trim_start_matches('#');
    if digits.len() != 6 {
        return None;
    }
    let channel = |i: usize| u8::from_str_radix(digits.get(i..i + 2)?, 16).ok();
    Some([channel(0)?, channel(2)?, channel(4)?])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saved_colours_round_trip_and_never_aim_edits_at_transparent() {
        let slots = Slots { active: Slot::Transparent, last_color: Slot::Sub, main: [1, 1, 1], sub: [2, 2, 2] };
        let saved: SavedColors = slots.into();
        assert!(Slots::from(saved) == slots);

        let mut bad = saved;
        bad.last_color = Slot::Transparent;
        assert_eq!(Slots::from(bad).last_color, Slot::Main);
    }

    #[test]
    fn a_colour_edit_while_transparent_goes_to_the_last_colour_slot() {
        let mut slots = Slots { active: Slot::Main, last_color: Slot::Main, main: [1, 1, 1], sub: [2, 2, 2] };
        slots.select(Slot::Sub);
        slots.select(Slot::Transparent);
        assert_eq!(slots.active, Slot::Transparent);
        assert_eq!(slots.color(Slot::Transparent), None);

        slots.store_edit([9, 9, 9]);
        assert_eq!(slots.active, Slot::Sub);
        assert_eq!(slots.sub, [9, 9, 9]);
        assert_eq!(slots.main, [1, 1, 1]);
    }

    #[test]
    fn panels_fill_the_window_and_only_give_way_down_to_their_minimums() {
        assert_eq!(fit_panels([72.0, 340.0, 208.0], 720.0), [72.0, 340.0, 308.0]);
        assert_eq!(fit_panels([72.0, 340.0, 208.0], 570.0), [72.0, 340.0, 158.0]);
        // The last panel gives way first, then the one above it.
        assert_eq!(fit_panels([72.0, 340.0, 208.0], 500.0), [72.0, 278.0, 150.0]);
        assert_eq!(fit_panels([72.0, 340.0, 208.0], 410.0), [60.0, 200.0, 150.0]);
        // A saved height below the minimum is raised.
        assert_eq!(fit_panels([10.0, 200.0, 150.0], 410.0), [60.0, 200.0, 150.0]);
    }

    #[test]
    fn growing_a_panel_grows_the_window_but_shrinking_one_does_not() {
        let heights = [72.0, 340.0, 208.0];
        assert_eq!(drag_panel(heights, 1, 30.0), ([72.0, 370.0, 208.0], 30.0));
        assert_eq!(drag_panel(heights, 1, -30.0), ([72.0, 310.0, 238.0], 0.0));
        // Stops at the minimum, and the rest of the drag does nothing.
        assert_eq!(drag_panel(heights, 0, -30.0), ([60.0, 340.0, 220.0], 0.0));
    }

    #[test]
    fn the_smallest_window_fits_the_minimum_panels_under_the_title_bar() {
        assert_eq!(MIN_WINDOW_HEIGHT, 430.0);
    }

    #[test]
    fn hex_round_trips_and_rejects_partial_input() {
        assert_eq!(to_hex([18, 52, 86]), "#123456");
        assert_eq!(from_hex("#123456"), Some([18, 52, 86]));
        assert_eq!(from_hex("abcdef"), Some([0xAB, 0xCD, 0xEF]));
        assert_eq!(from_hex("#1234"), None);
        assert_eq!(from_hex("#12345g"), None);
    }
}
