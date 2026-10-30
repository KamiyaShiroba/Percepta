//! CMYK through the colour profiles CSP itself is set to use. CSP takes no CMYK over
//! Companion Mode, so its values have to be converted to RGB here.

use std::fs;
use std::path::{Path, PathBuf};

use lcms2::{Flags, Intent, PixelFormat, Profile, Transform};
use rusqlite::{Connection, OpenFlags};

use crate::color::Channels;

pub type Cmyk = [f64; 4];

/// lcms measures CMYK ink in percent, unlike everything else in this program.
const INK_SCALE: f64 = 100.0;
const CONFIG_PREFIX: &str = "CLIPStudioPaintVer";

#[derive(Debug, Clone, PartialEq, Eq)]
struct CspProfiles {
    rgb_path: String,
    cmyk_path: String,
    rendering_intent: i64,
}

/// Old CSP versions leave their settings behind, so the live one is the newest.
fn newest_config() -> Option<PathBuf> {
    let root = PathBuf::from(std::env::var_os("APPDATA")?).join("CELSYSUserData");
    let mut parents = vec![root.clone()];
    parents.push(root.join("CELSYS"));
    parents
        .iter()
        .filter_map(|parent| fs::read_dir(parent).ok())
        .flatten()
        .flatten()
        .filter(|entry| entry.file_name().to_string_lossy().starts_with(CONFIG_PREFIX))
        .map(|entry| entry.path().join("Preference").join("Config.sqlite"))
        .filter_map(|path| Some((fs::metadata(&path).ok()?.modified().ok()?, path)))
        .max_by_key(|(modified, _)| *modified)
        .map(|(_, path)| path)
}

/// Opens the database without taking any lock, as CSP may have it open for writing.
fn read_profiles(config: &Path) -> Option<CspProfiles> {
    let path = config.to_string_lossy().replace('\\', "/");
    let encoded: String = path
        .chars()
        .map(|c| match c {
            ' ' => "%20".to_string(),
            '%' => "%25".to_string(),
            '#' => "%23".to_string(),
            '?' => "%3F".to_string(),
            other => other.to_string(),
        })
        .collect();
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI;
    let db = Connection::open_with_flags(format!("file:///{encoded}?mode=ro&immutable=1"), flags).ok()?;
    db.query_row(
        "select IccProfileRGBProfilePath, IccProfileCMYKProfilePath, IccProfileRenderingIntent \
         from IccProfile limit 1",
        [],
        |row| {
            Ok(CspProfiles {
                rgb_path: row.get(0)?,
                cmyk_path: row.get(1)?,
                rendering_intent: row.get(2)?,
            })
        },
    )
    .ok()
}

fn intent_from_setting(setting: i64) -> Intent {
    match setting {
        0 => Intent::Perceptual,
        2 => Intent::Saturation,
        3 => Intent::AbsoluteColorimetric,
        _ => Intent::RelativeColorimetric,
    }
}

struct Transforms {
    to_rgb: Transform<[f64; 4], [f64; 3]>,
    to_cmyk: Transform<[f64; 3], [f64; 4]>,
}

impl Transforms {
    fn new(profiles: &CspProfiles) -> Option<Self> {
        let cmyk = Profile::new_file(&profiles.cmyk_path).ok()?;
        // An unreadable RGB profile is no reason to give up on the CMYK one.
        let rgb = Profile::new_file(&profiles.rgb_path).unwrap_or_else(|_| Profile::new_srgb());
        let intent = intent_from_setting(profiles.rendering_intent);
        let flags = Flags::BLACKPOINT_COMPENSATION;
        Some(Self {
            to_rgb: Transform::new_flags(&cmyk, PixelFormat::CMYK_DBL, &rgb, PixelFormat::RGB_DBL, intent, flags).ok()?,
            to_cmyk: Transform::new_flags(&rgb, PixelFormat::RGB_DBL, &cmyk, PixelFormat::CMYK_DBL, intent, flags).ok()?,
        })
    }
}

/// Converts between CMYK and RGB, by CSP's profiles when they can be read and otherwise
/// by the plain formula, which `profile_name` makes clear.
pub struct CmykConverter {
    transforms: Option<Transforms>,
    profile_name: Option<String>,
}

impl CmykConverter {
    pub fn from_csp_settings() -> Self {
        let profiles = newest_config().and_then(|config| read_profiles(&config));
        let transforms = profiles.as_ref().and_then(Transforms::new);
        let profile_name = transforms.as_ref().and(profiles.as_ref()).map(|p| profile_file_name(&p.cmyk_path));
        Self { transforms, profile_name }
    }

    /// `None` when falling back to the formula.
    pub fn profile_name(&self) -> Option<&str> {
        self.profile_name.as_deref()
    }

    pub fn to_rgb(&self, cmyk: Cmyk) -> Channels {
        let Some(transforms) = &self.transforms else {
            return formula_to_rgb(cmyk);
        };
        let mut rgb = [[0.0; 3]];
        transforms.to_rgb.transform_pixels(&[cmyk.map(|ink| ink * INK_SCALE)], &mut rgb);
        rgb[0].map(|channel| channel.clamp(0.0, 1.0))
    }

    pub fn from_rgb(&self, rgb: Channels) -> Cmyk {
        let Some(transforms) = &self.transforms else {
            return formula_from_rgb(rgb);
        };
        let mut cmyk = [[0.0; 4]];
        transforms.to_cmyk.transform_pixels(&[rgb], &mut cmyk);
        cmyk[0].map(|ink| (ink / INK_SCALE).clamp(0.0, 1.0))
    }
}

fn profile_file_name(path: &str) -> String {
    // Split by hand: these are Windows paths whichever way this is built or tested.
    let name = path.rsplit(['\\', '/']).next().unwrap_or(path);
    name.rsplit_once('.').map_or(name, |(stem, _)| stem).to_string()
}

fn formula_to_rgb([c, m, y, k]: Cmyk) -> Channels {
    [(1.0 - c) * (1.0 - k), (1.0 - m) * (1.0 - k), (1.0 - y) * (1.0 - k)]
}

fn formula_from_rgb([r, g, b]: Channels) -> Cmyk {
    let k = 1.0 - r.max(g).max(b);
    if k >= 1.0 {
        return [0.0, 0.0, 0.0, 1.0];
    }
    let ink = |channel: f64| (1.0 - channel - k) / (1.0 - k);
    [ink(r), ink(g), ink(b), k]
}

#[cfg(test)]
mod tests {
    use super::*;

    const SYSTEM_PROFILE: &str = r"C:\Windows\System32\spool\drivers\color\JapanColor2001Coated.icc";

    #[test]
    fn formula_round_trips_and_handles_black() {
        let rgb = [0.2, 0.6, 0.4];
        let back = formula_to_rgb(formula_from_rgb(rgb));
        for (a, b) in back.iter().zip(rgb) {
            assert!((a - b).abs() < 1e-9);
        }
        assert_eq!(formula_from_rgb([0.0; 3]), [0.0, 0.0, 0.0, 1.0]);
        assert_eq!(formula_to_rgb([0.0; 4]), [1.0; 3]);
    }

    #[test]
    fn names_profiles_by_file_without_extension() {
        assert_eq!(profile_file_name(r"C:\color\JapanColor2001Coated.icc"), "JapanColor2001Coated");
        assert_eq!(profile_file_name("sRGB Color Space Profile.icm"), "sRGB Color Space Profile");
    }

    #[test]
    fn unknown_intents_fall_back_to_relative() {
        assert!(matches!(intent_from_setting(1), Intent::RelativeColorimetric));
        assert!(matches!(intent_from_setting(99), Intent::RelativeColorimetric));
        assert!(matches!(intent_from_setting(0), Intent::Perceptual));
    }

    #[test]
    fn reads_the_settings_table_without_locking() {
        let path = std::env::temp_dir().join(format!("percepta test {}.sqlite", std::process::id()));
        let _ = fs::remove_file(&path);
        let db = Connection::open(&path).unwrap();
        db.execute_batch(
            r"create table IccProfile (
                IccProfileRGBProfilePath text,
                IccProfileCMYKProfilePath text,
                IccProfileRenderingIntent integer
              );
              insert into IccProfile values ('C:\rgb.icm', 'C:\cmyk.icc', 1);",
        )
        .unwrap();
        drop(db);

        let profiles = read_profiles(&path).unwrap();
        assert_eq!(profiles.rgb_path, r"C:\rgb.icm");
        assert_eq!(profiles.cmyk_path, r"C:\cmyk.icc");
        assert_eq!(profiles.rendering_intent, 1);
        fs::remove_file(&path).unwrap();
    }

    #[test]
    fn a_missing_profile_is_not_usable() {
        let profiles = CspProfiles {
            rgb_path: "missing.icm".into(),
            cmyk_path: "missing.icc".into(),
            rendering_intent: 1,
        };
        assert!(Transforms::new(&profiles).is_none());
    }

    /// Uses a profile that Windows may or may not have, so it passes without one.
    #[test]
    fn a_real_profile_converts_sensibly() {
        if !Path::new(SYSTEM_PROFILE).exists() {
            return;
        }
        let profiles = CspProfiles {
            rgb_path: "missing, so sRGB".into(),
            cmyk_path: SYSTEM_PROFILE.into(),
            rendering_intent: 1,
        };
        let converter = CmykConverter {
            transforms: Transforms::new(&profiles),
            profile_name: Some(profile_file_name(&profiles.cmyk_path)),
        };
        assert!(converter.transforms.is_some());

        // No ink is paper white. Relative intent makes it pure white.
        let white = converter.to_rgb([0.0; 4]);
        assert!(white.iter().all(|c| *c > 0.98), "{white:?}");
        // Full black ink is dark, and cyan is clearly bluer than it is red.
        let black = converter.to_rgb([0.0, 0.0, 0.0, 1.0]);
        assert!(black.iter().all(|c| *c < 0.3), "{black:?}");
        let [red, _, blue] = converter.to_rgb([1.0, 0.0, 0.0, 0.0]);
        assert!(blue > red + 0.3, "{red} {blue}");

        // Going to CMYK and back lands close to an in-gamut colour.
        let rgb = [0.6, 0.45, 0.4];
        let back = converter.to_rgb(converter.from_rgb(rgb));
        for (a, b) in back.iter().zip(rgb) {
            assert!((a - b).abs() < 0.03, "{back:?}");
        }
    }
}
