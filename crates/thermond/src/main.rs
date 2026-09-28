//! `thermond`: samples hardware sensors and system metrics, keeps history, and
//! serves both over `$XDG_RUNTIME_DIR/thermon.sock` (see `thermon_core::protocol`).

mod alerting;
mod client;
mod state;

use std::fs;
use std::io;
use std::io::Write;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

use thermon_core::config::Config;
use thermon_core::protocol::default_socket_path;
use thermon_core::sampler::Sampler;

use crate::state::{ConfigWatch, Shared};

const USAGE: &str = "\
usage: thermond [--socket PATH] [--config PATH|none] [--interval-ms N] [--root PATH]

  --socket       socket path (default $XDG_RUNTIME_DIR/thermon.sock)
  --config       config file (default ~/.config/thermon/config.toml),
                 reloaded when it changes; `none` uses built-in defaults
  --interval-ms  sample interval, 200..=60000 (default: config, else 1000)
  --root         read /sys and /proc under PATH instead of / (testing)";

/// Each client costs a thread; a bar, a GUI and a few CLI calls is the norm.
const MAX_CLIENTS: usize = 32;

struct Args {
    socket: PathBuf,
    config: Option<PathBuf>,
    interval: Option<Duration>,
    root: PathBuf,
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(Some(a)) => a,
        Ok(None) => return ExitCode::SUCCESS,
        Err(msg) => {
            eprintln!("thermond: {msg}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    match run(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("thermond: {e}");
            ExitCode::FAILURE
        }
    }
}

fn parse_args() -> Result<Option<Args>, String> {
    let mut socket = None;
    let mut interval = None;
    let mut config = Config::default_path();
    let mut root = PathBuf::from("/");
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut value = |name: &str| it.next().ok_or(format!("{name} needs a value"));
        match arg.as_str() {
            "--socket" => socket = Some(PathBuf::from(value("--socket")?)),
            "--root" => root = PathBuf::from(value("--root")?),
            "--config" => {
                config = match value("--config")?.as_str() {
                    "none" => None,
                    p => Some(PathBuf::from(p)),
                }
            }
            "--interval-ms" => {
                let ms: u64 = value("--interval-ms")?
                    .parse()
                    .map_err(|_| "--interval-ms needs a number".to_string())?;
                if !(200..=60_000).contains(&ms) {
                    return Err("--interval-ms must be 200..=60000".into());
                }
                interval = Some(Duration::from_millis(ms));
            }
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(None);
            }
            "-V" | "--version" => {
                println!("thermond {}", env!("CARGO_PKG_VERSION"));
                return Ok(None);
            }
            other => return Err(format!("unexpected argument {other:?}")),
        }
    }
    let socket = socket
        .or_else(default_socket_path)
        .ok_or("XDG_RUNTIME_DIR is not set; pass --socket")?;
    Ok(Some(Args {
        socket,
        config,
        interval,
        root,
    }))
}

fn run(args: Args) -> io::Result<()> {
    // A broken config must not keep the monitor from starting.
    let config = match &args.config {
        Some(path) => Config::load(path).unwrap_or_else(|e| {
            eprintln!("thermond: using defaults: {e}");
            Config::default()
        }),
        None => Config::default(),
    };
    let interval = args
        .interval
        .or(config.interval_ms.map(Duration::from_millis))
        .unwrap_or(Duration::from_secs(1));
    let sampler = Sampler::new(&args.root, config)?;
    state::log_unmatched(&sampler);
    let inv = sampler.inventory();
    eprintln!(
        "thermond {}: {} chips, {} sensors, {} GPUs, {} CPUs; sampling every {} ms",
        env!("CARGO_PKG_VERSION"),
        inv.chips.len(),
        inv.chips.iter().map(|c| c.sensors.len()).sum::<usize>(),
        inv.gpus.len(),
        inv.cpu_count,
        interval.as_millis()
    );

    let (listener, _lock) = bind(&args.socket)?;
    eprintln!("thermond: listening on {}", args.socket.display());

    let shared = Shared::new(sampler.inventory().clone(), interval);
    state::spawn_sampler(Arc::clone(&shared), sampler, ConfigWatch::new(args.config));

    let clients = Arc::new(AtomicUsize::new(0));
    for stream in listener.incoming() {
        let stream = match stream {
            Ok(s) => s,
            Err(e) => {
                eprintln!("thermond: accept: {e}");
                continue;
            }
        };
        if clients.load(Ordering::Relaxed) >= MAX_CLIENTS {
            eprintln!("thermond: too many clients, refusing connection");
            // Say why, so a client doesn't just see a reset.
            let mut stream = stream;
            let _ = stream.set_write_timeout(Some(Duration::from_millis(100)));
            let _ =
                stream.write_all(b"{\"v\":1,\"type\":\"error\",\"error\":\"too many clients\"}\n");
            continue;
        }
        // Returned on drop, so a panicking client thread or a failed spawn
        // can't leak its slot.
        let slot = Slot::take(&clients);
        let shared = Arc::clone(&shared);
        let spawned = thread::Builder::new().name("client".into()).spawn(move || {
            let _slot = slot;
            if let Err(e) = client::handle(stream, shared)
                && !matches!(
                    e.kind(),
                    io::ErrorKind::BrokenPipe | io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                )
            {
                eprintln!("thermond: client: {e}");
            }
        });
        if let Err(e) = spawned {
            eprintln!("thermond: spawn client thread: {e}");
        }
    }
    Ok(())
}

struct Slot(Arc<AtomicUsize>);

impl Slot {
    fn take(clients: &Arc<AtomicUsize>) -> Slot {
        clients.fetch_add(1, Ordering::Relaxed);
        Slot(Arc::clone(clients))
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Bind the socket, replacing a stale one but never a live daemon's. The
/// returned lock file must be kept open for the life of the process.
fn bind(path: &Path) -> io::Result<(UnixListener, fs::File)> {
    let lock_path = PathBuf::from(format!("{}.lock", path.display()));
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)?;
    if lock.try_lock().is_err() {
        return Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            format!("another thermond holds {}", lock_path.display()),
        ));
    }
    if let Ok(meta) = fs::symlink_metadata(path) {
        // Only ever remove a socket: a typo'd --socket must not delete a file.
        if !meta.file_type().is_socket() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!(
                    "{} exists and is not a socket; refusing to replace it",
                    path.display()
                ),
            ));
        }
        if UnixStream::connect(path).is_ok() {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                format!(
                    "another thermond is already listening on {}",
                    path.display()
                ),
            ));
        }
        fs::remove_file(path)?;
    }
    let listener = UnixListener::bind(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok((listener, lock))
}
