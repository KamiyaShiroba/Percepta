//! Remembers what should outlive the app: the login, so reconnecting needs no QR
//! scan, and the colours, so they are back on the next start.

use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::i18n;
use crate::color::Model;
use crate::csp::auth::Pairing;
use crate::csp::{Rgb, Slot};
use crate::layout::{Flexible, Home, Node, Panel};
use crate::picker::SliderTab;

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct SavedLayout {
    /// The window's top-left corner, in physical pixels.
    pub position: Option<[i32; 2]>,
    /// The window's outer size, in physical pixels of the monitor it is on.
    pub size: Option<[i32; 2]>,
    pub wheel: Model,
    pub slider: SliderTab,
    /// How the panels are arranged, with sizes at 100% scale.
    #[serde(default)]
    pub dock: Option<Node>,
    #[serde(default)]
    pub flexible: Flexible,
    /// What 1.0.1 saved instead of `dock`: the heights of the colour slots, the wheel and the
    /// sliders, at 100% scale.
    #[serde(default, skip_serializing)]
    pub panels: Option<[i32; 3]>,
    /// Whether the window that these first fields describe is open.
    #[serde(default = "always")]
    pub main_open: bool,
    #[serde(default)]
    pub floating: Vec<SavedWindow>,
    #[serde(default)]
    pub hidden: Vec<HiddenPanel>,
    /// Where the settings window was relative to this one, in physical pixels.
    #[serde(default)]
    pub settings_offset: Option<[i32; 2]>,
    /// The settings window's outer size, in physical pixels.
    #[serde(default)]
    pub settings_size: Option<[i32; 2]>,
}

fn always() -> bool {
    true
}

/// A window other than the first, open or not.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct SavedWindow {
    pub id: u64,
    pub node: Node,
    #[serde(default)]
    pub flexible: Flexible,
    pub open: bool,
    pub position: Option<[i32; 2]>,
    pub size: Option<[i32; 2]>,
}

/// A panel taken out of a window on its own, which still has a place to go back to.
#[derive(Clone, Copy, PartialEq, Debug, Serialize, Deserialize)]
pub struct HiddenPanel {
    pub panel: Panel,
    pub window: u64,
    pub home: Home,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct SavedColors {
    pub active: Slot,
    pub last_color: Slot,
    pub main: Rgb,
    pub sub: Rgb,
}

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct SavedSettings {
    pub restore_csp: bool,
    pub check_updates: bool,
    /// The background process that follows CSP.
    pub daemon: bool,
    /// The code of a language, or `i18n::SYSTEM`.
    pub language: String,
}

impl Default for SavedSettings {
    fn default() -> Self {
        Self { restore_csp: true, check_updates: true, daemon: false, language: i18n::SYSTEM.to_owned() }
    }
}

fn path() -> Option<PathBuf> {
    let dir = std::env::var_os("APPDATA")?;
    Some(PathBuf::from(dir).join("Percepta").join("session.json"))
}

pub fn load() -> Option<Pairing> {
    serde_json::from_str(&fs::read_to_string(path()?).ok()?).ok()
}

pub fn save(pairing: &Pairing) -> Result<(), String> {
    let path = path().ok_or("APPDATA is not set")?;
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let text = serde_json::to_string(pairing).map_err(|e| e.to_string())?;
    fs::write(path, text).map_err(|e| e.to_string())
}

fn load_json<T: DeserializeOwned>(name: &str) -> Option<T> {
    let path = path()?.with_file_name(name);
    serde_json::from_str(&fs::read_to_string(path).ok()?).ok()
}

fn save_json<T: Serialize>(name: &str, value: &T) -> Result<(), String> {
    let path = path().ok_or("APPDATA is not set")?.with_file_name(name);
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let text = serde_json::to_string(value).map_err(|e| e.to_string())?;
    fs::write(path, text).map_err(|e| e.to_string())
}

pub fn load_colors() -> Option<SavedColors> {
    load_json("colors.json")
}

pub fn save_colors(colors: &SavedColors) -> Result<(), String> {
    save_json("colors.json", colors)
}

pub fn load_layout() -> Option<SavedLayout> {
    load_json("layout.json")
}

pub fn save_layout(layout: &SavedLayout) -> Result<(), String> {
    save_json("layout.json", layout)
}

pub fn load_settings() -> SavedSettings {
    load_json("settings.json").unwrap_or_default()
}

pub fn save_settings(settings: &SavedSettings) -> Result<(), String> {
    save_json("settings.json", settings)
}

/// Keeps a record of the connection that outlives the app, because drops are too
/// random to catch with the window open. Failing to write it must not disturb anything.
pub fn log_connection(event: &str) {
    let Some(path) = path().map(|p| p.with_file_name("connection.log")) else { return };
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(file, "{seconds} {event}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_layout_saved_before_sizes_existed_still_loads() {
        let old = r#"{"position":[76,76],"wheel":"Hsv","slider":{"Color":"Hsv"}}"#;
        let layout: SavedLayout = serde_json::from_str(old).unwrap();
        assert_eq!(layout.position, Some([76, 76]));
        assert_eq!(layout.size, None);
        assert!(layout.main_open);
        assert!(layout.floating.is_empty());
    }

    #[test]
    fn settings_saved_with_the_old_close_action_still_load() {
        let old = r#"{"restore_csp":false,"check_updates":true,"close_action":"Quit"}"#;
        let settings: SavedSettings = serde_json::from_str(old).unwrap();
        assert!(!settings.restore_csp);
    }

    #[test]
    fn windows_and_hidden_panels_survive_saving() {
        let old = r#"{"position":[1,2],"wheel":"Hsv","slider":{"Color":"Hsv"}}"#;
        let mut layout: SavedLayout = serde_json::from_str(old).unwrap();
        layout.main_open = false;
        layout.floating.push(SavedWindow {
            id: 4,
            node: Node::tabs(vec![Panel::Wheel]),
            flexible: Flexible::Auto,
            open: true,
            position: Some([5, 6]),
            size: Some([7, 8]),
        });
        layout.hidden.push(HiddenPanel {
            panel: Panel::Slots,
            window: 0,
            home: Home {
                beside: Panel::Sliders,
                drop: crate::layout::Drop::Side { axis: crate::layout::Axis::Vertical, before: true },
                size: Some(92.0),
            },
        });
        let again: SavedLayout = serde_json::from_str(&serde_json::to_string(&layout).unwrap()).unwrap();
        assert_eq!(again, layout);
    }

    #[test]
    fn a_layout_comes_back_with_its_arrangement_and_without_the_old_heights() {
        let old = r#"{"position":[1,2],"wheel":"Hsv","slider":{"Color":"Hsv"},"panels":[80,300,170]}"#;
        let mut layout: SavedLayout = serde_json::from_str(old).unwrap();
        assert_eq!(layout.panels, Some([80, 300, 170]));
        layout.dock = Some(Node::stack([80.0, 300.0, 170.0]));

        let text = serde_json::to_string(&layout).unwrap();
        assert!(!text.contains("[80,300,170]"), "{text}");
        let again: SavedLayout = serde_json::from_str(&text).unwrap();
        assert_eq!(again.dock, layout.dock);
    }
}

pub fn clear() {
    if let Some(path) = path() {
        let _ = fs::remove_file(path);
    }
}
