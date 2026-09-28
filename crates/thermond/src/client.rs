//! One thread per connected client.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::time::Duration;

use thermon_core::history::parse_range;
use thermon_core::protocol::{
    AlertLog, Body, Command, MAX_REQUEST_BYTES, ProcessList, ProcessQuery, Request, Response,
    Topic, VERSION,
};

use crate::state::{Shared, State};

/// A one-shot client that sends nothing for this long is dropped, so idle or
/// half-sent connections can't hold client slots forever.
const IDLE_TIMEOUT: Duration = Duration::from_secs(120);
/// Replies to one-shot requests must be read within this long.
const REPLY_TIMEOUT: Duration = Duration::from_secs(10);

pub fn handle(stream: UnixStream, shared: Arc<Shared>) -> io::Result<()> {
    stream.set_read_timeout(Some(IDLE_TIMEOUT))?;
    stream.set_write_timeout(Some(REPLY_TIMEOUT))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut out = stream;
    let mut line = String::new();

    loop {
        line.clear();
        let n = (&mut reader)
            .take(MAX_REQUEST_BYTES as u64 + 1)
            .read_line(&mut line)?;
        if n == 0 {
            return Ok(());
        }
        if line.len() > MAX_REQUEST_BYTES {
            send(&mut out, &Response::error("request too long"))?;
            return Ok(());
        }
        if line.trim().is_empty() {
            continue;
        }

        let req: Request = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                send(&mut out, &Response::error(format!("bad request: {e}")))?;
                continue;
            }
        };
        if req.v != VERSION {
            send(
                &mut out,
                &Response::error(format!(
                    "unsupported protocol version {} (daemon speaks {VERSION})",
                    req.v
                )),
            )?;
            continue;
        }

        let body = match req.cmd {
            Command::Snapshot => snapshot(&shared),
            Command::Inventory => Body::Inventory {
                data: (*shared.lock().inventory).clone(),
            },
            Command::History { mut series, range } => match parse_range(&range) {
                Some(range) => {
                    // Repeated names would only multiply work under the lock.
                    series.sort();
                    series.dedup();
                    Body::History {
                        data: shared.lock().history.query(&series, range),
                    }
                }
                None => Body::Error {
                    error: format!("bad range {range:?}; use e.g. 90s, 15m, 1h, 24h"),
                },
            },
            Command::Processes(query) => processes(&shared, query),
            Command::Alerts => Body::Alerts {
                data: alert_log(&shared.lock()),
            },
            Command::Subscribe {
                topics,
                interval_ms,
                processes,
            } => {
                return subscribe(
                    reader.into_inner(),
                    out,
                    &shared,
                    &topics,
                    interval_ms,
                    processes,
                );
            }
        };
        send(&mut out, &Response::new(body))?;
    }
}

fn send(out: &mut impl Write, msg: &Response) -> io::Result<()> {
    let mut buf = serde_json::to_vec(msg).map_err(io::Error::other)?;
    buf.push(b'\n');
    out.write_all(&buf)
}

fn snapshot(shared: &Shared) -> Body {
    // Only empty during the first sample after startup.
    let st = shared.wait_while(shared.lock(), shared.interval * 3, |s| s.snapshot.is_none());
    match &st.snapshot {
        Some(s) => Body::Snapshot {
            data: (**s).clone(),
        },
        None => Body::Error {
            error: "no sample yet".into(),
        },
    }
}

fn process_list(st: &State, query: ProcessQuery) -> Option<ProcessList> {
    let p = st.procs.as_ref()?;
    Some(ProcessList {
        ts_ms: p.ts_ms,
        total: p.list.len(),
        processes: query.apply(&p.list),
    })
}

fn processes(shared: &Shared, query: ProcessQuery) -> Body {
    let mut st = shared.lock();
    st.want_processes();
    let fresh = |st: &State| {
        st.procs
            .as_ref()
            .is_some_and(|p| p.taken.elapsed() < shared.interval * 3 / 2)
    };
    if !fresh(&st) {
        // Scanning was off: a warm-up scan plus one real scan.
        let seen = st.proc_seq;
        st = shared.wait_while(st, shared.interval * 3 + Duration::from_secs(1), |s| {
            s.proc_seq == seen
        });
    }
    match process_list(&st, query) {
        Some(data) => Body::Processes { data },
        None => Body::Error {
            error: "process list not ready".into(),
        },
    }
}

fn subscribe(
    mut input: UnixStream,
    mut out: UnixStream,
    shared: &Shared,
    topics: &[Topic],
    interval_ms: Option<u64>,
    query: ProcessQuery,
) -> io::Result<()> {
    let want_snapshot = topics.contains(&Topic::Snapshot);
    let want_procs = topics.contains(&Topic::Processes);
    let want_alerts = topics.contains(&Topic::Alerts);
    let base_ms = shared.interval.as_millis().max(1) as u64;
    // Send a snapshot every `every` samples.
    let every = interval_ms
        .map_or(1, |ms| ms.saturating_add(base_ms / 2) / base_ms)
        .max(1);

    // Reads only drain stray input; a stalled reader is dropped by the write
    // timeout.
    input.set_read_timeout(Some(Duration::from_millis(1)))?;
    out.set_write_timeout(Some(shared.interval * 5))?;

    let (mut seen_seq, mut seen_inv, mut seen_proc) = (0, 0, 0);
    let mut last_sent_seq: Option<u64> = None;
    // Alerts: current state first, then each new event.
    let mut seen_alert = None;

    loop {
        let mut msgs = Vec::new();
        {
            let mut st = shared.lock();
            if want_procs {
                st.want_processes();
            }
            // Process lists come from their own thread, between samples.
            st = shared.wait_while(st, shared.interval * 3, |s| {
                s.seq == seen_seq && !(want_procs && s.proc_seq != seen_proc)
            });
            seen_seq = st.seq;

            if st.inventory_seq != seen_inv {
                seen_inv = st.inventory_seq;
                msgs.push(Body::Inventory {
                    data: (*st.inventory).clone(),
                });
            }
            let due = last_sent_seq.is_none_or(|last| st.seq >= last.saturating_add(every));
            if want_snapshot
                && due
                && let Some(s) = &st.snapshot
            {
                last_sent_seq = Some(st.seq);
                msgs.push(Body::Snapshot {
                    data: (**s).clone(),
                });
            }
            if want_procs
                && st.proc_seq != seen_proc
                && let Some(data) = process_list(&st, query)
            {
                seen_proc = st.proc_seq;
                msgs.push(Body::Processes { data });
            }
            if want_alerts {
                match seen_alert {
                    None => msgs.push(Body::Alerts {
                        data: alert_log(&st),
                    }),
                    Some(seq) => msgs.extend(
                        st.alerts
                            .since(seq)
                            .into_iter()
                            .map(|(_, data)| Body::Alert { data }),
                    ),
                }
                seen_alert = Some(st.alerts.last_seq());
            }
        }

        for body in msgs {
            // Client went away, or stopped reading for longer than the timeout.
            if send(&mut out, &Response::new(body)).is_err() {
                return Ok(());
            }
        }
        if disconnected(&mut input) {
            return Ok(());
        }
    }
}

fn alert_log(st: &State) -> AlertLog {
    AlertLog {
        active: st.alerts.active.clone(),
        recent: st.alerts.recent(),
    }
}

/// True once the peer has fully closed. EOF isn't enough: a half-closed
/// client (`echo ... | socat`) is still listening; only a full close raises POLLHUP.
fn disconnected(input: &mut UnixStream) -> bool {
    let mut pfd = libc::pollfd {
        fd: input.as_raw_fd(),
        events: libc::POLLIN | libc::POLLRDHUP,
        revents: 0,
    };
    // SAFETY: one valid pollfd, zero timeout.
    let n = unsafe { libc::poll(&mut pfd, 1, 0) };
    if n < 0 {
        return io::Error::last_os_error().kind() != io::ErrorKind::Interrupted;
    }
    if pfd.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
        return true;
    }
    if pfd.revents & libc::POLLIN != 0 {
        // Drain anything the client sent after subscribing (or read EOF).
        let mut buf = [0u8; 256];
        let _ = input.read(&mut buf);
    }
    false
}
