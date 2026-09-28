//! Wire protocol between `thermond` and its clients: one JSON object per line
//! in each direction over a Unix socket.
//!
//! ```text
//! → {"v":1,"cmd":"snapshot"}
//! ← {"v":1,"type":"snapshot","data":{...}}
//! → {"v":1,"cmd":"history","series":["cpu.usage"],"range":"15m"}
//! → {"v":1,"cmd":"processes","sort":"cpu","limit":20}
//! → {"v":1,"cmd":"alerts"}
//! → {"v":1,"cmd":"subscribe","topics":["snapshot","processes","alerts"],"interval_ms":2000}
//! ```
//!
//! A connection can send any number of one-shot requests. `subscribe` turns the
//! connection into a server→client stream until the client disconnects; it
//! starts with an `inventory` message and repeats it whenever hardware changes.
//! With the `alerts` topic it then sends an `alerts` message (what is firing
//! now) followed by one `alert` message per alert as it happens.

use std::env;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::alerts::Alert;
use crate::history::HistoryResponse;
use crate::processes::ProcessInfo;
use crate::sampler::{Inventory, Snapshot};

pub const VERSION: u32 = 1;

/// Longest request line the daemon accepts.
pub const MAX_REQUEST_BYTES: usize = 64 * 1024;

/// `$XDG_RUNTIME_DIR/thermon.sock`.
pub fn default_socket_path() -> Option<PathBuf> {
    let dir = env::var_os("XDG_RUNTIME_DIR").filter(|d| !d.is_empty())?;
    Some(PathBuf::from(dir).join("thermon.sock"))
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    #[serde(default = "version")]
    pub v: u32,
    #[serde(flatten)]
    pub cmd: Command,
}

impl Request {
    pub fn new(cmd: Command) -> Self {
        Request { v: VERSION, cmd }
    }
}

fn version() -> u32 {
    VERSION
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Command {
    Snapshot,
    Inventory,
    History {
        /// Empty = every series.
        #[serde(default)]
        series: Vec<String>,
        /// `90s`, `15m`, `1h`, `24h`.
        #[serde(default = "default_range")]
        range: String,
    },
    Processes(ProcessQuery),
    /// Firing alerts and the recent alert log.
    Alerts,
    Subscribe {
        topics: Vec<Topic>,
        /// Minimum spacing between snapshot messages; defaults to every sample.
        #[serde(default)]
        interval_ms: Option<u64>,
        #[serde(default)]
        processes: ProcessQuery,
    },
}

fn default_range() -> String {
    "5m".into()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Topic {
    Snapshot,
    Processes,
    Alerts,
    /// A topic from a newer client; ignored so the rest of the stream works.
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessQuery {
    #[serde(default)]
    pub sort: SortKey,
    #[serde(default = "default_limit")]
    pub limit: usize,
}

impl Default for ProcessQuery {
    fn default() -> Self {
        ProcessQuery {
            sort: SortKey::default(),
            limit: default_limit(),
        }
    }
}

fn default_limit() -> usize {
    20
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SortKey {
    #[default]
    Cpu,
    Mem,
    Pid,
    Name,
}

impl ProcessQuery {
    /// Sort (descending for cpu/mem) and truncate.
    pub fn apply(&self, procs: &[ProcessInfo]) -> Vec<ProcessInfo> {
        let mut out = procs.to_vec();
        match self.sort {
            SortKey::Cpu => out.sort_by(|a, b| {
                b.cpu_percent
                    .total_cmp(&a.cpu_percent)
                    .then(b.rss_kib.cmp(&a.rss_kib))
            }),
            SortKey::Mem => out.sort_by_key(|p| std::cmp::Reverse(p.rss_kib)),
            SortKey::Pid => out.sort_by_key(|p| p.pid),
            SortKey::Name => out.sort_by_key(|p| p.name.to_lowercase()),
        }
        out.truncate(self.limit);
        out
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProcessList {
    pub ts_ms: u64,
    /// Processes before `limit` was applied.
    pub total: usize,
    pub processes: Vec<ProcessInfo>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AlertLog {
    /// Firing now, worst first.
    pub active: Vec<Alert>,
    /// Recent firing/resolved events, oldest first (bounded).
    pub recent: Vec<Alert>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response {
    pub v: u32,
    #[serde(flatten)]
    pub body: Body,
}

impl Response {
    pub fn new(body: Body) -> Self {
        Response { v: VERSION, body }
    }

    pub fn error(msg: impl Into<String>) -> Self {
        Response::new(Body::Error { error: msg.into() })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
// Messages are built, serialized and dropped; boxing the snapshot buys nothing.
#[allow(clippy::large_enum_variant)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Body {
    Snapshot { data: Snapshot },
    Inventory { data: Inventory },
    History { data: HistoryResponse },
    Processes { data: ProcessList },
    Alerts { data: AlertLog },
    Alert { data: Alert },
    Error { error: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Request {
        serde_json::from_str(s).unwrap()
    }

    #[test]
    fn requests_parse_with_defaults() {
        assert_eq!(parse(r#"{"v":1,"cmd":"snapshot"}"#).cmd, Command::Snapshot);
        // v is optional.
        assert_eq!(
            parse(r#"{"cmd":"inventory"}"#),
            Request::new(Command::Inventory)
        );
        assert_eq!(
            parse(r#"{"cmd":"history"}"#).cmd,
            Command::History {
                series: vec![],
                range: "5m".into()
            }
        );
        assert_eq!(
            parse(r#"{"cmd":"processes","sort":"mem"}"#).cmd,
            Command::Processes(ProcessQuery {
                sort: SortKey::Mem,
                limit: 20
            })
        );
        assert_eq!(
            parse(r#"{"cmd":"subscribe","topics":["snapshot"],"interval_ms":2000}"#).cmd,
            Command::Subscribe {
                topics: vec![Topic::Snapshot],
                interval_ms: Some(2000),
                processes: ProcessQuery::default(),
            }
        );
        assert!(serde_json::from_str::<Request>(r#"{"cmd":"reboot"}"#).is_err());
    }

    #[test]
    fn requests_round_trip() {
        let r = Request::new(Command::Processes(ProcessQuery {
            sort: SortKey::Name,
            limit: 3,
        }));
        let s = serde_json::to_string(&r).unwrap();
        assert_eq!(s, r#"{"v":1,"cmd":"processes","sort":"name","limit":3}"#);
        assert_eq!(parse(&s), r);
    }

    #[test]
    fn error_response_shape() {
        let s = serde_json::to_string(&Response::error("nope")).unwrap();
        assert_eq!(s, r#"{"v":1,"type":"error","error":"nope"}"#);
        let back: Response = serde_json::from_str(&s).unwrap();
        assert_eq!(
            back.body,
            Body::Error {
                error: "nope".into()
            }
        );
    }
}
