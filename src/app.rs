//! The main window: the colour panels, and the link that keeps them in sync with CSP.

use std::collections::{HashMap, VecDeque};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use eframe::egui::text_selection::LabelSelectionState;
use eframe::egui::{self, Color32, RichText};
use windows_sys::Win32::Foundation::RECT;

use crate::color::ColorState;
use crate::csp::auth::{parse_qr_url, Pairing};
use crate::csp::client::CspSlots;
use crate::csp::worker::{Command, Event, Worker};
use crate::csp::{Rgb, Slot};
use crate::frame::{self, CORNER_RADIUS, DRAG_THRESHOLD, TITLE_BAR_HEIGHT, TITLE_BUTTON_WIDTH};
use crate::layout::{self, Axis, Drop as DockDrop, Node, Panel, DEFAULT_HEIGHTS, MIN_HEIGHTS, MODULE_TITLE_HEIGHT};
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
const MODULE_TITLE_FILL: Color32 = Color32::from_gray(240);
const ACTIVE_TAB_FILL: Color32 = Color32::from_gray(250);
const MODULE_TITLE_PADDING: f32 = 8.0;
/// The part of a module's title strip, from its top, for landing above the module.
const DROP_ABOVE_ZONE: f32 = 6.0;
/// How far below its title strip still means landing among its tabs.
const DROP_TAB_ZONE: f32 = 20.0;
/// The part of a module, from its bottom, for landing below it.
const DROP_BELOW_ZONE: f32 = 24.0;
/// In physical pixels, at any scale: the edge of a mark where a window would land, and the
/// whole thickness of the line it goes in along, beside a module or between tabs.
const DROP_EDGE: i32 = 6;
const DROP_LINE: i32 = 24;
const WINDOW_EDGE: Color32 = Color32::from_gray(150);
const WINDOW_EDGE_WIDTH: f32 = 1.0;
const CONTENT_MARGIN: f32 = 8.0;
const PANEL_PADDING: f32 = 3.0;
/// How far from the line between two panels it can be grabbed.
const SPLITTER_GRAB: f32 = 4.0;
const SPLITTER_ACTIVE: Color32 = Color32::from_gray(120);
pub const DEFAULT_WINDOW_HEIGHT: f32 =
    TITLE_BAR_HEIGHT + DEFAULT_HEIGHTS[0] + DEFAULT_HEIGHTS[1] + DEFAULT_HEIGHTS[2];
pub const MIN_WINDOW_HEIGHT: f32 = TITLE_BAR_HEIGHT + MIN_HEIGHTS[0] + MIN_HEIGHTS[1] + MIN_HEIGHTS[2];
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

impl<T: PartialEq> Saved<T> {
    fn new(on_disk: T) -> Self {
        Self { on_disk, due: None }
    }

    /// Returns whether `current` should be written now.
    fn is_due(&mut self, current: &T, ctx: &egui::Context) -> bool {
        if *current == self.on_disk {
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
    /// The user pressed on another window, which can't take the keyboard by that alone.
    end_requested: bool,
}

/// A window of its own, holding a panel or a stack of tabs.
struct FloatingWindow {
    id: u64,
    node: Node,
    /// Once it has been found and given its frame.
    hwnd: Option<isize>,
    /// Where to put it when it first appears, in physical pixels.
    position: [i32; 2],
    /// When it first appears.
    size: egui::Vec2,
    close_icon: IconButton,
}

impl FloatingWindow {
    fn title(&self) -> String {
        format!("Percepta 窗口 {}", self.id)
    }
}

/// A panel or a stack of tabs on its way somewhere, while the layout stays as it is.
struct PanelDrag {
    /// The window it comes from, or `None` for the main window.
    source: Option<u64>,
    panels: Vec<Panel>,
    /// The area it fills now, at 100% scale.
    size: egui::Vec2,
    /// How far into that area the pointer holds it, in physical pixels.
    grab: [i32; 2],
}

/// Where a stack of tabs in the main window is, at 100% scale from the window's corner.
struct DockTarget {
    panels: Vec<Panel>,
    rect: egui::Rect,
    /// Where each tab begins, and where the last one ends.
    tab_edges: Vec<f32>,
}

pub struct App {
    window: Option<PaletteWindow>,
    /// By window.
    keyboards: HashMap<isize, LentKeyboard>,
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
    dock: Node,
    dragging_splitter: bool,
    floating: Vec<FloatingWindow>,
    next_window_id: u64,
    dragging: Option<PanelDrag>,
    ghost: frame::DragGhost,
    /// The window that the ghost is standing in for, until it appears.
    ghost_until: Option<u64>,
    /// Floating windows to take back into the main window.
    docking_back: Vec<u64>,
    dock_targets: Vec<DockTarget>,
    main_scale: f32,
    /// The height that the main window's minimum was last set for.
    main_min_height: f32,
    drop_indicator: frame::DropIndicator,

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
                dock: None,
                panels: None,
                settings_offset: None,
                settings_size: None,
            });
        picker.set_modes(layout.wheel, layout.slider);
        let dock = saved_dock(&layout);
        let (settings_offset, settings_size) = (layout.settings_offset, layout.settings_size);
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
            keyboards: HashMap::new(),
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
            settings_offset,
            settings_size,
            settings_icon: IconButton::new(SETTINGS_ICON),
            close_icon: IconButton::new(CLOSE_ICON),
            settings_close_icon: IconButton::new(CLOSE_ICON),
            tray,
            close_dialog_open: false,
            quitting: false,
            confirm_update: false,
            dock,
            dragging_splitter: false,
            floating: Vec::new(),
            next_window_id: 1,
            dragging: None,
            ghost: frame::DragGhost::new(),
            ghost_until: None,
            docking_back: Vec::new(),
            dock_targets: Vec::new(),
            main_scale: 1.0,
            main_min_height: 0.0,
            drop_indicator: frame::DropIndicator::new(),
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
        if self.saved_slots.is_due(&self.slots, ctx) {
            self.save_colors();
        }
        let layout = self.current_layout();
        if self.saved_layout.is_due(&layout, ctx) {
            self.save_layout(layout);
        }
        if self.saved_settings.is_due(&self.settings, ctx) {
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
        SavedLayout {
            position,
            size,
            wheel,
            slider,
            dock: Some(self.dock_with_floating().rounded()),
            panels: None,
            settings_offset: self.settings_offset,
            settings_size: self.settings_size,
        }
    }

    /// Floating windows don't outlive the app yet, so what is saved has their panels back
    /// in the main window.
    fn dock_with_floating(&self) -> Node {
        let mut dock = self.dock.clone();
        for panels in self.floating.iter().map(|window| window.node.panels()) {
            if let Some(&last) = dock.panels().last() {
                dock.dock(last, DockDrop::Side { axis: Axis::Vertical, before: false }, panels);
            }
        }
        dock
    }

    fn save_colors(&mut self) {
        self.saved_slots.written(self.slots);
        if let Err(e) = session::save_colors(&self.slots.into()) {
            self.log(format!("保存颜色失败：{e}"));
        }
    }

    fn save_layout(&mut self, layout: SavedLayout) {
        let result = session::save_layout(&layout);
        self.saved_layout.written(layout);
        if let Err(e) = result {
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
        if !self.settings.restore_csp || self.keyboards.values().any(|keyboard| keyboard.typing) {
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

    fn main_hwnd(&self) -> isize {
        self.window.as_ref().map_or(0, PaletteWindow::hwnd)
    }

    /// Lends a window the keyboard only while a text field has focus, or where text can be
    /// `selectable`, while some is selected to be copied.
    fn sync_keyboard(&mut self, ctx: &egui::Context, hwnd: isize, selectable: bool) {
        let selecting = selectable && ctx.plugin::<LabelSelectionState>().lock().has_selection();
        let wants_keyboard = ctx.egui_wants_keyboard_input() || selecting;
        if ctx.input(|i| i.pointer.any_pressed()) {
            for (_, other) in self.keyboards.iter_mut().filter(|(other, keyboard)| **other != hwnd && keyboard.typing) {
                other.end_requested = true;
                ctx.request_repaint();
            }
        }
        let lent = self.keyboards.entry(hwnd).or_default();
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
            if !platform::set_typing(hwnd, wants_keyboard) {
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
        self.cloak_floating(true);
    }

    fn cloak_floating(&self, cloaked: bool) {
        for hwnd in self.floating.iter().filter_map(|window| window.hwnd) {
            frame::set_cloaked(hwnd as _, cloaked);
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
                self.cloak_floating(false);
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

    /// The panels fill the window, with lines between them that resize the one before them.
    fn panels_ui(&mut self, ui: &mut egui::Ui) {
        let area = ui.max_rect();
        self.main_scale = ui.ctx().pixels_per_point();
        self.dock_targets.clear();
        let mut dock = std::mem::take(&mut self.dock);
        let min_height = dock.min_size(Axis::Vertical);
        if min_height != self.main_min_height {
            self.main_min_height = min_height;
            let size = egui::vec2(dock.min_size(Axis::Horizontal), TITLE_BAR_HEIGHT + min_height);
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::MinInnerSize(size));
        }
        // Windows only applies a minimum the next time the window is resized.
        if area.height() + 0.5 < min_height {
            let size = egui::vec2(area.width(), TITLE_BAR_HEIGHT + min_height);
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::InnerSize(size));
        }
        if !self.dragging_splitter {
            dock.fit(area.width(), area.height());
        }
        let mut changed = false;
        let mut grow = egui::Vec2::ZERO;
        self.node_ui(ui, &mut dock, area, ui.scope_id().with("dock"), &mut changed, &mut grow, None);
        if changed {
            self.color_edited();
        }
        if grow.x > 0.0 || grow.y > 0.0 {
            let width = dock.total(Axis::Horizontal).unwrap_or(area.width() + grow.x);
            let height = dock.total(Axis::Vertical).unwrap_or(area.height() + grow.y);
            let size = egui::vec2(width, TITLE_BAR_HEIGHT + height);
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::InnerSize(size));
        }
        self.dock = dock;
    }

    fn node_ui(
        &mut self,
        ui: &mut egui::Ui,
        node: &mut Node,
        rect: egui::Rect,
        id: egui::Id,
        changed: &mut bool,
        grow: &mut egui::Vec2,
        source: Option<u64>,
    ) {
        let (axis, children) = match node {
            Node::Split { axis, children } => (*axis, children),
            Node::Tabs { panels, active } => {
                self.stack_ui(ui, panels, active, rect, id, changed, source);
                return;
            }
        };
        let start = along(rect.min, axis);
        for index in 0..children.len() {
            let before: f32 = children[..index].iter().map(|child| child.size).sum();
            let child_rect = span(rect, axis, start + before, start + before + children[index].size);
            let grown_before = *grow;
            self.node_ui(ui, &mut children[index].node, child_rect, id.with(index), changed, grow, source);
            // A nested split that had the window grow has made this child bigger too.
            children[index].size += along((*grow - grown_before).to_pos2(), axis);
            if index + 1 < children.len() {
                let line = start + before + children[index].size;
                let delta = self.splitter_ui(ui, rect, axis, line, id.with(("splitter", index)));
                let grown = layout::drag_line(children, axis, index, delta);
                match axis {
                    Axis::Horizontal => grow.x += grown,
                    Axis::Vertical => grow.y += grown,
                }
            }
        }
    }

    /// A panel or a stack of tabs under its title strip, in the main window.
    fn stack_ui(
        &mut self,
        ui: &mut egui::Ui,
        panels: &[Panel],
        active: &mut usize,
        rect: egui::Rect,
        id: egui::Id,
        changed: &mut bool,
        source: Option<u64>,
    ) {
        let strip = egui::Rect::from_min_size(rect.min, egui::vec2(rect.width(), MODULE_TITLE_HEIGHT));
        let content = egui::Rect::from_min_max(egui::pos2(rect.left(), strip.bottom()), rect.max);
        let background = ui.interact(strip, id.with("strip"), egui::Sense::drag());
        ui.painter().rect_filled(strip, 0.0, MODULE_TITLE_FILL);
        let (tear, tab_edges) = tabs_ui(ui, panels, active, strip, id);
        let tear = tear.or_else(|| dragged_far_enough(ui, &background).then(|| panels.to_vec()));
        if let Some(panels) = tear {
            self.start_panel_drag(ui, source, self.main_hwnd(), panels, rect);
        }
        if source.is_none() {
            self.dock_targets.push(DockTarget { panels: panels.to_vec(), rect, tab_edges });
        }
        if let Some(&panel) = panels.get(*active).or(panels.first()) {
            *changed |= self.panel_ui(ui, panel, content);
        }
    }

    fn start_panel_drag(&mut self, ui: &egui::Ui, source: Option<u64>, hwnd: isize, panels: Vec<Panel>, rect: egui::Rect) {
        if self.dragging.is_some() {
            return;
        }
        let Some(pointer) = ui.ctx().input(|input| input.pointer.press_origin()) else { return };
        let scale = ui.ctx().pixels_per_point();
        let grab = (pointer - rect.min) * scale;
        let [x, y] = frame::window_origin(hwnd as _);
        self.ghost.show(RECT {
            left: x + (rect.left() * scale) as i32,
            top: y + (rect.top() * scale) as i32,
            right: x + (rect.right() * scale) as i32,
            bottom: y + (rect.bottom() * scale) as i32,
        });
        self.dragging = Some(PanelDrag { source, panels, size: rect.size(), grab: [grab.x as i32, grab.y as i32] });
        ui.ctx().request_repaint_of(egui::ViewportId::ROOT);
    }

    /// Keeps the ghost under the cursor, and when the pen lifts, does what was asked.
    fn sync_panel_drag(&mut self, ctx: &egui::Context) {
        if let Some(id) = self.ghost_until {
            let window = self.floating.iter().find(|window| window.id == id);
            if window.is_none_or(|window| window.hwnd.is_some()) {
                self.ghost.hide();
                self.ghost_until = None;
            } else {
                ctx.request_repaint();
            }
        }
        let Some(drag) = &self.dragging else { return };
        let [x, y] = platform::cursor_position();
        let position = [x - drag.grab[0], y - drag.grab[1]];
        if pen_or_mouse_down() {
            self.ghost.move_to(position);
            ctx.request_repaint();
            return;
        }
        let Some(drag) = self.dragging.take() else { return };
        let spot = self.drop_at_cursor(&drag);
        let over_main = self.cursor_over_main();
        let node = match drag.source {
            None => Some(&mut self.dock),
            Some(id) => self.floating.iter_mut().find(|window| window.id == id).map(|window| &mut window.node),
        };
        // A window has to keep something.
        let Some(node) = node.filter(|node| node.panels().len() > drag.panels.len()) else {
            self.ghost.hide();
            return;
        };
        if spot.is_none() && over_main {
            self.ghost.hide();
            return;
        }
        for &panel in &drag.panels {
            node.remove(panel);
        }
        if let Some((target, drop, _)) = spot {
            self.dock.dock(target, drop, drag.panels);
            self.ghost.hide();
            return;
        }
        let id = self.next_window_id;
        self.next_window_id += 1;
        self.floating.push(FloatingWindow {
            id,
            node: Node::tabs(drag.panels),
            hwnd: None,
            position,
            size: drag.size,
            close_icon: IconButton::new(CLOSE_ICON),
        });
        self.ghost_until = Some(id);
        ctx.request_repaint();
    }

    fn cursor_over_main(&self) -> bool {
        let Some((position, size)) = self.window.as_ref().and_then(PaletteWindow::placement) else { return false };
        let [x, y] = platform::cursor_position();
        (position[0]..position[0] + size[0]).contains(&x) && (position[1]..position[1] + size[1]).contains(&y)
    }

    /// Where `drag`, dropped at the cursor, would land in the main window, and the area to
    /// mark for that, at 100% scale from the window's corner: all of a module to join its
    /// tabs, or with no width or height, the line where it would go in between tabs or beside
    /// a module. The target is a panel that stays where it is after the drag, so a stack can't
    /// land on itself.
    fn drop_at_cursor(&self, drag: &PanelDrag) -> Option<(Panel, DockDrop, egui::Rect)> {
        if platform::is_cloaked() || !self.cursor_over_main() {
            return None;
        }
        let (position, _) = self.window.as_ref()?.placement()?;
        let [x, y] = platform::cursor_position();
        let point = egui::pos2((x - position[0]) as f32, (y - position[1]) as f32) / self.main_scale;
        // The title bar above the first module counts as the top of that module.
        let target = self.dock_targets.iter().find(|target| target.rect.contains(point)).or_else(|| {
            self.dock_targets.first().filter(|first| point.y < first.rect.top())
        })?;
        let moving = |panel: &&Panel| drag.source.is_none() && drag.panels.contains(panel);
        let &panel = target.panels.iter().find(|panel| !moving(panel))?;
        let rect = target.rect;
        let strip_bottom = rect.top() + MODULE_TITLE_HEIGHT;
        let side = |before: bool, line: f32| {
            let mark = egui::Rect::from_x_y_ranges(rect.x_range(), line..=line);
            (panel, DockDrop::Side { axis: Axis::Vertical, before }, mark)
        };
        if point.y < rect.top() + DROP_ABOVE_ZONE {
            return Some(side(true, rect.top()));
        }
        if point.y < strip_bottom + DROP_TAB_ZONE {
            let tab_centers = target.tab_edges.windows(2).map(|tab| (tab[0] + tab[1]) / 2.0);
            let index = tab_centers.filter(|center| point.x > *center).count();
            let line = target.tab_edges.get(index).copied().unwrap_or(rect.left());
            let mark = egui::Rect::from_x_y_ranges(line..=line, rect.top()..=strip_bottom);
            // Where that is once what is moving has left, if it comes from these same tabs.
            let leaving = target.panels[..index.min(target.panels.len())].iter().filter(|panel| moving(panel)).count();
            return Some((panel, DockDrop::Tab(index - leaving), mark));
        }
        if point.y > rect.bottom() - DROP_BELOW_ZONE {
            return Some(side(false, rect.bottom()));
        }
        Some((panel, DockDrop::Tab(usize::MAX), rect))
    }

    /// Windows take their own title bar drags, and say when one has ended.
    fn sync_moved_windows(&mut self) {
        let hwnd = frame::take_move_end();
        let Some(index) = self.floating.iter().position(|window| window.hwnd == Some(hwnd)) else { return };
        let window = &self.floating[index];
        let panels = window.node.panels();
        let drag = PanelDrag { source: Some(window.id), panels, size: egui::Vec2::ZERO, grab: [0, 0] };
        if let Some((target, drop, _)) = self.drop_at_cursor(&drag) {
            self.floating.remove(index);
            self.dock.dock(target, drop, drag.panels);
        }
    }

    /// While something is being dragged, shows where it would land.
    fn sync_drop_indicator(&mut self, ctx: &egui::Context) {
        let moving = frame::moving_window();
        let floating = self.floating.iter().find(|window| window.hwnd == Some(moving));
        let moved = floating.map(|window| PanelDrag {
            source: Some(window.id),
            panels: window.node.panels(),
            size: egui::Vec2::ZERO,
            grab: [0, 0],
        });
        let Some(drag) = self.dragging.as_ref().or(moved.as_ref()) else {
            self.drop_indicator.hide();
            return;
        };
        ctx.request_repaint();
        let spot = self.drop_at_cursor(drag);
        let placement = self.window.as_ref().and_then(PaletteWindow::placement);
        let (Some((_, _, rect)), Some((position, size))) = (spot, placement) else {
            self.drop_indicator.hide();
            return;
        };
        let scale = self.main_scale;
        let pixels = |value: f32| (value * scale).round() as i32;
        // A line along one axis is kept whole at the window's edge.
        let line = |at: f32, length: i32| {
            let start = (pixels(at) - DROP_LINE / 2).clamp(0, (length - DROP_LINE).max(0));
            (start, start + DROP_LINE)
        };
        let span = |start: f32, end: f32, length: i32| (pixels(start).clamp(0, length), pixels(end).clamp(0, length));
        let ((left, right), (top, bottom), edges) = if rect.width() == 0.0 {
            (line(rect.left(), size[0]), span(rect.top(), rect.bottom(), size[1]), [DROP_EDGE, 0])
        } else if rect.height() == 0.0 {
            (span(rect.left(), rect.right(), size[0]), line(rect.top(), size[1]), [0, DROP_EDGE])
        } else {
            (span(rect.left(), rect.right(), size[0]), span(rect.top(), rect.bottom(), size[1]), [DROP_EDGE, DROP_EDGE])
        };
        self.drop_indicator.show(
            RECT {
                left: position[0] + left,
                top: position[1] + top,
                right: position[0] + right,
                bottom: position[1] + bottom,
            },
            edges,
        );
    }

    fn floating_windows(&mut self, ctx: &egui::Context) {
        let mut windows = std::mem::take(&mut self.floating);
        for window in &mut windows {
            self.floating_window(ctx, window);
        }
        self.floating = windows;
        for id in std::mem::take(&mut self.docking_back) {
            if let Some(index) = self.floating.iter().position(|window| window.id == id) {
                let window = self.floating.remove(index);
                if let Some(&last) = self.dock.panels().last() {
                    let drop = DockDrop::Side { axis: Axis::Vertical, before: false };
                    self.dock.dock(last, drop, window.node.panels());
                }
            }
        }
    }

    fn floating_window(&mut self, ctx: &egui::Context, window: &mut FloatingWindow) {
        let title = window.title();
        let min_size = [window.node.min_size(Axis::Horizontal), window.node.min_size(Axis::Vertical)];
        let builder = egui::ViewportBuilder::default()
            .with_title(title.clone())
            .with_inner_size(window.size)
            .with_min_inner_size(min_size)
            .with_decorations(false)
            .with_maximize_button(false)
            .with_taskbar(false)
            // Hidden until it has been placed, as with the settings window.
            .with_visible(window.hwnd.is_some());
        let id = egui::ViewportId::from_hash_of(("floating", window.id));
        ctx.show_viewport_immediate(id, builder, |ui, _class| {
            let ctx = ui.ctx().clone();
            let Some(hwnd) = window.hwnd.or_else(|| {
                let hwnd = platform::find_own_window(&title)?;
                platform::guard_window(hwnd, true);
                frame::install(hwnd, true, platform::palette_shadow_allowed);
                platform::move_window(hwnd, window.position);
                platform::raise_new_window(hwnd);
                window.hwnd = Some(hwnd as isize);
                ctx.request_repaint_of(egui::ViewportId::ROOT);
                Some(hwnd as isize)
            }) else {
                return;
            };
            self.sync_keyboard(&ctx, hwnd, false);
            self.sync_csp_restore(&ctx);
            let frame = egui::Frame::central_panel(ui.style()).inner_margin(0.0);
            egui::CentralPanel::default().frame(frame).show(ui, |ui| {
                let closed = self.floating_contents_ui(ui, window, hwnd);
                window_edge(ui);
                if closed || ui.input(|input| input.viewport().close_requested()) {
                    self.docking_back.push(window.id);
                }
            });
        });
    }

    /// The title strip here is also the window's title bar. Returns whether it was closed.
    fn floating_contents_ui(&mut self, ui: &mut egui::Ui, window: &mut FloatingWindow, hwnd: isize) -> bool {
        let rect = ui.max_rect();
        let id = ui.scope_id().with("floating");
        let strip = egui::Rect::from_min_size(rect.min, egui::vec2(rect.width(), MODULE_TITLE_HEIGHT));
        let content = egui::Rect::from_min_max(egui::pos2(rect.left(), strip.bottom()), rect.max);
        ui.painter().rect_filled(strip, 0.0, TITLE_BAR_FILL);
        let Node::Tabs { panels, active } = &mut window.node else { return false };
        let (tear, tab_edges) = tabs_ui(ui, panels, active, strip, id);
        // A lone panel has nothing but its name there, which drags the window like the rest.
        let tabs_end = tab_edges.last().copied().unwrap_or(rect.left());
        let caption_left = if panels.len() > 1 { tabs_end - rect.left() } else { 0.0 };
        frame::set_caption_left(hwnd as _, caption_left);
        if let Some(torn) = tear {
            self.start_panel_drag(ui, Some(window.id), hwnd, torn, rect);
        }
        let close = egui::Rect::from_min_max(egui::pos2(strip.right() - TITLE_BUTTON_WIDTH, strip.top()), strip.max);
        let closed = window.close_icon.show_in(ui, close, id.with("close"));
        let panel = panels.get(*active).or(panels.first()).copied();
        if let Some(panel) = panel {
            if self.panel_ui(ui, panel, content) {
                self.color_edited();
            }
        }
        closed
    }

    /// Returns whether the colour was changed.
    fn panel_ui(&mut self, ui: &mut egui::Ui, panel: Panel, rect: egui::Rect) -> bool {
        let content = rect.shrink2(egui::vec2(CONTENT_MARGIN, PANEL_PADDING));
        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(content));
        child.set_clip_rect(rect.intersect(ui.clip_rect()));
        match panel {
            Panel::Slots => {
                self.color_header_ui(&mut child);
                false
            }
            Panel::Wheel => self.picker.wheel_panel(&mut child, &mut self.color),
            Panel::Sliders => self.picker.slider_panel(&mut child, &mut self.color),
        }
    }

    /// The line at `position` along `axis`, across `area`. Returns how far it was dragged.
    fn splitter_ui(&mut self, ui: &mut egui::Ui, area: egui::Rect, axis: Axis, position: f32, id: egui::Id) -> f32 {
        let grab = span(area, axis, position - SPLITTER_GRAB, position + SPLITTER_GRAB);
        let cursor = match axis {
            Axis::Horizontal => egui::CursorIcon::ResizeHorizontal,
            Axis::Vertical => egui::CursorIcon::ResizeVertical,
        };
        let response = ui.interact(grab, id, egui::Sense::drag()).on_hover_cursor(cursor);
        let color = if response.hovered() || response.dragged() {
            SPLITTER_ACTIVE
        } else {
            ui.visuals().widgets.noninteractive.bg_stroke.color
        };
        let stroke = egui::Stroke::new(1.0, color);
        match axis {
            Axis::Horizontal => {
                let range = egui::Rangef::new(area.top() + CONTENT_MARGIN, area.bottom() - CONTENT_MARGIN);
                ui.painter().vline(position, range, stroke);
            }
            Axis::Vertical => {
                let range = egui::Rangef::new(area.left() + CONTENT_MARGIN, area.right() - CONTENT_MARGIN);
                ui.painter().hline(range, position, stroke);
            }
        }
        if response.drag_started() {
            self.dragging_splitter = true;
        }
        if response.drag_stopped() {
            self.dragging_splitter = false;
        }
        if !response.dragged() {
            return 0.0;
        }
        along(response.drag_delta().to_pos2(), axis)
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
        if !self.keyboards.values().any(|keyboard| keyboard.typing) {
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
                platform::guard_window(hwnd, false);
                // Only ever active because the user went to it, so its shadow has no conditions.
                frame::install(hwnd, true, |_| true);
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
            if self.keyboards.remove(&hwnd).is_some_and(|keyboard| keyboard.typing) {
                platform::set_typing(hwnd, false);
                self.restore_after_typing = true;
            }
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
                self.sync_keyboard(ui.ctx(), hwnd, true);
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
        self.sync_keyboard(&ctx, self.main_hwnd(), false);
        self.sync_csp_restore(&ctx);
        self.sync_saved_state(&ctx);
        self.sync_panel_drag(&ctx);
        self.sync_moved_windows();
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
        self.sync_drop_indicator(&ctx);
        self.floating_windows(&ctx);

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

/// The name of a panel, or the tabs of several, along a title strip. Returns the panels that
/// the user started dragging out, if any, and where each tab begins and the last one ends.
fn tabs_ui(
    ui: &mut egui::Ui,
    panels: &[Panel],
    active: &mut usize,
    strip: egui::Rect,
    id: egui::Id,
) -> (Option<Vec<Panel>>, Vec<f32>) {
    let font = egui::FontId::proportional(TITLE_FONT_SIZE);
    let mut left = strip.left();
    let mut edges = vec![left];
    let mut torn = None;
    for (index, &panel) in panels.iter().enumerate() {
        let galley = ui.painter().layout_no_wrap(panel_name(panel).to_owned(), font.clone(), TITLE_TEXT);
        let width = galley.size().x + MODULE_TITLE_PADDING * 2.0;
        let tab = egui::Rect::from_min_size(egui::pos2(left, strip.top()), egui::vec2(width, strip.height()));
        if panels.len() > 1 {
            let response = ui.interact(tab, id.with(("tab", index)), egui::Sense::click_and_drag());
            // As soon as it is pressed, so that it is what shows when it comes to be dragged.
            if response.is_pointer_button_down_on() {
                *active = index;
            }
            if dragged_far_enough(ui, &response) {
                torn = Some(vec![panel]);
            }
            if index == *active {
                ui.painter().rect_filled(tab, 0.0, ACTIVE_TAB_FILL);
            }
        }
        let text = egui::pos2(tab.left() + MODULE_TITLE_PADDING, tab.center().y - galley.size().y / 2.0);
        ui.painter().galley(text, galley, TITLE_TEXT);
        left += width;
        edges.push(left);
    }
    (torn, edges)
}

/// Whether what `response` is about is being dragged, by more than a shaky pen would.
fn dragged_far_enough(ui: &egui::Ui, response: &egui::Response) -> bool {
    response.dragged()
        && ui.ctx().input(|input| {
            let pointer = &input.pointer;
            pointer.press_origin().zip(pointer.latest_pos()).is_some_and(|(from, to)| (to - from).length() >= DRAG_THRESHOLD)
        })
}

fn panel_name(panel: Panel) -> &'static str {
    match panel {
        Panel::Slots => "颜色",
        Panel::Wheel => "色环",
        Panel::Sliders => "滑块",
    }
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

/// What the saved layout says about the panels, or failing that, the default arrangement.
fn saved_dock(layout: &SavedLayout) -> Node {
    // Those heights had no title strips.
    let upgraded = layout.panels.map(|heights| Node::stack(heights.map(|height| height as f32 + MODULE_TITLE_HEIGHT)));
    layout.dock.clone().or(upgraded).filter(Node::is_complete).unwrap_or_default()
}

fn along(point: egui::Pos2, axis: Axis) -> f32 {
    match axis {
        Axis::Horizontal => point.x,
        Axis::Vertical => point.y,
    }
}

/// The part of `rect` from `start` to `end` along `axis`, and all of it across.
fn span(rect: egui::Rect, axis: Axis, start: f32, end: f32) -> egui::Rect {
    match axis {
        Axis::Horizontal => egui::Rect::from_x_y_ranges(start..=end, rect.y_range()),
        Axis::Vertical => egui::Rect::from_x_y_ranges(rect.x_range(), start..=end),
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
    fn a_layout_from_1_0_1_keeps_its_heights_and_damaged_ones_fall_back() {
        let (wheel, slider) = Picker::default().modes();
        let mut layout = SavedLayout {
            position: None,
            size: None,
            wheel,
            slider,
            dock: None,
            panels: Some([80, 300, 170]),
            settings_offset: None,
            settings_size: None,
        };
        assert_eq!(saved_dock(&layout), Node::stack([100.0, 320.0, 190.0]));

        layout.dock = Some(Node::stack([90.0, 310.0, 180.0]));
        assert_eq!(saved_dock(&layout), Node::stack([90.0, 310.0, 180.0]));

        layout.dock = Some(Node::Tabs { panels: vec![Panel::Wheel], active: 0 });
        assert_eq!(saved_dock(&layout), Node::default());
    }

    #[test]
    fn the_smallest_window_fits_the_minimum_panels_under_the_title_bar() {
        assert_eq!(MIN_WINDOW_HEIGHT, 490.0);
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
