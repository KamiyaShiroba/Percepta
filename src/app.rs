//! The main window: the colour panels, and the link that keeps them in sync with CSP.

use std::collections::VecDeque;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, RichText};

use crate::color::ColorState;
use crate::csp::auth::{parse_qr_url, Pairing};
use crate::csp::client::CspSlots;
use crate::csp::worker::{Command, Event, Worker};
use crate::csp::{Rgb, Slot};
use crate::picker::{app_icon_pixels, switch, IconButton, Picker};
use crate::platform::{click_csp_title_bar, pen_or_mouse_down, PaletteWindow};
use crate::session::{SavedColors, SavedLayout, SavedSettings};
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
const SLOT_CHIP_HEIGHT: f32 = 32.0;
const SLOT_CHIP_GAP: f32 = 6.0;
/// The active slot's outline sits around the colour, with this much room for it.
const SLOT_OUTLINE_ROOM: f32 = 3.0;
const SLOT_CORNER: f32 = 5.0;
const HEX_WIDTH: f32 = 76.0;
const STATUS_DOT_SIZE: f32 = 9.0;
const CONNECTED: Color32 = Color32::from_rgb(70, 180, 100);
const WAITING: Color32 = Color32::from_rgb(205, 180, 40);
const DISCONNECTED: Color32 = Color32::from_gray(150);
const SETTINGS_WIDTH: f32 = 290.0;
/// Room left around the settings box, including its own padding and shadow.
const SETTINGS_WINDOW_MARGIN: f32 = 32.0;
/// Room beside the address field for its button.
const CONNECT_BUTTON_ROOM: f32 = 52.0;
const LOG_HEIGHT: f32 = 160.0;
const CHECKER_SIZE: f32 = 6.0;
const CHECKER_LIGHT: Color32 = Color32::from_gray(235);
const CHECKER_DARK: Color32 = Color32::from_gray(190);
const ACTIVE_SLOT_WIDTH: f32 = 2.0;
/// The mark on the settings button when an update is waiting.
const UPDATE_DOT_RADIUS: f32 = 3.0;

/// Things are saved once they stop changing, not on every step of a drag.
const SAVE_DELAY: Duration = Duration::from_secs(1);
const SETTINGS_ICON: &str = include_str!("../assets/settings.svg");

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

pub struct App {
    window: Option<PaletteWindow>,
    typing: bool,
    /// The window has actually become active since typing started.
    typing_active: bool,
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
    settings_icon: IconButton,

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
        let layout = session::load_layout().unwrap_or(SavedLayout { position: None, size: None, wheel, slider });
        picker.set_modes(layout.wheel, layout.slider);
        if let Some(window) = &window {
            window.set_icon(app_icon_pixels);
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
            typing: false,
            typing_active: false,
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
            settings_icon: IconButton::new(SETTINGS_ICON),
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
        SavedLayout { position, size, wheel, slider }
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
            ctx.request_repaint()
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
        if !self.settings.restore_csp || self.typing {
            self.restore_at = None;
            return;
        }
        let (released, down) = ctx.input(|i| (i.pointer.any_released(), i.pointer.any_down()));
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
            Err(reason) => self.log(format!("没有恢复 CSP：{reason}")),
        }
    }

    /// Lends the window the keyboard only while a text field has focus.
    fn sync_keyboard(&mut self, ctx: &egui::Context) {
        let wants_keyboard = ctx.egui_wants_keyboard_input();
        if wants_keyboard != self.typing {
            self.typing = wants_keyboard;
            self.typing_active = false;
            let granted = self.window.as_mut().is_none_or(|w| w.set_typing(wants_keyboard));
            if !granted {
                self.log("Windows 拒绝切换键盘焦点，请再点一次输入框");
            }
        }
        if self.typing {
            let window_active = ctx.input(|i| i.focused);
            if window_active {
                self.typing_active = true;
            } else if self.typing_active {
                // The user went back to CSP; end the edit so the next click on
                // a slider does not activate the window again.
                ctx.memory_mut(|memory| {
                    if let Some(id) = memory.focused() {
                        memory.surrender_focus(id);
                    }
                });
                ctx.request_repaint();
            }
        }
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
        if !self.typing {
            self.hex = to_hex(rgb);
        }
        let mut chosen = None;
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = SLOT_CHIP_GAP;
            let unit = (ui.available_width() - SLOT_CHIP_GAP * 2.0) / SLOT_WIDTHS.iter().sum::<f32>();
            let slots = [(Slot::Main, "前景色"), (Slot::Sub, "背景色"), (Slot::Transparent, "透明色")];
            for ((slot, name), width) in slots.into_iter().zip(SLOT_WIDTHS) {
                if slot_chip(ui, unit * width, self.slots.color(slot), self.slots.active == slot, name) {
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
                    self.settings_open = true;
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
        ui.heading("设置");
        ui.add_space(4.0);
        ui.label(RichText::new("连接 CSP").strong());
        self.connection_ui(ui);
        switch(ui, &mut self.settings.restore_csp, "抬笔后自动点击 CSP 标题栏")
            .on_hover_text("修复用笔操作取色器后，数位板按键映射错误和笔刷光标卡住的问题");
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
        ui.add_space(4.0);
        if ui.button("关闭").clicked() {
            self.settings_open = false;
        }
    }

    fn update_ui(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        ui.label(format!("当前版本：{}", update::CURRENT_VERSION));
        let mut install = None;
        let mut check = false;
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
                if ui.button("下载并安装").on_hover_text("Percepta 会关闭，装好后自动重新打开").clicked() {
                    install = Some(release.clone());
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
        self.sync_keyboard(&ctx);
        self.sync_csp_restore(&ctx);
        self.sync_saved_state(&ctx);
        self.updater.poll();
        if matches!(self.updater.state, update::State::Installing) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }

        egui::CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                self.color_header_ui(ui);
                ui.separator();
                let mut changed = self.picker.wheel_panel(ui, &mut self.color);
                ui.separator();
                changed |= self.picker.slider_panel(ui, &mut self.color);
                if changed {
                    self.color_edited();
                }
            });
        });

        if self.settings_open {
            let modal = egui::Modal::new(ui.scope_id().with("settings")).show(&ctx, |ui| {
                let room = ui.ctx().content_rect().size() - egui::Vec2::splat(SETTINGS_WINDOW_MARGIN);
                ui.set_width(SETTINGS_WIDTH.min(room.x));
                egui::ScrollArea::vertical().max_height(room.y).show(ui, |ui| self.settings_ui(ui));
            });
            if modal.should_close() {
                self.settings_open = false;
            }
        }
    }
}

/// A colour slot's button: its colour, or a checkerboard for transparent, outlined in
/// the accent colour while active. Returns whether it was clicked.
fn slot_chip(ui: &mut egui::Ui, width: f32, rgb: Option<Rgb>, active: bool, name: &str) -> bool {
    let size = egui::vec2(width, SLOT_CHIP_HEIGHT);
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
    fn hex_round_trips_and_rejects_partial_input() {
        assert_eq!(to_hex([18, 52, 86]), "#123456");
        assert_eq!(from_hex("#123456"), Some([18, 52, 86]));
        assert_eq!(from_hex("abcdef"), Some([0xAB, 0xCD, 0xEF]));
        assert_eq!(from_hex("#1234"), None);
        assert_eq!(from_hex("#12345g"), None);
    }
}
