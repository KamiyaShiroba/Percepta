#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod color;
mod csp;
mod icc;
mod picker;
mod platform;
mod qr;
mod session;
mod update;

const ICON_SIZE: u32 = 256;

fn main() -> eframe::Result {
    // eframe sets its own default icon the first time the window is focused, unless
    // it is given this one.
    let icon = eframe::egui::IconData {
        rgba: picker::app_icon_pixels(ICON_SIZE as usize),
        width: ICON_SIZE,
        height: ICON_SIZE,
    };
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("Percepta")
            .with_inner_size([380.0, 620.0])
            .with_min_inner_size([240.0, 400.0])
            .with_maximize_button(false)
            .with_icon(icon)
            .with_active(false),
        ..Default::default()
    };
    eframe::run_native(
        "Percepta",
        options,
        Box::new(|cc| Ok(Box::new(app::App::new(cc)))),
    )
}
