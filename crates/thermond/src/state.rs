//! Shared state and the sampler thread.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use thermon_core::config::Config;

use crate::alerting::{AlertLog, Alerter};
use thermon_core::history::{History, default_tiers};
use thermon_core::processes::{ProcessInfo, ProcessScanner};
use thermon_core::sampler::{Inventory, Sampler, Snapshot, now_ms};

/// Minimum time one `processes` request keeps process scanning on; longer
/// for slow sample intervals (see `lease_for`).
pub const PROCESS_LEASE: Duration = Duration::from_secs(10);

fn lease_for(interval: Duration) -> Duration {
    PROCESS_LEASE.max(interval * 3)
}

pub struct Shared {
    state: Mutex<State>,
    /// Notified after every sample.
    pub changed: Condvar,
    pub interval: Duration,
}

pub struct State {
    /// Incremented after every sample.
    pub seq: u64,
    pub snapshot: Option<Arc<Snapshot>>,
    pub inventory: Arc<Inventory>,
    /// Incremented when the inventory changes.
    pub inventory_seq: u64,
    pub history: History,
    pub procs: Option<Arc<ProcessSample>>,
    /// Incremented when a new process list is published.
    pub proc_seq: u64,
    pub procs_wanted_until: Instant,
    pub lease: Duration,
    pub alerts: AlertLog,
}

pub struct ProcessSample {
    pub ts_ms: u64,
    pub taken: Instant,
    pub list: Vec<ProcessInfo>,
}

impl Shared {
    pub fn new(inventory: Inventory, interval: Duration) -> Arc<Self> {
        Arc::new(Shared {
            state: Mutex::new(State {
                seq: 0,
                snapshot: None,
                inventory: Arc::new(inventory),
                inventory_seq: 1,
                history: History::new(interval, &default_tiers(interval)),
                procs: None,
                proc_seq: 0,
                procs_wanted_until: Instant::now(),
                lease: lease_for(interval),
                alerts: AlertLog::default(),
            }),
            changed: Condvar::new(),
            interval,
        })
    }

    pub fn lock(&self) -> MutexGuard<'_, State> {
        // A panicking client thread must not take the daemon down with it.
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Wait until `pred` is false or `timeout` passes.
    pub fn wait_while<'a>(
        &self,
        guard: MutexGuard<'a, State>,
        timeout: Duration,
        pred: impl FnMut(&mut State) -> bool,
    ) -> MutexGuard<'a, State> {
        match self.changed.wait_timeout_while(guard, timeout, pred) {
            Ok((g, _)) => g,
            Err(e) => e.into_inner().0,
        }
    }
}

impl State {
    pub fn want_processes(&mut self) {
        self.procs_wanted_until = self.procs_wanted_until.max(Instant::now() + self.lease);
    }
}

/// Reloads the config file when its modification time changes.
pub struct ConfigWatch {
    path: Option<PathBuf>,
    mtime: Option<SystemTime>,
}

impl ConfigWatch {
    pub fn new(path: Option<PathBuf>) -> Self {
        let mut w = ConfigWatch { path, mtime: None };
        w.mtime = w.current_mtime();
        w
    }

    fn current_mtime(&self) -> Option<SystemTime> {
        fs::metadata(self.path.as_ref()?)
            .and_then(|m| m.modified())
            .ok()
    }

    /// A freshly loaded config if the file changed (or was removed) since the
    /// last check. Parse errors are logged and the old config kept.
    fn poll(&mut self) -> Option<Config> {
        let mtime = self.current_mtime();
        let path = self.path.as_ref()?;
        if mtime == self.mtime {
            return None;
        }
        self.mtime = mtime;
        match Config::load(path) {
            Ok(c) => {
                eprintln!("thermond: reloaded {}", path.display());
                Some(c)
            }
            Err(e) => {
                eprintln!("thermond: keeping previous config: {e}");
                None
            }
        }
    }
}

pub fn log_unmatched(sampler: &Sampler) {
    for key in sampler.unmatched() {
        eprintln!("thermond: config: [sensors.{key:?}] matches no sensor");
    }
}

/// Exits the daemon if the thread holding it panics, so systemd restarts it
/// instead of clients reading frozen data.
struct ExitOnPanic(&'static str);

impl Drop for ExitOnPanic {
    fn drop(&mut self) {
        if thread::panicking() {
            eprintln!("thermond: {} thread panicked; exiting", self.0);
            std::process::exit(1);
        }
    }
}

pub fn spawn_sampler(shared: Arc<Shared>, sampler: Sampler, config: ConfigWatch) {
    let root = sampler.root().to_path_buf();
    {
        let shared = Arc::clone(&shared);
        thread::Builder::new()
            .name("processes".into())
            .spawn(move || {
                let _guard = ExitOnPanic("processes");
                scan_processes(&shared, &root);
            })
            .expect("spawn process thread");
    }
    thread::Builder::new()
        .name("sampler".into())
        .spawn(move || {
            let _guard = ExitOnPanic("sampler");
            run(shared, sampler, config);
        })
        .expect("spawn sampler thread");
}

/// Scans processes once per sample while the lease is held. Separate from the
/// sampler because a `/proc/<pid>` read can block on a stuck process.
fn scan_processes(shared: &Shared, root: &Path) {
    // A fresh scanner's first scan has no CPU deltas: warm-up only.
    let mut scanner: Option<ProcessScanner> = None;
    let mut seen = 0;
    loop {
        let want = {
            let st = shared.wait_while(shared.lock(), shared.interval * 3, |s| s.seq == seen);
            seen = st.seq;
            st.procs_wanted_until > Instant::now()
        };
        if !want {
            if scanner.take().is_some() {
                shared.lock().procs = None;
            }
            continue;
        }
        let warm = scanner.is_some();
        match scanner.get_or_insert_with(ProcessScanner::new).scan(root) {
            Ok(list) if warm => {
                {
                    let mut st = shared.lock();
                    st.procs = Some(Arc::new(ProcessSample {
                        ts_ms: now_ms(),
                        taken: Instant::now(),
                        list,
                    }));
                    st.proc_seq += 1;
                }
                shared.changed.notify_all();
            }
            Ok(_) => {}
            Err(e) => {
                eprintln!("thermond: process scan failed: {e}");
                scanner = None;
            }
        }
    }
}

/// CLOCK_BOOTTIME in ms: keeps counting through suspend, never steps.
fn boot_ms() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: valid out-pointer to a timespec.
    unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut ts) };
    ts.tv_sec as u64 * 1000 + ts.tv_nsec as u64 / 1_000_000
}

fn run(shared: Arc<Shared>, mut sampler: Sampler, mut config: ConfigWatch) {
    let mut alerter = Alerter::new(&sampler.config().alerts);
    let mut next = Instant::now();

    loop {
        let snapshot = sampler.sample();
        let series = snapshot.series();
        let mut changed = sampler.maybe_rediscover();
        if let Some(new) = config.poll() {
            if new.interval_ms != sampler.config().interval_ms {
                eprintln!("thermond: interval_ms changed; restart thermond to apply it");
            }
            alerter.set_config(&new.alerts);
            changed |= sampler.set_config(new);
            log_unmatched(&sampler);
        }
        let inventory = changed.then(|| sampler.inventory().clone());

        // Zombie detection only has data while someone is scanning processes.
        let procs = shared.lock().procs.clone();
        let (events, active) = alerter.update(
            sampler.inventory(),
            &snapshot,
            procs.as_ref().map(|p| p.list.as_slice()),
        );

        {
            let mut st = shared.lock();
            for a in events {
                st.alerts.push(a);
            }
            st.alerts.active = active;
            st.history.push_clocked(snapshot.ts_ms, boot_ms(), &series);
            st.snapshot = Some(Arc::new(snapshot));
            st.seq += 1;
            if let Some(inv) = inventory {
                st.inventory = Arc::new(inv);
                st.inventory_seq += 1;
            }
        }
        shared.changed.notify_all();

        next += shared.interval;
        let now = Instant::now();
        if next > now {
            thread::sleep(next - now);
        } else {
            // Fell behind (suspend, heavy load): resync instead of bursting.
            next = now;
        }
    }
}
