//! `thermon-gui`: a desktop window for thermond.

mod app;
mod data;
mod style;

use std::path::PathBuf;
use std::process::ExitCode;

use eframe::egui;
use thermon_core::client::socket_path;

const USAGE: &str = "\
usage: thermon-gui [--socket PATH] [--tab sensors|processes]

  --socket  thermond socket (default $XDG_RUNTIME_DIR/thermon.sock)
  --tab     view to open on (default sensors)

Colours follow the current Omarchy theme; set THERMON_THEME to a
colors.toml to use another palette.";

fn main() -> ExitCode {
    let mut socket = None;
    let mut processes = false;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--socket" => match args.next() {
                Some(p) => socket = Some(PathBuf::from(p)),
                None => return usage("--socket needs a path"),
            },
            "--tab" => match args.next().as_deref() {
                Some("sensors") => processes = false,
                Some("processes") => processes = true,
                _ => return usage("--tab needs sensors or processes"),
            },
            "-h" | "--help" => {
                println!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            "-V" | "--version" => {
                println!("thermon-gui {}", env!("CARGO_PKG_VERSION"));
                return ExitCode::SUCCESS;
            }
            other => return usage(&format!("unexpected argument {other:?}")),
        }
    }
    let socket = match socket_path(socket) {
        Ok(s) => s,
        Err(e) => return usage(&e),
    };

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Thermon")
            // Hyprland window rules match on this.
            .with_app_id("thermon")
            .with_inner_size([1100.0, 720.0])
            .with_min_inner_size([640.0, 400.0]),
        ..Default::default()
    };
    let result = eframe::run_native(
        "thermon",
        options,
        Box::new(move |cc| {
            let data = data::Data::start(cc.egui_ctx.clone(), socket);
            Ok(Box::new(app::App::new(cc, data, processes)))
        }),
    );
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("thermon-gui: {e}");
            ExitCode::FAILURE
        }
    }
}

fn usage(msg: &str) -> ExitCode {
    eprintln!("thermon-gui: {msg}\n\n{USAGE}");
    ExitCode::from(2)
}
