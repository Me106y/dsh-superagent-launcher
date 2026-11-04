//! One CJK font, borrowed from the system, so the Chinese interface is not full of empty boxes.
//!
//! egui ships no CJK font and bundling one would mean redistributing it, so this module asks fontconfig
//! (`fc-match`) for a font that covers Chinese, or falls back to well-known paths. Nothing is bundled and
//! nothing is downloaded. `DSH_SUPERAGENT_FONT=path[:index]` overrides the search.

use eframe::egui;
use std::{
    path::{Path, PathBuf},
    process::Command,
    sync::Arc,
};

/// Add the CJK font as a fallback of both families. Returns false when no such font is installed.
pub fn install(ctx: &egui::Context) -> bool {
    let Some((path, index)) = candidate() else {
        return false;
    };
    let Ok(bytes) = std::fs::read(&path) else {
        return false;
    };
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        "cjk".to_owned(),
        Arc::new(egui::FontData::from_blob(Arc::new(bytes), index)),
    );
    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        fonts
            .families
            .entry(family)
            .or_default()
            .push("cjk".to_owned());
    }
    ctx.set_fonts(fonts);
    true
}

/// Whether a CJK font can be found without actually loading it.
pub fn available() -> bool {
    candidate().is_some()
}

fn candidate() -> Option<(PathBuf, u32)> {
    if let Some(spec) = std::env::var_os("DSH_SUPERAGENT_FONT") {
        if let Some(found) = parse_spec(&spec.to_string_lossy()) {
            return Some(found);
        }
    }

    #[cfg(target_os = "macos")]
    for path in [
        "/System/Library/Fonts/PingFang.ttc",
        "/System/Library/Fonts/STHeiti Medium.ttc",
        "/System/Library/Fonts/STHeiti Light.ttc",
    ] {
        if Path::new(path).is_file() {
            return Some((PathBuf::from(path), 0));
        }
    }

    #[cfg(not(target_os = "macos"))]
    {
        // `lang=zh` only matches fonts that cover Chinese. Named families are not used: fontconfig happily
        // substitutes an unrelated font when the family is missing.
        if let Some(found) = fc_match("sans-serif:lang=zh") {
            return Some(found);
        }
        for path in [
            "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
            "/usr/share/fonts/truetype/droid/DroidSansFallbackFull.ttf",
            "/usr/share/fonts/google-noto-cjk/NotoSansCJK-Regular.ttc",
        ] {
            if Path::new(path).is_file() {
                return Some((PathBuf::from(path), 0));
            }
        }
    }

    None
}

#[cfg(not(target_os = "macos"))]
fn fc_match(pattern: &str) -> Option<(PathBuf, u32)> {
    let output = Command::new("fc-match")
        .args(["-f", "%{file}\t%{index}", pattern])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let (file, index) = text.split_once('\t')?;
    let path = PathBuf::from(file.trim());
    if !path.is_file() {
        return None;
    }
    Some((path, index.trim().parse().unwrap_or(0)))
}

/// `path` or `path:index`, used by `DSH_SUPERAGENT_FONT`.
fn parse_spec(spec: &str) -> Option<(PathBuf, u32)> {
    let (path, index) = match spec.rsplit_once(':') {
        Some((path, index)) if index.chars().all(|c| c.is_ascii_digit()) && !index.is_empty() => {
            (path, index.parse().unwrap_or(0))
        }
        _ => (spec, 0),
    };
    let path = PathBuf::from(path);
    path.is_file().then_some((path, index))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_system_font_really_covers_chinese() {
        if !available() {
            return;
        }
        let ctx = egui::Context::default();
        assert!(install(&ctx));
        // Fonts are only built inside a frame.
        let mut covered = false;
        let _ = ctx.run_ui(egui::RawInput::default(), |ctx| {
            covered = ctx.fonts_mut(|fonts| {
                fonts.has_glyphs(&egui::FontId::proportional(14.0), "中文仓库仓库")
            });
        });
        assert!(covered, "the chosen font has no Chinese glyphs");
    }
}
