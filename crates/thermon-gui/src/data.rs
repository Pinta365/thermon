//! Background threads that talk to thermond and fill [`Shared`]. The UI only
//! reads it; every update asks egui for a repaint, so an idle window with no
//! new data does no work.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use eframe::egui;
use thermon_core::client::Client;
use thermon_core::history::HistoryResponse;
use thermon_core::peak::{RecentPeak, THROTTLE_PEAK_WINDOW};
use thermon_core::protocol::{Body, Command, ProcessList, ProcessQuery, Response, SortKey, Topic};
use thermon_core::sampler::{Inventory, Snapshot};

const RETRY: Duration = Duration::from_secs(2);
/// Processes fetched for the table; sorting and filtering happen locally.
const PROCESS_LIMIT: usize = 2000;

#[derive(Default)]
pub struct Shared {
    pub inventory: Option<Inventory>,
    pub snapshot: Option<Snapshot>,
    pub processes: Option<ProcessList>,
    pub history: Option<HistoryResponse>,
    /// State of the snapshot stream, which is what "connected" means in the UI.
    pub connected: bool,
    pub error: Option<String>,
    /// Errors of the other streams, shown where their data is shown.
    pub processes_error: Option<String>,
    pub history_error: Option<String>,
    /// When the latest snapshot arrived, and the spacing before it: the UI
    /// marks data stale when it's much older than the usual spacing.
    pub received: Option<Instant>,
    pub spacing: Duration,
    /// Recent peak per-CPU clock (for the throttle heuristic).
    pub peak_freq_khz: Option<u64>,
    peak: Option<RecentPeak>,
}

impl Shared {
    /// No snapshot for well over the usual spacing (daemon stalled or gone).
    pub fn stale(&self) -> bool {
        match self.received {
            None => false,
            Some(at) => {
                !self.connected || at.elapsed() > (self.spacing * 3).max(Duration::from_secs(5))
            }
        }
    }
}

#[derive(Clone, PartialEq)]
pub struct HistoryRequest {
    pub series: Vec<String>,
    pub range: String,
}

pub struct Data {
    shared: Arc<Mutex<Shared>>,
    want_processes: Arc<AtomicBool>,
    history_request: Arc<Mutex<Option<HistoryRequest>>>,
}

impl Data {
    pub fn start(ctx: egui::Context, socket: PathBuf) -> Data {
        let data = Data {
            shared: Arc::default(),
            want_processes: Arc::default(),
            history_request: Arc::default(),
        };
        {
            let (shared, ctx, socket) = (data.shared.clone(), ctx.clone(), socket.clone());
            spawn("snapshots", move || snapshots(&shared, &ctx, &socket));
        }
        {
            let (shared, ctx, socket) = (data.shared.clone(), ctx.clone(), socket.clone());
            let want = data.want_processes.clone();
            spawn("processes", move || {
                processes(&shared, &ctx, &socket, &want)
            });
        }
        {
            let (shared, req) = (data.shared.clone(), data.history_request.clone());
            spawn("history", move || history(&shared, &ctx, &socket, &req));
        }
        data
    }

    pub fn lock(&self) -> MutexGuard<'_, Shared> {
        self.shared.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Process scanning costs the daemon real work, so only ask while visible.
    pub fn want_processes(&self, on: bool) {
        self.want_processes.store(on, Ordering::Relaxed);
    }

    pub fn request_history(&self, req: HistoryRequest) {
        let mut cur = self
            .history_request
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if cur.as_ref() != Some(&req) {
            *cur = Some(req);
        }
    }
}

fn spawn(name: &str, f: impl FnOnce() + Send + 'static) {
    thread::Builder::new()
        .name(name.into())
        .spawn(f)
        .expect("spawn data thread");
}

fn lock(shared: &Mutex<Shared>) -> MutexGuard<'_, Shared> {
    shared.lock().unwrap_or_else(|e| e.into_inner())
}

fn disconnected(shared: &Mutex<Shared>, ctx: &egui::Context, err: String) {
    let mut s = lock(shared);
    s.connected = false;
    s.error = Some(err);
    drop(s);
    ctx.request_repaint();
}

/// Read one subscription message.
fn next(c: &mut Client) -> Result<Body, String> {
    let line = c
        .read_line()
        .map_err(|e| e.to_string())?
        .ok_or("thermond closed the connection")?;
    let resp: Response =
        serde_json::from_str(&line).map_err(|e| format!("bad message from thermond: {e}"))?;
    match resp.body {
        Body::Error { error } => Err(error),
        body => Ok(body),
    }
}

fn snapshots(shared: &Mutex<Shared>, ctx: &egui::Context, socket: &Path) {
    loop {
        let result = (|| -> Result<(), String> {
            let mut c = Client::connect(socket)?;
            c.send(Command::Subscribe {
                topics: vec![Topic::Snapshot],
                interval_ms: None,
                processes: ProcessQuery::default(),
            })
            .map_err(|e| e.to_string())?;
            loop {
                let body = next(&mut c)?;
                let mut s = lock(shared);
                s.connected = true;
                s.error = None;
                match body {
                    Body::Inventory { data } => s.inventory = Some(data),
                    Body::Snapshot { data } => {
                        let now = Instant::now();
                        if let Some(prev) = s.received {
                            s.spacing = now - prev;
                        }
                        s.received = Some(now);
                        if let Some(max) = data.cpu.freq_khz.iter().flatten().max() {
                            let peak = s
                                .peak
                                .get_or_insert_with(|| RecentPeak::new(THROTTLE_PEAK_WINDOW))
                                .push(now, *max);
                            s.peak_freq_khz = Some(peak);
                        }
                        s.snapshot = Some(data);
                    }
                    _ => {}
                }
                drop(s);
                ctx.request_repaint();
            }
        })();
        if let Err(e) = result {
            disconnected(shared, ctx, e);
        }
        thread::sleep(RETRY);
    }
}

fn processes(shared: &Mutex<Shared>, ctx: &egui::Context, socket: &Path, want: &AtomicBool) {
    loop {
        if !want.load(Ordering::Relaxed) {
            thread::sleep(Duration::from_millis(200));
            continue;
        }
        let result = (|| -> Result<(), String> {
            let mut c = Client::connect(socket)?;
            c.send(Command::Subscribe {
                topics: vec![Topic::Processes],
                interval_ms: None,
                processes: ProcessQuery {
                    sort: SortKey::Cpu,
                    limit: PROCESS_LIMIT,
                },
            })
            .map_err(|e| e.to_string())?;
            // Dropping the connection lets the daemon's process lease lapse.
            while want.load(Ordering::Relaxed) {
                if let Body::Processes { data } = next(&mut c)? {
                    let mut s = lock(shared);
                    s.processes = Some(data);
                    s.processes_error = None;
                    drop(s);
                    ctx.request_repaint();
                }
            }
            Ok(())
        })();
        if let Err(e) = result {
            // Only this stream failed; snapshots may be fine.
            lock(shared).processes_error = Some(e);
            ctx.request_repaint();
            thread::sleep(RETRY);
        }
        lock(shared).processes = None;
    }
}

/// Re-fetches the requested history whenever it changes, and otherwise once
/// per point interval (1 s, 10 s or 1 min depending on the range).
fn history(
    shared: &Mutex<Shared>,
    ctx: &egui::Context,
    socket: &Path,
    request: &Mutex<Option<HistoryRequest>>,
) {
    let mut client: Option<Client> = None;
    let mut last: Option<(HistoryRequest, Instant, Duration)> = None;
    loop {
        thread::sleep(Duration::from_millis(100));
        let Some(req) = request.lock().unwrap_or_else(|e| e.into_inner()).clone() else {
            continue;
        };
        let due = match &last {
            Some((prev, at, every)) => *prev != req || at.elapsed() >= *every,
            None => true,
        };
        if !due {
            continue;
        }
        if client.is_none() {
            match Client::connect(socket) {
                Ok(c) => client = Some(c),
                Err(_) => {
                    // The snapshot thread reports connection state.
                    thread::sleep(RETRY);
                    continue;
                }
            }
        }
        let cmd = Command::History {
            series: req.series.clone(),
            range: req.range.clone(),
        };
        match client.as_mut().map(|c| c.request(cmd)) {
            Some(Ok(Body::History { data })) => {
                let every = Duration::from_millis(data.interval_ms.max(1000));
                let mut s = lock(shared);
                s.history = Some(data);
                s.history_error = None;
                drop(s);
                ctx.request_repaint();
                last = Some((req, Instant::now(), every));
            }
            other => {
                let err = match other {
                    Some(Err(e)) => e,
                    _ => "unexpected reply to history".into(),
                };
                lock(shared).history_error = Some(err);
                ctx.request_repaint();
                client = None;
                // Back off: a persistent error (say, a protocol mismatch after
                // an upgrade) must not become a 10 Hz reconnect loop.
                thread::sleep(RETRY);
            }
        }
    }
}
