//! The interface's languages. Each is a JSON file of keys and texts, with `{1}` and so on for
//! the values that go in them: those in `assets/lang/` are built in, and a file in the
//! `languages` folder beside the settings adds to the built-in one of the same name, or is a
//! new language. Whatever a language lacks comes from English.

use std::collections::HashMap;
use std::fmt::Display;
use std::fs;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::OnceLock;

use windows_sys::Win32::Globalization::{GetUserDefaultUILanguage, LCIDToLocaleName};

/// In the settings: whatever Windows is in, as far as there is a language for it.
pub const SYSTEM: &str = "system";
const FALLBACK: &str = "en";
const TRADITIONAL_CHINESE: &str = "zh-TW";
const SIMPLIFIED_CHINESE: &str = "zh-CN";
/// What a language calls itself, in its own file.
const NAME_KEY: &str = "language.name";
const FOLDER: &str = "languages";
const LOCALE_NAME_LENGTH: usize = 85;

const BUILT_IN: [(&str, &str); 6] = [
    (FALLBACK, include_str!("../assets/lang/en.json")),
    (SIMPLIFIED_CHINESE, include_str!("../assets/lang/zh-CN.json")),
    (TRADITIONAL_CHINESE, include_str!("../assets/lang/zh-TW.json")),
    ("ja", include_str!("../assets/lang/ja.json")),
    ("ko", include_str!("../assets/lang/ko.json")),
    // Classical Chinese, for fun. No Windows locale leads here, so it has to be chosen.
    ("lzh", include_str!("../assets/lang/lzh.json")),
];

struct Language {
    code: String,
    /// Kept for as long as the program runs, so that the UI can hold on to them for free.
    texts: HashMap<String, &'static str>,
}

impl Language {
    fn name(&self) -> &'static str {
        self.texts.get(NAME_KEY).copied().unwrap_or(FALLBACK)
    }
}

struct Catalog {
    languages: Vec<Language>,
    problems: Vec<String>,
}

static CATALOG: OnceLock<Catalog> = OnceLock::new();
/// Into the catalog's languages, of which the first is the fallback.
static CURRENT: AtomicUsize = AtomicUsize::new(0);

fn parse(json: &str) -> Result<HashMap<String, &'static str>, serde_json::Error> {
    // Notepad and others may put one there.
    let json = json.strip_prefix('\u{feff}').unwrap_or(json);
    let texts: HashMap<String, String> = serde_json::from_str(json)?;
    Ok(texts.into_iter().map(|(key, text)| (key, &*text.leak())).collect())
}

fn catalog() -> &'static Catalog {
    CATALOG.get_or_init(|| {
        let mut languages: Vec<Language> = BUILT_IN
            .iter()
            .map(|(code, json)| Language {
                code: (*code).to_owned(),
                // The tests read every one of them.
                texts: parse(json).unwrap_or_default(),
            })
            .collect();
        let mut problems = Vec::new();
        let folder = std::env::var_os("APPDATA").map(|dir| std::path::Path::new(&dir).join("Percepta").join(FOLDER));
        let files = folder.and_then(|folder| fs::read_dir(folder).ok()).into_iter().flatten().flatten();
        for path in files.map(|file| file.path()).filter(|path| path.extension().is_some_and(|e| e == "json")) {
            let Some(code) = path.file_stem().map(|stem| stem.to_string_lossy().into_owned()) else { continue };
            let read = fs::read_to_string(&path).map_err(|e| e.to_string());
            match read.and_then(|json| parse(&json).map_err(|e| e.to_string())) {
                Ok(texts) => match languages.iter_mut().find(|language| language.code == code) {
                    Some(language) => language.texts.extend(texts),
                    None => languages.push(Language { code, texts }),
                },
                Err(reason) => problems.push(format!("{code}: {reason}")),
            }
        }
        Catalog { languages, problems }
    })
}

/// What went wrong reading the files of added languages, for the log.
pub fn problems() -> Vec<String> {
    catalog().problems.iter().map(|problem| {
        let (file, reason) = problem.split_once(": ").unwrap_or((problem, ""));
        fill(text("log.language_file"), &[&file, &reason])
    }).collect()
}

/// The code and name of each language.
pub fn choices() -> Vec<(&'static str, &'static str)> {
    catalog().languages.iter().map(|language| (language.code.as_str(), language.name())).collect()
}

/// The name of a language by its code, unless it is `SYSTEM` or unknown.
pub fn name_of(code: &str) -> Option<&'static str> {
    catalog().languages.iter().find(|language| language.code == code).map(Language::name)
}

/// Switches to a language by its code, or to the system's for `SYSTEM` and anything unknown.
pub fn set(code: &str) {
    let languages = &catalog().languages;
    let code = if languages.iter().any(|language| language.code == code) { code.to_owned() } else { system_code() };
    let index = languages.iter().position(|language| language.code == code).unwrap_or(0);
    CURRENT.store(index, Ordering::Relaxed);
}

pub fn current_code() -> &'static str {
    catalog().languages[CURRENT.load(Ordering::Relaxed)].code.as_str()
}

fn system_code() -> String {
    let mut name = [0u16; LOCALE_NAME_LENGTH];
    // SAFETY: the pointer and length describe `name`.
    let written = unsafe { LCIDToLocaleName(GetUserDefaultUILanguage() as u32, name.as_mut_ptr(), name.len() as i32, 0) };
    let locale = String::from_utf16_lossy(&name[..(written.max(1) - 1) as usize]);
    let codes: Vec<&str> = catalog().languages.iter().map(|language| language.code.as_str()).collect();
    best_match(&locale, &codes).to_owned()
}

/// The language for a Windows locale like `zh-HK` or `de-DE`: exactly that, or Chinese in the
/// script it is written in, or the same language anywhere, or else English.
fn best_match<'a>(locale: &str, codes: &[&'a str]) -> &'a str {
    let locale = locale.to_lowercase();
    let (language, region) = locale.split_once('-').unwrap_or((&locale, ""));
    let find = |wanted: &str| codes.iter().find(|code| code.eq_ignore_ascii_case(wanted)).copied();
    let chinese = (language == "zh").then(|| {
        if region.contains("hant") || ["tw", "hk", "mo"].contains(&region) { TRADITIONAL_CHINESE } else { SIMPLIFIED_CHINESE }
    });
    find(&locale).or_else(|| chinese.and_then(find)).or_else(|| find(language)).unwrap_or(FALLBACK)
}

/// The text of `key` in the current language.
pub fn text(key: &'static str) -> &'static str {
    let languages = &catalog().languages;
    let found = |language: &Language| language.texts.get(key).copied();
    found(&languages[CURRENT.load(Ordering::Relaxed)]).or_else(|| found(&languages[0])).unwrap_or(key)
}

/// Puts `values` in the place of `{1}`, `{2}` and so on, in whatever order the text has them.
pub fn fill(template: &str, values: &[&dyn Display]) -> String {
    let mut filled = String::new();
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        filled.push_str(&rest[..open]);
        rest = &rest[open..];
        let close = rest.find('}');
        let value = close.and_then(|close| rest[1..close].parse::<usize>().ok()).and_then(|n| values.get(n.checked_sub(1)?));
        match (value, close) {
            (Some(value), Some(close)) => {
                filled.push_str(&value.to_string());
                rest = &rest[close + 1..];
            }
            // Not a value, or one that a hand-made language file made up.
            _ => {
                filled.push('{');
                rest = &rest[1..];
            }
        }
    }
    filled.push_str(rest);
    filled
}

/// The text of a key in the current language, with the values in it if there are any.
macro_rules! tr {
    ($key:literal) => {
        $crate::i18n::text($key)
    };
    ($key:literal, $($value:expr),+ $(,)?) => {
        $crate::i18n::fill($crate::i18n::text($key), &[$(&$value as &dyn std::fmt::Display),+])
    };
}
pub(crate) use tr;

/// A key that is only turned into text when it reaches the user, through `text`.
macro_rules! key {
    ($key:literal) => {
        $key
    };
}
pub(crate) use key;

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    const SOURCES: &[&str] = &[
        include_str!("app.rs"),
        include_str!("daemon.rs"),
        include_str!("i18n.rs"),
        include_str!("picker.rs"),
        include_str!("platform.rs"),
        include_str!("tray.rs"),
        include_str!("update.rs"),
        include_str!("main.rs"),
        include_str!("csp/client.rs"),
        include_str!("csp/worker.rs"),
    ];

    /// The first argument of every call of these macros in the code.
    fn keys_in_use() -> HashSet<&'static str> {
        let mut keys = HashSet::from([NAME_KEY, "log.language_file"]);
        for source in SOURCES {
            for marker in ["tr!(\"", "key!(\""] {
                for (start, _) in source.match_indices(marker) {
                    // Not the end of a longer name, like `include_str!`, or this very test.
                    if source[..start].ends_with(|letter: char| letter.is_alphanumeric() || letter == '_' || letter == '[') {
                        continue;
                    }
                    let rest = &source[start + marker.len()..];
                    keys.insert(&rest[..rest.find('"').unwrap()]);
                }
            }
        }
        keys
    }

    fn built_in() -> Vec<(&'static str, HashMap<String, &'static str>)> {
        BUILT_IN.iter().map(|(code, json)| (*code, parse(json).unwrap())).collect()
    }

    fn values_in(text: &str) -> Vec<usize> {
        let mut values = Vec::new();
        for (start, _) in text.match_indices('{') {
            let close = text[start..].find('}').unwrap();
            values.push(text[start + 1..start + close].parse().unwrap());
        }
        values.sort_unstable();
        values
    }

    #[test]
    fn every_built_in_language_has_what_the_code_asks_for_and_nothing_else() {
        let keys = keys_in_use();
        assert!(keys.len() > 100, "found only {}", keys.len());
        for (code, texts) in built_in() {
            let missing: Vec<_> = keys.iter().filter(|key| !texts.contains_key(**key)).collect();
            assert!(missing.is_empty(), "{code} lacks {missing:?}");
            let unused: Vec<_> = texts.keys().filter(|key| !keys.contains(key.as_str())).collect();
            assert!(unused.is_empty(), "{code} has unused {unused:?}");
        }
    }

    #[test]
    fn every_translation_takes_the_same_values_as_english() {
        let languages = built_in();
        let english = &languages[0].1;
        for (code, texts) in &languages {
            for (key, text) in texts {
                assert_eq!(values_in(text), values_in(english[key]), "{code} {key}");
            }
        }
    }

    #[test]
    fn every_built_in_language_has_its_own_name() {
        let names: HashSet<_> = built_in().iter().map(|(_, texts)| texts[NAME_KEY]).collect();
        assert_eq!(names.len(), BUILT_IN.len());
    }

    #[test]
    fn a_file_saved_with_a_byte_order_mark_is_read_all_the_same() {
        let texts = parse("\u{feff}{\"panel.colors\":\"X\"}").unwrap();
        assert_eq!(texts["panel.colors"], "X");
    }

    #[test]
    fn values_go_where_the_text_wants_them() {
        assert_eq!(fill("{1}:{2} ...", &[&"1.2.3.4", &80]), "1.2.3.4:80 ...");
        assert_eq!(fill("{2} of {1}", &[&"a", &"b"]), "b of a");
        assert_eq!(fill("no values", &[]), "no values");
        // A file that asks for what isn't there, or isn't right, doesn't take anything down.
        assert_eq!(fill("{3} {x} {", &[&"a"]), "{3} {x} {");
        // A value that looks like a placeholder is left as it is.
        assert_eq!(fill("{1} {2}", &[&"{2}", &"b"]), "{2} b");
    }

    #[test]
    fn the_system_decides_the_language_and_anything_else_gets_english() {
        let codes = [FALLBACK, SIMPLIFIED_CHINESE, TRADITIONAL_CHINESE, "ja", "ko"];
        assert_eq!(best_match("zh-CN", &codes), "zh-CN");
        assert_eq!(best_match("zh-SG", &codes), "zh-CN");
        assert_eq!(best_match("zh-TW", &codes), "zh-TW");
        assert_eq!(best_match("zh-HK", &codes), "zh-TW");
        assert_eq!(best_match("zh-Hant-MO", &codes), "zh-TW");
        assert_eq!(best_match("ja-JP", &codes), "ja");
        assert_eq!(best_match("ko-KR", &codes), "ko");
        assert_eq!(best_match("en-GB", &codes), "en");
        assert_eq!(best_match("de-DE", &codes), "en");
        assert_eq!(best_match("", &codes), "en");
        // A language added by hand counts too.
        assert_eq!(best_match("de-DE", &["en", "de"]), "de");
    }
}
