#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod color;
mod csp;
mod frame;
mod icc;
mod picker;
mod platform;
mod qr;
mod session;
mod tray;
mod update;

const ICON_SIZE: u32 = 256;

fn main() -> eframe::Result {
    if tray::show_running_instance() {
        return Ok(());
    }
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
            .with_inner_size([380.0, app::DEFAULT_WINDOW_HEIGHT])
            .with_min_inner_size([240.0, app::MIN_WINDOW_HEIGHT])
            .with_decorations(false)
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
