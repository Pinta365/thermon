//! Blocking client for `thermond`'s socket, shared by the CLI and the GUI.

use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::protocol::{Body, Command, Request, Response};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(200);
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);

pub struct Client {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
}

impl Client {
    pub fn connect(path: &Path) -> Result<Client, String> {
        let stream = UnixStream::connect(path).map_err(|e| not_running(path, &e))?;
        let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));
        Ok(Client {
            reader: BufReader::new(stream.try_clone().map_err(|e| e.to_string())?),
            writer: stream,
        })
    }

    pub fn send(&mut self, cmd: Command) -> io::Result<()> {
        let mut line = serde_json::to_vec(&Request::new(cmd)).map_err(io::Error::other)?;
        line.push(b'\n');
        self.writer.write_all(&line)
    }

    /// Next raw line from the daemon, without the newline. `None` on EOF.
    pub fn read_line(&mut self) -> io::Result<Option<String>> {
        let mut line = String::new();
        if self.reader.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        line.truncate(line.trim_end().len());
        Ok(Some(line))
    }

    /// Send a one-shot request and wait for its reply. Error replies become `Err`.
    pub fn request(&mut self, cmd: Command) -> Result<Body, String> {
        self.send(cmd).map_err(|e| format!("send: {e}"))?;
        // A stopped or wedged daemon must not hang the caller forever. Long
        // enough for a cold `processes` request at the slowest interval.
        let _ = self.writer.set_read_timeout(Some(REQUEST_TIMEOUT));
        let line = self.read_line();
        let _ = self.writer.set_read_timeout(None);
        let line = line
            .map_err(|e| match e.kind() {
                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => {
                    "thermond didn't answer in time".to_string()
                }
                _ => format!("read: {e}"),
            })?
            .ok_or("thermond closed the connection")?;
        let resp: Response =
            serde_json::from_str(&line).map_err(|e| format!("bad reply from thermond: {e}"))?;
        match resp.body {
            Body::Error { error } => Err(format!("thermond: {error}")),
            body => Ok(body),
        }
    }
}

fn not_running(path: &Path, e: &io::Error) -> String {
    format!(
        "can't reach thermond at {} ({e}).\n\
         Start it with `systemctl --user start thermond`, or use `thermon dump` to read sensors directly.",
        path.display()
    )
}

pub fn socket_path(explicit: Option<PathBuf>) -> Result<PathBuf, String> {
    explicit
        .or_else(crate::protocol::default_socket_path)
        .ok_or_else(|| "XDG_RUNTIME_DIR is not set; pass --socket".to_string())
}
