//! Light and dark palettes, and where the user's choice is remembered.
//!
//! Both palettes keep the launcher's mint accent from the icon and stay close in contrast, so switching themes
//! changes the surface, not the hierarchy. `Auto` follows the desktop's colour scheme.

use eframe::egui::{
    self, Color32, CornerRadius, FontFamily, FontId, Shadow, Stroke, TextStyle, Vec2,
};

/// What the user picked. `System` follows the desktop's light/dark preference.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum ThemeChoice {
    #[default]
    System,
    Light,
    Dark,
}

impl ThemeChoice {
    pub fn stored(self) -> &'static str {
        match self {
            ThemeChoice::System => "system",
            ThemeChoice::Light => "light",
            ThemeChoice::Dark => "dark",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "system" => Some(ThemeChoice::System),
            "light" => Some(ThemeChoice::Light),
            "dark" => Some(ThemeChoice::Dark),
            _ => None,
        }
    }
}

/// Every colour the window needs, per theme.
#[derive(Clone, Copy)]
pub struct Palette {
    pub panel: Color32,
    pub window: Color32,
    pub surface: Color32,
    pub surface_hover: Color32,
    pub extreme: Color32,
    pub text: Color32,
    pub weak: Color32,
    pub accent: Color32,
    pub accent_soft: Color32,
    pub on_accent: Color32,
    pub selection: Color32,
    pub stroke: Color32,
    pub warn: Color32,
    pub error: Color32,
    pub shadow: Color32,
}

const DARK: Palette = Palette {
    panel: Color32::from_rgb(0x0d, 0x17, 0x1d),
    window: Color32::from_rgb(0x10, 0x1c, 0x24),
    surface: Color32::from_rgb(0x16, 0x25, 0x2e),
    surface_hover: Color32::from_rgb(0x1c, 0x2f, 0x3a),
    extreme: Color32::from_rgb(0x0a, 0x13, 0x19),
    text: Color32::from_rgb(0xe8, 0xf0, 0xf3),
    weak: Color32::from_rgb(0x93, 0xa7, 0xb2),
    accent: Color32::from_rgb(0x45, 0xd1, 0x93),
    accent_soft: Color32::from_rgb(0x12, 0x2a, 0x24),
    on_accent: Color32::from_rgb(0x07, 0x13, 0x0e),
    selection: Color32::from_rgb(0x18, 0x36, 0x2c),
    stroke: Color32::from_rgb(0x24, 0x37, 0x42),
    warn: Color32::from_rgb(0xe2, 0xa1, 0x5c),
    error: Color32::from_rgb(0xe6, 0x81, 0x7d),
    shadow: Color32::from_black_alpha(110),
};

const LIGHT: Palette = Palette {
    panel: Color32::from_rgb(0xf5, 0xf6, 0xf2),
    window: Color32::from_rgb(0xff, 0xff, 0xff),
    surface: Color32::from_rgb(0xea, 0xee, 0xe8),
    surface_hover: Color32::from_rgb(0xe1, 0xe7, 0xdf),
    extreme: Color32::from_rgb(0xff, 0xff, 0xff),
    text: Color32::from_rgb(0x16, 0x23, 0x2a),
    weak: Color32::from_rgb(0x5c, 0x6f, 0x78),
    accent: Color32::from_rgb(0x12, 0x75, 0x4f),
    accent_soft: Color32::from_rgb(0xdf, 0xf2, 0xe8),
    on_accent: Color32::from_rgb(0xff, 0xff, 0xff),
    selection: Color32::from_rgb(0xcf, 0xea, 0xdd),
    stroke: Color32::from_rgb(0xd5, 0xdd, 0xd6),
    warn: Color32::from_rgb(0x96, 0x56, 0x0f),
    error: Color32::from_rgb(0xb4, 0x43, 0x3f),
    shadow: Color32::from_black_alpha(30),
};

pub fn palette(dark: bool) -> Palette {
    if dark { DARK } else { LIGHT }
}

fn widget(
    bg_fill: Color32,
    weak_bg_fill: Color32,
    stroke: Color32,
    text: Color32,
) -> egui::style::WidgetVisuals {
    egui::style::WidgetVisuals {
        bg_fill,
        weak_bg_fill,
        bg_stroke: Stroke::new(1.0, stroke),
        corner_radius: CornerRadius::same(7),
        fg_stroke: Stroke::new(1.0, text),
        expansion: 0.0,
    }
}

/// Install both palettes and the typographic scale, then select the user's preference.
///
/// egui keeps one style per theme, so `Auto` needs no work when the desktop scheme flips.
pub fn apply(ctx: &egui::Context, choice: ThemeChoice) {
    for (theme, dark) in [(egui::Theme::Dark, true), (egui::Theme::Light, false)] {
        let mut style = (*ctx.style_of(theme)).clone();
        style.visuals = visuals(dark);
        style.text_styles = [
            (
                TextStyle::Heading,
                FontId::new(21.0, FontFamily::Proportional),
            ),
            (TextStyle::Body, FontId::new(14.0, FontFamily::Proportional)),
            (
                TextStyle::Button,
                FontId::new(14.0, FontFamily::Proportional),
            ),
            (
                TextStyle::Small,
                FontId::new(11.5, FontFamily::Proportional),
            ),
            (
                TextStyle::Monospace,
                FontId::new(12.5, FontFamily::Monospace),
            ),
        ]
        .into();
        style.spacing.item_spacing = Vec2::new(8.0, 6.0);
        style.spacing.button_padding = Vec2::new(10.0, 5.0);
        ctx.set_style_of(theme, style);
    }
    ctx.set_theme(match choice {
        ThemeChoice::System => egui::ThemePreference::System,
        ThemeChoice::Light => egui::ThemePreference::Light,
        ThemeChoice::Dark => egui::ThemePreference::Dark,
    });
}

/// The visuals of one theme, without touching the context.
fn visuals(dark: bool) -> egui::Visuals {
    let p = palette(dark);
    let mut visuals = if dark {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    };
    visuals.dark_mode = dark;
    visuals.weak_text_color = Some(p.weak);
    visuals.hyperlink_color = p.accent;
    visuals.faint_bg_color = p.surface;
    visuals.extreme_bg_color = p.extreme;
    visuals.text_edit_bg_color = Some(p.extreme);
    visuals.code_bg_color = p.surface;
    visuals.warn_fg_color = p.warn;
    visuals.error_fg_color = p.error;
    visuals.window_fill = p.window;
    visuals.panel_fill = p.panel;
    visuals.window_stroke = Stroke::new(1.0, p.stroke);
    visuals.window_corner_radius = CornerRadius::same(12);
    visuals.menu_corner_radius = CornerRadius::same(10);
    visuals.window_shadow = Shadow {
        offset: [0, 12],
        blur: 32,
        spread: 0,
        color: p.shadow,
    };
    visuals.popup_shadow = Shadow {
        offset: [0, 8],
        blur: 24,
        spread: 0,
        color: p.shadow,
    };
    visuals.selection = egui::style::Selection {
        bg_fill: p.selection,
        stroke: Stroke::new(1.0, p.accent),
    };
    visuals.striped = false;
    visuals.interact_cursor = Some(egui::CursorIcon::PointingHand);

    let hovered_stroke = if dark {
        Color32::from_rgb(0x2f, 0x6b, 0x57)
    } else {
        Color32::from_rgb(0xa8, 0xd2, 0xbd)
    };
    visuals.widgets.noninteractive = widget(p.panel, Color32::TRANSPARENT, p.stroke, p.text);
    visuals.widgets.inactive = widget(p.surface, p.surface, p.stroke, p.text);
    visuals.widgets.hovered = widget(p.surface_hover, p.surface_hover, hovered_stroke, p.text);
    visuals.widgets.active = widget(p.accent_soft, p.accent_soft, p.accent, p.text);
    visuals.widgets.open = widget(p.accent_soft, p.accent_soft, p.accent, p.text);

    visuals
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_choice_survives_the_config_file() {
        for choice in [ThemeChoice::System, ThemeChoice::Light, ThemeChoice::Dark] {
            assert_eq!(ThemeChoice::parse(choice.stored()), Some(choice));
        }
    }

    #[test]
    fn both_palettes_stay_readable() {
        fn channel(value: u8) -> f32 {
            let value = f32::from(value) / 255.0;
            if value <= 0.03928 {
                value / 12.92
            } else {
                ((value + 0.055) / 1.055).powf(2.4)
            }
        }
        fn luminance(color: Color32) -> f32 {
            0.2126 * channel(color.r()) + 0.7152 * channel(color.g()) + 0.0722 * channel(color.b())
        }
        fn contrast(a: Color32, b: Color32) -> f32 {
            let (high, low) = (
                luminance(a).max(luminance(b)),
                luminance(a).min(luminance(b)),
            );
            (high + 0.05) / (low + 0.05)
        }

        for dark in [false, true] {
            let palette = palette(dark);
            assert!(
                contrast(palette.text, palette.panel) >= 7.0,
                "body text, dark={dark}"
            );
            assert!(
                contrast(palette.weak, palette.panel) >= 4.5,
                "weak text, dark={dark}"
            );
            assert!(
                contrast(palette.accent, palette.panel) >= 4.5,
                "accent, dark={dark}"
            );
            assert!(
                contrast(palette.on_accent, palette.accent) >= 4.5,
                "on accent, dark={dark}"
            );
        }
    }
}
