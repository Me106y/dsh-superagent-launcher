//! Persisted preferences: the theme and the interface language, in one small JSON file.
//!
//! The file lives beside the other per-user configuration: `$XDG_CONFIG_HOME/dsh-superagent-launcher` on
//! Linux and `~/Library/Application Support/dsh-superagent-launcher` on macOS. An older `theme.json` from
//! before the language existed is still read for the theme.

use crate::{i18n::Lang, platform::home, theme::ThemeChoice};
use serde_json::Value;
use std::{fs, path::PathBuf};

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub struct Settings {
    pub theme: ThemeChoice,
    pub lang: Lang,
}

fn config_dir() -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        home().join("Library/Application Support/dsh-superagent-launcher")
    }
    #[cfg(not(target_os = "macos"))]
    {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home().join(".config"))
            .join("dsh-superagent-launcher")
    }
}

fn path() -> PathBuf {
    config_dir().join("settings.json")
}

fn legacy_path() -> PathBuf {
    config_dir().join("theme.json")
}

pub fn load() -> Settings {
    let mut settings = Settings::default();
    if let Ok(text) = fs::read_to_string(path()) {
        if let Ok(value) = serde_json::from_str::<Value>(&text) {
            settings.theme = value["theme"]
                .as_str()
                .and_then(ThemeChoice::parse)
                .unwrap_or_default();
            settings.lang = value["lang"]
                .as_str()
                .and_then(Lang::parse)
                .unwrap_or_default();
        }
        return settings;
    }
    if let Ok(text) = fs::read_to_string(legacy_path()) {
        if let Ok(value) = serde_json::from_str::<Value>(&text) {
            settings.theme = value["theme"]
                .as_str()
                .and_then(ThemeChoice::parse)
                .unwrap_or_default();
        }
    }
    settings
}

pub fn save(settings: Settings) {
    if let Some(parent) = path().parent() {
        let _ = fs::create_dir_all(parent);
    }
    let body = serde_json::json!({
        "theme": settings.theme.stored(),
        "lang": settings.lang.stored(),
    })
    .to_string();
    let _ = fs::write(path(), body);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_follow_the_desktop() {
        let settings = Settings::default();
        assert_eq!(settings.theme, ThemeChoice::System);
        assert_eq!(settings.lang, Lang::System);
    }
}
