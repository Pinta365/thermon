//! Maps a [`Palette`] onto egui's visuals.

use eframe::egui::{self, Color32, Stroke, Visuals};
use thermon_core::health::Severity;
use thermon_core::theme::{Palette, Rgb};

pub fn c(rgb: Rgb) -> Color32 {
    Color32::from_rgb(rgb.0, rgb.1, rgb.2)
}

pub fn apply(ctx: &egui::Context, p: &Palette) {
    let mut v = if p.dark {
        Visuals::dark()
    } else {
        Visuals::light()
    };
    let fg = c(p.foreground);

    v.override_text_color = Some(fg);
    v.panel_fill = c(p.panel);
    v.window_fill = c(p.background);
    v.extreme_bg_color = c(p.panel);
    v.faint_bg_color = c(p.raised);
    v.code_bg_color = c(p.raised);
    v.hyperlink_color = c(p.accent);
    v.warn_fg_color = c(p.warn);
    v.error_fg_color = c(p.crit);
    v.selection.bg_fill = c(p.selection);
    v.selection.stroke = Stroke::new(1.0, c(p.accent));
    v.window_stroke = Stroke::new(1.0, c(p.selection));

    let w = &mut v.widgets;
    w.noninteractive.bg_fill = c(p.background);
    w.noninteractive.weak_bg_fill = c(p.background);
    w.noninteractive.bg_stroke = Stroke::new(1.0, c(p.selection));
    w.noninteractive.fg_stroke = Stroke::new(1.0, fg);
    for (state, fill) in [
        (&mut w.inactive, c(p.raised)),
        (&mut w.hovered, c(p.selection)),
        (&mut w.active, c(p.selection)),
        (&mut w.open, c(p.raised)),
    ] {
        state.bg_fill = fill;
        state.weak_bg_fill = fill;
        state.fg_stroke = Stroke::new(1.0, fg);
    }
    w.hovered.bg_stroke = Stroke::new(1.0, c(p.accent));
    w.active.bg_stroke = Stroke::new(1.0, c(p.accent));

    ctx.set_visuals(v);
}

pub fn severity(p: &Palette, s: Severity) -> Color32 {
    match s {
        Severity::Ok => c(p.ok),
        Severity::Info => c(p.accent),
        Severity::Warn => c(p.warn),
        Severity::Crit => c(p.crit),
    }
}

/// Colour for a sensor value against its thresholds.
pub fn level(p: &Palette, value: Option<f64>, warn: Option<f64>, crit: Option<f64>) -> Color32 {
    match value {
        Some(v) if crit.is_some_and(|c| v >= c) => c(p.crit),
        Some(v) if warn.is_some_and(|w| v >= w) => c(p.warn),
        _ => c(p.foreground),
    }
}

pub fn series(p: &Palette, i: usize) -> Color32 {
    c(p.series[i % p.series.len().max(1)])
}
