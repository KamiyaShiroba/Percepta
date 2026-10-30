//! Colour models and sRGB gamut mapping. Pure maths, no UI.

use serde::{Deserialize, Serialize};

use crate::csp::Rgb;

pub type Channels = [f64; 3];

/// Tolerance for treating a channel as inside 0..=1 or a saturation as zero.
const EPSILON: f64 = 1e-4;
/// Below this an sRGB colour is grey to the eye, and its OKLCH hue is just noise.
const GREY_CHROMA: f64 = 2e-3;
/// Largest colour difference the eye cannot see, from CSS Color 4.
const JUST_NOTICEABLE: f64 = 0.02;
const PEAK_SEARCH_STEPS: u32 = 32;
const PEAK_REFINE_STEPS: u32 = 12;

/// Coordinates per model: RGB and the HSV/HLS S, V, L are 0..=1, hues are degrees
/// and OKLCH is `[L 0..=1, C, H]`. HLS keeps CSP's H, L, S order.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Model {
    Rgb,
    Hsv,
    Hls,
    Oklch,
}

pub fn hsv_to_rgb([h, s, v]: Channels) -> Channels {
    let sector = h.rem_euclid(360.0) / 60.0;
    let chroma = v * s;
    let x = chroma * (1.0 - (sector % 2.0 - 1.0).abs());
    let m = v - chroma;
    let [r, g, b] = match sector as u32 {
        0 => [chroma, x, 0.0],
        1 => [x, chroma, 0.0],
        2 => [0.0, chroma, x],
        3 => [0.0, x, chroma],
        4 => [x, 0.0, chroma],
        _ => [chroma, 0.0, x],
    };
    [r + m, g + m, b + m]
}

pub fn hls_to_rgb([h, l, s]: Channels) -> Channels {
    let chroma = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let top = l + chroma / 2.0;
    let saturation = if top > 0.0 { chroma / top } else { 0.0 };
    hsv_to_rgb([h, saturation, top])
}

fn hue_of([r, g, b]: Channels) -> f64 {
    let max = r.max(g).max(b);
    let delta = max - r.min(g).min(b);
    if delta <= 0.0 {
        0.0
    } else if max == r {
        (60.0 * ((g - b) / delta)).rem_euclid(360.0)
    } else if max == g {
        60.0 * ((b - r) / delta) + 120.0
    } else {
        60.0 * ((r - g) / delta) + 240.0
    }
}

pub fn rgb_to_hsv(rgb: Channels) -> Channels {
    let max = rgb[0].max(rgb[1]).max(rgb[2]);
    let min = rgb[0].min(rgb[1]).min(rgb[2]);
    let saturation = if max > 0.0 { (max - min) / max } else { 0.0 };
    [hue_of(rgb), saturation, max]
}

pub fn rgb_to_hls(rgb: Channels) -> Channels {
    let max = rgb[0].max(rgb[1]).max(rgb[2]);
    let min = rgb[0].min(rgb[1]).min(rgb[2]);
    let lightness = (max + min) / 2.0;
    let divisor = 1.0 - (2.0 * lightness - 1.0).abs();
    let saturation = if divisor > 0.0 { (max - min) / divisor } else { 0.0 };
    [hue_of(rgb), lightness, saturation]
}

fn to_linear(c: f64) -> f64 {
    if c.abs() <= 0.04045 {
        c / 12.92
    } else {
        c.signum() * ((c.abs() + 0.055) / 1.055).powf(2.4)
    }
}

fn to_gamma(c: f64) -> f64 {
    if c.abs() <= 0.0031308 {
        c * 12.92
    } else {
        c.signum() * (1.055 * c.abs().powf(1.0 / 2.4) - 0.055)
    }
}

pub fn rgb_to_oklch(rgb: Channels) -> Channels {
    let [r, g, b] = rgb.map(to_linear);
    let l = (0.4122214708 * r + 0.5363325363 * g + 0.0514459929 * b).cbrt();
    let m = (0.2119034982 * r + 0.6806995451 * g + 0.1073969566 * b).cbrt();
    let s = (0.0883024619 * r + 0.2817188376 * g + 0.6299787005 * b).cbrt();
    let lightness = 0.2104542553 * l + 0.7936177850 * m - 0.0040720468 * s;
    let a = 1.9779984951 * l - 2.4285922050 * m + 0.4505937099 * s;
    let b = 0.0259040371 * l + 0.7827717662 * m - 0.8086757660 * s;
    [lightness, a.hypot(b), b.atan2(a).to_degrees().rem_euclid(360.0)]
}

/// May return channels outside 0..=1 for colours beyond the sRGB gamut.
pub fn oklch_to_rgb_unclipped([lightness, chroma, hue]: Channels) -> Channels {
    let (sin, cos) = hue.to_radians().sin_cos();
    let (a, b) = (chroma * cos, chroma * sin);
    let l = (lightness + 0.3963377774 * a + 0.2158037573 * b).powi(3);
    let m = (lightness - 0.1055613458 * a - 0.0638541728 * b).powi(3);
    let s = (lightness - 0.0894841775 * a - 1.2914855480 * b).powi(3);
    [
        4.0767416621 * l - 3.3077115913 * m + 0.2309699292 * s,
        -1.2684380046 * l + 2.6097574011 * m - 0.3413193965 * s,
        -0.0041960863 * l - 0.7034186147 * m + 1.7076147010 * s,
    ]
    .map(to_gamma)
}

pub fn in_srgb(rgb: Channels) -> bool {
    rgb.iter().all(|c| (-EPSILON..=1.0 + EPSILON).contains(c))
}

fn clip(rgb: Channels) -> Channels {
    rgb.map(|c| c.clamp(0.0, 1.0))
}

/// Distance in OKLab, where 1.0 spans black to white.
fn delta_e_ok(from: Channels, to: Channels) -> f64 {
    let to_lab = |[l, c, h]: Channels| {
        let (sin, cos) = h.to_radians().sin_cos();
        [l, c * cos, c * sin]
    };
    let (from, to) = (to_lab(from), to_lab(to));
    ((from[0] - to[0]).powi(2) + (from[1] - to[1]).powi(2) + (from[2] - to[2]).powi(2)).sqrt()
}

/// The CSS Color 4 gamut mapping: keeps lightness and hue and lowers chroma until
/// clipping the rest away stays invisible, so the result is not needlessly grey.
pub fn gamut_map(oklch: Channels) -> Channels {
    let [lightness, chroma, hue] = oklch;
    if lightness >= 1.0 {
        return [1.0; 3];
    }
    if lightness <= 0.0 {
        return [0.0; 3];
    }
    let exact = oklch_to_rgb_unclipped(oklch);
    if in_srgb(exact) {
        return clip(exact);
    }
    let visible_clip = |candidate: Channels| {
        let clipped = clip(oklch_to_rgb_unclipped(candidate));
        (clipped, delta_e_ok(rgb_to_oklch(clipped), candidate))
    };
    let (mut clipped, difference) = visible_clip(oklch);
    if difference < JUST_NOTICEABLE {
        return clipped;
    }
    let (mut low, mut high) = (0.0, chroma);
    let mut low_in_gamut = true;
    while high - low > EPSILON {
        let middle = (low + high) / 2.0;
        let candidate = [lightness, middle, hue];
        if low_in_gamut && in_srgb(oklch_to_rgb_unclipped(candidate)) {
            low = middle;
            continue;
        }
        let (candidate_clipped, difference) = visible_clip(candidate);
        clipped = candidate_clipped;
        if difference < JUST_NOTICEABLE {
            if JUST_NOTICEABLE - difference < EPSILON {
                return clipped;
            }
            low_in_gamut = false;
            low = middle;
        } else {
            high = middle;
        }
    }
    clipped
}

/// Bisects for the sRGB boundary. Unreliable very near black, where every small
/// chroma rounds to "in gamut"; `GamutSlice` avoids asking it there.
fn search_srgb_chroma(lightness: f64, hue: f64) -> f64 {
    let (mut low, mut high) = (0.0, 0.5);
    while high - low > EPSILON {
        let middle = (low + high) / 2.0;
        if in_srgb(oklch_to_rgb_unclipped([lightness, middle, hue])) {
            low = middle;
        } else {
            high = middle;
        }
    }
    low
}

/// The sRGB gamut at one hue, as seen in the lightness-chroma plane: roughly a
/// triangle with its tip at the highest chroma this hue reaches.
#[derive(Clone, Copy, Debug)]
pub struct GamutSlice {
    hue: f64,
    pub peak_lightness: f64,
    pub peak_chroma: f64,
}

impl GamutSlice {
    pub fn new(hue: f64) -> Self {
        let step_size = 1.0 / PEAK_SEARCH_STEPS as f64;
        let chroma_at = |lightness: f64| search_srgb_chroma(lightness, hue);
        let (_, best) = (1..PEAK_SEARCH_STEPS)
            .map(|step| step as f64 * step_size)
            .map(|lightness| (chroma_at(lightness), lightness))
            .fold((0.0, 0.5), |best, next| if next.0 > best.0 { next } else { best });
        // The tip is a sharp corner, so narrow in on it with a ternary search.
        let (mut low, mut high) = (best - step_size, best + step_size);
        for _ in 0..PEAK_REFINE_STEPS {
            let third = (high - low) / 3.0;
            if chroma_at(low + third) < chroma_at(high - third) {
                low += third;
            } else {
                high -= third;
            }
        }
        let peak_lightness = (low + high) / 2.0;
        Self { hue, peak_lightness, peak_chroma: chroma_at(peak_lightness) }
    }

    /// The highest chroma inside sRGB at this lightness. Below the tip the boundary
    /// is where one RGB channel reaches zero, and Oklab scales evenly towards black,
    /// so that edge is exactly a straight line from black to the tip.
    pub fn max_chroma(&self, lightness: f64) -> f64 {
        if lightness <= self.peak_lightness {
            self.peak_chroma * lightness.max(0.0) / self.peak_lightness
        } else {
            search_srgb_chroma(lightness, self.hue)
        }
    }

    /// The highest lightness inside sRGB at this chroma, on the edge above the tip.
    fn upper_lightness(&self, chroma: f64) -> f64 {
        let (mut low, mut high) = (self.peak_lightness, 1.0);
        while high - low > EPSILON {
            let middle = (low + high) / 2.0;
            if self.max_chroma(middle) >= chroma {
                low = middle;
            } else {
                high = middle;
            }
        }
        low
    }

    /// Pulls a pointer position onto the gamut, returning `(lightness, chroma)`. Valid
    /// points stay put. Others first go to the nearest point of the straight triangle
    /// of white, black and the tip, as the HLS triangle does, measured the way the
    /// square shows it: `chroma_span` is the chroma across the square's full width.
    /// The real upper edge curves inwards of that triangle, so lightness then stops
    /// at it. Going straight to the nearest point of the curve would jump around,
    /// because a concave edge can be equally near at two places.
    pub fn clamp_pointer(&self, lightness: f64, chroma: f64, chroma_span: f64) -> (f64, f64) {
        let lower = chroma * self.peak_lightness / self.peak_chroma;
        if (0.0..=self.peak_chroma).contains(&chroma)
            && (lower..=self.upper_lightness(chroma)).contains(&lightness)
        {
            return (lightness, chroma);
        }
        let [x, lightness] = nearest_on_triangle(
            [chroma / chroma_span, lightness],
            [[0.0, 1.0], [0.0, 0.0], [self.peak_chroma / chroma_span, self.peak_lightness]],
        );
        let chroma = x * chroma_span;
        (lightness.min(self.upper_lightness(chroma)), chroma)
    }
}

/// The nearest point on a triangle's outline. This divides the outside into a strip
/// extending from each edge, bounded by the lines perpendicular to it at its ends,
/// where the result slides along that edge in step with the pointer; and a wedge
/// beyond each corner, where the result holds at that corner.
pub fn nearest_on_triangle(point: [f64; 2], corners: [[f64; 2]; 3]) -> [f64; 2] {
    let (nearest, _) = (0..3)
        .map(|edge| {
            let (start, end) = (corners[edge], corners[(edge + 1) % 3]);
            let direction = [end[0] - start[0], end[1] - start[1]];
            let along = (point[0] - start[0]) * direction[0] + (point[1] - start[1]) * direction[1];
            let length_squared = direction[0] * direction[0] + direction[1] * direction[1];
            let t = (along / length_squared).clamp(0.0, 1.0);
            let on_edge = [start[0] + direction[0] * t, start[1] + direction[1] * t];
            (on_edge, (on_edge[0] - point[0]).powi(2) + (on_edge[1] - point[1]).powi(2))
        })
        .fold(([0.0, 0.0], f64::MAX), |best, next| if next.1 < best.1 { next } else { best });
    nearest
}

/// The colour in every model at once. Each model keeps its own coordinates, because
/// converting through RGB would lose the hue at grey and an out-of-sRGB OKLCH value.
#[derive(Clone, Copy, Debug)]
pub struct ColorState {
    rgb: Channels,
    hsv: Channels,
    hls: Channels,
    oklch: Channels,
    /// Whether `oklch` lies outside sRGB, so `rgb` only approximates it.
    pub gamut_mapped: bool,
    /// Stops OKLCH chroma at the sRGB boundary instead of mapping it.
    pub limit_to_srgb: bool,
}

impl ColorState {
    pub fn from_rgb8(rgb8: Rgb) -> Self {
        let rgb = bytes_to_channels(rgb8);
        Self {
            rgb,
            hsv: rgb_to_hsv(rgb),
            hls: rgb_to_hls(rgb),
            oklch: rgb_to_oklch(rgb),
            gamut_mapped: false,
            limit_to_srgb: false,
        }
    }

    pub fn rgb8(&self) -> Rgb {
        self.rgb.map(channel_to_byte)
    }

    pub fn coords(&self, model: Model) -> Channels {
        match model {
            Model::Rgb => self.rgb,
            Model::Hsv => self.hsv,
            Model::Hls => self.hls,
            Model::Oklch => self.oklch,
        }
    }

    pub fn set_rgb8(&mut self, rgb8: Rgb) {
        self.set(Model::Rgb, bytes_to_channels(rgb8));
    }

    /// Takes new coordinates in one model and updates the others to match.
    pub fn set(&mut self, model: Model, coords: Channels) {
        let previous = *self;
        match model {
            Model::Rgb => self.rgb = coords,
            Model::Hsv => {
                self.hsv = coords;
                self.rgb = hsv_to_rgb(coords);
            }
            Model::Hls => {
                self.hls = coords;
                self.rgb = hls_to_rgb(coords);
            }
            Model::Oklch => {
                let mut coords = coords;
                if self.limit_to_srgb {
                    coords[1] = coords[1].min(GamutSlice::new(coords[2]).max_chroma(coords[0]));
                }
                self.oklch = coords;
                self.rgb = gamut_map(coords);
            }
        }
        if model != Model::Hsv {
            self.hsv = sticky_hsv(rgb_to_hsv(self.rgb), previous.hsv);
        }
        if model != Model::Hls {
            self.hls = sticky_hls(rgb_to_hls(self.rgb), previous.hls);
        }
        if model != Model::Oklch {
            self.oklch = sticky_oklch(rgb_to_oklch(self.rgb), previous.oklch);
        }
        self.gamut_mapped = !in_srgb(oklch_to_rgb_unclipped(self.oklch));
    }
}

/// Keeps channels that a colour no longer defines, so dragging through black or
/// grey does not reset the hue.
fn sticky_hsv(mut new: Channels, old: Channels) -> Channels {
    if new[2] <= EPSILON {
        (new[0], new[1]) = (old[0], old[1]);
    } else if new[1] <= EPSILON {
        new[0] = old[0];
    }
    new
}

fn sticky_hls(mut new: Channels, old: Channels) -> Channels {
    if new[1] <= EPSILON || new[1] >= 1.0 - EPSILON {
        (new[0], new[2]) = (old[0], old[2]);
    } else if new[2] <= EPSILON {
        new[0] = old[0];
    }
    new
}

fn sticky_oklch(mut new: Channels, old: Channels) -> Channels {
    if new[1] < GREY_CHROMA {
        new[1] = 0.0;
        new[2] = old[2];
    }
    new
}

fn bytes_to_channels(rgb8: Rgb) -> Channels {
    rgb8.map(|byte| byte as f64 / 255.0)
}

fn channel_to_byte(channel: f64) -> u8 {
    (channel * 255.0).round().clamp(0.0, 255.0) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_SPAN: f64 = 0.33;

    fn assert_close(actual: Channels, expected: Channels, tolerance: f64) {
        for (a, e) in actual.iter().zip(expected) {
            assert!((a - e).abs() <= tolerance, "{actual:?} != {expected:?}");
        }
    }

    #[test]
    fn hsv_and_hls_match_known_colours() {
        assert_close(hsv_to_rgb([0.0, 1.0, 1.0]), [1.0, 0.0, 0.0], 1e-9);
        assert_close(hsv_to_rgb([120.0, 1.0, 1.0]), [0.0, 1.0, 0.0], 1e-9);
        assert_close(hsv_to_rgb([360.0, 1.0, 1.0]), [1.0, 0.0, 0.0], 1e-9);
        assert_close(hls_to_rgb([240.0, 0.5, 1.0]), [0.0, 0.0, 1.0], 1e-9);
        assert_close(hls_to_rgb([0.0, 1.0, 1.0]), [1.0, 1.0, 1.0], 1e-9);
        assert_close(hls_to_rgb([0.0, 0.25, 0.0]), [0.25, 0.25, 0.25], 1e-9);
        assert_close(rgb_to_hsv([0.0, 0.33, 0.1]), [138.181818, 1.0, 0.33], 1e-5);
        assert_close(rgb_to_hls([1.0, 1.0, 0.0]), [60.0, 0.5, 1.0], 1e-9);
    }

    #[test]
    fn every_model_round_trips_through_rgb() {
        for r in 0..=5 {
            for g in 0..=5 {
                for b in 0..=5 {
                    let rgb = [r as f64 / 5.0, g as f64 / 5.0, b as f64 / 5.0];
                    assert_close(hsv_to_rgb(rgb_to_hsv(rgb)), rgb, 1e-9);
                    assert_close(hls_to_rgb(rgb_to_hls(rgb)), rgb, 1e-9);
                    assert_close(oklch_to_rgb_unclipped(rgb_to_oklch(rgb)), rgb, 1e-5);
                }
            }
        }
    }

    #[test]
    fn oklch_matches_published_values() {
        // Reference values from the CSS Color 4 examples for pure sRGB primaries.
        assert_close(rgb_to_oklch([1.0, 0.0, 0.0]), [0.62796, 0.25768, 29.234], 1e-3);
        assert_close(rgb_to_oklch([0.0, 1.0, 0.0]), [0.86644, 0.29483, 142.495], 1e-3);
        assert_close(rgb_to_oklch([0.0, 0.0, 1.0]), [0.45201, 0.31321, 264.052], 1e-3);
    }

    #[test]
    fn gamut_map_leaves_srgb_colours_alone() {
        let lch = rgb_to_oklch([0.2, 0.5, 0.7]);
        assert_close(gamut_map(lch), [0.2, 0.5, 0.7], 1e-4);
    }

    #[test]
    fn gamut_map_handles_the_ends_of_lightness() {
        assert_eq!(gamut_map([1.2, 0.2, 30.0]), [1.0; 3]);
        assert_eq!(gamut_map([0.0, 0.2, 30.0]), [0.0; 3]);
    }

    #[test]
    fn gamut_map_stays_in_srgb_near_the_requested_hue_and_lightness() {
        let wanted = [0.7, 0.35, 150.0];
        assert!(!in_srgb(oklch_to_rgb_unclipped(wanted)));
        let rgb = gamut_map(wanted);
        assert!(rgb.iter().all(|c| (0.0..=1.0).contains(c)));
        let [lightness, chroma, hue] = rgb_to_oklch(rgb);
        assert!((lightness - wanted[0]).abs() < JUST_NOTICEABLE);
        assert!((hue - wanted[2]).abs() < 5.0, "hue drifted to {hue}");
        assert!(chroma < wanted[1]);
    }

    #[test]
    fn max_chroma_sits_on_the_gamut_boundary() {
        for hue in [30.0, 110.0, 150.0, 200.0, 300.0, 350.0] {
            let slice = GamutSlice::new(hue);
            for lightness in [0.1, 0.3, 0.5, 0.7, 0.9] {
                let chroma = slice.max_chroma(lightness);
                assert!(
                    in_srgb(oklch_to_rgb_unclipped([lightness, chroma - 0.002, hue])),
                    "hue {hue} lightness {lightness} chroma {chroma} is outside"
                );
                assert!(
                    !in_srgb(oklch_to_rgb_unclipped([lightness, chroma + 0.002, hue])),
                    "hue {hue} lightness {lightness} chroma {chroma} is inside"
                );
            }
        }
    }

    #[test]
    fn clamp_pointer_holds_the_tip_past_its_chroma() {
        let slice = GamutSlice::new(30.0);
        for lightness in [0.0, 0.3, 0.6, 1.0] {
            let (l, c) = slice.clamp_pointer(lightness, 0.5, TEST_SPAN);
            assert!((c - slice.peak_chroma).abs() < 1e-9);
            assert!((l - slice.peak_lightness).abs() < 0.001, "{l} from {lightness}");
        }
    }

    #[test]
    fn clamp_pointer_leaves_inside_points_alone_and_lands_on_the_real_edges() {
        let slice = GamutSlice::new(30.0);
        assert_eq!(slice.clamp_pointer(0.5, 0.05, TEST_SPAN), (0.5, 0.05));

        // Past the curved upper edge, the result sits on that curve.
        let (above, chroma) = slice.clamp_pointer(1.0, slice.peak_chroma / 2.0, TEST_SPAN);
        assert!(chroma > 0.0 && chroma < slice.peak_chroma);
        assert!(in_srgb(oklch_to_rgb_unclipped([above, chroma, 30.0])));
        assert!(!in_srgb(oklch_to_rgb_unclipped([above + 0.01, chroma, 30.0])));

        // Past the straight lower edge, it sits on that line.
        let (below, chroma) = slice.clamp_pointer(0.0, slice.peak_chroma / 2.0, TEST_SPAN);
        let on_line = chroma * slice.peak_lightness / slice.peak_chroma;
        assert!(chroma > 0.0 && (below - on_line).abs() < 1e-9);
    }

    #[test]
    fn clamp_pointer_holds_each_corner_within_its_wedge() {
        let slice = GamutSlice::new(30.0);
        // Far above white and far below black, a little chroma changes nothing.
        let (white, chroma) = slice.clamp_pointer(1.6, 0.02, TEST_SPAN);
        assert!((white - 1.0).abs() < 0.001 && chroma == 0.0, "{white} {chroma}");
        let (black, chroma) = slice.clamp_pointer(-0.6, 0.02, TEST_SPAN);
        assert!(black.abs() < 1e-9 && chroma == 0.0, "{black} {chroma}");
    }

    #[test]
    fn nearest_on_triangle_slides_along_an_edge_within_its_strip() {
        let corners = [[0.0, 0.0], [0.0, 1.0], [1.0, 0.0]];
        // Moving along the edge moves the result equally, however far out the pointer is.
        assert_eq!(nearest_on_triangle([-0.5, 0.25], corners), [0.0, 0.25]);
        assert_eq!(nearest_on_triangle([-0.5, 0.75], corners), [0.0, 0.75]);
        assert_eq!(nearest_on_triangle([-3.0, 0.75], corners), [0.0, 0.75]);
    }

    #[test]
    fn nearest_on_triangle_holds_a_corner_within_its_wedge() {
        let corners = [[0.0, 0.0], [0.0, 1.0], [1.0, 0.0]];
        assert_eq!(nearest_on_triangle([-0.5, -0.5], corners), [0.0, 0.0]);
        assert_eq!(nearest_on_triangle([-0.1, -3.0], corners), [0.0, 0.0]);
    }

    #[test]
    fn the_edge_below_the_tip_ends_exactly_at_black() {
        let slice = GamutSlice::new(30.0);
        assert_eq!(slice.max_chroma(0.0), 0.0);
        assert!(slice.max_chroma(0.01) < 0.01);
    }

    #[test]
    fn peak_chroma_matches_the_primaries() {
        let [red_lightness, red_chroma, red_hue] = rgb_to_oklch([1.0, 0.0, 0.0]);
        let red = GamutSlice::new(red_hue);
        assert!((red.peak_chroma - red_chroma).abs() < 0.005);
        assert!((red.peak_lightness - red_lightness).abs() < 0.005);
        // Pure blue sits on a sliver of the gamut that bends away from its hue line,
        // cut off from the main area, so the usable peak stops a little short of it.
        let [_, blue_chroma, blue_hue] = rgb_to_oklch([0.0, 0.0, 1.0]);
        let blue_peak = GamutSlice::new(blue_hue).peak_chroma;
        assert!(blue_peak > 0.27 && blue_peak < blue_chroma, "{blue_peak}");
    }

    #[test]
    fn state_keeps_hue_when_passing_through_grey_and_black() {
        let mut state = ColorState::from_rgb8([255, 0, 0]);
        state.set(Model::Hsv, [200.0, 1.0, 1.0]);
        let oklch_hue = state.coords(Model::Oklch)[2];

        state.set(Model::Hsv, [200.0, 0.0, 0.5]);
        assert_eq!(state.coords(Model::Hsv)[0], 200.0);
        assert_eq!(state.coords(Model::Hls)[0], 200.0);
        assert_eq!(state.coords(Model::Oklch)[2], oklch_hue);

        state.set(Model::Hls, [200.0, 0.0, 0.7]);
        assert_eq!(state.coords(Model::Hsv)[0], 200.0);
        assert_eq!(state.coords(Model::Hls)[2], 0.7);
    }

    #[test]
    fn state_remembers_the_requested_oklch_when_out_of_gamut() {
        let mut state = ColorState::from_rgb8([0, 0, 0]);
        let wanted = [0.7, 0.35, 150.0];
        state.set(Model::Oklch, wanted);
        assert_eq!(state.coords(Model::Oklch), wanted);
        assert!(state.gamut_mapped);

        state.set(Model::Oklch, [0.7, 0.05, 150.0]);
        assert!(!state.gamut_mapped);
    }

    #[test]
    fn limiting_stops_chroma_at_the_boundary() {
        let mut state = ColorState::from_rgb8([0, 0, 0]);
        state.limit_to_srgb = true;
        state.set(Model::Oklch, [0.7, 0.35, 150.0]);
        assert!(state.coords(Model::Oklch)[1] < 0.35);
        assert!(!state.gamut_mapped);
    }

    #[test]
    fn limiting_restores_chroma_when_the_caller_keeps_aiming_for_it() {
        // What dragging the hue ring does: blue allows more chroma than yellow.
        let target = 0.2;
        let mut state = ColorState::from_rgb8([0, 0, 0]);
        state.limit_to_srgb = true;
        state.set(Model::Oklch, [0.5, target, 264.0]);
        assert_eq!(state.coords(Model::Oklch)[1], target);

        state.set(Model::Oklch, [0.5, target, 110.0]);
        assert!(state.coords(Model::Oklch)[1] < target);

        state.set(Model::Oklch, [0.5, target, 264.0]);
        assert_eq!(state.coords(Model::Oklch)[1], target);
    }

    #[test]
    fn bytes_survive_the_state() {
        let mut state = ColorState::from_rgb8([18, 52, 86]);
        assert_eq!(state.rgb8(), [18, 52, 86]);
        state.set_rgb8([200, 100, 5]);
        assert_eq!(state.rgb8(), [200, 100, 5]);
    }
}
