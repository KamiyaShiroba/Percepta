//! The colour wheel and slider panels. They are separate panels so that they can
//! later be docked, tabbed and torn off independently.

use eframe::egui::{
    self, pos2, vec2, Color32, ColorImage, Mesh, Pos2, Rect, Response, Sense, Shape, Stroke,
    TextureHandle, TextureOptions, Vec2,
};

use serde::{Deserialize, Serialize};

use crate::color::{
    gamut_map, hls_to_rgb, hsv_to_rgb, in_srgb, nearest_on_triangle, oklch_to_rgb_unclipped,
    Channels, ColorState, GamutSlice, Model,
};
use crate::csp::Rgb;
use crate::icc::{Cmyk, CmykConverter};

const MAX_WHEEL_SIZE: f32 = 270.0;
/// Room around the ring for the hue cursor, which sticks out past its edges.
const RING_MARGIN: f32 = 3.0;
const RING_BORDER: Color32 = Color32::from_gray(165);
const RING_BORDER_WIDTH: f32 = 1.0;
const RING_CURSOR_HALF_WIDTH: f32 = 7.0;
const RING_CURSOR_OVERHANG: f32 = 2.5;
/// Inner radius of the hue ring, relative to its outer radius.
const RING_INNER: f32 = 0.82;
const ICON_RING_INNER: f32 = 0.5;
/// Half the square's side, relative to the outer radius; leaves a gap inside the ring.
const SQUARE_HALF: f32 = 0.54;
/// Circumradius of the HLS triangle, relative to the outer radius.
const TRIANGLE_RADIUS: f32 = 0.79;
const TRIANGLE_ASPECT: f32 = 1.5 / 1.732_050_8;
/// Puts hue 138 at about 3 o'clock, the way CSP's wheel is laid out.
const HUE_OFFSET_DEGREES: f32 = -145.0;
const RING_TEXTURE_SIZE: usize = 640;
const SQUARE_TEXTURE_SIZE: usize = 256;
/// Hue resolution of the cached square, in steps per degree.
const SQUARE_HUE_STEPS: f64 = 4.0;

/// The OKLCH square's fixed width: just past sRGB's highest chroma of about 0.32, so
/// every hue's tip is in view and changing hue never moves the cursor.
const OKLCH_MAX_CHROMA: f64 = 0.33;
/// Where the OKLCH hue ring is drawn: colourful, yet inside sRGB at most hues.
const OKLCH_RING_LIGHTNESS: f64 = 0.75;
const OKLCH_RING_CHROMA: f64 = 0.14;
/// Hue tracks need visible colour even when the current colour is nearly grey.
const OKLCH_MIN_TRACK_CHROMA: f64 = 0.08;
const HATCH_PERIOD: usize = 14;
const HATCH_WIDTH: usize = 4;
const INVALID_GREY: f64 = 0.62;
const INVALID_GREY_MIX: f64 = 0.65;
const HATCH_DARKEN: f64 = 0.72;
/// Points along the curved edge above the tip; the edge below it is straight.
const BOUNDARY_SAMPLES: usize = 96;
const BOUNDARY_WIDTH: f32 = 1.5;
const BOUNDARY_HALO_WIDTH: f32 = 3.0;

const SLIDER_SEGMENTS: usize = 64;
const SLIDER_HEIGHT: f32 = 24.0;
const TRACK_HEIGHT: f32 = 15.0;
const LABEL_WIDTH: f32 = 14.0;
const MIN_TRACK_WIDTH: f32 = 40.0;
const VALUE_WIDTH: f32 = 54.0;
const STEP_BUTTON_WIDTH: f32 = 18.0;
const CONTROL_SPACING: f32 = 4.0;
const OUT_OF_GAMUT_STRIP: f32 = 3.0;
const THUMB_HALF_WIDTH: f32 = 5.0;
/// Relative to the standard control height.
const SWITCH_HEIGHT: f32 = 0.9;
const TAB_SPACING: f32 = 2.0;
const TAB_PADDING: f32 = 3.0;
const TAB_BOTTOM_GAP: f32 = 3.0;
/// Below this the switch and the values beside it only fit with a shorter label.
const FULL_LIMIT_LABEL_WIDTH: f32 = 295.0;
const BADGE_SIZE: f32 = 14.0;
const BADGE_CORNER: f32 = 3.0;
const BADGE_FILL: Color32 = Color32::from_gray(50);
const BADGE_FONT_SIZE: f32 = 10.5;
const BADGE_VALUE_GAP: f32 = 4.0;
const BADGE_GROUP_GAP: f32 = 6.0;
const ICON_BUTTON_SIZE: f32 = 22.0;
const ICON_IDLE: Color32 = Color32::from_gray(130);
/// CMYK has this many sliders, and every tab takes the height they need.
const MAX_SLIDER_ROWS: usize = 4;
const SLIDER_ROW_GAP: f32 = 2.0;

const WHEEL_MODELS: [(Model, &str); 3] =
    [(Model::Hsv, "HSV"), (Model::Hls, "HLS"), (Model::Oklch, "OKLCH")];
const SLIDER_TABS: [(SliderTab, &str); 5] = [
    (SliderTab::Color(Model::Rgb), "RGB"),
    (SliderTab::Color(Model::Hsv), "HSV"),
    (SliderTab::Color(Model::Hls), "HLS"),
    (SliderTab::Color(Model::Oklch), "OKLCH"),
    (SliderTab::Cmyk, "CMYK"),
];
const CMYK_CHANNELS: [Channel; 4] = [
    percent_channel("C"),
    percent_channel("M"),
    percent_channel("Y"),
    percent_channel("K"),
];

/// CMYK stands apart from `Model` as it has four channels, and because it takes a
/// colour profile to convert, it lives here rather than in `ColorState`.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum SliderTab {
    Color(Model),
    Cmyk,
}

struct Channel {
    label: &'static str,
    /// Largest coordinate, in the units of `ColorState`.
    max: f64,
    /// Multiplier from coordinate to the number the user sees.
    display: f64,
    decimals: usize,
    /// In displayed units.
    step: f64,
    suffix: &'static str,
}

const fn channel(
    label: &'static str,
    max: f64,
    display: f64,
    decimals: usize,
    step: f64,
    suffix: &'static str,
) -> Channel {
    Channel { label, max, display, decimals, step, suffix }
}

const fn hue_channel(decimals: usize) -> Channel {
    channel("H", 360.0, 1.0, decimals, 1.0, "°")
}

const fn percent_channel(label: &'static str) -> Channel {
    channel(label, 1.0, 100.0, 0, 1.0, "%")
}

fn channels(model: Model) -> [Channel; 3] {
    match model {
        Model::Rgb => [
            channel("R", 1.0, 255.0, 0, 1.0, ""),
            channel("G", 1.0, 255.0, 0, 1.0, ""),
            channel("B", 1.0, 255.0, 0, 1.0, ""),
        ],
        Model::Hsv => [hue_channel(0), percent_channel("S"), percent_channel("V")],
        Model::Hls => [hue_channel(0), percent_channel("L"), percent_channel("S")],
        Model::Oklch => [
            channel("L", 1.0, 100.0, 1, 1.0, "%"),
            channel("C", OKLCH_MAX_CHROMA, 1.0, 3, 0.005, ""),
            hue_channel(1),
        ],
    }
}

/// The colour a model's coordinates show on screen, and whether it is exact.
fn display_rgb(model: Model, coords: Channels) -> (Channels, bool) {
    match model {
        Model::Rgb => (coords, true),
        Model::Hsv => (hsv_to_rgb(coords), true),
        Model::Hls => (hls_to_rgb(coords), true),
        Model::Oklch => (gamut_map(coords), in_srgb(oklch_to_rgb_unclipped(coords))),
    }
}

/// Coordinates for one point along a slider's track. Hue tracks show a full
/// rainbow rather than the current colour's possibly dark or grey version.
fn track_coords(model: Model, current: Channels, index: usize, value: f64) -> Channels {
    let mut coords = current;
    coords[index] = value;
    match (model, index) {
        (Model::Hsv, 0) => [value, 1.0, 1.0],
        (Model::Hls, 0) => [value, 0.5, 1.0],
        (Model::Oklch, 2) if coords[1] < OKLCH_MIN_TRACK_CHROMA => {
            [OKLCH_RING_LIGHTNESS, OKLCH_MIN_TRACK_CHROMA, value]
        }
        _ => coords,
    }
}

fn to_color32(rgb: Channels) -> Color32 {
    let [r, g, b] = rgb.map(|c| (c * 255.0).round().clamp(0.0, 255.0) as u8);
    Color32::from_rgb(r, g, b)
}

/// The sRGB boundary in the OKLCH square as 0..=1 positions: the curved upper edge
/// running from white down to the tip, which comes last. Black is always the
/// square's bottom-left corner.
fn boundary_points(slice: &GamutSlice) -> Vec<[f32; 2]> {
    let position = |lightness: f64, chroma: f64| {
        [(chroma / OKLCH_MAX_CHROMA) as f32, (1.0 - lightness) as f32]
    };
    let mut points: Vec<[f32; 2]> = (0..BOUNDARY_SAMPLES)
        .map(|sample| {
            let fraction = sample as f64 / BOUNDARY_SAMPLES as f64;
            let lightness = 1.0 + (slice.peak_lightness - 1.0) * fraction;
            position(lightness, slice.max_chroma(lightness))
        })
        .collect();
    points.push(position(slice.peak_lightness, slice.peak_chroma));
    points
}

/// Pulls a position in the 0..=1 box around the HLS triangle onto the triangle, with
/// the same rule at all three edges and corners; see `nearest_on_triangle`.
fn clamp_to_triangle(x: f64, y: f64) -> (f64, f64) {
    if x >= 0.0 && x <= triangle_width(1.0 - y) {
        return (x, y);
    }
    // In the triangle's true proportions, where its edges and their perpendiculars
    // look the way they do on screen.
    let aspect = TRIANGLE_ASPECT as f64;
    let [x, y] = nearest_on_triangle([x * aspect, y], [[0.0, 0.0], [0.0, 1.0], [aspect, 0.5]]);
    (x / aspect, y)
}

/// The width of the HLS triangle at this lightness, as a fraction of its full
/// width. It is a slice through the HLS double cone: a point at each end, and
/// the pure hue at the widest point.
fn triangle_width(lightness: f64) -> f64 {
    1.0 - (2.0 * lightness - 1.0).abs()
}

/// Turns a position in the inner area, each axis 0..=1 with y growing downwards,
/// into coordinates. Horizontal is saturation or chroma, vertical is value or
/// lightness.
fn square_coords(model: Model, hue: f64, x: f64, y: f64) -> Channels {
    match model {
        Model::Hls => {
            let lightness = 1.0 - y;
            let width = triangle_width(lightness);
            let saturation = if width > 0.0 { (x / width).min(1.0) } else { 1.0 };
            [hue, lightness, saturation]
        }
        Model::Oklch => [1.0 - y, x * OKLCH_MAX_CHROMA, hue],
        _ => [hue, x, 1.0 - y],
    }
}

fn square_position(model: Model, coords: Channels) -> (f64, f64) {
    match model {
        Model::Hls => (coords[2] * triangle_width(coords[1]), 1.0 - coords[1]),
        Model::Oklch => (coords[1] / OKLCH_MAX_CHROMA, 1.0 - coords[0]),
        _ => (coords[1], 1.0 - coords[2]),
    }
}

/// The area inside the ring that the model's picker occupies.
fn inner_rect(model: Model, center: Pos2, radius: f32) -> Rect {
    if model == Model::Hls {
        // Equilateral, with its apex pointing right and its centre on the wheel's.
        let circumradius = radius * TRIANGLE_RADIUS;
        let height = circumradius * 3f32.sqrt();
        Rect::from_min_size(
            pos2(center.x - circumradius / 2.0, center.y - height / 2.0),
            vec2(height * TRIANGLE_ASPECT, height),
        )
    } else {
        Rect::from_center_size(center, Vec2::splat(radius * SQUARE_HALF * 2.0))
    }
}

fn inner_texture_size(model: Model) -> [usize; 2] {
    if model == Model::Hls {
        let height = SQUARE_TEXTURE_SIZE;
        [(height as f32 * TRIANGLE_ASPECT) as usize, height]
    } else {
        [SQUARE_TEXTURE_SIZE; 2]
    }
}

fn hue_of(model: Model, coords: Channels) -> f64 {
    if model == Model::Oklch {
        coords[2]
    } else {
        coords[0]
    }
}

/// RGBA pixels of the HSV square or the HLS triangle.
fn hue_area_pixels(model: Model, hue: f64, [width, height]: [usize; 2]) -> Vec<u8> {
    let mut pixels = Vec::with_capacity(width * height * 4);
    let (last_x, last_y) = ((width - 1) as f64, (height - 1) as f64);
    for row in 0..height {
        for column in 0..width {
            let (x, y) = (column as f64 / last_x, row as f64 / last_y);
            let coords = square_coords(model, hue, x, y);
            let alpha = if model == Model::Hls {
                // Antialiases the two slanted edges; 0.866 turns horizontal
                // distance into distance from an edge sloped at 60 degrees.
                let inside = (triangle_width(1.0 - y) - x) * last_x * 0.866 + 0.5;
                (inside.clamp(0.0, 1.0) * 255.0) as u8
            } else {
                255
            };
            let [r, g, b, _] = to_color32(display_rgb(model, coords).0).to_array();
            pixels.extend_from_slice(&[r, g, b, alpha]);
        }
    }
    pixels
}

/// The highest in-sRGB chroma on each row of the OKLCH square, top row first.
fn oklch_row_limits(slice: &GamutSlice, rows: usize) -> Vec<f64> {
    let last = (rows - 1) as f64;
    (0..rows)
        .map(|row| slice.max_chroma(1.0 - row as f64 / last))
        .collect()
}

/// RGBA pixels of the OKLCH square. Outside sRGB each row repeats its boundary
/// colour, which is close to what gamut mapping produces and far cheaper to draw.
/// When limited, that area is unselectable, so it is greyed and hatched.
fn oklch_pixels(hue: f64, limited: bool, limits: &[f64], size: usize) -> Vec<u8> {
    let mut pixels = Vec::with_capacity(size * size * 4);
    let last = (size - 1) as f64;
    for (row, &limit) in limits.iter().enumerate() {
        let lightness = 1.0 - row as f64 / last;
        let boundary = oklch_to_rgb_unclipped([lightness, limit, hue]);
        for column in 0..size {
            let chroma = column as f64 / last * OKLCH_MAX_CHROMA;
            let rgb = if chroma <= limit {
                oklch_to_rgb_unclipped([lightness, chroma, hue])
            } else if limited {
                let shade = if (column + row) % HATCH_PERIOD < HATCH_WIDTH { HATCH_DARKEN } else { 1.0 };
                boundary.map(|c| (c + (INVALID_GREY - c) * INVALID_GREY_MIX) * shade)
            } else {
                boundary
            };
            let [r, g, b, _] = to_color32(rgb).to_array();
            pixels.extend_from_slice(&[r, g, b, 255]);
        }
    }
    pixels
}

fn ring_hue_color(model: Model, hue: f64) -> Channels {
    match model {
        Model::Oklch => gamut_map([OKLCH_RING_LIGHTNESS, OKLCH_RING_CHROMA, hue]),
        _ => hsv_to_rgb([hue, 1.0, 1.0]),
    }
}

fn hue_to_angle(hue: f64) -> f32 {
    (hue as f32 + HUE_OFFSET_DEGREES).to_radians()
}

fn angle_to_hue(offset: Vec2) -> f64 {
    (offset.y.atan2(offset.x).to_degrees() - HUE_OFFSET_DEGREES).rem_euclid(360.0) as f64
}

/// RGBA pixels of the app's icon: a hue ring, thick enough to read at small sizes.
pub fn app_icon_pixels(size: usize) -> Vec<u8> {
    ring_pixels(Model::Oklch, size, ICON_RING_INNER)
}

/// RGBA pixels of a hue ring with antialiased edges. `inner` is its inner radius,
/// relative to its outer radius.
fn ring_pixels(model: Model, size: usize, inner: f32) -> Vec<u8> {
    let mut pixels = Vec::with_capacity(size * size * 4);
    let radius = size as f32 / 2.0;
    for row in 0..size {
        for column in 0..size {
            let offset = vec2(column as f32 + 0.5 - radius, row as f32 + 0.5 - radius);
            let distance = offset.length();
            let coverage = (radius - distance).min(distance - radius * inner) + 0.5;
            let alpha = (coverage.clamp(0.0, 1.0) * 255.0) as u8;
            let [r, g, b, _] = to_color32(ring_hue_color(model, angle_to_hue(offset))).to_array();
            pixels.extend_from_slice(&[r, g, b, alpha]);
        }
    }
    pixels
}

/// Tight enough for all five slider tabs to fit the narrowest window. The wheel's
/// tabs match them.
fn tighten_tabs(ui: &mut egui::Ui) {
    ui.spacing_mut().item_spacing.x = TAB_SPACING;
    ui.spacing_mut().button_padding.x = TAB_PADDING;
}

/// A modern on/off switch. Reports a change through `Response::changed`.
pub fn switch(ui: &mut egui::Ui, on: &mut bool, label: &str) -> Response {
    ui.horizontal(|ui| {
        let height = ui.spacing().interact_size.y * SWITCH_HEIGHT;
        let (rect, track) = ui.allocate_exact_size(vec2(height * 1.8, height), Sense::click());
        let text = ui.add(egui::Label::new(label).sense(Sense::click()));
        let mut response = track | text;
        if response.clicked() {
            *on = !*on;
            response.mark_changed();
        }

        let progress = ui.ctx().animate_bool_responsive(response.id, *on);
        let off_color = ui.visuals().widgets.inactive.bg_fill;
        let on_color = ui.visuals().selection.stroke.color;
        let radius = height / 2.0;
        ui.painter().rect_filled(rect, radius, off_color.lerp_to_gamma(on_color, progress));
        let knob_x = egui::lerp(rect.left() + radius..=rect.right() - radius, progress);
        ui.painter().circle_filled(pos2(knob_x, rect.center().y), radius * 0.78, Color32::WHITE);
        response
    })
    .inner
}

/// An SVG icon that needs no background: it shows state by changing its own colour.
/// Rendered white and tinted when drawn, at the screen's real resolution.
pub struct IconButton {
    svg: &'static str,
    texture: Option<(u32, TextureHandle)>,
}

impl IconButton {
    pub const fn new(svg: &'static str) -> Self {
        Self { svg, texture: None }
    }

    /// `active` keeps it in the accent colour, as while its window is open.
    pub fn show(&mut self, ui: &mut egui::Ui, active: bool, hover_text: &str) -> bool {
        let (rect, response) = ui.allocate_exact_size(Vec2::splat(ICON_BUTTON_SIZE), Sense::click());
        let response = response.on_hover_text(hover_text);
        let pixels = (ICON_BUTTON_SIZE * ui.pixels_per_point()).ceil() as u32;
        if self.texture.as_ref().is_none_or(|(size, _)| *size != pixels) {
            self.texture = render_svg(self.svg, pixels).map(|image| {
                (pixels, ui.ctx().load_texture("icon", image, TextureOptions::LINEAR))
            });
        }
        let visuals = ui.visuals();
        let tint = if active || response.is_pointer_button_down_on() {
            visuals.selection.stroke.color
        } else if response.hovered() {
            visuals.widgets.hovered.fg_stroke.color
        } else {
            ICON_IDLE
        };
        if let Some((_, texture)) = &self.texture {
            let whole = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
            ui.painter().image(texture.id(), rect, whole, tint);
        }
        response.clicked()
    }
}

/// `None` only if the SVG is invalid, which the tests rule out for the ones in use.
fn render_svg(svg: &str, size: u32) -> Option<ColorImage> {
    use resvg::{tiny_skia, usvg};
    let white = svg.replace("currentColor", "#ffffff");
    let tree = usvg::Tree::from_str(&white, &usvg::Options::default()).ok()?;
    let mut pixmap = tiny_skia::Pixmap::new(size, size)?;
    let scale = size as f32 / tree.size().width();
    resvg::render(&tree, tiny_skia::Transform::from_scale(scale, scale), &mut pixmap.as_mut());
    Some(ColorImage::from_rgba_premultiplied([size as usize; 2], pixmap.data()))
}

#[derive(Clone, Copy)]
enum WheelDrag {
    Ring,
    Square,
    /// The press began outside both, so dragging onto them must not grab them.
    Nothing,
}

pub struct Picker {
    wheel_model: Model,
    slider_tab: SliderTab,
    converter: CmykConverter,
    /// The user's CMYK numbers, kept as typed for as long as `cmyk_rgb` is the colour.
    /// Deriving them back from RGB would make the sliders jump.
    cmyk: Cmyk,
    cmyk_rgb: Option<Rgb>,
    wheel_drag: Option<WheelDrag>,
    /// The OKLCH chroma when a hue drag began, on the ring or the H slider. Limiting
    /// to sRGB can lower the chroma on the way through some hues, so each step of the
    /// drag aims for this.
    hue_target_chroma: Option<f64>,
    /// RGB and hue-based wheels share a ring texture; OKLCH has its own.
    rainbow_ring: Option<TextureHandle>,
    oklch_ring: Option<TextureHandle>,
    square: Option<TextureHandle>,
    square_key: Option<(Model, i64, bool)>,
    /// See `boundary_points`.
    boundary: Vec<[f32; 2]>,
}

impl Default for Picker {
    fn default() -> Self {
        Self {
            wheel_model: Model::Hsv,
            slider_tab: SliderTab::Color(Model::Hsv),
            converter: CmykConverter::from_csp_settings(),
            cmyk: [0.0; 4],
            cmyk_rgb: None,
            wheel_drag: None,
            hue_target_chroma: None,
            rainbow_ring: None,
            oklch_ring: None,
            square: None,
            square_key: None,
            boundary: Vec::new(),
        }
    }
}

impl Picker {
    pub fn modes(&self) -> (Model, SliderTab) {
        (self.wheel_model, self.slider_tab)
    }

    pub fn set_modes(&mut self, wheel: Model, slider: SliderTab) {
        // A hand-edited settings file must not leave the wheel in a mode it cannot draw.
        if WHEEL_MODELS.iter().any(|(model, _)| *model == wheel) {
            self.wheel_model = wheel;
        }
        self.slider_tab = slider;
    }

    /// Returns whether the user changed the colour.
    pub fn wheel_panel(&mut self, ui: &mut egui::Ui, color: &mut ColorState) -> bool {
        self.end_hue_drag_on_release(ui);
        ui.horizontal(|ui| {
            tighten_tabs(ui);
            for (model, name) in WHEEL_MODELS {
                ui.selectable_value(&mut self.wheel_model, model, name);
            }
        });
        ui.add_space(TAB_BOTTOM_GAP);
        let size = ui.available_width().min(MAX_WHEEL_SIZE);
        let mut changed = ui.vertical_centered(|ui| self.wheel(ui, color, size)).inner;
        ui.horizontal(|ui| {
            // Either panel in OKLCH can run into the limit, and this is the one place
            // with room for the switch whichever tab the sliders are on.
            if self.wheel_model == Model::Oklch || self.slider_tab == SliderTab::Color(Model::Oklch) {
                let roomy = ui.available_width() >= FULL_LIMIT_LABEL_WIDTH;
                changed |= limit_switch(ui, color, if roomy { "限制在 sRGB 内" } else { "sRGB" });
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                value_readout(ui, &value_parts(self.wheel_model, color.coords(self.wheel_model)));
            });
        });
        changed
    }

    /// Returns whether the user changed the colour.
    pub fn slider_panel(&mut self, ui: &mut egui::Ui, color: &mut ColorState) -> bool {
        self.end_hue_drag_on_release(ui);
        let mut changed = false;
        ui.horizontal(|ui| {
            tighten_tabs(ui);
            for (tab, name) in SLIDER_TABS {
                ui.selectable_value(&mut self.slider_tab, tab, name);
            }
        });
        ui.add_space(TAB_BOTTOM_GAP);
        ui.scope(|ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            changed |= match self.slider_tab {
                SliderTab::Color(model) => self.model_sliders(ui, color, model),
                SliderTab::Cmyk => self.cmyk_sliders(ui, color),
            };
        });
        if changed {
            ui.ctx().request_repaint();
        }
        changed
    }

    fn model_sliders(&mut self, ui: &mut egui::Ui, color: &mut ColorState, model: Model) -> bool {
        let channels = channels(model);
        let gap = slider_gap(channels.len());
        let mut changed = false;
        for (index, channel) in channels.iter().enumerate() {
            if index > 0 {
                ui.add_space(gap);
            }
            let coords = color.coords(model);
            let sample = |value| display_rgb(model, track_coords(model, coords, index, value));
            let Some(mut value) = slider_row(ui, channel, coords[index], sample) else {
                continue;
            };
            // RGB holds whole bytes: anything finer could not reach CSP anyway.
            if model == Model::Rgb {
                value = (value * channel.display).round() / channel.display;
            }
            let mut updated = coords;
            updated[index] = value;
            let dragging_hue = model == Model::Oklch && index == 2 && ui.input(|i| i.pointer.any_down());
            if dragging_hue {
                updated[1] = *self.hue_target_chroma.get_or_insert(coords[1]);
            }
            color.set(model, updated);
            changed = true;
        }
        changed
    }

    fn cmyk_sliders(&mut self, ui: &mut egui::Ui, color: &mut ColorState) -> bool {
        let rgb = color.rgb8();
        if self.cmyk_rgb != Some(rgb) {
            self.cmyk = self.converter.from_rgb(rgb.map(|byte| byte as f64 / 255.0));
            self.cmyk_rgb = Some(rgb);
        }
        let gap = slider_gap(CMYK_CHANNELS.len());
        let mut changed = false;
        for (index, channel) in CMYK_CHANNELS.iter().enumerate() {
            if index > 0 {
                ui.add_space(gap);
            }
            let cmyk = self.cmyk;
            let converter = &self.converter;
            let sample = |value| {
                let mut inks = cmyk;
                inks[index] = value;
                (converter.to_rgb(inks), true)
            };
            if let Some(value) = slider_row(ui, channel, cmyk[index], sample) {
                self.cmyk[index] = value;
                changed = true;
            }
        }
        if changed {
            let rgb = self.converter.to_rgb(self.cmyk).map(|c| (c * 255.0).round() as u8);
            color.set_rgb8(rgb);
            self.cmyk_rgb = Some(rgb);
        }
        changed
    }

    /// The profile CMYK is converted through, or `None` for the plain formula.
    pub fn cmyk_profile_name(&self) -> Option<&str> {
        self.converter.profile_name()
    }

    /// Picks up a profile changed in CSP while this has been running.
    pub fn reload_cmyk_profile(&mut self) {
        self.converter = CmykConverter::from_csp_settings();
        self.cmyk_rgb = None;
    }

    fn end_hue_drag_on_release(&mut self, ui: &egui::Ui) {
        if !ui.input(|i| i.pointer.any_down()) {
            self.hue_target_chroma = None;
        }
    }

    fn wheel(&mut self, ui: &mut egui::Ui, color: &mut ColorState, size: f32) -> bool {
        let (rect, response) = ui.allocate_exact_size(Vec2::splat(size), Sense::click_and_drag());
        let center = rect.center();
        let radius = size / 2.0 - RING_MARGIN;
        let model = self.wheel_model;
        let square = inner_rect(model, center, radius);
        let limited = color.limit_to_srgb;
        let coords = color.coords(model);
        let hue = hue_of(model, coords);

        let mut changed = false;
        if !response.is_pointer_button_down_on() {
            self.wheel_drag = None;
        } else if let Some(pointer) = response.interact_pointer_pos() {
            let offset = pointer - center;
            let drag = *self.wheel_drag.get_or_insert_with(|| {
                // Anywhere inside the ring counts, as the point is pulled onto the
                // picker: the gaps around it are too thin to mean anything else.
                if offset.length() < radius * RING_INNER {
                    WheelDrag::Square
                } else if offset.length() <= radius {
                    WheelDrag::Ring
                } else {
                    WheelDrag::Nothing
                }
            });
            match drag {
                WheelDrag::Ring => {
                    let mut updated = coords;
                    if model == Model::Oklch {
                        updated[1] = *self.hue_target_chroma.get_or_insert(coords[1]);
                        updated[2] = angle_to_hue(offset);
                    } else {
                        updated[0] = angle_to_hue(offset);
                    }
                    color.set(model, updated);
                    changed = true;
                }
                WheelDrag::Square => {
                    let x = ((pointer.x - square.left()) / square.width()) as f64;
                    let y = ((pointer.y - square.top()) / square.height()) as f64;
                    let (x, y) = match model {
                        Model::Hls => clamp_to_triangle(x, y),
                        Model::Oklch if limited => {
                            let slice = GamutSlice::new(hue);
                            let (lightness, chroma) = slice.clamp_pointer(
                                1.0 - y,
                                x * OKLCH_MAX_CHROMA,
                                OKLCH_MAX_CHROMA,
                            );
                            (chroma / OKLCH_MAX_CHROMA, 1.0 - lightness)
                        }
                        _ => (x.clamp(0.0, 1.0), y.clamp(0.0, 1.0)),
                    };
                    color.set(model, square_coords(model, hue, x, y));
                    changed = true;
                }
                WheelDrag::Nothing => {}
            }
        }

        let coords = color.coords(model);
        let hue = hue_of(model, coords);
        let ring = self.ring_texture(ui.ctx(), model);
        let painter = ui.painter_at(rect);
        let whole = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
        let ring_rect = Rect::from_center_size(center, Vec2::splat(radius * 2.0));
        painter.image(ring, ring_rect, whole, Color32::WHITE);
        let border = Stroke::new(RING_BORDER_WIDTH, RING_BORDER);
        painter.circle_stroke(center, radius, border);
        painter.circle_stroke(center, radius * RING_INNER, border);

        let square_texture = self.square_texture(ui.ctx(), model, hue, limited);
        painter.image(square_texture, square, whole, Color32::WHITE);
        if model != Model::Hls {
            painter.rect_stroke(
                square,
                0.0,
                ui.visuals().widgets.noninteractive.bg_stroke,
                egui::StrokeKind::Outside,
            );
        }
        if model == Model::Oklch {
            let upper: Vec<Pos2> = self
                .boundary
                .iter()
                .map(|[x, y]| {
                    pos2(square.left() + x * square.width(), square.top() + y * square.height())
                })
                .collect();
            let tip = upper.last().copied().unwrap_or(square.left_bottom());
            let lower = vec![tip, square.left_bottom()];
            let area = painter.with_clip_rect(square);
            // Drawn as two lines meeting at a dot: a single line's join at so sharp
            // a corner comes out notched.
            for (width, color) in [
                (BOUNDARY_HALO_WIDTH, Color32::from_black_alpha(90)),
                (BOUNDARY_WIDTH, Color32::WHITE),
            ] {
                area.add(Shape::line(upper.clone(), Stroke::new(width, color)));
                area.add(Shape::line(lower.clone(), Stroke::new(width, color)));
                area.circle_filled(tip, width / 2.0, color);
            }
        }

        draw_ring_cursor(&painter, center, radius, hue, to_color32(ring_hue_color(model, hue)));

        let (x, y) = square_position(model, coords);
        let point = pos2(
            square.left() + x.clamp(0.0, 1.0) as f32 * square.width(),
            square.top() + y.clamp(0.0, 1.0) as f32 * square.height(),
        );
        draw_cursor(&painter, point);
        changed
    }

    fn ring_texture(&mut self, ctx: &egui::Context, model: Model) -> egui::TextureId {
        let slot = if model == Model::Oklch {
            &mut self.oklch_ring
        } else {
            &mut self.rainbow_ring
        };
        slot.get_or_insert_with(|| {
            let size = [RING_TEXTURE_SIZE; 2];
            let image =
                ColorImage::from_rgba_unmultiplied(size, &ring_pixels(model, RING_TEXTURE_SIZE, RING_INNER));
            ctx.load_texture("hue ring", image, TextureOptions::LINEAR)
        })
        .id()
    }

    fn square_texture(
        &mut self,
        ctx: &egui::Context,
        model: Model,
        hue: f64,
        limited: bool,
    ) -> egui::TextureId {
        let key = (model, (hue * SQUARE_HUE_STEPS).round() as i64, limited);
        if self.square_key != Some(key) || self.square.is_none() {
            self.square_key = Some(key);
            let size = inner_texture_size(model);
            let pixels = if model == Model::Oklch {
                let slice = GamutSlice::new(hue);
                self.boundary = boundary_points(&slice);
                oklch_pixels(hue, limited, &oklch_row_limits(&slice, size[1]), size[1])
            } else {
                hue_area_pixels(model, hue, size)
            };
            let image = ColorImage::from_rgba_unmultiplied(size, &pixels);
            match &mut self.square {
                Some(texture) => texture.set(image, TextureOptions::LINEAR),
                None => {
                    self.square = Some(ctx.load_texture("wheel square", image, TextureOptions::LINEAR));
                }
            }
        }
        self.square.as_ref().map_or(egui::TextureId::default(), TextureHandle::id)
    }
}

fn limit_switch(ui: &mut egui::Ui, color: &mut ColorState, label: &str) -> bool {
    let changed = switch(ui, &mut color.limit_to_srgb, label)
        .on_hover_text("把 OKLCH 限制在 sRGB 色域内")
        .changed();
    if changed {
        color.set(Model::Oklch, color.coords(Model::Oklch));
    }
    changed
}

/// The spacing that spreads a tab's rows over the height of the tallest tab.
fn slider_gap(rows: usize) -> f32 {
    let tallest = MAX_SLIDER_ROWS as f32 * SLIDER_HEIGHT + (MAX_SLIDER_ROWS - 1) as f32 * SLIDER_ROW_GAP;
    (tallest - rows as f32 * SLIDER_HEIGHT) / (rows - 1) as f32
}

/// The current values in a model, as CSP shows them beside its wheel.
fn value_parts(model: Model, [a, b, c]: Channels) -> [(&'static str, String); 3] {
    let whole = |value: f64| format!("{value:.0}");
    match model {
        Model::Hls => [("H", whole(a)), ("L", whole(b * 100.0)), ("S", whole(c * 100.0))],
        Model::Oklch => [("L", format!("{:.1}", a * 100.0)), ("C", format!("{b:.3}")), ("H", whole(c))],
        _ => [("H", whole(a)), ("S", whole(b * 100.0)), ("V", whole(c * 100.0))],
    }
}

/// Draws the values with their letters on small dark badges, which keeps the two apart.
/// For a right-to-left layout, so the last value sits at the edge.
fn value_readout(ui: &mut egui::Ui, parts: &[(&str, String); 3]) {
    ui.spacing_mut().item_spacing.x = BADGE_VALUE_GAP;
    for (index, (letter, value)) in parts.iter().enumerate().rev() {
        ui.label(value);
        let (badge, _) = ui.allocate_exact_size(Vec2::splat(BADGE_SIZE), Sense::hover());
        ui.painter().rect_filled(badge, BADGE_CORNER, BADGE_FILL);
        ui.painter().text(
            badge.center(),
            egui::Align2::CENTER_CENTER,
            *letter,
            egui::FontId::proportional(BADGE_FONT_SIZE),
            Color32::WHITE,
        );
        if index > 0 {
            ui.add_space(BADGE_GROUP_GAP);
        }
    }
}

/// A box across the ring at the current hue, filled with that hue's colour.
fn draw_ring_cursor(painter: &egui::Painter, center: Pos2, radius: f32, hue: f64, fill: Color32) {
    let angle = hue_to_angle(hue);
    let along = vec2(angle.cos(), angle.sin());
    let across = vec2(-along.y, along.x);
    let inner = radius * RING_INNER - RING_CURSOR_OVERHANG;
    let outer = radius + RING_CURSOR_OVERHANG;
    let corners = |inset: f32| {
        let (near, far) = (inner + inset, outer - inset);
        let half = RING_CURSOR_HALF_WIDTH - inset;
        vec![
            center + along * near - across * half,
            center + along * far - across * half,
            center + along * far + across * half,
            center + along * near + across * half,
        ]
    };
    for (inset, color) in [(0.0, Color32::BLACK), (1.0, Color32::WHITE), (2.5, fill)] {
        painter.add(Shape::convex_polygon(corners(inset), color, Stroke::NONE));
    }
}

fn draw_cursor(painter: &egui::Painter, point: Pos2) {
    painter.circle_stroke(point, 6.0, Stroke::new(1.5, Color32::BLACK));
    painter.circle_stroke(point, 4.5, Stroke::new(1.5, Color32::WHITE));
}

/// Draws a slider, and returns its new value if the user moved it. `sample` gives the
/// colour shown at a value along the track, and whether that colour is exact.
fn slider_row(
    ui: &mut egui::Ui,
    channel: &Channel,
    current: f64,
    sample: impl Fn(f64) -> (Channels, bool),
) -> Option<f64> {
    let mut edited = None;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = CONTROL_SPACING;
        ui.add_sized([LABEL_WIDTH, SLIDER_HEIGHT], egui::Label::new(channel.label));

        // Laid out from the right so that the track takes whatever width remains.
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            for (label, sign) in [("+", 1.0), ("−", -1.0)] {
                let button = egui::Button::new(label).small();
                if ui.add_sized([STEP_BUTTON_WIDTH, SLIDER_HEIGHT], button).clicked() {
                    edited = Some(current + sign * channel.step / channel.display);
                }
            }

            let mut displayed = current * channel.display;
            let value = egui::DragValue::new(&mut displayed)
                .range(0.0..=channel.max * channel.display)
                .max_decimals(channel.decimals)
                .speed(channel.step)
                .suffix(channel.suffix);
            if ui.add_sized([VALUE_WIDTH, SLIDER_HEIGHT], value).changed() {
                edited = Some(displayed / channel.display);
            }

            let size = vec2(ui.available_width().max(MIN_TRACK_WIDTH), SLIDER_HEIGHT);
            let (rect, response) = ui.allocate_exact_size(size, Sense::click_and_drag());
            // Inset so that the thumb stays whole at both ends of the track.
            let span = rect.shrink2(vec2(THUMB_HALF_WIDTH, 0.0));
            if response.is_pointer_button_down_on() {
                if let Some(pointer) = response.interact_pointer_pos() {
                    let fraction = ((pointer.x - span.left()) / span.width()).clamp(0.0, 1.0) as f64;
                    edited = Some(fraction * channel.max);
                }
            }
            edited = edited.map(|value: f64| value.clamp(0.0, channel.max));
            draw_track(ui, rect, span, channel, edited.unwrap_or(current), sample);
        });
    });
    edited
}

fn draw_track(
    ui: &egui::Ui,
    rect: Rect,
    span: Rect,
    channel: &Channel,
    value: f64,
    sample: impl Fn(f64) -> (Channels, bool),
) {
    let track = Rect::from_min_size(span.min, vec2(span.width(), TRACK_HEIGHT));
    let painter = ui.painter_at(rect);
    let mut mesh = Mesh::default();
    let mut out_of_gamut = Vec::new();
    for segment in 0..=SLIDER_SEGMENTS {
        let fraction = segment as f32 / SLIDER_SEGMENTS as f32;
        let (rgb, exact) = sample(fraction as f64 * channel.max);
        let x = track.left() + fraction * track.width();
        let fill = to_color32(rgb);
        mesh.colored_vertex(pos2(x, track.top()), fill);
        mesh.colored_vertex(pos2(x, track.bottom()), fill);
        if segment > 0 {
            let base = (segment as u32 - 1) * 2;
            mesh.add_triangle(base, base + 1, base + 2);
            mesh.add_triangle(base + 1, base + 2, base + 3);
        }
        if !exact && segment < SLIDER_SEGMENTS {
            out_of_gamut.push((x, x + track.width() / SLIDER_SEGMENTS as f32));
        }
    }
    painter.add(Shape::mesh(mesh));
    for (left, right) in out_of_gamut {
        let strip = Rect::from_min_max(
            pos2(left, track.bottom() - OUT_OF_GAMUT_STRIP),
            pos2(right, track.bottom()),
        );
        painter.rect_filled(strip, 0.0, Color32::from_black_alpha(160));
    }
    painter.rect_stroke(
        track,
        0.0,
        ui.visuals().widgets.noninteractive.bg_stroke,
        egui::StrokeKind::Outside,
    );

    let x = track.left() + (value / channel.max).clamp(0.0, 1.0) as f32 * track.width();
    let tip = pos2(x, track.bottom() + 1.0);
    let base_y = rect.bottom() - 1.0;
    painter.add(Shape::convex_polygon(
        vec![tip, pos2(x + THUMB_HALF_WIDTH, base_y), pos2(x - THUMB_HALF_WIDTH, base_y)],
        ui.visuals().widgets.inactive.fg_stroke.color,
        Stroke::NONE,
    ));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One image of an .ico: a bottom-up BGRA bitmap, double height for the mask that
    /// follows it. The mask stays empty, as the alpha channel already says it all.
    fn ico_image(size: usize) -> Vec<u8> {
        let header_size = 40u32;
        let mut image = Vec::new();
        image.extend_from_slice(&header_size.to_le_bytes());
        image.extend_from_slice(&(size as i32).to_le_bytes());
        image.extend_from_slice(&(size as i32 * 2).to_le_bytes());
        image.extend_from_slice(&1u16.to_le_bytes());
        image.extend_from_slice(&32u16.to_le_bytes());
        image.resize(header_size as usize, 0);
        for row in app_icon_pixels(size).chunks_exact(size * 4).rev() {
            for pixel in row.chunks_exact(4) {
                image.extend_from_slice(&[pixel[2], pixel[1], pixel[0], pixel[3]]);
            }
        }
        let mask_row = size.div_ceil(32) * 4;
        image.resize(image.len() + mask_row * size, 0);
        image
    }

    /// Run after changing the icon: the exe and the installer embed the file.
    #[test]
    #[ignore = "writes assets/percepta.ico"]
    fn write_exe_icon() {
        const SIZES: [usize; 7] = [16, 24, 32, 48, 64, 128, 256];
        let images = SIZES.map(ico_image);
        let mut file = vec![0, 0, 1, 0];
        file.extend_from_slice(&(SIZES.len() as u16).to_le_bytes());
        let mut offset = 6 + 16 * SIZES.len();
        for (size, image) in SIZES.iter().zip(&images) {
            // A stored size of 0 means 256.
            file.extend_from_slice(&[*size as u8, *size as u8, 0, 0, 1, 0, 32, 0]);
            file.extend_from_slice(&(image.len() as u32).to_le_bytes());
            file.extend_from_slice(&(offset as u32).to_le_bytes());
            offset += image.len();
        }
        file.extend(images.concat());
        std::fs::write(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/percepta.ico"), file).unwrap();
    }

    #[test]
    fn inner_axes_match_each_model() {
        assert_eq!(square_coords(Model::Hsv, 90.0, 0.25, 0.25), [90.0, 0.25, 0.75]);
        // At lightness 0.75 the triangle is half as wide as at its apex.
        assert_eq!(square_coords(Model::Hls, 90.0, 0.25, 0.25), [90.0, 0.75, 0.5]);
        assert_eq!(square_coords(Model::Oklch, 90.0, 1.0, 0.25), [0.75, OKLCH_MAX_CHROMA, 90.0]);
    }

    #[test]
    fn hls_triangle_keeps_its_corners_and_clamps_outside_points() {
        assert_eq!(square_coords(Model::Hls, 90.0, 1.0, 0.5), [90.0, 0.5, 1.0]);
        assert_eq!(square_coords(Model::Hls, 90.0, 0.9, 0.1)[2], 1.0);
        assert_eq!(square_coords(Model::Hls, 90.0, 0.0, 0.0)[1], 1.0);
        assert_eq!(square_coords(Model::Hls, 90.0, 0.0, 1.0)[1], 0.0);
    }

    #[test]
    fn inner_position_inverts_inner_coords() {
        for model in [Model::Hsv, Model::Hls, Model::Oklch] {
            let coords = square_coords(model, 90.0, 0.25, 0.75);
            let (x, y) = square_position(model, coords);
            assert!((x - 0.25).abs() < 1e-9 && (y - 0.75).abs() < 1e-9);
        }
    }

    #[test]
    fn hls_triangle_is_equilateral() {
        let rect = inner_rect(Model::Hls, pos2(0.0, 0.0), 100.0);
        let apex = pos2(rect.right(), rect.center().y);
        let slanted = apex.distance(rect.left_top());
        assert!((slanted - rect.height()).abs() < 0.01);
        assert!((apex.x - 79.0).abs() < 0.01);
    }

    #[test]
    fn hls_pixels_are_transparent_outside_the_triangle() {
        let size = [16, 16];
        let pixels = hue_area_pixels(Model::Hls, 150.0, size);
        let alpha = |column: usize, row: usize| pixels[(row * size[0] + column) * 4 + 3];
        assert_eq!(alpha(15, 0), 0);
        assert_eq!(alpha(15, 15), 0);
        assert_eq!(alpha(0, 8), 255);
    }

    #[test]
    fn limiting_only_changes_the_invalid_part_of_the_oklch_square() {
        let size = 32;
        let limits = oklch_row_limits(&GamutSlice::new(150.0), size);
        let free = oklch_pixels(150.0, false, &limits, size);
        let limited = oklch_pixels(150.0, true, &limits, size);
        let pixel = |pixels: &[u8], column: usize, row: usize| pixels[(row * size + column) * 4];
        // Column 0 is grey and always valid; the top-right corner never is.
        let row = size / 2;
        assert_eq!(pixel(&free, 0, row), pixel(&limited, 0, row));
        assert_ne!(pixel(&free, size - 1, 0), pixel(&limited, size - 1, 0));
    }

    #[test]
    fn oklch_boundary_ends_at_the_exact_tip() {
        let slice = GamutSlice::new(30.0);
        let points = boundary_points(&slice);
        let tip = points[points.len() - 1];
        assert!((tip[0] as f64 - slice.peak_chroma / OKLCH_MAX_CHROMA).abs() < 1e-6);
        assert!((tip[1] as f64 - (1.0 - slice.peak_lightness)).abs() < 1e-6);
        assert!(points[0][1] == 0.0);
    }

    #[test]
    fn oklch_row_limits_reach_zero_at_black() {
        let limits = oklch_row_limits(&GamutSlice::new(150.0), 64);
        assert_eq!(limits[63], 0.0);
    }

    #[test]
    fn triangle_leaves_inside_points_alone() {
        assert_eq!(clamp_to_triangle(0.25, 0.5), (0.25, 0.5));
        assert_eq!(clamp_to_triangle(0.0, 0.5), (0.0, 0.5));
    }

    #[test]
    fn pointer_past_the_tip_holds_it_within_its_wedge() {
        for y in [0.3, 0.4, 0.5, 0.6, 0.7] {
            let (x, nearest_y) = clamp_to_triangle(1.3, y);
            assert!((x - 1.0).abs() < 1e-9 && (nearest_y - 0.5).abs() < 1e-9, "at y {y}");
        }
    }

    #[test]
    fn pointer_past_the_white_and_black_corners_holds_them() {
        // Above white and left of black, within the lines perpendicular to their edges.
        assert_eq!(clamp_to_triangle(0.0, -0.5), (0.0, 0.0));
        assert_eq!(clamp_to_triangle(-0.5, -0.1), (0.0, 0.0));
        assert_eq!(clamp_to_triangle(0.0, 1.5), (0.0, 1.0));
        assert_eq!(clamp_to_triangle(-0.5, 1.1), (0.0, 1.0));
    }

    #[test]
    fn pointer_beyond_an_edge_lands_on_that_edge() {
        let on_upper = clamp_to_triangle(0.6, 0.0);
        assert!((on_upper.1 - on_upper.0 / 2.0).abs() < 1e-9, "{on_upper:?}");
        let on_lower = clamp_to_triangle(0.6, 1.0);
        assert!((on_lower.1 - (1.0 - on_lower.0 / 2.0)).abs() < 1e-9, "{on_lower:?}");
        // Left of the grey edge, the height stays the same.
        assert_eq!(clamp_to_triangle(-0.2, 0.4), (0.0, 0.4));
    }

    #[test]
    fn moving_parallel_to_an_edge_slides_the_result_equally() {
        // Two pointers offset along the upper edge, whose direction is (1, 0.5) in this
        // box, land the same offset apart along it.
        let first = clamp_to_triangle(0.4, 0.05);
        let second = clamp_to_triangle(0.6, 0.15);
        assert!((second.0 - first.0 - 0.2).abs() < 1e-9, "{first:?} then {second:?}");
        assert!((second.1 - first.1 - 0.1).abs() < 1e-9, "{first:?} then {second:?}");
    }

    #[test]
    fn every_hue_tip_fits_in_the_oklch_square() {
        for hue in (0..360).step_by(10) {
            assert!(GamutSlice::new(hue as f64).peak_chroma <= OKLCH_MAX_CHROMA, "hue {hue}");
        }
    }

    #[test]
    fn hue_angle_round_trips() {
        for hue in [0.0, 60.0, 138.0, 359.0] {
            let angle = hue_to_angle(hue);
            let back = angle_to_hue(vec2(angle.cos(), angle.sin()));
            assert!((back - hue).abs() < 0.01, "{hue} came back as {back}");
        }
    }

    #[test]
    fn app_icon_is_a_ring_with_clear_corners_and_centre() {
        let size = 32;
        let pixels = app_icon_pixels(size);
        assert_eq!(pixels.len(), size * size * 4);
        let alpha = |column: usize, row: usize| pixels[(row * size + column) * 4 + 3];
        assert_eq!(alpha(0, 0), 0);
        assert_eq!(alpha(size / 2, size / 2), 0);
        assert_eq!(alpha(size / 2, 3), 255);
    }

    #[test]
    fn the_settings_icon_renders_as_a_filled_shape_with_a_hole() {
        let size = 48;
        let image = render_svg(include_str!("../assets/settings.svg"), size as u32).unwrap();
        let alpha = |column: usize, row: usize| image.pixels[row * size + column].a();
        assert_eq!(alpha(0, 0), 0, "corner");
        assert_eq!(alpha(size / 2, size / 2), 0, "centre hole");
        assert_eq!(alpha(size / 2, size / 2 - 10), 255, "gear body");
        assert!(image.pixels.iter().all(|p| p.a() == 0 || p.r() == p.a()), "must be white to tint");
    }

    #[test]
    fn saved_modes_cannot_put_the_wheel_in_rgb() {
        let mut picker = Picker::default();
        picker.set_modes(Model::Rgb, SliderTab::Cmyk);
        assert_eq!(picker.modes(), (Model::Hsv, SliderTab::Cmyk));
        picker.set_modes(Model::Oklch, SliderTab::Color(Model::Rgb));
        assert_eq!(picker.modes(), (Model::Oklch, SliderTab::Color(Model::Rgb)));
    }

    #[test]
    fn every_slider_tab_fills_the_same_height() {
        let height = |rows: usize| rows as f32 * SLIDER_HEIGHT + (rows - 1) as f32 * slider_gap(rows);
        assert_eq!(height(3), height(MAX_SLIDER_ROWS));
        assert_eq!(slider_gap(MAX_SLIDER_ROWS), SLIDER_ROW_GAP);
    }

    #[test]
    fn value_parts_match_each_model() {
        let text = |model, coords| {
            value_parts(model, coords).map(|(letter, value)| format!("{letter} {value}")).join("  ")
        };
        assert_eq!(text(Model::Hls, [24.2, 0.617, 0.664]), "H 24  L 62  S 66");
        assert_eq!(text(Model::Hsv, [210.0, 0.79, 0.34]), "H 210  S 79  V 34");
        assert_eq!(text(Model::Oklch, [0.7, 0.2, 150.0]), "L 70.0  C 0.200  H 150");
    }

    #[test]
    fn hsv_square_covers_every_pixel() {
        let pixels = hue_area_pixels(Model::Hsv, 150.0, [8, 8]);
        assert!(pixels.chunks(4).all(|pixel| pixel[3] == 255));
    }

    #[test]
    fn hue_tracks_ignore_the_current_saturation_and_value() {
        let dark = [138.0, 0.0, 0.1];
        assert_eq!(track_coords(Model::Hsv, dark, 0, 60.0), [60.0, 1.0, 1.0]);
        assert_eq!(track_coords(Model::Hsv, dark, 2, 0.5), [138.0, 0.0, 0.5]);
    }
}
