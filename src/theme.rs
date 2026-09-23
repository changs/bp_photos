//! Borrows the look of the user's Ghostty terminal: its font and colour theme.
//!
//! Reads Ghostty's config (`font-family`, `font-size`, `theme`, and colour overrides), resolves
//! the theme file, and finds the font among installed fonts. Anything missing falls back to
//! egui's defaults.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use eframe::egui::{self, Color32, FontData, FontDefinitions, FontFamily, Stroke, TextStyle, Theme};

/// Terminal colours that matter for the UI.
#[derive(Clone, Debug, Default)]
struct Colors {
    background: Option<Color32>,
    foreground: Option<Color32>,
    selection_bg: Option<Color32>,
    selection_fg: Option<Color32>,
    cursor: Option<Color32>,
    palette: HashMap<u8, Color32>,
}

impl Colors {
    fn apply(&mut self, key: &str, value: &str) {
        match key {
            "background" => self.background = parse_color(value),
            "foreground" => self.foreground = parse_color(value),
            "selection-background" => self.selection_bg = parse_color(value),
            "selection-foreground" => self.selection_fg = parse_color(value),
            "cursor-color" => self.cursor = parse_color(value),
            "palette" => {
                if let Some((i, c)) = value.split_once('=')
                    && let (Ok(i), Some(c)) = (i.trim().parse(), parse_color(c))
                {
                    self.palette.insert(i, c);
                }
            }
            _ => {}
        }
    }

    fn merged(&self, over: &Colors) -> Colors {
        let mut palette = self.palette.clone();
        palette.extend(over.palette.iter().map(|(k, v)| (*k, *v)));
        Colors {
            background: over.background.or(self.background),
            foreground: over.foreground.or(self.foreground),
            selection_bg: over.selection_bg.or(self.selection_bg),
            selection_fg: over.selection_fg.or(self.selection_fg),
            cursor: over.cursor.or(self.cursor),
            palette,
        }
    }
}

pub struct GhosttyLook {
    pub summary: String,
}

fn parse_color(v: &str) -> Option<Color32> {
    let hex = v.trim().trim_start_matches('#');
    if hex.len() != 6 {
        return None;
    }
    let n = u32::from_str_radix(hex, 16).ok()?;
    Some(Color32::from_rgb((n >> 16) as u8, (n >> 8) as u8, n as u8))
}

/// `key = value` lines, in order (keys like `palette` repeat).
fn parse_kv(text: &str) -> Vec<(String, String)> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.trim().to_string(), v.trim().trim_matches('"').to_string()))
        .collect()
}

fn config_paths() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(x) = std::env::var_os("XDG_CONFIG_HOME") {
        dirs.push(PathBuf::from(x).join("ghostty"));
    }
    if let Some(home) = dirs::home_dir() {
        dirs.push(home.join(".config/ghostty"));
        dirs.push(home.join("Library/Application Support/com.mitchellh.ghostty"));
    }
    dirs.iter().flat_map(|d| [d.join("config"), d.join("config.ghostty")]).collect()
}

fn theme_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(x) = std::env::var_os("XDG_CONFIG_HOME") {
        dirs.push(PathBuf::from(x).join("ghostty/themes"));
    }
    if let Some(home) = dirs::home_dir() {
        dirs.push(home.join(".config/ghostty/themes"));
    }
    if let Some(r) = std::env::var_os("GHOSTTY_RESOURCES_DIR") {
        dirs.push(PathBuf::from(r).join("themes"));
    }
    dirs.push("/Applications/Ghostty.app/Contents/Resources/ghostty/themes".into());
    dirs.push("/usr/share/ghostty/themes".into());
    dirs.push("/usr/local/share/ghostty/themes".into());
    dirs
}

fn load_theme(name: &str) -> Option<Colors> {
    let path = if Path::new(name).is_absolute() {
        Some(PathBuf::from(name))
    } else {
        theme_dirs().into_iter().map(|d| d.join(name)).find(|p| p.is_file())
    }?;
    let mut c = Colors::default();
    for (k, v) in parse_kv(&std::fs::read_to_string(path).ok()?) {
        c.apply(&k, &v);
    }
    Some(c)
}

fn font_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(home) = dirs::home_dir() {
        dirs.push(home.join("Library/Fonts"));
        dirs.push(home.join(".local/share/fonts"));
        dirs.push(home.join(".fonts"));
    }
    for d in ["/Library/Fonts", "/System/Library/Fonts", "/usr/share/fonts", "/usr/local/share/fonts"] {
        dirs.push(d.into());
    }
    dirs
}

fn normalise(s: &str) -> String {
    s.chars().filter(char::is_ascii_alphanumeric).collect::<String>().to_lowercase()
}

/// Finds the regular-weight file for a font family by its file name, e.g. "TX-02" → TX-02-Regular.otf.
fn find_font(family: &str) -> Option<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>, depth: u8) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() && depth < 4 {
                walk(&p, out, depth + 1);
            } else if crate::import::extension(&p).is_some_and(|x| x == "ttf" || x == "otf") {
                out.push(p);
            }
        }
    }
    let want = normalise(family);
    let mut files = Vec::new();
    for d in font_dirs() {
        walk(&d, &mut files, 0);
    }
    const STYLES: [&str; 14] = [
        "bold", "italic", "oblique", "condensed", "light", "thin", "black", "medium", "semi", "extra", "heavy", "retina",
        "book", "nerd",
    ];
    files
        .into_iter()
        .filter_map(|p| {
            let stem = normalise(&p.file_stem()?.to_string_lossy());
            let rest = stem.strip_prefix(&want)?.to_string();
            let rank = match rest.as_str() {
                "regular" | "" => 0,
                r if !STYLES.iter().any(|s| r.contains(s)) => 1,
                _ => return None,
            };
            Some((rank, stem.len(), p))
        })
        .min_by_key(|(rank, len, _)| (*rank, *len))
        .map(|(.., p)| p)
}

fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    let l = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgb(l(a.r(), b.r()), l(a.g(), b.g()), l(a.b(), b.b()))
}

fn is_dark(c: Color32) -> bool {
    (0.2126 * c.r() as f32 + 0.7152 * c.g() as f32 + 0.0722 * c.b() as f32) < 128.0
}

/// egui visuals built from a terminal colour scheme.
fn visuals(c: &Colors) -> Option<egui::Visuals> {
    let (bg, fg) = (c.background?, c.foreground?);
    let dark = is_dark(bg);
    let mut v = if dark { egui::Visuals::dark() } else { egui::Visuals::light() };
    let accent = c.cursor.or(c.palette.get(&4).copied()).unwrap_or(v.selection.bg_fill);
    let sel_bg = c.selection_bg.unwrap_or(accent);
    let sel_fg = c.selection_fg.unwrap_or(fg);
    let muted = mix(bg, fg, 0.6);

    v.panel_fill = bg;
    v.window_fill = mix(bg, fg, 0.03);
    v.window_stroke = Stroke::new(1.0, mix(bg, fg, 0.15));
    v.extreme_bg_color = c.palette.get(&0).copied().filter(|p| *p != bg).unwrap_or(mix(bg, Color32::BLACK, 0.3));
    v.faint_bg_color = mix(bg, fg, 0.04);
    v.code_bg_color = mix(bg, fg, 0.08);
    v.hyperlink_color = c.palette.get(&12).or(c.palette.get(&4)).copied().unwrap_or(accent);
    v.warn_fg_color = c.palette.get(&3).copied().unwrap_or(v.warn_fg_color);
    v.error_fg_color = c.palette.get(&1).copied().unwrap_or(v.error_fg_color);
    v.selection.bg_fill = sel_bg;
    v.selection.stroke = Stroke::new(1.0, sel_fg);
    v.text_cursor.stroke = Stroke::new(2.0, accent);

    let w = &mut v.widgets;
    w.noninteractive.bg_fill = bg;
    w.noninteractive.weak_bg_fill = bg;
    w.noninteractive.bg_stroke = Stroke::new(1.0, mix(bg, fg, 0.12));
    w.noninteractive.fg_stroke = Stroke::new(1.0, mix(bg, fg, 0.85));
    for (state, t) in [(&mut w.inactive, 0.08), (&mut w.hovered, 0.15), (&mut w.active, 0.22), (&mut w.open, 0.12)] {
        state.bg_fill = mix(bg, fg, t);
        state.weak_bg_fill = mix(bg, fg, t);
        state.fg_stroke = Stroke::new(1.0, fg);
    }
    w.inactive.bg_stroke = Stroke::NONE;
    w.inactive.fg_stroke = Stroke::new(1.0, mix(muted, fg, 0.6));
    w.hovered.bg_stroke = Stroke::new(1.0, accent);
    w.active.bg_stroke = Stroke::new(1.0, accent);
    Some(v)
}

/// Applies the Ghostty font and colours to egui, if a Ghostty config is found.
pub fn apply_ghostty(ctx: &egui::Context) -> Option<GhosttyLook> {
    let path = config_paths().into_iter().find(|p| p.is_file())?;
    let config = parse_kv(&std::fs::read_to_string(&path).ok()?);
    let get = |k: &str| config.iter().rev().find(|(key, _)| key == k).map(|(_, v)| v.as_str());
    let mut summary = Vec::new();

    // Colours: theme file first, then inline overrides from the config.
    let mut inline = Colors::default();
    for (k, v) in &config {
        inline.apply(k, v);
    }
    let mut light = None;
    let mut dark = None;
    match get("theme") {
        Some(spec) if spec.contains(':') => {
            // "light:Name,dark:Name"
            for part in spec.split(',') {
                match part.split_once(':') {
                    Some(("light", name)) => light = load_theme(name.trim()),
                    Some(("dark", name)) => dark = load_theme(name.trim()),
                    _ => {}
                }
            }
            summary.push(format!("theme {spec}"));
        }
        Some(name) => {
            if let Some(t) = load_theme(name) {
                if t.background.is_some_and(is_dark) { dark = Some(t) } else { light = Some(t) }
                summary.push(format!("theme {name}"));
            }
        }
        None if inline.background.is_some() => {
            if inline.background.is_some_and(is_dark) { dark = Some(inline.clone()) } else { light = Some(inline.clone()) }
            summary.push("config colours".into());
        }
        None => {}
    }
    for (theme, colors) in [(Theme::Dark, dark), (Theme::Light, light)] {
        // Config-level colours override the theme's, as in Ghostty.
        if let Some(v) = colors.map(|c| c.merged(&inline)).as_ref().and_then(visuals) {
            ctx.set_visuals_of(theme, v);
        }
    }

    // Font: used for all UI text, with egui's fonts kept as fallbacks (for icons/emoji).
    let size: f32 = get("font-size").and_then(|s| s.parse().ok()).unwrap_or(13.0);
    let body = (size * 0.85).clamp(11.0, 20.0);
    if let Some(family) = get("font-family")
        && let Some(file) = find_font(family)
        && let Ok(bytes) = std::fs::read(&file)
    {
        let mut fonts = FontDefinitions::default();
        fonts.font_data.insert("ghostty".into(), FontData::from_owned(bytes).into());
        for fam in [FontFamily::Proportional, FontFamily::Monospace] {
            fonts.families.entry(fam).or_default().insert(0, "ghostty".into());
        }
        ctx.set_fonts(fonts);
        summary.push(format!("font {family}"));
    }
    for theme in [Theme::Dark, Theme::Light] {
        ctx.style_mut_of(theme, |s| {
            for (style, scale) in [
                (TextStyle::Small, 0.8),
                (TextStyle::Body, 1.0),
                (TextStyle::Button, 1.0),
                (TextStyle::Monospace, 1.0),
                (TextStyle::Heading, 1.35),
            ] {
                if let Some(f) = s.text_styles.get_mut(&style) {
                    f.size = body * scale;
                }
            }
        });
    }

    (!summary.is_empty()).then(|| GhosttyLook { summary: format!("Ghostty look: {}", summary.join(", ")) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_theme_file() {
        let mut c = Colors::default();
        for (k, v) in parse_kv("palette = 4=#a277ff\nbackground = #15141b\n# comment\nforeground = #edecee") {
            c.apply(&k, &v);
        }
        assert_eq!(c.background, Some(Color32::from_rgb(0x15, 0x14, 0x1b)));
        assert_eq!(c.palette.get(&4), Some(&Color32::from_rgb(0xa2, 0x77, 0xff)));
        let v = visuals(&c).unwrap();
        assert!(v.dark_mode);
        assert_eq!(v.panel_fill, Color32::from_rgb(0x15, 0x14, 0x1b));
    }
}
