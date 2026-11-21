//! Mercury's look: graphite surfaces with a single quicksilver-cyan accent, generous spacing,
//! soft 10px radii. Deliberately unlike Hermes's terminal aesthetic.

use egui::{Color32, CornerRadius, Stroke, Theme, Visuals};

pub struct Palette {
    pub bg: Color32,
    pub surface: Color32,
    pub raised: Color32,
    pub border: Color32,
    pub text: Color32,
    pub muted: Color32,
    pub accent: Color32,
    pub accent_soft: Color32,
    pub user_bubble: Color32,
    pub danger: Color32,
    pub warn: Color32,
    pub ok: Color32,
}

pub const DARK: Palette = Palette {
    bg: Color32::from_rgb(0x12, 0x15, 0x1a),
    surface: Color32::from_rgb(0x19, 0x1d, 0x24),
    raised: Color32::from_rgb(0x22, 0x27, 0x30),
    border: Color32::from_rgb(0x2d, 0x33, 0x3d),
    text: Color32::from_rgb(0xdc, 0xe1, 0xe8),
    muted: Color32::from_rgb(0x8a, 0x94, 0xa3),
    accent: Color32::from_rgb(0x5e, 0xc8, 0xd8),
    accent_soft: Color32::from_rgb(0x1d, 0x3a, 0x40),
    user_bubble: Color32::from_rgb(0x1f, 0x2f, 0x36),
    danger: Color32::from_rgb(0xef, 0x6f, 0x6c),
    warn: Color32::from_rgb(0xf2, 0xb5, 0x5a),
    ok: Color32::from_rgb(0x7c, 0xd3, 0x8f),
};

pub const LIGHT: Palette = Palette {
    bg: Color32::from_rgb(0xf5, 0xf6, 0xf8),
    surface: Color32::from_rgb(0xff, 0xff, 0xff),
    raised: Color32::from_rgb(0xec, 0xee, 0xf2),
    border: Color32::from_rgb(0xd9, 0xdd, 0xe3),
    text: Color32::from_rgb(0x1d, 0x22, 0x2a),
    muted: Color32::from_rgb(0x5f, 0x69, 0x78),
    accent: Color32::from_rgb(0x0b, 0x86, 0x9a),
    accent_soft: Color32::from_rgb(0xd7, 0xf0, 0xf3),
    user_bubble: Color32::from_rgb(0xe3, 0xf3, 0xf5),
    danger: Color32::from_rgb(0xc6, 0x37, 0x34),
    warn: Color32::from_rgb(0xb5, 0x6d, 0x00),
    ok: Color32::from_rgb(0x2b, 0x8a, 0x48),
};

pub fn palette(dark: bool) -> &'static Palette {
    if dark { &DARK } else { &LIGHT }
}

fn visuals(p: &Palette, dark: bool) -> Visuals {
    let mut v = if dark { Visuals::dark() } else { Visuals::light() };
    let r = CornerRadius::same(8);
    v.panel_fill = p.bg;
    v.window_fill = p.surface;
    v.window_stroke = Stroke::new(1.0, p.border);
    v.window_corner_radius = CornerRadius::same(12);
    v.menu_corner_radius = CornerRadius::same(10);
    v.faint_bg_color = p.surface;
    v.extreme_bg_color = p.surface;
    v.code_bg_color = p.raised;
    v.hyperlink_color = p.accent;
    v.warn_fg_color = p.warn;
    v.error_fg_color = p.danger;
    v.selection.bg_fill = p.accent_soft;
    v.selection.stroke = Stroke::new(1.0, p.accent);
    v.override_text_color = Some(p.text);
    for w in [&mut v.widgets.noninteractive, &mut v.widgets.inactive, &mut v.widgets.hovered, &mut v.widgets.active, &mut v.widgets.open] {
        w.corner_radius = r;
    }
    v.widgets.noninteractive.bg_fill = p.surface;
    v.widgets.noninteractive.weak_bg_fill = p.surface;
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, p.border);
    v.widgets.inactive.bg_fill = p.raised;
    v.widgets.inactive.weak_bg_fill = p.raised;
    v.widgets.inactive.bg_stroke = Stroke::NONE;
    v.widgets.hovered.bg_fill = p.accent_soft;
    v.widgets.hovered.weak_bg_fill = p.accent_soft;
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, p.accent);
    v.widgets.active.bg_fill = p.accent_soft;
    v.widgets.active.weak_bg_fill = p.accent_soft;
    v.widgets.active.bg_stroke = Stroke::new(1.0, p.accent);
    v
}

pub fn install(ctx: &egui::Context, dark: bool, scale: f32) {
    ctx.set_visuals_of(Theme::Dark, visuals(&DARK, true));
    ctx.set_visuals_of(Theme::Light, visuals(&LIGHT, false));
    ctx.set_theme(if dark { Theme::Dark } else { Theme::Light });
    ctx.options_mut(|o| o.zoom_with_keyboard = false); // Mercury handles Ctrl +/-/0 and persists it
    ctx.all_styles_mut(|s| {
        // Larger base type than egui's defaults (12.5pt body), before any zoom.
        use egui::{FontFamily, FontId, TextStyle};
        s.text_styles = [
            (TextStyle::Small, FontId::new(12.5, FontFamily::Proportional)),
            (TextStyle::Body, FontId::new(15.5, FontFamily::Proportional)),
            (TextStyle::Button, FontId::new(15.0, FontFamily::Proportional)),
            (TextStyle::Monospace, FontId::new(14.0, FontFamily::Monospace)),
            (TextStyle::Heading, FontId::new(22.0, FontFamily::Proportional)),
        ]
        .into();
        s.spacing.item_spacing = egui::vec2(8.0, 6.0);
        s.spacing.button_padding = egui::vec2(10.0, 5.0);
        s.spacing.interact_size.y = 26.0;
    });
    ctx.set_zoom_factor(crate::config::clamp_scale(if scale > 0.0 { scale } else { 1.0 }));
}

/// System fonts appended as fallbacks so model output in other scripts (CJK, Cyrillic, symbols)
/// renders instead of showing boxes. Nothing is bundled; whatever the distro ships is used.
const FALLBACK_FONTS: &[&str] = &[
    "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
    "/usr/share/fonts/TTF/DejaVuSans.ttf",
    "/usr/share/fonts/dejavu/DejaVuSans.ttf",
    "/usr/share/fonts/dejavu-sans-fonts/DejaVuSans.ttf",
    "/usr/share/fonts/truetype/noto/NotoSans-Regular.ttf",
    "/usr/share/fonts/noto/NotoSans-Regular.ttf",
    "/usr/share/fonts/google-noto/NotoSans-Regular.ttf",
    "/usr/share/fonts/truetype/noto/NotoSansSymbols2-Regular.ttf",
    "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/google-noto-cjk/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/opentype/noto/NotoSansCJKsc-Regular.otf",
    "/usr/share/fonts/truetype/wqy/wqy-microhei.ttc",
    "/usr/share/fonts/truetype/droid/DroidSansFallbackFull.ttf",
];

pub fn install_fallback_fonts(ctx: &egui::Context) -> usize {
    let mut defs = egui::FontDefinitions::default();
    let mut added = 0;
    for path in FALLBACK_FONTS {
        let Ok(bytes) = std::fs::read(path) else { continue };
        let name = format!("system:{path}");
        defs.font_data.insert(name.clone(), std::sync::Arc::new(egui::FontData::from_owned(bytes)));
        for fam in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
            defs.families.entry(fam).or_default().push(name.clone());
        }
        added += 1;
    }
    if added > 0 {
        ctx.set_fonts(defs);
    }
    added
}
