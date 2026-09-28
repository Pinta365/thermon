//! Runs the real `thermond` binary against the captured fixture and talks to it
//! over its socket.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

struct Daemon {
    child: Child,
    socket: PathBuf,
}

impl Daemon {
    fn start(name: &str) -> Daemon {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/ryzen-rx9070");
        Daemon::start_with(name, &root, &["--config", "none"])
    }

    fn start_with(name: &str, root: &std::path::Path, extra: &[&str]) -> Daemon {
        // Short path: sun_path is limited to 108 bytes.
        let socket =
            std::env::temp_dir().join(format!("thermond-{}-{name}.sock", std::process::id()));
        let _ = std::fs::remove_file(&socket);
        let child = Command::new(env!("CARGO_BIN_EXE_thermond"))
            .arg("--root")
            .arg(root)
            .arg("--socket")
            .arg(&socket)
            .args(["--interval-ms", "200"])
            .args(extra)
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn thermond");
        let deadline = Instant::now() + Duration::from_secs(5);
        while UnixStream::connect(&socket).is_err() {
            assert!(Instant::now() < deadline, "thermond did not start");
            thread::sleep(Duration::from_millis(20));
        }
        Daemon { child, socket }
    }

    fn connect(&self) -> Conn {
        let stream = UnixStream::connect(&self.socket).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        Conn {
            reader: BufReader::new(stream.try_clone().unwrap()),
            writer: stream,
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.socket);
    }
}

struct Conn {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
}

impl Conn {
    fn send_raw(&mut self, line: &str) {
        self.writer
            .write_all(format!("{line}\n").as_bytes())
            .unwrap();
    }

    fn recv(&mut self) -> Value {
        let mut line = String::new();
        self.reader.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap_or_else(|e| panic!("{e}: {line:?}"))
    }

    fn req(&mut self, v: Value) -> Value {
        self.send_raw(&v.to_string());
        self.recv()
    }
}

#[test]
fn one_shot_requests() {
    let d = Daemon::start("oneshot");
    let mut c = d.connect();

    let r = c.req(json!({"v": 1, "cmd": "snapshot"}));
    assert_eq!(r["type"], "snapshot");
    assert!(r["data"]["sensors"]["amdgpu@0000:03:00.0/junction"].is_number());

    let r = c.req(json!({"cmd": "inventory"}));
    assert_eq!(r["data"]["chips"].as_array().unwrap().len(), 8);
    assert_eq!(r["data"]["cpu_count"], 8);

    thread::sleep(Duration::from_millis(500));
    let r =
        c.req(json!({"cmd": "history", "series": ["k10temp@0000:00:18.3/tctl"], "range": "1m"}));
    assert_eq!(r["data"]["interval_ms"], 200);
    assert!(
        r["data"]["series"]["k10temp@0000:00:18.3/tctl"]
            .as_array()
            .unwrap()
            .len()
            >= 2
    );

    // The fixture has no /proc/<pid> dirs: an empty list, not an error.
    let r = c.req(json!({"cmd": "processes"}));
    assert_eq!(r["type"], "processes");
    assert_eq!(r["data"]["total"], 0);
}

#[test]
fn errors_keep_the_connection_open() {
    let d = Daemon::start("errors");
    let mut c = d.connect();

    c.send_raw("not json");
    assert_eq!(c.recv()["type"], "error");
    assert_eq!(c.req(json!({"v": 99, "cmd": "snapshot"}))["type"], "error");
    assert_eq!(
        c.req(json!({"cmd": "history", "range": "soon"}))["type"],
        "error"
    );
    assert_eq!(c.req(json!({"cmd": "snapshot"}))["type"], "snapshot");

    // Oversized requests are refused and the connection closed.
    c.send_raw(&"x".repeat(70 * 1024));
    assert_eq!(c.recv()["error"], "request too long");
}

#[test]
fn subscription_streams_at_requested_rate() {
    let d = Daemon::start("subscribe");
    let mut c = d.connect();
    c.send_raw(
        &json!({"cmd": "subscribe", "topics": ["snapshot"], "interval_ms": 400}).to_string(),
    );

    assert_eq!(c.recv()["type"], "inventory");
    let mut stamps = Vec::new();
    for _ in 0..4 {
        let m = c.recv();
        assert_eq!(m["type"], "snapshot");
        stamps.push(m["data"]["ts_ms"].as_u64().unwrap());
    }
    // 400 ms requested on a 200 ms sampler: every other sample.
    for w in stamps.windows(2) {
        let gap = w[1] - w[0];
        assert!((300..=600).contains(&gap), "gap {gap} ms in {stamps:?}");
    }
}

#[test]
fn refuses_to_steal_a_live_socket() {
    let d = Daemon::start("steal");
    let out = Command::new(env!("CARGO_BIN_EXE_thermond"))
        .arg("--socket")
        .arg(&d.socket)
        .output()
        .unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("another thermond"), "{err}");
    // The original daemon is unaffected.
    assert_eq!(
        d.connect().req(json!({"cmd": "snapshot"}))["type"],
        "snapshot"
    );
}

/// A writable copy of the fixture (symlinks kept), removed on drop.
struct TempTree(PathBuf);

impl TempTree {
    fn copy_fixture(name: &str) -> TempTree {
        let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/ryzen-rx9070");
        let dst = std::env::temp_dir().join(format!("thermond-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dst);
        let ok = Command::new("cp")
            .arg("-a")
            .arg(&src)
            .arg(&dst)
            .status()
            .unwrap();
        assert!(ok.success());
        TempTree(dst)
    }
}

impl Drop for TempTree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn alerts_fire_and_resolve() {
    let tree = TempTree::copy_fixture("alerts");
    let config = tree.0.join("config.toml");
    // No hold, no desktop notifications from a test.
    std::fs::write(&config, "[alerts]\nhold_s = 0\nnotify = false\n").unwrap();
    let tctl = tree.0.join("sys/class/hwmon/hwmon5/temp1_input");
    assert!(
        std::fs::read_to_string(&tctl).is_ok(),
        "fixture layout changed"
    );

    let d = Daemon::start_with("alerts", &tree.0, &["--config", config.to_str().unwrap()]);
    let mut c = d.connect();
    c.send_raw(&json!({"cmd": "subscribe", "topics": ["alerts"]}).to_string());
    assert_eq!(c.recv()["type"], "inventory");
    let first = c.recv();
    assert_eq!(first["type"], "alerts");
    assert_eq!(first["data"]["active"], json!([]));

    // Tctl has no crit, so the automatic CPU warn is 95 °C.
    std::fs::write(&tctl, "99000\n").unwrap();
    let fired = c.recv();
    assert_eq!(fired["type"], "alert", "{fired}");
    assert_eq!(fired["data"]["state"], "firing");
    assert_eq!(fired["data"]["key"], "temp:k10temp@0000:00:18.3/tctl");
    assert_eq!(fired["data"]["finding"]["severity"], "warn");

    // One-shot view agrees.
    let log = d.connect().req(json!({"cmd": "alerts"}));
    assert_eq!(log["data"]["active"].as_array().unwrap().len(), 1);

    std::fs::write(&tctl, "45000\n").unwrap();
    let resolved = c.recv();
    assert_eq!(resolved["data"]["state"], "resolved", "{resolved}");
    let log = d.connect().req(json!({"cmd": "alerts"}));
    assert_eq!(log["data"]["active"], json!([]));
    assert_eq!(log["data"]["recent"].as_array().unwrap().len(), 2);
}

#[test]
fn never_deletes_a_file_that_is_not_a_socket() {
    let path = std::env::temp_dir().join(format!("thermond-{}-precious.txt", std::process::id()));
    std::fs::write(&path, "precious").unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_thermond"))
        .args(["--config", "none", "--socket"])
        .arg(&path)
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "precious");
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{}.lock", path.display()));
}

#[test]
fn half_closed_subscriber_keeps_its_stream() {
    let d = Daemon::start("halfclose");
    let mut c = d.connect();
    c.send_raw(&json!({"cmd": "subscribe", "topics": ["snapshot", "future-topic"]}).to_string());
    // `echo ... | socat` style: done writing, still reading.
    c.writer.shutdown(std::net::Shutdown::Write).unwrap();
    assert_eq!(c.recv()["type"], "inventory");
    // Several snapshots keep coming (an unknown topic didn't reject the request).
    for _ in 0..4 {
        assert_eq!(c.recv()["type"], "snapshot");
    }
}
