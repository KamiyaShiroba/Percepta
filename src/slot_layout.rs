//! Where the colour slot panel puts its parts, whatever shape it is given.
//!
//! The panel's shape picks one of three arrangements. Each has a largest size and keeps its
//! proportions: past that a bigger panel only adds empty room around it, centred. The buttons
//! stay for as long as they can; the text, and then the HEX field, make way first.

use eframe::egui::{pos2, vec2, Align2, Pos2, Rect, Vec2};

use crate::layout::{MIN_WIDTHS, SLOTS_SQUARE_MIN_HEIGHT};

/// What a panel has around its content: the side margins, and the title strip and padding.
const PANEL_CHROME: Vec2 = vec2(16.0, 26.0);

/// At least this many times wider than high, it is a row of slots above a row of the rest.
const WIDE_RATIO: f32 = 2.0;
/// At least this many times higher than wide, it is that turned on its side.
const TALL_RATIO: f32 = 2.0;
const CHIP_WIDTHS: [f32; 3] = [5.0, 2.0, 2.0];
const CHIP_GAP: f32 = 6.0;
const ROW_HEIGHT: f32 = 22.0;
const ROW_GAP: f32 = 3.0;
const ITEM_GAP: f32 = 8.0;
const HEX_LABEL_WIDTH: f32 = 24.0;
const HEX_WIDTH: f32 = 76.0;
const STATUS_WIDTH: f32 = 52.0;
pub const DOT_SIZE: f32 = 9.0;
const SETTINGS_SIZE: f32 = 22.0;
/// How far the background slot sits from the foreground one, relative to their size.
const OVERLAP_OFFSET: f32 = 0.5;
/// The transparent slot below them, as a strip as wide as the pair.
const TRANSPARENT_STRIP: f32 = 0.5;
const MIN_TRANSPARENT_STRIP: f32 = 20.0;
const TRANSPARENT_GAP: f32 = 2.0;
/// The row a part sits in, with the gap that sets it apart.
const ROW_ROOM: f32 = ROW_HEIGHT + ROW_GAP;
/// Slots shorter than this are worth more than the HEX field.
const TALL_HEX_MIN_CHIPS: f32 = 60.0;

const ROW_MAX_WIDTH: f32 = 640.0;
const ROW_MAX_CHIP_HEIGHT: f32 = 64.0;
/// Of the foreground slot's width, so that the narrower slots do not become bars.
const ROW_CHIP_HEIGHT: f32 = 0.75;
const SQUARE_MAX_WIDTH: f32 = 180.0;
const SQUARE_MAX_PAIR: f32 = 120.0;
const MAX_TRANSPARENT_STRIP: f32 = 32.0;
const TALL_MAX_WIDTH: f32 = 120.0;
/// The slots together, relative to their width.
const TALL_MAX_CHIPS: f32 = 3.0;

pub struct Arrangement {
    pub main: Rect,
    pub sub: Rect,
    pub transparent: Rect,
    pub hex_label: Option<Rect>,
    pub hex: Option<Rect>,
    pub status: Option<Rect>,
    pub dot: Pos2,
    pub settings: Rect,
}

pub fn arrange(area: Rect) -> Arrangement {
    let panel = area.size() + PANEL_CHROME;
    if panel.y >= panel.x * TALL_RATIO {
        tall(area)
    } else if panel.x >= MIN_WIDTHS[0] && (panel.x >= panel.y * WIDE_RATIO || panel.y < SLOTS_SQUARE_MIN_HEIGHT) {
        wide(area)
    } else {
        square(area)
    }
}

/// The lengths of the three slots along a line, without their gaps.
fn chip_lengths(total: f32) -> [f32; 3] {
    let unit = (total - CHIP_GAP * 2.0).max(0.0) / CHIP_WIDTHS.iter().sum::<f32>();
    CHIP_WIDTHS.map(|width| width * unit)
}

fn wide(area: Rect) -> Arrangement {
    let width = area.width().min(ROW_MAX_WIDTH);
    let lengths = chip_lengths(width);
    let chip_height = (area.height() - ROW_ROOM).min(ROW_MAX_CHIP_HEIGHT).min(lengths[0] * ROW_CHIP_HEIGHT);
    let area = Align2::CENTER_CENTER.align_size_within_rect(vec2(width, chip_height + ROW_ROOM), area);
    let row_top = area.bottom() - ROW_HEIGHT;
    let chip = |index: usize| {
        let before: f32 = lengths[..index].iter().sum::<f32>() + CHIP_GAP * index as f32;
        Rect::from_min_size(area.min + vec2(before, 0.0), vec2(lengths[index], chip_height))
    };

    let row_item = |left: f32, width: f32| Rect::from_min_size(pos2(left, row_top), vec2(width, ROW_HEIGHT));
    let settings = row_item(area.right() - SETTINGS_SIZE, SETTINGS_SIZE);
    let dot_room = DOT_SIZE + ITEM_GAP;
    let room = area.width() - SETTINGS_SIZE - ITEM_GAP - dot_room;
    let show_field = room >= HEX_WIDTH + ITEM_GAP;
    let show_label = room >= HEX_LABEL_WIDTH + HEX_WIDTH + ITEM_GAP * 2.0;
    let show_status = room >= HEX_LABEL_WIDTH + HEX_WIDTH + STATUS_WIDTH + ITEM_GAP * 2.0;

    let mut left = area.left();
    let mut place = |shown: bool, width: f32| {
        shown.then(|| {
            let item = row_item(left, width);
            left += width + ITEM_GAP;
            item
        })
    };
    let hex_label = place(show_label, HEX_LABEL_WIDTH);
    let hex = place(show_field, HEX_WIDTH);
    let dot = pos2(left + DOT_SIZE / 2.0, row_top + ROW_HEIGHT / 2.0);
    let status = show_status.then(|| row_item(left + dot_room, STATUS_WIDTH));

    Arrangement { main: chip(0), sub: chip(1), transparent: chip(2), hex_label, hex, status, dot, settings }
}

/// The width of the overlapping slots, in a space for them and the strip below.
fn pair_extent(width: f32, height: f32) -> f32 {
    let fitted = (height - TRANSPARENT_GAP) / (1.0 + OVERLAP_OFFSET + TRANSPARENT_STRIP);
    (height - TRANSPARENT_GAP - (fitted * TRANSPARENT_STRIP).max(MIN_TRANSPARENT_STRIP)).min(width)
}

fn square(area: Rect) -> Arrangement {
    let width = area.width().min(SQUARE_MAX_WIDTH);
    let beside_button = SETTINGS_SIZE + ROW_GAP;
    // The HEX field is centred too, so the settings button only stays in its row if both fit.
    let button_on_top = width < HEX_WIDTH + beside_button * 2.0;
    let above = if button_on_top { ROW_ROOM } else { 0.0 };

    let extent = pair_extent(width, area.height() - above - ROW_ROOM).clamp(0.0, SQUARE_MAX_PAIR);
    let side = extent / (1.0 + OVERLAP_OFFSET);
    let strip = (side * TRANSPARENT_STRIP).clamp(MIN_TRANSPARENT_STRIP, MAX_TRANSPARENT_STRIP);
    let slots_height = extent + TRANSPARENT_GAP + strip;
    let size = vec2(width, above + slots_height + ROW_ROOM);
    let area = Align2::CENTER_CENTER.align_size_within_rect(size, area);

    let corner = if button_on_top { Align2::LEFT_TOP } else { Align2::LEFT_BOTTOM };
    let settings = corner.align_size_within_rect(Vec2::splat(SETTINGS_SIZE), area);
    let hex = Rect::from_min_size(pos2(area.center().x - HEX_WIDTH / 2.0, area.bottom() - ROW_HEIGHT), vec2(HEX_WIDTH, ROW_HEIGHT));
    let min = pos2(area.center().x - extent / 2.0, area.top() + above);
    let main = Rect::from_min_size(min, Vec2::splat(side));
    let sub = main.translate(Vec2::splat(side * OVERLAP_OFFSET));
    let transparent = Rect::from_min_size(min + vec2(0.0, extent + TRANSPARENT_GAP), vec2(extent, strip));

    Arrangement {
        main,
        sub,
        transparent,
        hex_label: None,
        hex: Some(hex),
        status: None,
        dot: area.right_top() + vec2(-DOT_SIZE / 2.0, DOT_SIZE / 2.0),
        settings,
    }
}

fn tall(area: Rect) -> Arrangement {
    let width = area.width().min(TALL_MAX_WIDTH);
    let show_hex = width >= HEX_WIDTH && area.height() - ROW_ROOM * 2.0 >= TALL_HEX_MIN_CHIPS;
    let rows = if show_hex { ROW_ROOM * 2.0 } else { ROW_ROOM };
    let chips_height = (area.height() - rows).min(width * TALL_MAX_CHIPS);
    let area = Align2::CENTER_CENTER.align_size_within_rect(vec2(width, chips_height + rows), area);

    let row_top = area.bottom() - ROW_HEIGHT;
    let settings = Align2::LEFT_BOTTOM.align_size_within_rect(Vec2::splat(SETTINGS_SIZE), area);
    let dot = pos2(area.right() - DOT_SIZE / 2.0, row_top + ROW_HEIGHT / 2.0);
    let hex = show_hex.then(|| {
        let top = row_top - ROW_ROOM;
        Rect::from_min_size(pos2(area.center().x - HEX_WIDTH / 2.0, top), vec2(HEX_WIDTH, ROW_HEIGHT))
    });

    let lengths = chip_lengths(chips_height);
    let chip = |index: usize| {
        let before: f32 = lengths[..index].iter().sum::<f32>() + CHIP_GAP * index as f32;
        Rect::from_min_size(area.min + vec2(0.0, before), vec2(width, lengths[index]))
    };

    Arrangement { main: chip(0), sub: chip(1), transparent: chip(2), hex_label: None, hex, status: None, dot, settings }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area(width: f32, height: f32) -> Rect {
        Rect::from_min_size(pos2(10.0, 20.0), vec2(width, height))
    }

    fn assert_inside(area: Rect, layout: &Arrangement) {
        let parts = [layout.main, layout.sub, layout.transparent, layout.settings];
        for part in parts.into_iter().chain(layout.hex).chain(layout.hex_label).chain(layout.status) {
            assert!(area.expand(0.01).contains_rect(part), "{part:?} is outside {area:?}");
        }
        assert!(area.expand(0.01).contains(layout.dot));
    }

    #[test]
    fn every_shape_keeps_its_parts_inside_and_never_covers_the_settings_button_with_hex() {
        for width in (34..600).step_by(7) {
            for height in (54..800).step_by(7) {
                // Windows keeps a panel out of this corner.
                if width < 104 && height < 174 {
                    continue;
                }
                let area = area(width as f32, height as f32);
                let layout = arrange(area);
                assert_inside(area, &layout);
                if let Some(hex) = layout.hex {
                    assert!(!hex.intersects(layout.settings), "{area:?}");
                }
                for chip in [layout.main, layout.sub, layout.transparent] {
                    assert!(!chip.intersects(layout.settings), "{area:?}");
                }
                assert!(!layout.transparent.intersects(layout.sub), "{area:?}");
            }
        }
    }

    #[test]
    fn a_long_row_shows_everything_in_the_order_it_always_had() {
        let layout = arrange(area(554.0, 86.0));
        assert!(layout.main.width() > layout.sub.width());
        assert_eq!(layout.main.top(), layout.sub.top());
        let (label, hex, status) = (layout.hex_label.unwrap(), layout.hex.unwrap(), layout.status.unwrap());
        assert!(label.right() <= hex.left() && hex.right() <= layout.dot.x && layout.dot.x <= status.left());
        assert!(status.right() <= layout.settings.left());
    }

    #[test]
    fn a_narrowing_row_drops_the_text_before_the_field() {
        let layout = arrange(area(170.0, 54.0));
        assert!(layout.status.is_none() && layout.hex_label.is_some() && layout.hex.is_some());
        let layout = arrange(area(140.0, 54.0));
        assert!(layout.hex_label.is_none() && layout.hex.is_some());
        let layout = arrange(area(125.0, 54.0));
        assert!(layout.hex.is_none());
    }

    #[test]
    fn near_a_square_the_slots_overlap_above_the_transparent_strip_and_hex() {
        let layout = arrange(area(154.0, 180.0));
        assert!(layout.main.intersects(layout.sub));
        assert!(layout.main.left() < layout.sub.left() && layout.main.top() < layout.sub.top());
        assert!(layout.transparent.top() >= layout.sub.bottom());
        assert_eq!(layout.transparent.width(), layout.sub.right() - layout.main.left());
        assert!(layout.hex.unwrap().top() >= layout.transparent.bottom());
        assert!(layout.settings.left() == 10.0 && layout.settings.bottom() == layout.hex.unwrap().bottom());
        assert!(layout.dot.x > layout.sub.right() - 1.0 && layout.dot.y < layout.main.center().y);
    }

    #[test]
    fn near_a_square_it_stays_centred_and_a_narrow_one_moves_the_settings_button_up() {
        for (width, height) in [(154.0, 180.0), (94.0, 174.0), (300.0, 250.0)] {
            let area = area(width, height);
            let layout = arrange(area);
            let pair = layout.main.union(layout.sub);
            assert!((pair.center().x - area.center().x).abs() < 0.01, "{area:?}");
            assert!((layout.hex.unwrap().center().x - area.center().x).abs() < 0.01, "{area:?}");
        }
        let narrow = area(94.0, 174.0);
        let layout = arrange(narrow);
        assert_eq!(layout.settings.left(), narrow.left());
        assert!(layout.main.top() >= layout.settings.bottom());
    }

    #[test]
    fn the_shape_follows_the_panel_with_its_title_strip_and_margins() {
        let is_tall = |width, height| {
            let layout = arrange(area(width, height));
            layout.main.bottom() <= layout.sub.top()
        };
        let is_wide = |width, height| {
            let layout = arrange(area(width, height));
            layout.main.right() <= layout.sub.left()
        };
        // 100 by 200 is exactly upright, and a little wider is not.
        assert!(is_tall(84.0, 174.0));
        assert!(!is_tall(85.0, 174.0) && !is_wide(85.0, 174.0));
        // Too low for the overlapping slots.
        assert!(is_wide(104.0, 117.0));
        // Too narrow for a row. Only a window being resized gets here, for a moment.
        assert!(!is_wide(103.0, 117.0) && !is_tall(103.0, 117.0));
        assert!(is_wide(484.0, 174.0) && !is_wide(483.0, 224.0));
        assert!(is_tall(34.0, 200.0));
    }

    #[test]
    fn a_bigger_panel_past_the_largest_size_only_adds_empty_room_around_it() {
        fn extent(layout: &Arrangement) -> Rect {
            layout.main.union(layout.sub).union(layout.transparent).union(layout.settings)
        }
        for (reached, bigger) in [
            ((640.0, 89.0), (1200.0, 300.0)),
            ((180.0, 190.0), (500.0, 500.0)),
            ((120.0, 410.0), (300.0, 900.0)),
        ] {
            let reached = extent(&arrange(area(reached.0, reached.1)));
            let bigger_area = area(bigger.0, bigger.1);
            let bigger = extent(&arrange(bigger_area));
            assert!((reached.size() - bigger.size()).length() < 0.01, "{reached:?} {bigger:?}");
            assert!((bigger.center() - bigger_area.center()).length() < 60.0, "{bigger:?}");
        }
    }

    #[test]
    fn a_row_in_a_narrow_panel_keeps_its_slots_from_becoming_bars() {
        let layout = arrange(area(104.0, 174.0));
        assert!(layout.main.height() <= layout.main.width());
    }

    #[test]
    fn upright_the_foreground_is_on_top_and_narrow_loses_hex() {
        let layout = arrange(area(100.0, 400.0));
        assert!(layout.main.bottom() <= layout.sub.top() && layout.sub.bottom() <= layout.transparent.top());
        assert!(layout.main.height() > layout.sub.height());
        assert!(layout.hex.is_some());
        assert_eq!(layout.settings.bottom(), 395.0);
        assert!(arrange(area(74.0, 400.0)).hex.is_none());
    }
}
