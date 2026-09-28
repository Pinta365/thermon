//! Colours for thermon's UIs.
//!
//! On Omarchy the palette follows the current theme
//! (`~/.local/state/omarchy/current/theme/colors.toml`). `THERMON_THEME` can
//! point at any file in that format. Everywhere else, and for any key a theme
//! leaves out, a built-in palette is used.

use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb(pub u8, pub u8, pub u8);

impl Rgb {
    /// `#rgb`, `#rrggbb` or `#rrggbbaa` (alpha ignored), `#` optional.
    pub fn parse(s: &str) -> Option<Rgb> {
        let h = s.trim().trim_start_matches('#');
        // Byte slicing below needs plain hex (from_str_radix alone accepts "+f").
        if !h.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        let hex = |s: &str| u8::from_str_radix(s, 16).ok();
        match h.len() {
            3 => {
                let d = |i: usize| hex(&h[i..i + 1]).map(|v| v * 17);
                Some(Rgb(d(0)?, d(1)?, d(2)?))
            }
            6 | 8 => Some(Rgb(hex(&h[0..2])?, hex(&h[2..4])?, hex(&h[4..6])?)),
            _ => None,
        }
    }
}

impl Serialize for Rgb {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&format!("#{:02x}{:02x}{:02x}", self.0, self.1, self.2))
    }
}

/// Semantic colours. `ok`/`warn`/`crit` colour sensor levels; `series` are
/// distinct line colours for charts.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Palette {
    pub dark: bool,
    /// Window background.
    pub background: Rgb,
    /// Side panels, headers.
    pub panel: Rgb,
    /// Inputs, table stripes, hovered widgets.
    pub raised: Rgb,
    pub foreground: Rgb,
    pub dim: Rgb,
    pub accent: Rgb,
    pub selection: Rgb,
    pub ok: Rgb,
    pub warn: Rgb,
    pub crit: Rgb,
    pub series: Vec<Rgb>,
}

impl Default for Palette {
    /// A neutral dark palette for systems without an Omarchy theme.
    fn default() -> Self {
        Palette {
            dark: true,
            background: Rgb(0x1b, 0x1d, 0x22),
            panel: Rgb(0x15, 0x17, 0x1b),
            raised: Rgb(0x2a, 0x2d, 0x34),
            foreground: Rgb(0xd6, 0xd9, 0xde),
            dim: Rgb(0x80, 0x86, 0x90),
            accent: Rgb(0x6e, 0xa8, 0xfe),
            selection: Rgb(0x3a, 0x40, 0x4c),
            ok: Rgb(0x7f, 0xc9, 0x8a),
            warn: Rgb(0xe7, 0xc1, 0x6a),
            crit: Rgb(0xef, 0x6f, 0x6c),
            series: vec![
                Rgb(0x6e, 0xa8, 0xfe),
                Rgb(0xe7, 0x8a, 0xd0),
                Rgb(0x6c, 0xd4, 0xc4),
                Rgb(0xf0, 0xa0, 0x6a),
                Rgb(0xa8, 0xd8, 0x7a),
                Rgb(0xc3, 0x9b, 0xf5),
                Rgb(0xe7, 0xc1, 0x6a),
            ],
        }
    }
}

impl Palette {
    /// Build from an Omarchy `colors.toml`. Unknown keys are ignored; missing
    /// or unparseable colours keep the default.
    pub fn from_colors_toml(s: &str) -> Result<Palette, String> {
        let table: HashMap<String, toml::Value> = toml::from_str(s).map_err(|e| e.to_string())?;
        let get = |k: &str| table.get(k).and_then(|v| v.as_str()).and_then(Rgb::parse);
        let mut p = Palette::default();
        if let Some(mode) = table.get("mode").and_then(|v| v.as_str()) {
            p.dark = mode != "light";
        }
        let set = |slot: &mut Rgb, keys: &[&str]| {
            if let Some(c) = keys.iter().find_map(|k| get(k)) {
                *slot = c;
            }
        };
        set(&mut p.background, &["background"]);
        set(&mut p.panel, &["dark_background", "darker_background"]);
        set(&mut p.raised, &["lighter_background", "selection"]);
        set(&mut p.foreground, &["foreground"]);
        set(&mut p.dim, &["dark_foreground", "muted"]);
        set(&mut p.accent, &["accent", "blue"]);
        set(&mut p.selection, &["selection", "lighter_background"]);
        set(&mut p.ok, &["green"]);
        set(&mut p.warn, &["yellow", "orange"]);
        set(&mut p.crit, &["red"]);

        let series: Vec<Rgb> = [
            "blue",
            "magenta",
            "cyan",
            "orange",
            "green",
            "bright_magenta",
            "yellow",
        ]
        .iter()
        .filter_map(|k| get(k))
        .fold(Vec::new(), |mut acc, c| {
            // Themes often reuse one hex for two names; keep lines distinct.
            if !acc.contains(&c) {
                acc.push(c);
            }
            acc
        });
        if series.len() >= 3 {
            p.series = series;
        }
        Ok(p)
    }
}

/// WCAG relative luminance.
fn luminance(c: Rgb) -> f64 {
    let ch = |v: u8| {
        let v = f64::from(v) / 255.0;
        if v <= 0.039_28 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * ch(c.0) + 0.7152 * ch(c.1) + 0.0722 * ch(c.2)
}

/// WCAG contrast ratio, 1..=21.
pub fn contrast(a: Rgb, b: Rgb) -> f64 {
    let (la, lb) = (luminance(a), luminance(b));
    (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
}

fn mix(a: Rgb, b: Rgb, t: f64) -> Rgb {
    let m = |x: u8, y: u8| (f64::from(x) + (f64::from(y) - f64::from(x)) * t).round() as u8;
    Rgb(m(a.0, b.0), m(a.1, b.1), m(a.2, b.2))
}

impl Palette {
    /// Push text colours toward the foreground until they reach `min`
    /// contrast on every background (pale accents on light themes).
    pub fn readable(mut self, min: f64) -> Palette {
        let backgrounds = [self.background, self.panel, self.raised];
        let fg = self.foreground;
        for c in [
            &mut self.dim,
            &mut self.warn,
            &mut self.crit,
            &mut self.ok,
            &mut self.accent,
        ] {
            let mut t = 0.0;
            while t < 1.0
                && backgrounds
                    .iter()
                    .any(|bg| contrast(mix(*c, fg, t), *bg) < min)
            {
                t += 0.05;
            }
            *c = mix(*c, fg, t.min(1.0));
        }
        self
    }
}

/// Where the palette comes from: `THERMON_THEME`, else the current Omarchy
/// theme if there is one.
pub fn theme_path() -> Option<PathBuf> {
    if let Some(p) = env::var_os("THERMON_THEME").filter(|p| !p.is_empty()) {
        return Some(PathBuf::from(p));
    }
    let state = env::var_os("XDG_STATE_HOME")
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            env::var_os("HOME")
                .filter(|h| !h.is_empty())
                .map(|h| PathBuf::from(h).join(".local/state"))
        })?;
    let p = state.join("omarchy/current/theme/colors.toml");
    p.exists().then_some(p)
}

/// Load the palette from `path`, falling back to the default. The error, if
/// any, is returned alongside so callers can log it.
pub fn load(path: Option<&Path>) -> (Palette, Option<String>) {
    let Some(path) = path else {
        return (Palette::default(), None);
    };
    match fs::read_to_string(path) {
        Ok(s) => match Palette::from_colors_toml(&s) {
            Ok(p) => (p, None),
            Err(e) => (Palette::default(), Some(format!("{}: {e}", path.display()))),
        },
        Err(e) => (Palette::default(), Some(format!("{}: {e}", path.display()))),
    }
}

/// Reloads the palette when the theme file changes. The file briefly vanishes
/// while Omarchy switches themes; that keeps the current palette.
pub struct ThemeWatch {
    path: Option<PathBuf>,
    mtime: Option<SystemTime>,
}

impl ThemeWatch {
    pub fn new() -> (ThemeWatch, Palette) {
        let path = theme_path();
        let (palette, _) = load(path.as_deref());
        let mut w = ThemeWatch { path, mtime: None };
        w.mtime = w.current_mtime();
        (w, palette)
    }

    fn current_mtime(&self) -> Option<SystemTime> {
        fs::metadata(self.path.as_ref()?)
            .and_then(|m| m.modified())
            .ok()
    }

    /// A new palette if the theme changed since the last call.
    pub fn poll(&mut self) -> Option<Palette> {
        // The theme may appear later (first login) or a new one be chosen.
        let path = theme_path();
        let mtime = path
            .as_ref()
            .and_then(|p| fs::metadata(p).and_then(|m| m.modified()).ok());
        if path == self.path && mtime == self.mtime {
            return None;
        }
        // Mid-switch (or removed): keep what we have until a file is back.
        if mtime.is_none() && self.mtime.is_some() {
            return None;
        }
        self.path = path;
        self.mtime = mtime;
        Some(load(self.path.as_deref()).0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::fixture;

    #[test]
    fn parses_hex() {
        assert_eq!(Rgb::parse("#89b4fa"), Some(Rgb(0x89, 0xb4, 0xfa)));
        assert_eq!(Rgb::parse("89b4faff"), Some(Rgb(0x89, 0xb4, 0xfa)));
        assert_eq!(Rgb::parse("#fff"), Some(Rgb(255, 255, 255)));
        assert_eq!(Rgb::parse("#12345"), None);
        assert_eq!(Rgb::parse("#gggggg"), None);
        assert_eq!(
            serde_json::to_string(&Rgb(1, 2, 255)).unwrap(),
            r##""#0102ff""##
        );
    }

    #[test]
    fn omarchy_theme() {
        let s = fs::read_to_string(fixture("themes/catppuccin.colors.toml")).unwrap();
        let p = Palette::from_colors_toml(&s).unwrap();
        assert!(p.dark);
        assert_eq!(p.background, Rgb(0x1e, 0x1e, 0x2e));
        assert_eq!(p.panel, Rgb(0x16, 0x16, 0x22));
        assert_eq!(p.accent, Rgb(0x89, 0xb4, 0xfa));
        assert_eq!(p.crit, Rgb(0xf3, 0x8b, 0xa8));
        assert_eq!(p.warn, Rgb(0xf9, 0xe2, 0xaf));
        // blue == accent and bright_magenta == magenta: duplicates dropped.
        let mut dedup = p.series.clone();
        dedup.dedup();
        assert_eq!(dedup.len(), p.series.len());
        assert!(p.series.len() >= 5);
    }

    #[test]
    fn partial_and_light_themes_fall_back_per_key() {
        let p =
            Palette::from_colors_toml("mode = \"light\"\nbackground = \"#ffffff\"\nred = \"nope\"")
                .unwrap();
        assert!(!p.dark);
        assert_eq!(p.background, Rgb(255, 255, 255));
        let d = Palette::default();
        assert_eq!(p.crit, d.crit);
        assert_eq!(p.series, d.series);
        assert!(Palette::from_colors_toml("not = [toml").is_err());
    }

    #[test]
    fn non_ascii_colours_are_rejected_not_panics() {
        assert_eq!(Rgb::parse("#é1"), None);
        assert_eq!(Rgb::parse("aéééb"), None);
        assert_eq!(Rgb::parse("#+f+f+f"), None);
        assert!(Palette::from_colors_toml("accent = \"aéééb\"").is_ok());
    }

    #[test]
    fn readable_lifts_low_contrast_text() {
        // catppuccin-latte-like: pale yellow warn and grey dim on near-white.
        let p = Palette {
            dark: false,
            background: Rgb(0xef, 0xf1, 0xf5),
            panel: Rgb(0xe6, 0xe9, 0xef),
            raised: Rgb(0xdc, 0xe0, 0xe8),
            foreground: Rgb(0x4c, 0x4f, 0x69),
            warn: Rgb(0xdf, 0x8e, 0x1d),
            dim: Rgb(0xac, 0xb0, 0xbe),
            ..Palette::default()
        }
        .readable(4.5);
        for bg in [p.background, p.panel, p.raised] {
            assert!(contrast(p.warn, bg) >= 4.5, "{:?}", p.warn);
            assert!(contrast(p.dim, bg) >= 4.5, "{:?}", p.dim);
        }
        // Already-readable colours are untouched.
        let d = Palette::default();
        assert_eq!(d.clone().readable(3.0).accent, d.accent);
    }

    #[test]
    fn load_falls_back_with_error() {
        let (p, err) = load(Some(Path::new("/nonexistent/colors.toml")));
        assert_eq!(p, Palette::default());
        assert!(err.unwrap().contains("/nonexistent/colors.toml"));
        assert_eq!(load(None), (Palette::default(), None));
    }
}
