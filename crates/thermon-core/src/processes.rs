//! Per-process sampling from `/proc/<pid>`.

use std::{
    collections::HashMap,
    fs,
    io::{self, ErrorKind},
    path::Path,
};

use crate::procfs::read_stat;

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(ErrorKind::InvalidData, message.into())
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ProcessInfo {
    pub pid: u32,
    pub ppid: u32,
    pub name: String,
    pub cmdline: String,
    pub state: char,
    pub uid: Option<u32>,
    pub threads: u32,
    pub nice: i32,
    pub rss_kib: u64,
    pub cpu_percent: f32,
    /// Start time in clock ticks since boot. With `pid` it identifies the
    /// process even after the pid is reused; signal only if both still match.
    #[serde(default)]
    pub start_ticks: u64,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ProcStat {
    pub pid: u32,
    pub name: String,
    pub state: char,
    pub ppid: u32,
    pub utime: u64,
    pub stime: u64,
    pub nice: i32,
    pub threads: u32,
    pub start_ticks: u64,
}

pub fn parse_pid_stat(s: &str) -> io::Result<ProcStat> {
    let open = s
        .find('(')
        .ok_or_else(|| invalid_data("missing process name"))?;
    let pid = s[..open]
        .trim()
        .parse::<u32>()
        .map_err(|_| invalid_data("invalid process pid"))?;
    let close = s
        .rfind(')')
        .filter(|close| *close > open)
        .ok_or_else(|| invalid_data("unterminated process name"))?;
    let name = s[open + 1..close].to_owned();
    let tail = s[close + 1..].trim_start();
    let mut tail_chars = tail.chars();
    let state = tail_chars
        .next()
        .ok_or_else(|| invalid_data("missing process state"))?;
    if !state.is_ascii_alphabetic() {
        return Err(invalid_data("invalid process state"));
    }
    let fields = tail_chars.as_str();
    if !fields.is_empty() && !fields.starts_with(char::is_whitespace) {
        return Err(invalid_data("invalid process state separator"));
    }
    let fields = fields.split_whitespace().collect::<Vec<_>>();

    let parse = |index: usize, name: &str| {
        fields
            .get(index)
            .ok_or_else(|| invalid_data(format!("missing process {name}")))
    };
    let ppid = parse(0, "ppid")?
        .parse::<u32>()
        .map_err(|_| invalid_data("invalid process ppid"))?;
    let utime = parse(10, "utime")?
        .parse::<u64>()
        .map_err(|_| invalid_data("invalid process utime"))?;
    let stime = parse(11, "stime")?
        .parse::<u64>()
        .map_err(|_| invalid_data("invalid process stime"))?;
    let nice = parse(15, "nice")?
        .parse::<i32>()
        .map_err(|_| invalid_data("invalid process nice"))?;
    let threads = parse(16, "threads")?
        .parse::<u32>()
        .map_err(|_| invalid_data("invalid process threads"))?;
    let start_ticks = parse(18, "start_ticks")?
        .parse::<u64>()
        .map_err(|_| invalid_data("invalid process start_ticks"))?;

    Ok(ProcStat {
        pid,
        name,
        state,
        ppid,
        utime,
        stime,
        nice,
        threads,
        start_ticks,
    })
}

fn parse_status(s: &str) -> io::Result<(Option<u32>, u64)> {
    let mut uid = None;
    let mut rss_kib = 0;

    for line in s.lines() {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        match name {
            "Uid" => {
                let value = value
                    .split_whitespace()
                    .next()
                    .ok_or_else(|| invalid_data("missing process uid"))?
                    .parse::<u32>()
                    .map_err(|_| invalid_data("invalid process uid"))?;
                uid = Some(value);
            }
            "VmRSS" => {
                rss_kib = value
                    .split_whitespace()
                    .next()
                    .ok_or_else(|| invalid_data("missing process VmRSS"))?
                    .parse::<u64>()
                    .map_err(|_| invalid_data("invalid process VmRSS"))?;
            }
            _ => {}
        }
    }

    Ok((uid, rss_kib))
}

fn parse_cmdline(bytes: &[u8]) -> String {
    let cmdline = String::from_utf8_lossy(bytes).replace('\0', " ");
    let cmdline = cmdline.trim();
    let end = cmdline
        .char_indices()
        .map(|(index, _)| index)
        .take_while(|index| *index <= 512)
        .last()
        .unwrap_or(0);
    if cmdline.len() <= 512 {
        cmdline.to_owned()
    } else {
        cmdline[..end].to_owned()
    }
}

/// Any process can give itself a non-UTF-8 name (`prctl(PR_SET_NAME)`), which
/// appears in both `stat` and `status`; strict UTF-8 reads would hide it.
fn read_lossy(path: &Path) -> Option<String> {
    fs::read(path)
        .ok()
        .map(|b| String::from_utf8_lossy(&b).into_owned())
}

fn vanished(error: &io::Error) -> bool {
    matches!(error.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory)
        || error.raw_os_error() == Some(3)
}

/// Stateful process scanner that derives CPU usage from consecutive snapshots.
#[derive(Debug, Default)]
pub struct ProcessScanner {
    previous_ticks: HashMap<(u32, u64), u64>,
    previous_total: Option<u64>,
}

impl ProcessScanner {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn scan(&mut self, root: &Path) -> io::Result<Vec<ProcessInfo>> {
        let cpu_stat = read_stat(root)?;
        let total = cpu_stat.total.total();
        let total_delta = self
            .previous_total
            .map(|previous| total.saturating_sub(previous))
            .unwrap_or_default();
        let ncpu = cpu_stat.cores.len().max(1);
        let entries = fs::read_dir(root.join("proc"))?;
        let mut pids = Vec::new();

        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) if vanished(&error) => continue,
                Err(error) => return Err(error),
            };
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            let Ok(pid) = name.parse::<u32>() else {
                continue;
            };
            match entry.file_type() {
                Ok(file_type) if file_type.is_dir() => pids.push((pid, entry.path())),
                Ok(_) => {}
                Err(error) if vanished(&error) => {}
                Err(error) => return Err(error),
            }
        }
        pids.sort_unstable_by_key(|(pid, _)| *pid);

        let mut processes = Vec::new();
        let mut next_ticks = HashMap::new();
        for (pid, path) in pids {
            // One unreadable or malformed process must not hide all the others.
            let Some(stat) = read_lossy(&path.join("stat"))
                .and_then(|stat| parse_pid_stat(&stat).ok())
                .filter(|stat| stat.pid == pid)
            else {
                continue;
            };
            let Some((uid, rss_kib)) =
                read_lossy(&path.join("status")).and_then(|status| parse_status(&status).ok())
            else {
                continue;
            };
            let cmdline = fs::read(path.join("cmdline"))
                .map(|bytes| parse_cmdline(&bytes))
                .unwrap_or_default();
            let ticks = stat.utime.saturating_add(stat.stime);
            let key = (pid, stat.start_ticks);
            let cpu_percent = self
                .previous_ticks
                .get(&key)
                .filter(|_| total_delta > 0)
                .map(|previous| {
                    let delta = ticks.saturating_sub(*previous);
                    (delta as f32 / total_delta as f32 * ncpu as f32 * 100.0)
                        .clamp(0.0, ncpu as f32 * 100.0)
                })
                .unwrap_or(0.0);
            next_ticks.insert(key, ticks);
            processes.push(ProcessInfo {
                pid,
                ppid: stat.ppid,
                name: stat.name,
                cmdline,
                state: stat.state,
                uid,
                threads: stat.threads,
                nice: stat.nice,
                rss_kib,
                cpu_percent,
                start_ticks: stat.start_ticks,
            });
        }

        self.previous_ticks = next_ticks;
        self.previous_total = Some(total);
        Ok(processes)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
        sync::atomic::{AtomicUsize, Ordering},
    };

    use super::*;
    use crate::test_support::fixture;

    static TEMP_DIR_SEQUENCE: AtomicUsize = AtomicUsize::new(0);

    fn copy_tree(source: &Path, target: &Path) {
        fs::create_dir_all(target).unwrap();
        for entry in fs::read_dir(source).unwrap() {
            let entry = entry.unwrap();
            let target_path = target.join(entry.file_name());
            if entry.file_type().unwrap().is_dir() {
                copy_tree(&entry.path(), &target_path);
            } else {
                fs::copy(entry.path(), target_path).unwrap();
            }
        }
    }

    fn temp_fixture() -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "thermon-processes-test-{}-{}",
            std::process::id(),
            TEMP_DIR_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        copy_tree(&fixture("procs-basic"), &root);
        root
    }

    #[test]
    fn parses_stat_with_parentheses_in_the_name() {
        let stat =
            parse_pid_stat("42 (sd-pam) x) S 1 0 0 0 0 0 0 0 0 0 12 8 0 0 5 -3 7 0 99 0").unwrap();
        assert_eq!(stat.name, "sd-pam) x");
        assert_eq!(stat.ppid, 1);
        assert_eq!(stat.utime, 12);
        assert_eq!(stat.stime, 8);
        assert_eq!(stat.nice, -3);
        assert_eq!(stat.threads, 7);
        assert_eq!(stat.start_ticks, 99);
        assert_eq!(
            parse_pid_stat("not a stat file").unwrap_err().kind(),
            ErrorKind::InvalidData
        );
    }

    #[test]
    fn calculates_cpu_and_forgets_vanished_processes() {
        let root = temp_fixture();
        let mut scanner = ProcessScanner::new();
        let first_scan = scanner.scan(&root).unwrap();
        assert!(first_scan.iter().all(|process| process.cpu_percent == 0.0));
        let normal = first_scan
            .iter()
            .find(|process| process.pid == 100)
            .unwrap();
        assert_eq!(normal.cmdline, "/usr/bin/normal --flag value");
        let kernel_thread = first_scan
            .iter()
            .find(|process| process.pid == 200)
            .unwrap();
        assert_eq!(kernel_thread.rss_kib, 0);
        assert!(kernel_thread.cmdline.is_empty());

        fs::write(
            root.join("proc/stat"),
            "cpu 150 0 0 150 0 0 0 0\ncpu0 75 0 0 75\ncpu1 75 0 0 75\n",
        )
        .unwrap();
        fs::write(
            root.join("proc/100/stat"),
            "100 (normal process) S 1 0 0 0 0 0 0 0 0 0 70 10 0 0 5 0 2 0 1000 0\n",
        )
        .unwrap();
        fs::remove_dir_all(root.join("proc/200")).unwrap();
        fs::write(root.join("proc/300/stat"), "garbage").unwrap();

        let processes = scanner.scan(&root).unwrap();
        let normal = processes.iter().find(|process| process.pid == 100).unwrap();
        assert_eq!(normal.cpu_percent, 100.0);
        assert!(!processes.iter().any(|process| process.pid == 200));
        // Malformed stat is skipped, not fatal.
        assert!(!processes.iter().any(|process| process.pid == 300));
        assert!(processes.iter().any(|process| process.pid == 400));
        assert_eq!(scanner.previous_ticks.len(), processes.len());

        fs::write(
            root.join("proc/stat"),
            "cpu 200 0 0 200 0 0 0 0\ncpu0 100 0 0 100\ncpu1 100 0 0 100\n",
        )
        .unwrap();
        fs::write(
            root.join("proc/100/stat"),
            "100 (new process) R 1 0 0 0 0 0 0 0 0 0 120 10 0 0 5 0 2 0 2000 0\n",
        )
        .unwrap();
        let reused = scanner
            .scan(&root)
            .unwrap()
            .into_iter()
            .find(|process| process.pid == 100)
            .unwrap();
        assert_eq!(reused.cpu_percent, 0.0);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn non_utf8_names_are_not_hidden() {
        let root = temp_fixture();
        let mut stat = b"500 (hog".to_vec();
        stat.extend_from_slice(&[0xff, 0xfe]);
        stat.extend_from_slice(b") R 1 0 0 0 0 0 0 0 0 0 5 5 0 0 20 0 1 0 5000 0\n");
        fs::create_dir_all(root.join("proc/500")).unwrap();
        fs::write(root.join("proc/500/stat"), &stat).unwrap();
        let mut status = b"Name:\thog".to_vec();
        status.extend_from_slice(&[0xff, 0xfe]);
        status.extend_from_slice(b"\nUid:\t1000\t1000\t1000\t1000\nVmRSS:\t   100 kB\n");
        fs::write(root.join("proc/500/status"), &status).unwrap();
        fs::write(root.join("proc/500/cmdline"), b"").unwrap();

        let procs = ProcessScanner::new().scan(&root).unwrap();
        let hog = procs
            .iter()
            .find(|p| p.pid == 500)
            .expect("non-UTF-8 process listed");
        assert!(hog.name.starts_with("hog"));
        assert_eq!(hog.rss_kib, 100);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn scans_real_proc_for_this_process() {
        let process = ProcessScanner::new()
            .scan(Path::new("/"))
            .unwrap()
            .into_iter()
            .find(|process| process.pid == std::process::id())
            .unwrap();
        assert!(process.rss_kib > 0);
    }
}
