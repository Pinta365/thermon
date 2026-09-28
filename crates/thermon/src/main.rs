//! `thermon` command line client.

mod edit;
mod render;

use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::thread;
use std::time::Duration;

use serde::Serialize;
use thermon_core::config::Config;
use thermon_core::control::{self, Signal};
use thermon_core::health::{self, Context, Verdict};
use thermon_core::protocol::{Body, Command, ProcessQuery, SortKey, Topic};
use thermon_core::sampler::{Inventory, Sampler, Snapshot};

use thermon_core::client::{self, Client};

const USAGE: &str = "\
usage: thermon <command> [options]

commands:
  status                 current readings from thermond
  top                    processes by CPU (or --sort mem|pid|name)
  history [SERIES...]    sparklines and stats; all series if none given
  alerts                 alerts firing now and the recent alert log
  kill PID               end a process (--start-ticks N guards against pid reuse)
  watch                  stream thermond updates as JSON lines
  dump                   read sensors directly, without thermond
  config                 check the config file and list sensor ids for it
  config hide ID         hide a sensor in the config file
  config unhide ID       show a hidden sensor in the config file

options:
  --json                 JSON output (status, top, history, alerts, dump, config hide|unhide)
  --socket PATH          thermond socket (default $XDG_RUNTIME_DIR/thermon.sock)
  --range R              history range: 90s, 15m, 1h, 24h (default 5m)
  --sort K               top/watch process sort: cpu, mem, pid, name
  --limit N              top/watch process count (default 20)
  --interval-ms N        watch: minimum spacing between snapshots
  --processes            watch: include process lists
  --alerts               watch: include alert events
  --no-snapshots         watch: leave out snapshots (e.g. with --processes)
  --root PATH            dump/config: read from PATH instead of / (e.g. a fixture)
  --config PATH          dump/config: config file (default ~/.config/thermon/config.toml);
                         `none` for built-in defaults
  --start-ticks N        kill: only if the process started at N (from top --json)
  --signal S             kill: term (default) or kill";

#[derive(Default)]
struct Opts {
    json: bool,
    socket: Option<PathBuf>,
    root: Option<PathBuf>,
    config: Option<PathBuf>,
    range: Option<String>,
    sort: Option<SortKey>,
    limit: Option<usize>,
    interval_ms: Option<u64>,
    processes: bool,
    alerts: bool,
    no_snapshots: bool,
    start_ticks: Option<u64>,
    signal: Option<Signal>,
    positional: Vec<String>,
}

fn main() -> ExitCode {
    // Exit quietly on a closed pipe (`thermon status | head`) instead of
    // panicking in println!.
    // SAFETY: called before any other thread exists.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    let mut args = std::env::args().skip(1);
    let Some(command) = args.next() else {
        return usage_error("missing command");
    };
    if matches!(command.as_str(), "-h" | "--help" | "help") {
        println!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    if matches!(command.as_str(), "-V" | "--version") {
        println!("thermon {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }
    let opts = match parse_opts(args) {
        Ok(o) => o,
        Err(msg) => return usage_error(&msg),
    };

    if !matches!(command.as_str(), "history" | "kill" | "config")
        && let Some(extra) = opts.positional.first()
    {
        return usage_error(&format!("unexpected argument {extra:?}"));
    }
    if command == "config"
        && !opts.positional.is_empty()
        && !matches!(
            opts.positional.as_slice(),
            [action, _] if matches!(action.as_str(), "hide" | "unhide")
        )
    {
        return usage_error("config needs no arguments, or `hide ID` / `unhide ID`");
    }

    let result = match command.as_str() {
        "status" => status(&opts),
        "top" => top(&opts),
        "history" => history(&opts),
        "watch" => watch(&opts),
        "alerts" => alerts(&opts),
        "dump" => dump(&opts),
        "config" => config(&opts),
        "kill" => kill(&opts),
        other => return usage_error(&format!("unknown command {other:?}")),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(msg) => {
            eprintln!("thermon: {msg}");
            ExitCode::FAILURE
        }
    }
}

fn usage_error(msg: &str) -> ExitCode {
    eprintln!("thermon: {msg}\n\n{USAGE}");
    ExitCode::from(2)
}

fn parse_opts(mut args: impl Iterator<Item = String>) -> Result<Opts, String> {
    let mut o = Opts::default();
    while let Some(arg) = args.next() {
        let mut value = |name: &str| args.next().ok_or(format!("{name} needs a value"));
        match arg.as_str() {
            "--json" => o.json = true,
            "--processes" => o.processes = true,
            "--alerts" => o.alerts = true,
            "--no-snapshots" => o.no_snapshots = true,
            "--socket" => o.socket = Some(value("--socket")?.into()),
            "--root" => o.root = Some(value("--root")?.into()),
            "--config" => o.config = Some(value("--config")?.into()),
            "--range" => o.range = Some(value("--range")?),
            "--sort" => {
                o.sort = Some(match value("--sort")?.as_str() {
                    "cpu" => SortKey::Cpu,
                    "mem" => SortKey::Mem,
                    "pid" => SortKey::Pid,
                    "name" => SortKey::Name,
                    s => return Err(format!("unknown sort {s:?}; use cpu, mem, pid or name")),
                })
            }
            "--limit" => {
                o.limit = Some(
                    value("--limit")?
                        .parse()
                        .ok()
                        .filter(|n| *n > 0)
                        .ok_or("--limit needs a number above 0")?,
                )
            }
            "--start-ticks" => {
                o.start_ticks = Some(
                    value("--start-ticks")?
                        .parse()
                        .map_err(|_| "--start-ticks needs a number")?,
                )
            }
            "--signal" => {
                o.signal = Some(match value("--signal")?.as_str() {
                    "term" | "TERM" | "SIGTERM" => Signal::Term,
                    "kill" | "KILL" | "SIGKILL" => Signal::Kill,
                    s => return Err(format!("unknown signal {s:?}; use term or kill")),
                })
            }
            "--interval-ms" => {
                o.interval_ms = Some(
                    value("--interval-ms")?
                        .parse()
                        .map_err(|_| "--interval-ms needs a number")?,
                )
            }
            s if s.starts_with('-') => return Err(format!("unknown option {s:?}")),
            _ => o.positional.push(arg),
        }
    }
    Ok(o)
}

impl Opts {
    fn connect(&self) -> Result<Client, String> {
        Client::connect(&client::socket_path(self.socket.clone())?)
    }

    fn process_query(&self) -> ProcessQuery {
        let d = ProcessQuery::default();
        ProcessQuery {
            sort: self.sort.unwrap_or(d.sort),
            limit: self.limit.unwrap_or(d.limit),
        }
    }
}

fn print_json(v: &impl Serialize) -> Result<(), String> {
    println!(
        "{}",
        serde_json::to_string_pretty(v).map_err(|e| e.to_string())?
    );
    Ok(())
}

#[derive(Serialize)]
struct StatusJson<'a> {
    inventory: &'a Inventory,
    snapshot: &'a Snapshot,
    health: &'a Verdict,
}

fn inventory_and_snapshot(c: &mut Client) -> Result<(Inventory, Snapshot), String> {
    let Body::Inventory { data: inv } = c.request(Command::Inventory)? else {
        return Err("unexpected reply to inventory".into());
    };
    let Body::Snapshot { data: snap } = c.request(Command::Snapshot)? else {
        return Err("unexpected reply to snapshot".into());
    };
    Ok((inv, snap))
}

fn status(o: &Opts) -> Result<(), String> {
    let (inv, snap) = inventory_and_snapshot(&mut o.connect()?)?;
    let health = health::assess(
        &inv,
        &snap,
        &Context {
            peak_freq_khz: None,
            processes: None,
        },
    );
    if o.json {
        print_json(&StatusJson {
            inventory: &inv,
            snapshot: &snap,
            health: &health,
        })
    } else {
        print!("{}", render::health(&health));
        render::status(&inv, &snap);
        Ok(())
    }
}

fn top(o: &Opts) -> Result<(), String> {
    let Body::Processes { data } = o
        .connect()?
        .request(Command::Processes(o.process_query()))?
    else {
        return Err("unexpected reply to processes".into());
    };
    if o.json {
        print_json(&data)
    } else {
        render::processes(&data);
        Ok(())
    }
}

/// End a process. With `--start-ticks`, only if the pid still belongs to the
/// process that started then.
fn kill(o: &Opts) -> Result<(), String> {
    let [pid] = o.positional.as_slice() else {
        return Err("kill needs exactly one pid".into());
    };
    let pid: u32 = pid.parse().map_err(|_| format!("not a pid: {pid:?}"))?;
    let sig = o.signal.unwrap_or(Signal::Term);
    control::signal(pid, o.start_ticks, sig).map_err(|e| e.to_string())?;
    if !o.json {
        println!("sent {} to {pid}", sig.name());
    }
    Ok(())
}

fn alerts(o: &Opts) -> Result<(), String> {
    let Body::Alerts { data } = o.connect()?.request(Command::Alerts)? else {
        return Err("unexpected reply to alerts".into());
    };
    if o.json {
        print_json(&data)
    } else {
        print!("{}", render::alerts(&data));
        Ok(())
    }
}

fn history(o: &Opts) -> Result<(), String> {
    let mut c = o.connect()?;
    let Body::Inventory { data: inv } = c.request(Command::Inventory)? else {
        return Err("unexpected reply to inventory".into());
    };
    let cmd = Command::History {
        series: o.positional.clone(),
        range: o.range.clone().unwrap_or_else(|| "5m".into()),
    };
    let Body::History { data } = c.request(cmd)? else {
        return Err("unexpected reply to history".into());
    };
    // JSON consumers (the bar) ask for series that may not have data yet;
    // they get what exists. People typing names get told about typos.
    if !o.json
        && let Some(missing) = o.positional.iter().find(|s| !data.series.contains_key(*s))
    {
        return Err(format!(
            "no series {missing:?}; `thermon history` lists them all"
        ));
    }
    if o.json {
        print_json(&data)
    } else {
        render::history(&inv, &data);
        Ok(())
    }
}

/// Stream daemon messages as JSON lines until the daemon goes away. This is
/// what the omarchy-shell plugin runs; it restarts us when we exit.
fn watch(o: &Opts) -> Result<(), String> {
    let mut c = o.connect()?;
    let mut topics = Vec::new();
    if !o.no_snapshots {
        topics.push(Topic::Snapshot);
    }
    if o.processes {
        topics.push(Topic::Processes);
    }
    if o.alerts {
        topics.push(Topic::Alerts);
    }
    if topics.is_empty() {
        return Err("--no-snapshots needs --processes or --alerts".into());
    }
    c.send(Command::Subscribe {
        topics,
        interval_ms: o.interval_ms,
        processes: o.process_query(),
    })
    .map_err(|e| format!("send: {e}"))?;

    let stdout = io::stdout();
    while let Some(line) = c.read_line().map_err(|e| format!("read: {e}"))? {
        let mut out = stdout.lock();
        // A closed stdout normally ends us via SIGPIPE (see main); this
        // covers other write errors.
        if writeln!(out, "{line}").and_then(|()| out.flush()).is_err() {
            return Ok(());
        }
    }
    Err("thermond closed the connection".into())
}

impl Opts {
    /// `--config none` means built-in defaults, as for thermond.
    fn config_path(&self) -> Option<PathBuf> {
        match &self.config {
            Some(p) if p.as_os_str() == "none" => None,
            Some(p) => Some(p.clone()),
            None => Config::default_path(),
        }
    }

    fn sampler(&self) -> Result<Sampler, String> {
        let config = match self.config_path() {
            Some(p) => Config::load(&p)?,
            None => Config::default(),
        };
        let root = self.root.clone().unwrap_or_else(|| PathBuf::from("/"));
        Sampler::new(&root, config).map_err(|e| format!("{}: {e}", root.display()))
    }
}

/// Validate the config against the sensors present and list the sensor ids
/// it can refer to. Hidden sensors are listed too, so they can be un-hidden.
fn config(o: &Opts) -> Result<(), String> {
    if let [action, id] = o.positional.as_slice() {
        return edit_config(o, action, id);
    }
    let path = o.config_path();
    let configured = o.sampler()?;
    match &path {
        Some(p) if p.exists() => println!("{}: ok", p.display()),
        Some(p) => println!("{}: not found, using defaults", p.display()),
        None if o.config.is_some() => println!("--config none: using built-in defaults"),
        None => println!("no config path (HOME is unset), using defaults"),
    }
    for key in configured.unmatched() {
        println!("  warning: [sensors.{key:?}] matches no sensor");
    }

    // The same config without sensor entries shows every sensor.
    let bare = Config {
        sensors: Default::default(),
        ..configured.config().clone()
    };
    let root = o.root.clone().unwrap_or_else(|| PathBuf::from("/"));
    let everything = Sampler::new(&root, bare).map_err(|e| format!("{}: {e}", root.display()))?;

    println!("\nsensor ids (use as [sensors.\"<id>\"]; `*` wildcards allowed):");
    for s in everything.inventory().chips.iter().flat_map(|c| &c.sensors) {
        let applied = configured
            .inventory()
            .chips
            .iter()
            .flat_map(|c| &c.sensors)
            .find(|x| x.id == s.id);
        let detail = match applied {
            None => "hidden".to_string(),
            Some(x) => {
                let mut d = format!("label {:?}", x.label);
                if let Some(w) = x.warn {
                    d += &format!(", warn {w:.0}");
                }
                if let Some(c) = x.crit {
                    d += &format!(", crit {c:.0}");
                }
                d
            }
        };
        println!("  {:<40} {detail}", s.id);
    }
    Ok(())
}

#[derive(Serialize)]
struct ConfigEditJson {
    ok: bool,
    changed: bool,
}

fn edit_config(o: &Opts, action: &str, id: &str) -> Result<(), String> {
    let path = o
        .config_path()
        .ok_or_else(|| "no config file to edit".to_string())?;
    let outcome = match action {
        "hide" => edit::hide(&path, id)?,
        "unhide" => edit::unhide(&path, id)?,
        _ => unreachable!("validated config edit action"),
    };
    if o.json {
        println!(
            "{}",
            serde_json::to_string(&ConfigEditJson {
                ok: true,
                changed: outcome.changed,
            })
            .map_err(|error| error.to_string())?
        );
        Ok(())
    } else {
        println!("{}", outcome.message(id));
        Ok(())
    }
}

/// Sample locally: the same inventory + snapshot `status` shows, without a daemon.
fn dump(o: &Opts) -> Result<(), String> {
    let mut sampler = o.sampler()?;
    sampler.sample();
    // CPU usage needs two samples.
    thread::sleep(Duration::from_millis(250));
    let snap = sampler.sample();
    let inv = sampler.inventory();
    let health = health::assess(
        inv,
        &snap,
        &Context {
            peak_freq_khz: None,
            processes: None,
        },
    );
    if o.json {
        print_json(&StatusJson {
            inventory: inv,
            snapshot: &snap,
            health: &health,
        })
    } else {
        print!("{}", render::health(&health));
        render::status(inv, &snap);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_edit_rejects_config_none() {
        let options = Opts {
            config: Some("none".into()),
            ..Opts::default()
        };
        assert_eq!(
            edit_config(&options, "hide", "acpitz/temp1").unwrap_err(),
            "no config file to edit"
        );
    }
}
