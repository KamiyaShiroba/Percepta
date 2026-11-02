#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod color;
mod csp;
mod daemon;
mod frame;
mod i18n;
mod icc;
mod layout;
mod picker;
mod platform;
mod qr;
mod session;
mod slot_layout;
mod tray;
mod update;

const ICON_SIZE: u32 = 256;

fn main() -> eframe::Result {
    if std::env::args().any(|argument| argument == daemon::ARGUMENT) {
        daemon::run();
        return Ok(());
    }
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
            // As small as it can ever be, or a smaller saved size would not come back. The
            // app sets the real minimum as soon as it has the panels.
            .with_min_inner_size([layout::SLOTS_NARROWEST, layout::MIN_HEIGHTS[0]])
            .with_decorations(false)
            .with_maximize_button(false)
            .with_icon(icon)
            .with_active(false),
        // The driver busy-waits through every window's swap, which took more than a whole core
        // with three windows. The desktop compositor keeps frames whole, and the app paces them.
        glow_options: eframe::egui_glow::GlowConfiguration { vsync: false, ..Default::default() },
        ..Default::default()
    };
    let result = eframe::run_native(
        "Percepta",
        options,
        Box::new(|cc| Ok(Box::new(app::App::new(cc)))),
    );
    // There is no console to see this on, and a program that just doesn't open explains nothing.
    if let Err(e) = &result {
        i18n::set(&session::load_settings().language);
        platform::show_error(&i18n::tr!("error.graphics", e));
    }
    result
}
