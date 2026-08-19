//! Types and unix-socket framing shared between the MCP server and the review pane.
//!
//! Transport: one JSON object per line over a unix socket. The MCP server binds the
//! socket, passes its path to the pane via the ANNOT_SOCKET env var at pane-open,
//! and sends tagged `ServerMsg` frames: first the `Request`, then any number of
//! `Goto` navigation pushes while the human reviews (agent-guided walkthroughs).
//! The pane answers with a single `ReviewResult` line whenever the reviewer
//! decides; the server reads it on a mailbox thread, so callers choose blocking
//! (`exchange`) or non-blocking (`open` + `try_take`) semantics. EOF without a
//! result means the pane died or was closed — that maps to `Verdict::Cancelled`,
//! never a hang.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::{de::DeserializeOwned, Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 2;

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

/// Server→pane messages. v2: the channel stays open after the initial
/// request so the server can push navigation while the human reviews —
/// the substrate for agent-guided walkthroughs.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMsg {
    Request(ReviewRequest),
    Goto(GotoTarget),
}

/// A navigation push: focus this file at this NEW-side line (the post-change
/// numbering annotations use). Unknown files or out-of-range lines are
/// clamped or ignored by the pane, never an error — navigation is advisory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GotoTarget {
    pub file: String,
    pub line: u32,
    /// Which view the pane should show the target in: "diff" or "source".
    /// `None` keeps the pane's current view. Advisory like the rest of the
    /// target — a file with no source side (deleted/binary) ignores a
    /// source request. `#[serde(default)]` keeps the wire format compatible
    /// with v2 peers that don't send it (per the protocol-change policy).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub view: Option<String>,
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

    /// Send the request and hand back an open channel: the caller can push
    /// navigation to the pane and take the verdict whenever it lands. The
    /// verdict is read on a mailbox thread so nothing here ever blocks.
    ///
    /// EOF (the pane closed the connection without answering) is a genuine
    /// `Cancelled` verdict — that's a normal way for a review to end. A
    /// malformed line or any other read failure is a broken channel, not a
    /// human decision, so it's kept out of the mailbox's `Cancelled`
    /// vocabulary and surfaced as an `Err` instead — callers that treat
    /// `Cancelled` as "nothing to worry about" must not see a protocol bug
    /// dressed up as one.
    pub fn open(mut self, request: &ReviewRequest) -> Result<OpenReview> {
        write_json_line(&mut self.stream, &ServerMsg::Request(request.clone()))?;
        let writer = self.stream.try_clone().context("splitting review socket")?;
        let result = std::sync::Arc::new(std::sync::Mutex::new(None));
        let mailbox = std::sync::Arc::clone(&result);
        let read_stream = self.stream;
        std::thread::spawn(move || {
            let outcome: std::result::Result<ReviewResult, String> = {
                let mut reader = BufReader::new(read_stream);
                match read_json_line::<ReviewResult, _>(&mut reader) {
                    Ok(Some(result)) => Ok(result),
                    Ok(None) => Ok(ReviewResult::cancelled("review pane closed without a verdict")),
                    Err(err) => Err(format!("{err:#}")),
                }
            };
            if let Ok(mut slot) = mailbox.lock() {
                *slot = Some(outcome);
            }
        });
        Ok(OpenReview { writer, result })
    }

    /// Blocking convenience: send the request and wait for the verdict.
    ///
    /// `timeout`: `None` waits forever (the default); a lapsed deadline maps
    /// to a `Cancelled` verdict rather than an error — a slow or silent
    /// reviewer is not a protocol failure. A broken channel (malformed
    /// reply, read error) is a different thing and propagates as `Err`.
    pub fn exchange(self, request: &ReviewRequest, timeout: Option<Duration>) -> Result<ReviewResult> {
        self.open(request)?.wait(timeout)
    }
}

/// A review in flight: write half for navigation pushes, mailbox for the
/// verdict. Dropping it abandons the review (the pane's eventual reply lands
/// in the reader thread and is discarded).
pub struct OpenReview {
    writer: UnixStream,
    result: std::sync::Arc<std::sync::Mutex<Option<std::result::Result<ReviewResult, String>>>>,
}

impl OpenReview {
    /// Push navigation to the open pane (advisory; the pane clamps/ignores
    /// unknown targets).
    pub fn goto(&mut self, target: &GotoTarget) -> Result<()> {
        write_json_line(&mut self.writer, &ServerMsg::Goto(target.clone()))
            .context("pushing navigation to the review pane")
    }

    /// Take the verdict if the reviewer has delivered one. `Err` means the
    /// channel itself broke (malformed reply, read failure) — distinct from
    /// a `Cancelled` verdict, which means the pane closed cleanly without
    /// deciding.
    pub fn try_take(&self) -> Option<Result<ReviewResult>> {
        self.result
            .lock()
            .ok()
            .and_then(|mut slot| slot.take())
            .map(|outcome| outcome.map_err(|msg| anyhow::anyhow!(msg)))
    }

    /// Wait for the verdict, polling the mailbox. `None` waits forever; a
    /// lapsed deadline yields a `Cancelled` verdict ("review timed out").
    ///
    /// The background reader spawned by `open` owns the other half of this
    /// socket and blocks in a read until the pane answers or the connection
    /// closes. On a lapsed deadline nothing has told the pane the caller
    /// gave up, so that read would otherwise block forever (and the pane
    /// stays connected to a review nobody is waiting on) — shutting the
    /// socket down here unblocks the reader with an EOF so it can exit.
    pub fn wait(&self, timeout: Option<Duration>) -> Result<ReviewResult> {
        let start = std::time::Instant::now();
        loop {
            if let Some(result) = self.try_take() {
                return result;
            }
            if let Some(deadline) = timeout {
                if start.elapsed() >= deadline {
                    let _ = self.writer.shutdown(std::net::Shutdown::Both);
                    return Ok(ReviewResult::cancelled("review timed out"));
                }
            }
            std::thread::sleep(Duration::from_millis(50));
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
        match read_json_line::<ServerMsg, _>(&mut self.reader)? {
            Some(ServerMsg::Request(req)) if req.version == PROTOCOL_VERSION => Ok(req),
            Some(ServerMsg::Request(req)) => bail!(
                "protocol version mismatch: pane speaks v{PROTOCOL_VERSION}, server sent v{}",
                req.version
            ),
            Some(ServerMsg::Goto(_)) => bail!("protocol error: navigation before the review request"),
            None => bail!("server hung up before sending a review request"),
        }
    }

    /// Split into a result-sender and a live stream of navigation pushes.
    /// The buffered reader moves onto a thread (it may hold unconsumed
    /// bytes, so it cannot be re-cloned); the thread exits on server EOF or
    /// when the receiver is dropped.
    pub fn into_channel(self) -> (PaneChannel, mpsc::Receiver<GotoTarget>) {
        let (tx, rx) = mpsc::channel();
        let mut reader = self.reader;
        std::thread::spawn(move || loop {
            match read_json_line::<ServerMsg, _>(&mut reader) {
                Ok(Some(ServerMsg::Goto(target))) => {
                    if tx.send(target).is_err() {
                        break; // UI gone; nothing to navigate
                    }
                }
                Ok(Some(ServerMsg::Request(_))) => {} // one request per session; ignore
                Ok(None) | Err(_) => break,           // server hung up
            }
        });
        (PaneChannel { stream: self.stream }, rx)
    }
}

/// The pane's write half after `into_channel`: delivers the one verdict.
pub struct PaneChannel {
    stream: UnixStream,
}

impl PaneChannel {
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
            let (mut channel, _goto_rx) = conn.into_channel();
            channel.send_result(&ReviewResult {
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
            .exchange(
                &ReviewRequest {
                    version: PROTOCOL_VERSION,
                    working_dir: "/tmp/repo".into(),
                    baseline: None,
                    note: None,
                },
                None,
            )
            .unwrap();
        pane.join().unwrap();

        assert_eq!(result.verdict, Verdict::RequestChanges);
        assert_eq!(result.annotations.len(), 1);
        assert_eq!(result.annotations[0].lines.start, 3);
        let _ = std::fs::remove_file(&sock);
    }

    #[test]
    fn goto_pushes_reach_the_pane_while_the_review_is_open() {
        let dir = std::env::temp_dir().join(format!("annot-test-goto-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("t.sock");
        let _ = std::fs::remove_file(&sock);
        let listener = UnixListener::bind(&sock).unwrap();

        let sock_client = sock.clone();
        let pane = std::thread::spawn(move || {
            let mut conn = PaneConnection::connect(&sock_client).unwrap();
            let _ = conn.receive_request().unwrap();
            let (mut channel, goto_rx) = conn.into_channel();
            // Navigation arrives while the reviewer is "browsing".
            let first = goto_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            assert_eq!((first.file.as_str(), first.line), ("src/a.rs", 10));
            let second = goto_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            assert_eq!((second.file.as_str(), second.line), ("src/b.rs", 3));
            channel
                .send_result(&ReviewResult {
                    version: PROTOCOL_VERSION,
                    verdict: Verdict::Approve,
                    summary: None,
                    annotations: Vec::new(),
                })
                .unwrap();
        });

        let handoff = Handoff::accept(listener, Duration::from_secs(5)).unwrap();
        let mut open = handoff
            .open(&ReviewRequest {
                version: PROTOCOL_VERSION,
                working_dir: "/tmp/repo".into(),
                baseline: None,
                note: None,
            })
            .unwrap();
        // Nothing delivered yet: the mailbox is empty, not blocking.
        assert!(open.try_take().is_none());
        open.goto(&GotoTarget { file: "src/a.rs".into(), line: 10, view: None }).unwrap();
        open.goto(&GotoTarget { file: "src/b.rs".into(), line: 3, view: None }).unwrap();
        let result = open.wait(Some(Duration::from_secs(5))).unwrap();
        pane.join().unwrap();
        assert_eq!(result.verdict, Verdict::Approve);
        let _ = std::fs::remove_file(&sock);
    }

    #[test]
    fn pane_rejects_a_mismatched_protocol_version() {
        let dir = std::env::temp_dir().join(format!("annot-test-ver-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("t.sock");
        let _ = std::fs::remove_file(&sock);
        let listener = UnixListener::bind(&sock).unwrap();

        let sock_client = sock.clone();
        let pane = std::thread::spawn(move || {
            let mut conn = PaneConnection::connect(&sock_client).unwrap();
            conn.receive_request().unwrap_err()
        });

        let (mut stream, _) = listener.accept().unwrap();
        write_json_line(
            &mut stream,
            &ServerMsg::Request(ReviewRequest {
                version: 1, // stale server speaking the old revision
                working_dir: "/tmp".into(),
                baseline: None,
                note: None,
            }),
        )
        .unwrap();
        let err = pane.join().unwrap();
        assert!(err.to_string().contains("version mismatch"), "{err}");
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
            .exchange(
                &ReviewRequest {
                    version: PROTOCOL_VERSION,
                    working_dir: "/".into(),
                    baseline: None,
                    note: None,
                },
                None,
            )
            .unwrap();
        pane.join().unwrap();
        assert_eq!(result.verdict, Verdict::Cancelled);
        let _ = std::fs::remove_file(&sock);
    }

    #[test]
    fn exchange_times_out_when_pane_never_replies() {
        let dir = std::env::temp_dir().join(format!("annot-test-timeout-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("t.sock");
        let _ = std::fs::remove_file(&sock);
        let listener = UnixListener::bind(&sock).unwrap();

        let sock_client = sock.clone();
        let pane = std::thread::spawn(move || {
            // Connect and read the request, but never send a verdict — hold
            // the connection open (not EOF) so the timeout path is what
            // has to save us, not the EOF path.
            let mut conn = PaneConnection::connect(&sock_client).unwrap();
            let _ = conn.receive_request().unwrap();
            std::thread::sleep(Duration::from_secs(5));
        });

        let handoff = Handoff::accept(listener, Duration::from_secs(5)).unwrap();
        let result = handoff
            .exchange(
                &ReviewRequest {
                    version: PROTOCOL_VERSION,
                    working_dir: "/".into(),
                    baseline: None,
                    note: None,
                },
                Some(Duration::from_millis(200)),
            )
            .unwrap();

        assert_eq!(result.verdict, Verdict::Cancelled);
        assert_eq!(result.summary.as_deref(), Some("review timed out"));
        let _ = std::fs::remove_file(&sock);
        // Don't join `pane` — it's asleep for several seconds and we don't
        // need it to finish to consider this test done.
        drop(pane);
    }

    #[test]
    fn timeout_shuts_the_socket_down_so_the_pane_sees_it_close() {
        use std::io::Read;

        let dir = std::env::temp_dir().join(format!("annot-test-timeout-shutdown-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("t.sock");
        let _ = std::fs::remove_file(&sock);
        let listener = UnixListener::bind(&sock).unwrap();

        let sock_client = sock.clone();
        let pane = std::thread::spawn(move || {
            // Connect and read the request, then hold the connection open
            // without answering — the timeout is what has to close things,
            // not the pane hanging up on its own.
            let mut conn = PaneConnection::connect(&sock_client).unwrap();
            let _ = conn.receive_request().unwrap();
            // A bounded read timeout so an unfixed server (socket left
            // open, reader thread blocked forever) fails this test instead
            // of hanging it: the read should observe EOF well within this
            // window once `wait` shuts the socket down on its own timeout.
            conn.stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut buf = [0u8; 1];
            conn.stream.read(&mut buf)
        });

        let handoff = Handoff::accept(listener, Duration::from_secs(5)).unwrap();
        let result = handoff
            .exchange(
                &ReviewRequest {
                    version: PROTOCOL_VERSION,
                    working_dir: "/".into(),
                    baseline: None,
                    note: None,
                },
                Some(Duration::from_millis(200)),
            )
            .unwrap();
        assert_eq!(result.verdict, Verdict::Cancelled);

        // The pane's read must observe EOF (0 bytes) once the caller's
        // deadline passes, proving the server closed the socket rather than
        // leaving the background reader (and this connection) hanging.
        let n = pane.join().unwrap().unwrap();
        assert_eq!(n, 0, "pane's read must see EOF once the timed-out review shuts the socket down");
        let _ = std::fs::remove_file(&sock);
    }

    #[test]
    fn a_malformed_reply_is_a_tool_error_not_a_cancelled_verdict() {
        let dir = std::env::temp_dir().join(format!("annot-test-malformed-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("t.sock");
        let _ = std::fs::remove_file(&sock);
        let listener = UnixListener::bind(&sock).unwrap();

        let sock_client = sock.clone();
        let pane = std::thread::spawn(move || {
            let mut conn = PaneConnection::connect(&sock_client).unwrap();
            let _ = conn.receive_request().unwrap();
            // A broken channel, not a human decision: reply with a line
            // that isn't a valid ReviewResult at all.
            conn.stream.write_all(b"not valid json\n").unwrap();
        });

        let handoff = Handoff::accept(listener, Duration::from_secs(5)).unwrap();
        let result = handoff.exchange(
            &ReviewRequest {
                version: PROTOCOL_VERSION,
                working_dir: "/".into(),
                baseline: None,
                note: None,
            },
            None,
        );
        pane.join().unwrap();

        // Must be a genuine Err, not an Ok(Cancelled) — a caller that
        // treats Cancelled as "nothing to worry about" must see this.
        assert!(result.is_err(), "a malformed reply must surface as an error, not {result:?}");
        let _ = std::fs::remove_file(&sock);
    }

    #[test]
    fn docs_example_verdict_matches_the_current_protocol_version() {
        // docs/mcp-tools.md documents review_changes/collect_review's
        // response shape with a literal example ReviewResult. It's prose,
        // not code, so nothing else catches it drifting out of sync with a
        // future PROTOCOL_VERSION bump the way this one did (the example was
        // still showing v1 after the wire format moved to v2) — parse it for
        // real and pin its version field to the constant.
        let docs = include_str!("../docs/mcp-tools.md");
        let start = docs.find("```json\n").expect("docs' example verdict block") + "```json\n".len();
        let end = start + docs[start..].find("```").expect("closing fence for the example block");
        let example: ReviewResult = serde_json::from_str(docs[start..end].trim())
            .expect("docs' example verdict must itself be valid ReviewResult JSON");
        assert_eq!(
            example.version, PROTOCOL_VERSION,
            "docs/mcp-tools.md's documented example verdict is stale — update it alongside any PROTOCOL_VERSION bump"
        );
    }
}
