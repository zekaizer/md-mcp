//! The event journal and commit hook
//! ([ADR-0017](../../../docs/adr/0017-event-journal-and-hook.md)).
//!
//! One JSONL record per durable mutation, appended (fsynced) to
//! `.md-mcp/events.jsonl` right after the vault write succeeds. The stream is
//! at-least-once and best-effort complete: a crash between a mutation's commit
//! and its append can lose that one record. An optional hook command receives
//! each record on stdin, serialized, outside all guards.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt as _;
use tokio::sync::mpsc;

/// The journal file, inside the internal state directory.
const EVENTS_FILE: &str = ".md-mcp/events.jsonl";
/// Where the journal is renamed on rotation (one predecessor kept).
const ROTATED_FILE: &str = ".md-mcp/events.jsonl.1";
/// Rotation threshold.
const ROTATE_BYTES: u64 = 10 * 1024 * 1024;
/// A hook invocation past this is killed (the journal is the catch-up path).
const HOOK_TIMEOUT: Duration = Duration::from_secs(30);

/// One file mutation inside an event record.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "lowercase")]
pub enum EventOp {
    Create { path: String },
    Write { path: String },
    Delete { path: String },
    Move { path: String, to: String },
}

impl EventOp {
    /// The vault paths this op touches (both ends of a move) — what a
    /// path-scoped auto-commit stages (ADR-0018).
    pub fn touched_paths(&self) -> impl Iterator<Item = &str> {
        let (a, b) = match self {
            EventOp::Create { path } | EventOp::Write { path } | EventOp::Delete { path } => {
                (path.as_str(), None)
            }
            EventOp::Move { path, to } => (path.as_str(), Some(to.as_str())),
        };
        std::iter::once(a).chain(b)
    }

    /// Map a committed batch's outcomes to event ops.
    pub fn from_outcomes(outcomes: &[md_core::OpOutcome]) -> Vec<EventOp> {
        outcomes
            .iter()
            .map(|o| match o {
                md_core::OpOutcome::Written { path } => EventOp::Write { path: path.clone() },
                md_core::OpOutcome::Deleted { path, .. } => EventOp::Delete { path: path.clone() },
                md_core::OpOutcome::Moved { from, to } => EventOp::Move {
                    path: from.clone(),
                    to: to.clone(),
                },
            })
            .collect()
    }
}

/// One journal line. `batch_id` ties a destructive batch's record to its
/// transaction; non-destructive items and synthetic sync records carry none.
#[derive(Serialize)]
struct EventRecord<'a> {
    seq: u64,
    ts: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    batch_id: Option<&'a str>,
    tool: &'a str,
    ops: &'a [EventOp],
}

/// Just enough of a record to recover `seq` at startup.
#[derive(Deserialize)]
struct SeqOnly {
    seq: u64,
}

/// The journal writer: appends records with a monotonically increasing `seq`,
/// rotates by size, and feeds the hook queue.
pub struct EventSink {
    file: PathBuf,
    rotated: PathBuf,
    rotate_bytes: u64,
    /// Last emitted seq; the mutex also serializes appends.
    seq: Mutex<u64>,
    hook: Option<mpsc::UnboundedSender<String>>,
}

impl EventSink {
    /// Open the journal under `vault_root`, recovering `seq` from the last
    /// record (falling back to the rotated file after a fresh rotation).
    pub fn open(
        vault_root: &Path,
        hook: Option<mpsc::UnboundedSender<String>>,
    ) -> std::io::Result<Self> {
        let file = vault_root.join(EVENTS_FILE);
        let rotated = vault_root.join(ROTATED_FILE);
        std::fs::create_dir_all(file.parent().expect("events file has a parent"))?;
        let seq = last_seq(&file).or_else(|| last_seq(&rotated)).unwrap_or(0);
        Ok(Self {
            file,
            rotated,
            rotate_bytes: ROTATE_BYTES,
            seq: Mutex::new(seq),
            hook,
        })
    }

    /// Override the rotation threshold (tests).
    #[must_use]
    pub fn with_rotate_bytes(mut self, n: u64) -> Self {
        self.rotate_bytes = n;
        self
    }

    /// Append one record (fsynced) and feed it to the hook queue. Returns the
    /// record's `seq`.
    pub fn emit(
        &self,
        tool: &str,
        batch_id: Option<&str>,
        ops: &[EventOp],
    ) -> std::io::Result<u64> {
        let mut seq = self.seq.lock().expect("event sink poisoned");
        *seq += 1;
        let record = EventRecord {
            seq: *seq,
            ts: now_millis(),
            batch_id,
            tool,
            ops,
        };
        let mut line = serde_json::to_string(&record).map_err(std::io::Error::other)?;
        line.push('\n');

        // Rotate first, so one record is never split across files.
        if std::fs::metadata(&self.file).is_ok_and(|m| m.len() >= self.rotate_bytes) {
            std::fs::rename(&self.file, &self.rotated)?;
        }
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.file)?;
        f.write_all(line.as_bytes())?;
        f.sync_data()?;

        if let Some(tx) = &self.hook {
            // A closed queue (hook task gone) only loses push delivery; the
            // journal keeps the record.
            let _ = tx.send(line);
        }
        Ok(*seq)
    }
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// The `seq` of a journal file's last well-formed record.
fn last_seq(path: &Path) -> Option<u64> {
    let text = std::fs::read_to_string(path).ok()?;
    text.lines()
        .rev()
        .find_map(|l| serde_json::from_str::<SeqOnly>(l).ok())
        .map(|r| r.seq)
}

/// Spawn the serialized hook consumer; the returned sender is the queue.
///
/// Each queued record spawns `sh -c <command>` with the record JSON on stdin.
/// stdout is discarded (under the stdio transport it would corrupt JSON-RPC);
/// stderr passes through to the server log. Failures and timeouts are logged
/// and never retried — a consumer that missed pushes re-reads the journal from
/// its last processed `seq`.
pub fn spawn_hook(command: String) -> mpsc::UnboundedSender<String> {
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    tokio::spawn(async move {
        while let Some(line) = rx.recv().await {
            run_hook_once(&command, &line).await;
        }
    });
    tx
}

async fn run_hook_once(command: &str, line: &str) {
    let child = tokio::process::Command::new("/bin/sh")
        .arg("-c")
        .arg(command)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("cannot spawn commit hook: {e}");
            return;
        }
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(line.as_bytes()).await;
        // Dropping stdin closes the pipe so the hook sees EOF.
    }
    match tokio::time::timeout(HOOK_TIMEOUT, child.wait()).await {
        Ok(Ok(status)) if status.success() => {}
        Ok(Ok(status)) => tracing::warn!("commit hook exited with {status}"),
        Ok(Err(e)) => tracing::warn!("commit hook wait failed: {e}"),
        Err(_) => {
            let _ = child.kill().await;
            tracing::warn!("commit hook timed out after {HOOK_TIMEOUT:?}; killed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_lines(p: &Path) -> Vec<serde_json::Value> {
        std::fs::read_to_string(p)
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    #[test]
    fn emits_records_with_increasing_seq_and_recovers_after_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let sink = EventSink::open(dir.path(), None).unwrap();
        sink.emit(
            "create_notes",
            None,
            &[EventOp::Create {
                path: "a.md".into(),
            }],
        )
        .unwrap();
        sink.emit(
            "delete_notes",
            Some("b-1"),
            &[EventOp::Delete {
                path: "a.md".into(),
            }],
        )
        .unwrap();
        let lines = read_lines(&dir.path().join(EVENTS_FILE));
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["seq"], 1);
        assert_eq!(lines[0]["tool"], "create_notes");
        assert_eq!(lines[0]["ops"][0]["op"], "create");
        assert!(lines[0].get("batch_id").is_none());
        assert_eq!(lines[1]["seq"], 2);
        assert_eq!(lines[1]["batch_id"], "b-1");

        // Reopen: seq continues, never restarts.
        drop(sink);
        let sink = EventSink::open(dir.path(), None).unwrap();
        let seq = sink
            .emit(
                "append_notes",
                None,
                &[EventOp::Write {
                    path: "a.md".into(),
                }],
            )
            .unwrap();
        assert_eq!(seq, 3);
    }

    #[test]
    fn rotates_by_size_and_seq_survives_rotation() {
        let dir = tempfile::tempdir().unwrap();
        let sink = EventSink::open(dir.path(), None)
            .unwrap()
            .with_rotate_bytes(1);
        sink.emit(
            "create_notes",
            None,
            &[EventOp::Create {
                path: "a.md".into(),
            }],
        )
        .unwrap();
        // The file now exceeds 1 byte: the next emit rotates first.
        sink.emit(
            "create_notes",
            None,
            &[EventOp::Create {
                path: "b.md".into(),
            }],
        )
        .unwrap();
        let rotated = read_lines(&dir.path().join(ROTATED_FILE));
        let live = read_lines(&dir.path().join(EVENTS_FILE));
        assert_eq!(rotated.len(), 1);
        assert_eq!(live.len(), 1);
        assert_eq!(live[0]["seq"], 2);

        // Reopen right after a rotation that left the live file small: seq is
        // recovered from the live file's last record.
        drop(sink);
        let sink = EventSink::open(dir.path(), None).unwrap();
        assert_eq!(
            sink.emit(
                "create_notes",
                None,
                &[EventOp::Create {
                    path: "c.md".into()
                }]
            )
            .unwrap(),
            3
        );
    }

    #[tokio::test]
    async fn mutating_tools_emit_records_and_failures_do_not() {
        use crate::MdServer;
        use crate::tools_write::{CreateNotesRequest, EditItem, EditSectionsRequest, NoteInput};
        use rmcp::handler::server::wrapper::Parameters;

        let dir = tempfile::tempdir().unwrap();
        let vault = md_core::Vault::open(dir.path()).unwrap();
        let sink = EventSink::open(dir.path(), None).unwrap();
        let s = MdServer::new(vault).with_event_sink(sink);

        // One created note (its sibling fails: no record for it).
        s.create_notes(Parameters(CreateNotesRequest {
            notes: vec![
                NoteInput {
                    path: "a.md".into(),
                    content: "# A\nbody\n".into(),
                    frontmatter: None,
                },
                NoteInput {
                    path: ".md-mcp/evil.md".into(),
                    content: "x".into(),
                    frontmatter: None,
                },
            ],
            overwrite: false,
        }))
        .await
        .unwrap();

        // One destructive batch.
        s.edit_sections(Parameters(EditSectionsRequest {
            edits: vec![EditItem {
                path: "a.md".into(),
                heading_path: vec!["A".into()],
                occurrence: None,
                operation: crate::tools_write::OperationArg::Replace,
                scope: crate::tools_read::ScopeArg::Body,
                content: Some("new".into()),
                new_heading: None,
                destination: None,
                expected_hash: None,
            }],
        }))
        .await
        .unwrap();

        let lines = read_lines(&dir.path().join(EVENTS_FILE));
        assert_eq!(lines.len(), 2, "one create + one edit batch: {lines:?}");
        assert_eq!(lines[0]["tool"], "create_notes");
        assert_eq!(lines[0]["ops"][0]["path"], "a.md");
        assert_eq!(lines[1]["tool"], "edit_sections");
        assert_eq!(lines[1]["ops"][0]["op"], "write");
        assert!(
            lines[1]["batch_id"].is_string(),
            "destructive batch carries its id"
        );
    }

    #[tokio::test]
    async fn hook_receives_each_record_on_stdin() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("hook-out");
        let tx = spawn_hook(format!("cat >> {}", out.display()));
        let sink = EventSink::open(dir.path(), Some(tx)).unwrap();
        sink.emit(
            "create_notes",
            None,
            &[EventOp::Create {
                path: "a.md".into(),
            }],
        )
        .unwrap();
        // The hook runs asynchronously; poll briefly for its output.
        for _ in 0..100 {
            if out.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let text = std::fs::read_to_string(&out).unwrap();
        assert!(text.contains("\"tool\":\"create_notes\""), "got: {text}");
    }
}
