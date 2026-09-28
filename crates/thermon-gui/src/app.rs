//! The window: a health banner, a sensor tree with charts, and a process table.

use std::collections::{BTreeSet, HashMap};
use std::time::{Duration, Instant};

use eframe::egui::{self, Align, Color32, Layout, RichText, Ui};
use egui_extras::{Column, TableBuilder};
use egui_plot::{HoverPosition, Legend, Line, Plot, PlotPoints};
use thermon_core::control::{self, Signal};
use thermon_core::health::{self, Severity, Verdict};
use thermon_core::history::HistoryResponse;
use thermon_core::hwmon::{Category, Chip, SensorKind};
use thermon_core::processes::ProcessInfo;
use thermon_core::protocol::ProcessList;
use thermon_core::sampler::{Inventory, Snapshot};
use thermon_core::theme::{Palette, ThemeWatch};

use crate::data::{Data, HistoryRequest};
use crate::style;

const THEME_POLL: Duration = Duration::from_secs(2);
/// WCAG AA for body text; some themes' dim/warn colours fall well below it.
const MIN_CONTRAST: f64 = 4.5;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Sensors,
    Processes,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Range {
    FiveMin,
    Hour,
    Day,
}

impl Range {
    const ALL: [Range; 3] = [Range::FiveMin, Range::Hour, Range::Day];

    fn label(self) -> &'static str {
        match self {
            Range::FiveMin => "5 min",
            Range::Hour => "1 hour",
            Range::Day => "24 hours",
        }
    }

    fn arg(self) -> &'static str {
        match self {
            Range::FiveMin => "5m",
            Range::Hour => "1h",
            Range::Day => "24h",
        }
    }
}

/// Chart title, (series id, legend name) pairs, value scale, and a y value
/// to always include.
type ChartSpec<'a> = (&'a str, &'a [(String, String)], f64, Option<f64>);

#[derive(Clone, Copy, PartialEq, Eq)]
enum SortCol {
    Pid,
    Name,
    Cpu,
    Mem,
    Threads,
    Nice,
    User,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Action {
    End,
    Kill,
    /// Renice to this value.
    Lower(i32),
}

/// A process action the user has asked for and must confirm.
struct Pending {
    pid: u32,
    /// With `pid`, identifies the process that was shown; if the pid has
    /// been reused since, nothing is done.
    start_ticks: u64,
    name: String,
    action: Action,
}

/// Filtered, sorted row indices, rebuilt only when their inputs change (not
/// every frame: hovering the table repaints at the display's refresh rate).
struct RowCache {
    key: (u64, String, (SortCol, bool)),
    rows: Vec<usize>,
}

pub struct App {
    data: Data,
    palette: Palette,
    theme: ThemeWatch,
    theme_checked: Instant,
    tab: Tab,
    range: Range,
    /// Series drawn in the charts (sensor ids). Chosen in the sensor tree.
    selected: BTreeSet<String>,
    selection_seeded: bool,
    filter: String,
    sort: (SortCol, bool),
    selected_pid: Option<u32>,
    pending: Option<Pending>,
    message: Option<(String, bool)>,
    row_cache: Option<RowCache>,
    /// No fresh snapshot: values are drawn dimmed.
    stale: bool,
    users: HashMap<u32, String>,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, data: Data, processes: bool) -> App {
        let (theme, palette) = ThemeWatch::new();
        let palette = palette.readable(MIN_CONTRAST);
        style::apply(&cc.egui_ctx, &palette);
        App {
            data,
            palette,
            theme,
            theme_checked: Instant::now(),
            tab: if processes {
                Tab::Processes
            } else {
                Tab::Sensors
            },
            range: Range::FiveMin,
            selected: BTreeSet::new(),
            selection_seeded: false,
            filter: String::new(),
            sort: (SortCol::Cpu, true),
            selected_pid: None,
            pending: None,
            message: None,
            row_cache: None,
            stale: false,
            users: read_users(),
        }
    }

    fn poll_theme(&mut self, ctx: &egui::Context) {
        if self.theme_checked.elapsed() >= THEME_POLL {
            self.theme_checked = Instant::now();
            if let Some(p) = self.theme.poll() {
                let p = p.readable(MIN_CONTRAST);
                style::apply(ctx, &p);
                self.palette = p;
            }
        }
        // Keeps theme changes flowing even when no data arrives.
        ctx.request_repaint_after(THEME_POLL);
    }

    /// Start with the sensors people usually care about charted.
    fn seed_selection(&mut self, inv: &Inventory) {
        if self.selection_seeded || inv.chips.is_empty() {
            return;
        }
        self.selection_seeded = true;
        for chip in &inv.chips {
            let temps: Vec<_> = chip
                .sensors
                .iter()
                .filter(|s| s.kind == SensorKind::Temp)
                .collect();
            let pick = match chip.category {
                Category::Cpu => temps
                    .iter()
                    .find(|s| is_package(&s.label))
                    .or(temps.first()),
                Category::Gpu => temps
                    .iter()
                    .find(|s| s.label.eq_ignore_ascii_case("junction"))
                    .or(temps.first()),
                Category::Storage => temps.first(),
                _ => None,
            };
            if let Some(s) = pick {
                self.selected.insert(s.id.clone());
            }
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.poll_theme(&ctx);

        // Clone out of the lock so drawing never blocks the data threads.
        let (inv, snap, procs, history, connected, error, peak, errors) = {
            let s = self.data.lock();
            self.stale = s.stale();
            (
                s.inventory.clone(),
                s.snapshot.clone(),
                s.processes.clone(),
                s.history.clone(),
                s.connected,
                s.error.clone(),
                s.peak_freq_khz,
                (s.processes_error.clone(), s.history_error.clone()),
            )
        };
        if let Some(inv) = &inv {
            self.seed_selection(inv);
        }
        self.data.want_processes(self.tab == Tab::Processes);

        // History for charted sensors plus the load/memory series.
        let mut series: Vec<String> = self.selected.iter().cloned().collect();
        series.extend(["cpu.usage".to_string(), "mem.used_kib".to_string()]);
        if let Some(inv) = &inv {
            series.extend(inv.gpus.iter().map(|g| format!("gpu.{}.busy", g.id)));
        }
        self.data.request_history(HistoryRequest {
            series,
            range: self.range.arg().into(),
        });

        let verdict = match (&inv, &snap) {
            (Some(i), Some(s)) => Some(health::assess(
                i,
                s,
                // No process list, so the banner doesn't change with the tab;
                // zombies still come through thermond's alerts.
                &health::Context {
                    peak_freq_khz: peak,
                    processes: None,
                },
            )),
            _ => None,
        };

        egui::Panel::top("header").show(ui, |ui| {
            ui.add_space(6.0);
            self.header(ui, verdict.as_ref(), connected, error.as_deref());
            ui.add_space(6.0);
        });

        let (Some(inv), Some(snap)) = (inv, snap) else {
            egui::CentralPanel::default().show(ui, |ui| {
                ui.centered_and_justified(|ui| {
                    ui.label(
                        RichText::new(
                            "Waiting for thermond…\n\nStart it with: systemctl --user start thermond",
                        )
                        .color(style::c(self.palette.dim)),
                    );
                });
            });
            return;
        };

        match self.tab {
            Tab::Sensors => {
                egui::Panel::left("sensors")
                    .resizable(true)
                    .default_size(340.0)
                    .min_size(260.0)
                    .show(ui, |ui| {
                        egui::ScrollArea::vertical()
                            .show(ui, |ui| self.sensor_tree(ui, &inv, &snap));
                    });
                egui::CentralPanel::default().show(ui, |ui| {
                    if let Some(e) = &errors.1 {
                        ui.label(
                            RichText::new(format!("History: {e}"))
                                .color(style::c(self.palette.crit)),
                        );
                    }
                    self.charts(ui, &inv, history.as_ref())
                });
            }
            Tab::Processes => {
                egui::CentralPanel::default().show(ui, |ui| {
                    if let Some(e) = &errors.0 {
                        ui.label(
                            RichText::new(format!("Processes: {e}"))
                                .color(style::c(self.palette.crit)),
                        );
                    }
                    self.process_table(ui, procs.as_ref())
                });
            }
        }

        self.confirm_dialog(&ctx);
    }
}

// ---- header ----------------------------------------------------------------

impl App {
    fn header(
        &mut self,
        ui: &mut Ui,
        verdict: Option<&Verdict>,
        connected: bool,
        error: Option<&str>,
    ) {
        let p = self.palette.clone();
        ui.horizontal(|ui| {
            let (text, color) = match (connected, verdict) {
                (false, _) if self.stale => (
                    "Not connected · showing last known values".to_string(),
                    style::c(p.warn),
                ),
                (false, _) => ("Not connected".to_string(), style::c(p.dim)),
                (true, _) if self.stale => {
                    ("No fresh data from thermond".to_string(), style::c(p.warn))
                }
                (true, None) => ("Waiting for data".to_string(), style::c(p.dim)),
                (true, Some(v)) => (
                    match v.severity {
                        Severity::Ok | Severity::Info => "Healthy".to_string(),
                        Severity::Warn => format!(
                            "{} warning{}",
                            count(v, Severity::Warn),
                            plural(count(v, Severity::Warn))
                        ),
                        Severity::Crit => format!("{} critical", count(v, Severity::Crit)),
                    },
                    style::severity(
                        &p,
                        if v.severity == Severity::Info {
                            Severity::Ok
                        } else {
                            v.severity
                        },
                    ),
                ),
            };
            // The default font has no "●"; paint the dot.
            let (rect, _) = ui.allocate_exact_size(egui::vec2(12.0, 16.0), egui::Sense::hover());
            ui.painter().circle_filled(rect.center(), 5.0, color);
            ui.label(RichText::new(text).strong().size(16.0).color(color));

            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                for tab in [Tab::Processes, Tab::Sensors] {
                    let label = if tab == Tab::Sensors {
                        "Sensors"
                    } else {
                        "Processes"
                    };
                    ui.selectable_value(&mut self.tab, tab, label);
                }
                ui.separator();
                if self.tab == Tab::Sensors {
                    for r in Range::ALL.iter().rev() {
                        ui.selectable_value(&mut self.range, *r, r.label());
                    }
                }
            });
        });

        if !connected {
            if let Some(e) = error {
                ui.label(RichText::new(e).color(style::c(p.dim)));
            }
            return;
        }
        if let Some(v) = verdict {
            for f in &v.findings {
                ui.horizontal_wrapped(|ui| {
                    ui.label(
                        RichText::new(&f.title)
                            .strong()
                            .color(style::severity(&p, f.severity)),
                    );
                    ui.label(RichText::new(&f.detail).color(style::c(p.dim)));
                });
            }
        }
    }
}

fn count(v: &Verdict, s: Severity) -> usize {
    v.findings.iter().filter(|f| f.severity == s).count()
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

// ---- sensors ---------------------------------------------------------------

impl App {
    fn sensor_tree(&mut self, ui: &mut Ui, inv: &Inventory, snap: &Snapshot) {
        let p = self.palette.clone();
        let dim = style::c(p.dim);

        ui.add_space(4.0);
        egui::Grid::new("system").num_columns(2).spacing([12.0, 4.0]).show(ui, |ui| {
            ui.label(RichText::new("CPU").color(dim));
            let freq = snap.cpu.freq_khz.iter().flatten().max().map(|k| format!(" · {:.2} GHz", *k as f64 / 1e6));
            ui.label(format!("{}{}", pct(snap.cpu.usage), freq.unwrap_or_default()));
            ui.end_row();
            if let Some(m) = &snap.memory {
                ui.label(RichText::new("Memory").color(dim));
                ui.label(format!("{} / {}", gib(m.used_kib()), gib(m.total_kib)));
                ui.end_row();
                if m.swap_total_kib > 0 {
                    ui.label(RichText::new("Swap").color(dim));
                    ui.label(format!("{} / {}", gib(m.swap_used_kib()), gib(m.swap_total_kib)));
                    ui.end_row();
                }
            }
            let psi = |x: &Option<thermon_core::procfs::Pressure>| x.as_ref().map_or("—".into(), |x| format!("{:.1}%", x.some.avg10));
            ui.label(RichText::new("Pressure").color(dim)).on_hover_text(
                "Share of the last 10 s that tasks spent waiting for CPU, memory or IO (Linux PSI).",
            );
            ui.label(format!(
                "cpu {} · mem {} · io {}",
                psi(&snap.pressure.cpu),
                psi(&snap.pressure.memory),
                psi(&snap.pressure.io)
            ));
            ui.end_row();
        });
        ui.add_space(6.0);

        for chip in &inv.chips {
            ui.separator();
            ui.horizontal(|ui| {
                ui.label(RichText::new(inv.chip_title(chip)).strong());
                if let Some(meta) = gpu_meta(inv, chip, snap) {
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.label(RichText::new(meta).color(dim).small());
                    });
                }
            });
            for s in &chip.sensors {
                let value = snap.sensors.get(&s.id).copied();
                ui.horizontal(|ui| {
                    let mut on = self.selected.contains(&s.id);
                    if ui
                        .checkbox(&mut on, "")
                        .on_hover_text("Show in chart")
                        .changed()
                    {
                        if on {
                            self.selected.insert(s.id.clone());
                        } else {
                            self.selected.remove(&s.id);
                        }
                    }
                    ui.label(&s.label).on_hover_text(&s.id);
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let color = if self.stale {
                            style::c(p.dim)
                        } else {
                            style::level(&p, value, s.warn, s.crit)
                        };
                        ui.label(
                            RichText::new(format_value(s.kind, value))
                                .color(color)
                                .monospace(),
                        );
                    });
                });
            }
        }
        ui.add_space(8.0);
        ui.label(
            RichText::new("Labels, hidden sensors and thresholds: thermon config")
                .color(dim)
                .small(),
        );
    }

    fn charts(&mut self, ui: &mut Ui, inv: &Inventory, history: Option<&HistoryResponse>) {
        let Some(h) = history else {
            ui.label("Loading history…");
            return;
        };
        let p = self.palette.clone();
        let sensors: HashMap<&str, (&str, SensorKind)> = inv
            .chips
            .iter()
            .flat_map(|c| {
                c.sensors
                    .iter()
                    .map(move |s| (s.id.as_str(), (s.label.as_str(), s.kind)))
            })
            .collect();
        let chip_of: HashMap<&str, String> = inv
            .chips
            .iter()
            .flat_map(|c| {
                c.sensors
                    .iter()
                    .map(move |s| (s.id.as_str(), inv.chip_title(c)))
            })
            .collect();

        let mut temps = Vec::new();
        let mut fans = Vec::new();
        let mut power = Vec::new();
        for id in &self.selected {
            if let Some((label, kind)) = sensors.get(id.as_str()) {
                let name = format!(
                    "{} {}",
                    chip_of.get(id.as_str()).cloned().unwrap_or_default(),
                    label
                );
                match kind {
                    SensorKind::Temp => temps.push((id.clone(), name)),
                    SensorKind::Fan => fans.push((id.clone(), name)),
                    SensorKind::Power => power.push((id.clone(), name)),
                }
            }
        }
        let mut load = vec![("cpu.usage".to_string(), "CPU".to_string())];
        let kind = |g: &thermon_core::gpu::Gpu| match g.integrated {
            Some(true) => "Integrated GPU",
            _ => "GPU",
        };
        for g in &inv.gpus {
            // Two cards of the same kind would merge into one legend entry.
            let same = inv.gpus.iter().filter(|o| kind(o) == kind(g)).count();
            let name = if same > 1 {
                format!("{} {}", kind(g), g.id)
            } else {
                kind(g).to_string()
            };
            load.push((format!("gpu.{}.busy", g.id), name));
        }
        let memory = vec![("mem.used_kib".to_string(), "Used".to_string())];

        let mut charts: Vec<ChartSpec<'_>> = Vec::new();
        if !temps.is_empty() {
            charts.push(("Temperature °C", &temps, 1.0, None));
        }
        charts.push(("Load %", &load, 1.0, Some(100.0)));
        charts.push(("Memory GiB", &memory, 1.0 / (1024.0 * 1024.0), None));
        if !fans.is_empty() {
            charts.push(("Fans RPM", &fans, 1.0, None));
        }
        if !power.is_empty() {
            charts.push(("Power W", &power, 1.0, None));
        }

        let gap = 8.0;
        let each =
            ((ui.available_height() - gap * charts.len() as f32) / charts.len() as f32).max(120.0);
        egui::ScrollArea::vertical().show(ui, |ui| {
            for (i, (title, lines, scale, ymax)) in charts.iter().enumerate() {
                ui.label(RichText::new(*title).strong());
                self.chart(ui, i, h, lines, *scale, *ymax, each - 20.0, &p);
                ui.add_space(gap);
            }
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn chart(
        &self,
        ui: &mut Ui,
        index: usize,
        h: &HistoryResponse,
        lines: &[(String, String)],
        scale: f64,
        ymax: Option<f64>,
        height: f32,
        p: &Palette,
    ) {
        let step = h.interval_ms as f64 / 1000.0;
        // The newest point is `end_ts_ms` old, not "now" (coarse tiers flush
        // once a minute; a stalled daemon stops flushing at all).
        let age = (thermon_core::sampler::now_ms().saturating_sub(h.end_ts_ms)) as f64 / 1000.0;
        let mut plot = Plot::new(("chart", index))
            .height(height)
            .legend(Legend::default())
            .allow_scroll(false)
            .allow_zoom(false)
            .allow_drag(false)
            .show_background(false)
            .grid_color(style::c(p.selection))
            .include_y(0.0)
            .x_axis_formatter(|mark, _| ago(-mark.value))
            .label_formatter(|pos| match pos {
                HoverPosition::NearDataPoint {
                    plot_name,
                    position,
                    ..
                } if !plot_name.is_empty() => Some(format!(
                    "{plot_name}\n{:.1} · {}",
                    position.y,
                    ago(-position.x)
                )),
                _ => None,
            });
        if let Some(y) = ymax {
            plot = plot.include_y(y);
        }
        plot.show(ui, |pui| {
            for (i, (id, name)) in lines.iter().enumerate() {
                let Some(values) = h.series.get(id) else {
                    continue;
                };
                let n = values.len();
                let color = style::series(p, i);
                // Split at gaps so missing samples don't draw as a slope.
                let mut segment: Vec<[f64; 2]> = Vec::new();
                let flush = |seg: &mut Vec<[f64; 2]>, pui: &mut egui_plot::PlotUi| {
                    if !seg.is_empty() {
                        pui.line(
                            Line::new(name.clone(), PlotPoints::from(std::mem::take(seg)))
                                .color(color)
                                .width(1.5),
                        );
                    }
                };
                for (j, v) in values.iter().enumerate() {
                    match v {
                        Some(v) => {
                            segment.push([-((n - 1 - j) as f64) * step - age, *v as f64 * scale])
                        }
                        None => flush(&mut segment, pui),
                    }
                }
                flush(&mut segment, pui);
            }
        });
    }
}

// ---- processes -------------------------------------------------------------

impl App {
    fn process_table(&mut self, ui: &mut Ui, list: Option<&ProcessList>) {
        let p = self.palette.clone();
        let Some(list) = list else {
            ui.label(RichText::new("Scanning processes…").color(style::c(p.dim)));
            return;
        };
        let procs = &list.processes[..];

        let key = (list.ts_ms, self.filter.clone(), self.sort);
        if self.row_cache.as_ref().is_none_or(|c| c.key != key) {
            let rows = sorted_rows(procs, &self.filter, self.sort, &self.users);
            self.row_cache = Some(RowCache { key, rows });
        }
        let rows: Vec<&ProcessInfo> = self
            .row_cache
            .as_ref()
            .map(|c| c.rows.iter().map(|&i| &procs[i]).collect())
            .unwrap_or_default();
        let users = &self.users;
        let user = |uid: Option<u32>| {
            uid.map_or(String::new(), |u| {
                users.get(&u).cloned().unwrap_or_else(|| u.to_string())
            })
        };

        // Only act on a row the user can see.
        let selected = self
            .selected_pid
            .and_then(|pid| rows.iter().copied().find(|p| p.pid == pid));
        ui.horizontal(|ui| {
            ui.label("Filter");
            ui.add(
                egui::TextEdit::singleline(&mut self.filter)
                    .hint_text("name, command or pid")
                    .desired_width(240.0),
            );
            ui.label(
                RichText::new(if list.total > procs.len() {
                    format!(
                        "{} shown · {} busiest of {}",
                        rows.len(),
                        procs.len(),
                        list.total
                    )
                } else {
                    format!("{} of {}", rows.len(), list.total)
                })
                .color(style::c(p.dim)),
            );
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let enabled = selected.is_some();
                if ui
                    .add_enabled(enabled, egui::Button::new("Kill"))
                    .on_hover_text("SIGKILL: stops it immediately")
                    .clicked()
                {
                    let s = selected.unwrap();
                    self.pending = Some(Pending {
                        pid: s.pid,
                        start_ticks: s.start_ticks,
                        name: s.name.clone(),
                        action: Action::Kill,
                    });
                }
                if ui
                    .add_enabled(enabled, egui::Button::new("End"))
                    .on_hover_text("SIGTERM: asks it to quit")
                    .clicked()
                {
                    let s = selected.unwrap();
                    self.pending = Some(Pending {
                        pid: s.pid,
                        start_ticks: s.start_ticks,
                        name: s.name.clone(),
                        action: Action::End,
                    });
                }
                if ui
                    .add_enabled(
                        enabled && selected.is_some_and(|s| s.nice < 19),
                        egui::Button::new("Lower priority"),
                    )
                    .on_hover_text("Raise its nice value by 5. Only root can undo this.")
                    .clicked()
                {
                    let s = selected.unwrap();
                    self.pending = Some(Pending {
                        pid: s.pid,
                        start_ticks: s.start_ticks,
                        name: s.name.clone(),
                        action: Action::Lower((s.nice + 5).min(19)),
                    });
                }
            });
        });
        if let Some((msg, ok)) = &self.message {
            let color = if *ok {
                style::c(p.dim)
            } else {
                style::c(p.crit)
            };
            ui.label(RichText::new(msg).color(color));
        }
        ui.add_space(4.0);

        let header = |ui: &mut Ui, sort: &mut (SortCol, bool), col: SortCol, label: &str| {
            let arrow = if sort.0 == col {
                if sort.1 { " ⏷" } else { " ⏶" }
            } else {
                ""
            };
            if ui
                .selectable_label(sort.0 == col, format!("{label}{arrow}"))
                .clicked()
            {
                // Numbers default to biggest first, text to A→Z.
                *sort = if sort.0 == col {
                    (col, !sort.1)
                } else {
                    (
                        col,
                        !matches!(col, SortCol::Name | SortCol::User | SortCol::Pid),
                    )
                };
            }
        };
        let accent_fill = Color32::from_rgba_unmultiplied(p.accent.0, p.accent.1, p.accent.2, 40);
        TableBuilder::new(ui)
            .striped(true)
            .sense(egui::Sense::click())
            .column(Column::exact(70.0))
            .column(Column::initial(150.0).resizable(true))
            .column(Column::exact(88.0))
            .column(Column::exact(80.0))
            .column(Column::exact(60.0))
            .column(Column::exact(44.0))
            .column(Column::initial(90.0).resizable(true))
            .column(Column::remainder().clip(true))
            .header(22.0, |mut row| {
                let sort = &mut self.sort;
                row.col(|ui| header(ui, sort, SortCol::Pid, "PID"));
                row.col(|ui| header(ui, sort, SortCol::Name, "Name"));
                row.col(|ui| {
                    header(ui, sort, SortCol::Cpu, "CPU %/core");
                    ui.response().on_hover_text(
                        "100% is one full core, so busy processes can go above 100%",
                    );
                });
                row.col(|ui| header(ui, sort, SortCol::Mem, "Memory"));
                row.col(|ui| header(ui, sort, SortCol::Threads, "Thr"));
                row.col(|ui| header(ui, sort, SortCol::Nice, "Ni"));
                row.col(|ui| header(ui, sort, SortCol::User, "User"));
                row.col(|ui| {
                    ui.label(RichText::new("Command").strong());
                });
            })
            .body(|body| {
                body.rows(20.0, rows.len(), |mut row| {
                    let pr = rows[row.index()];
                    let is_sel = self.selected_pid == Some(pr.pid);
                    row.set_selected(is_sel);
                    let cells = [
                        pr.pid.to_string(),
                        pr.name.clone(),
                        format!("{:.1}", pr.cpu_percent),
                        kib(pr.rss_kib),
                        pr.threads.to_string(),
                        pr.nice.to_string(),
                        user(pr.uid),
                        if pr.cmdline.is_empty() {
                            format!("[{}]", pr.name)
                        } else {
                            pr.cmdline.clone()
                        },
                    ];
                    let mut clicked = false;
                    for (i, text) in cells.iter().enumerate() {
                        let (_, resp) = row.col(|ui| {
                            if is_sel {
                                ui.painter().rect_filled(ui.max_rect(), 0.0, accent_fill);
                            }
                            let t = RichText::new(text);
                            let t = if (2..=5).contains(&i) {
                                t.monospace()
                            } else {
                                t
                            };
                            ui.add(egui::Label::new(t).truncate().selectable(false));
                        });
                        clicked |= resp.clicked();
                    }
                    if clicked {
                        self.selected_pid = if is_sel { None } else { Some(pr.pid) };
                    }
                });
            });
    }

    fn confirm_dialog(&mut self, ctx: &egui::Context) {
        let Some(pending) = &self.pending else { return };
        let (button, heading, explain) = match pending.action {
            Action::End => (
                "End",
                format!("End {}?", pending.name),
                "SIGTERM asks it to quit; it may save or clean up first.".to_string(),
            ),
            Action::Kill => (
                "Kill",
                format!("Kill {}?", pending.name),
                "SIGKILL can't be caught: unsaved work in it is lost.".to_string(),
            ),
            Action::Lower(n) => (
                "Lower",
                format!("Lower the priority of {}?", pending.name),
                format!("Its nice value becomes {n}. Only root can raise it again."),
            ),
        };
        let mut close = false;
        let mut confirm = false;
        egui::Modal::new(egui::Id::new("confirm-action")).show(ctx, |ui| {
            ui.set_width(340.0);
            ui.heading(heading);
            ui.label(format!("PID {}. {explain}", pending.pid));
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button(button).clicked() {
                    confirm = true;
                }
                if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    close = true;
                }
            });
        });
        if confirm {
            let (pid, name, start) = (pending.pid, pending.name.clone(), Some(pending.start_ticks));
            let sent = |sig: Signal| {
                control::signal(pid, start, sig)
                    .map(|()| format!("Sent {} to {name} ({pid})", sig.name()))
            };
            let result = match pending.action {
                Action::End => sent(Signal::Term),
                Action::Kill => sent(Signal::Kill),
                Action::Lower(n) => control::renice(pid, start, n).map(|r| match r.failed {
                    0 => format!(
                        "{name} ({pid}) now runs at nice {n} ({} threads)",
                        r.changed
                    ),
                    f => format!(
                        "{name} ({pid}): {} threads at nice {n}, {f} unchanged ({})",
                        r.changed,
                        r.error.unwrap_or_default()
                    ),
                }),
            };
            self.message = Some(match result {
                Ok(msg) => (msg, true),
                Err(e) => (format!("Could not change {name} ({pid}): {e}"), false),
            });
            close = true;
        }
        if close {
            self.pending = None;
        }
    }
}

/// Indices of `procs` matching `filter`, in `sort` order.
fn sorted_rows(
    procs: &[ProcessInfo],
    filter: &str,
    (col, desc): (SortCol, bool),
    users: &HashMap<u32, String>,
) -> Vec<usize> {
    let filter = filter.to_lowercase();
    let mut rows: Vec<usize> = (0..procs.len())
        .filter(|&i| {
            let pr = &procs[i];
            filter.is_empty()
                || pr.name.to_lowercase().contains(&filter)
                || pr.cmdline.to_lowercase().contains(&filter)
                || pr.pid.to_string() == filter
        })
        .collect();
    let user = |uid: Option<u32>| uid.and_then(|u| users.get(&u).cloned()).unwrap_or_default();
    // Text keys computed once per process, not twice per comparison.
    let keys: Vec<String> = match col {
        SortCol::Name => procs.iter().map(|p| p.name.to_lowercase()).collect(),
        SortCol::User => procs.iter().map(|p| user(p.uid)).collect(),
        _ => Vec::new(),
    };
    rows.sort_by(|&ia, &ib| {
        let (a, b) = (&procs[ia], &procs[ib]);
        let o = match col {
            SortCol::Pid => a.pid.cmp(&b.pid),
            SortCol::Name | SortCol::User => keys[ia].cmp(&keys[ib]),
            SortCol::Cpu => a.cpu_percent.total_cmp(&b.cpu_percent),
            SortCol::Mem => a.rss_kib.cmp(&b.rss_kib),
            SortCol::Threads => a.threads.cmp(&b.threads),
            SortCol::Nice => a.nice.cmp(&b.nice),
        };
        if desc { o.reverse() } else { o }
    });
    rows
}

/// uid -> name from /etc/passwd (enough for local users; others show the uid).
fn read_users() -> HashMap<u32, String> {
    std::fs::read_to_string("/etc/passwd")
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            let mut f = l.split(':');
            let name = f.next()?;
            let uid = f.nth(1)?.parse().ok()?;
            Some((uid, name.to_string()))
        })
        .collect()
}

// ---- formatting ------------------------------------------------------------

fn is_package(label: &str) -> bool {
    let l = label.to_lowercase();
    l.starts_with("tctl") || l.starts_with("tdie") || l.starts_with("package")
}

fn gpu_meta(inv: &Inventory, chip: &Chip, snap: &Snapshot) -> Option<String> {
    let gpu = inv.gpu_for(chip)?;
    let st = snap.gpus.get(&gpu.id)?;
    let mut parts = Vec::new();
    if let Some(b) = st.busy_percent {
        parts.push(format!("{b}% busy"));
    }
    if let (Some(u), Some(t)) = (st.vram_used_bytes, st.vram_total_bytes) {
        parts.push(format!("{} / {}", gib(u / 1024), gib(t / 1024)));
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}

fn format_value(kind: SensorKind, v: Option<f64>) -> String {
    match v {
        None => "—".into(),
        Some(v) => match kind {
            SensorKind::Temp => format!("{v:.1} °C"),
            SensorKind::Fan => format!("{v:.0} rpm"),
            SensorKind::Power => format!("{v:.1} W"),
        },
    }
}

fn pct(v: Option<f32>) -> String {
    v.map_or("—".into(), |v| format!("{v:.0}%"))
}

fn gib(kib: u64) -> String {
    format!("{:.1} GiB", kib as f64 / (1024.0 * 1024.0))
}

fn kib(kib: u64) -> String {
    if kib >= 1024 * 1024 {
        format!("{:.1} GiB", kib as f64 / (1024.0 * 1024.0))
    } else if kib >= 1024 {
        format!("{:.0} MiB", kib as f64 / 1024.0)
    } else {
        format!("{kib} KiB")
    }
}

/// Seconds-ago as axis text: "now", "45s", "1m40s", "5m", "2h", "1h30m".
fn ago(secs: f64) -> String {
    let s = secs.round() as i64;
    let (h, m, sec) = (s / 3600, (s % 3600) / 60, s % 60);
    match () {
        _ if s <= 0 => "now".into(),
        _ if s < 60 => format!("{s}s"),
        _ if s < 3600 && sec == 0 => format!("{m}m"),
        _ if s < 3600 => format!("{m}m{sec:02}s"),
        _ if m == 0 => format!("{h}h"),
        _ => format!("{h}h{m:02}m"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_filter_and_sort() {
        let pr = |pid: u32, name: &str, cpu: f32| ProcessInfo {
            pid,
            ppid: 1,
            name: name.into(),
            cmdline: format!("/usr/bin/{name}"),
            state: 'S',
            uid: Some(1000),
            threads: 1,
            nice: 0,
            rss_kib: 0,
            cpu_percent: cpu,
            start_ticks: 0,
        };
        let procs = [
            pr(10, "zsh", 1.0),
            pr(11, "Alpha", 5.0),
            pr(12, "beta", 3.0),
        ];
        let users = HashMap::new();
        let by = |filter: &str, sort| -> Vec<u32> {
            sorted_rows(&procs, filter, sort, &users)
                .into_iter()
                .map(|i| procs[i].pid)
                .collect()
        };
        assert_eq!(by("", (SortCol::Cpu, true)), [11, 12, 10]);
        assert_eq!(by("", (SortCol::Name, false)), [11, 12, 10]);
        assert_eq!(by("bin/b", (SortCol::Pid, false)), [12]);
        assert_eq!(by("10", (SortCol::Pid, false)), [10]);
    }

    #[test]
    fn ago_labels() {
        assert_eq!(ago(0.0), "now");
        assert_eq!(ago(-3.0), "now");
        assert_eq!(ago(45.0), "45s");
        assert_eq!(ago(100.0), "1m40s");
        assert_eq!(ago(300.0), "5m");
        assert_eq!(ago(7200.0), "2h");
        assert_eq!(ago(5400.0), "1h30m");
    }

    #[test]
    fn units() {
        assert_eq!(kib(512), "512 KiB");
        assert_eq!(kib(2048), "2 MiB");
        assert_eq!(format_value(SensorKind::Fan, Some(1234.4)), "1234 rpm");
        assert_eq!(format_value(SensorKind::Temp, None), "—");
        assert!(is_package("Package id 0"));
        assert!(!is_package("Tccd1"));
    }
}
