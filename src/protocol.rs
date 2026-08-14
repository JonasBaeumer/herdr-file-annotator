//! Types and unix-socket framing shared between the MCP server and the review pane.
//!
//! Transport: one JSON object per line over a unix socket. The MCP server binds the
//! socket, passes its path to the pane via the ANNOT_SOCKET env var at pane-open,
//! sends a `ReviewRequest` line on accept, and blocks until the pane answers with a
//! single `ReviewResult` line. EOF without a result means the pane died or the user
//! closed it — the server maps that to `Verdict::Cancelled` instead of hanging.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::{de::DeserializeOwned, Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 1;

/// Env var carrying the handoff socket path into the pane process.
pub const SOCKET_ENV: &str = "ANNOT_SOCKET";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewRequest {
    pub version: u32,
    /// Repository the diff lives in (absolute path).
    pub working_dir: String,
    /// Git rev to diff against. `None` = all uncommitted changes (vs HEAD).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline: Option<String>,
    /// Optional message from the agent to the reviewer ("please check the retry logic").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Approve,
    RequestChanges,
    Reject,
    /// Pane closed without a decision (user quit, pane crashed, socket EOF).
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    /// Line numbers refer to the new (post-change) file.
    New,
    /// Line numbers refer to the old (pre-change) file — e.g. a comment on a deletion.
    Old,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LineRange {
    pub start: u32,
    pub end: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Annotation {
    /// Path relative to `working_dir`.
    pub file: String,
    pub lines: LineRange,
    pub side: Side,
    /// Semantic tag: "verify" | "fix" | "question" | "nit".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    pub comment: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewResult {
    pub version: u32,
    pub verdict: Verdict,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default)]
    pub annotations: Vec<Annotation>,
}

impl ReviewResult {
    pub fn cancelled(reason: &str) -> Self {
        ReviewResult {
            version: PROTOCOL_VERSION,
            verdict: Verdict::Cancelled,
            summary: Some(reason.to_string()),
            annotations: Vec::new(),
        }
    }
}

pub fn write_json_line<T: Serialize, W: Write>(writer: &mut W, value: &T) -> Result<()> {
    let mut line = serde_json::to_string(value)?;
    line.push('\n');
    writer.write_all(line.as_bytes())?;
    writer.flush()?;
    Ok(())
}

pub fn read_json_line<T: DeserializeOwned, R: BufRead>(reader: &mut R) -> Result<Option<T>> {
    let mut line = String::new();
    let n = reader.read_line(&mut line)?;
    if n == 0 {
        return Ok(None); // EOF
    }
    let value = serde_json::from_str(line.trim())
        .with_context(|| format!("malformed protocol line: {}", line.trim()))?;
    Ok(Some(value))
}

/// Server side: bind, then wait up to `accept_timeout` for the pane to connect.
/// Accepting runs on a throwaway thread so the timeout can't wedge the caller.
pub struct Handoff {
    stream: UnixStream,
}

impl Handoff {
    pub fn accept(listener: UnixListener, accept_timeout: Duration) -> Result<Self> {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(listener.accept());
        });
        match rx.recv_timeout(accept_timeout) {
            Ok(Ok((stream, _addr))) => Ok(Handoff { stream }),
            Ok(Err(err)) => Err(err).context("accepting pane connection"),
            Err(_) => bail!(
                "review pane did not connect within {}s — is the plugin linked and built?",
                accept_timeout.as_secs()
            ),
        }
    }

    /// Send the request, then block until the pane replies or hangs up.
    pub fn exchange(mut self, request: &ReviewRequest) -> Result<ReviewResult> {
        write_json_line(&mut self.stream, request)?;
        let mut reader = BufReader::new(self.stream);
        match read_json_line::<ReviewResult, _>(&mut reader)? {
            Some(result) => Ok(result),
            None => Ok(ReviewResult::cancelled("review pane closed without a verdict")),
        }
    }
}

/// Pane side: connect to the socket the server advertised via ANNOT_SOCKET.
pub struct PaneConnection {
    stream: UnixStream,
    reader: BufReader<UnixStream>,
}

impl PaneConnection {
    pub fn connect(path: &Path) -> Result<Self> {
        let stream = UnixStream::connect(path)
            .with_context(|| format!("connecting to handoff socket {}", path.display()))?;
        let reader = BufReader::new(stream.try_clone()?);
        Ok(PaneConnection { stream, reader })
    }

    pub fn receive_request(&mut self) -> Result<ReviewRequest> {
        match read_json_line::<ReviewRequest, _>(&mut self.reader)? {
            Some(req) if req.version == PROTOCOL_VERSION => Ok(req),
            Some(req) => bail!(
                "protocol version mismatch: pane speaks v{PROTOCOL_VERSION}, server sent v{}",
                req.version
            ),
            None => bail!("server hung up before sending a review request"),
        }
    }

    pub fn send_result(&mut self, result: &ReviewResult) -> Result<()> {
        write_json_line(&mut self.stream, result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn round_trip_over_unix_socket() {
        let dir = std::env::temp_dir().join(format!("annot-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("t.sock");
        let _ = std::fs::remove_file(&sock);
        let listener = UnixListener::bind(&sock).unwrap();

        let sock_client = sock.clone();
        let pane = std::thread::spawn(move || {
            let mut conn = PaneConnection::connect(&sock_client).unwrap();
            let req = conn.receive_request().unwrap();
            assert_eq!(req.working_dir, "/tmp/repo");
            conn.send_result(&ReviewResult {
                version: PROTOCOL_VERSION,
                verdict: Verdict::RequestChanges,
                summary: Some("see comments".into()),
                annotations: vec![Annotation {
                    file: "src/lib.rs".into(),
                    lines: LineRange { start: 3, end: 5 },
                    side: Side::New,
                    tag: Some("fix".into()),
                    comment: "handle the None case".into(),
                }],
            })
            .unwrap();
        });

        let handoff = Handoff::accept(listener, Duration::from_secs(5)).unwrap();
        let result = handoff
            .exchange(&ReviewRequest {
                version: PROTOCOL_VERSION,
                working_dir: "/tmp/repo".into(),
                baseline: None,
                note: None,
            })
            .unwrap();
        pane.join().unwrap();

        assert_eq!(result.verdict, Verdict::RequestChanges);
        assert_eq!(result.annotations.len(), 1);
        assert_eq!(result.annotations[0].lines.start, 3);
        let _ = std::fs::remove_file(&sock);
    }

    #[test]
    fn eof_maps_to_cancelled() {
        let dir = std::env::temp_dir().join(format!("annot-test-eof-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("t.sock");
        let _ = std::fs::remove_file(&sock);
        let listener = UnixListener::bind(&sock).unwrap();

        let sock_client = sock.clone();
        let pane = std::thread::spawn(move || {
            // Connect, read the request, then hang up without answering.
            let mut conn = PaneConnection::connect(&sock_client).unwrap();
            let _ = conn.receive_request().unwrap();
        });

        let handoff = Handoff::accept(listener, Duration::from_secs(5)).unwrap();
        let result = handoff
            .exchange(&ReviewRequest {
                version: PROTOCOL_VERSION,
                working_dir: "/".into(),
                baseline: None,
                note: None,
            })
            .unwrap();
        pane.join().unwrap();
        assert_eq!(result.verdict, Verdict::Cancelled);
        let _ = std::fs::remove_file(&sock);
    }
}
