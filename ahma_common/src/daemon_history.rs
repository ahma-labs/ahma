//! On-disk operation history for the per-user daemon (SPEC R-DAEMON.7).
//!
//! The hub's in-memory history dies with the daemon, and the daemon exits when
//! it has been idle. Without a file, "what ran in this project half an hour
//! ago" is answerable only while the process that happened to observe it is
//! still alive — which is exactly the case a user does not think about before
//! closing their editor.
//!
//! ## Shape
//!
//! One JSON object per line, append-only. This is the daemon's own format, not
//! the hub wire, so it is an ordinary tagged enum: nothing outside this process
//! reads it, and it is rewritten wholesale by whichever daemon owns the file.
//!
//! * `Started` — the operation's start record, with the instance it belongs to,
//!   so a replayed op still has a section to appear under.
//! * `Finished` — its terminal record **plus** the output window, written once
//!   at completion. Per-line persistence would turn a chatty build into
//!   megabytes of disk writes for a window that is bounded anyway.
//! * `InstanceEnded` — when an instance disconnected.
//!
//! ## Bounds
//!
//! The file is rotated by rename at [`HISTORY_MAX_BYTES`], keeping exactly one
//! predecessor, and only the last [`HISTORY_REPLAY_WINDOW`] is loaded back. A
//! torn final line — the normal result of a crash mid-write — is skipped with a
//! warning rather than treated as corruption, as is a record whose `kind` this
//! version does not know.

use crate::daemon_hub::{DaemonEvent, HISTORY_REPLAY_WINDOW, InstanceInfo};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tokio::io::AsyncWriteExt;
use tracing::{debug, warn};

/// Rotate once the file passes this size, keeping one predecessor.
pub const HISTORY_MAX_BYTES: u64 = 8 * 1024 * 1024;

/// Upper bound on lines read back at startup, so a pathological file cannot
/// stall the daemon's start.
const MAX_LINES_LOADED: usize = 20_000;

/// One line of the history file.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum HistoryRecord {
    /// An operation started, and the instance it belongs to.
    Started {
        ts: u64,
        instance: InstanceInfo,
        event: DaemonEvent,
    },
    /// An operation finished, with the output window as it stood at the end.
    Finished {
        ts: u64,
        instance_id: String,
        event: DaemonEvent,
        #[serde(default)]
        tail: Vec<(String, bool)>,
    },
    /// An instance disconnected.
    InstanceEnded { ts: u64, instance_id: String },
}

impl HistoryRecord {
    /// When this record happened, for the load window.
    pub fn timestamp(&self) -> u64 {
        match self {
            Self::Started { ts, .. }
            | Self::Finished { ts, .. }
            | Self::InstanceEnded { ts, .. } => *ts,
        }
    }
}

/// Where the history file lives.
///
/// Under a test harness it is per-run and private, keyed by the same
/// discriminator as the sockets (SPEC R-ISO.1): a test that wrote into the
/// developer's real history would also *read* it back, replaying a stranger's
/// operations into its own assertions.
pub fn history_path() -> Option<PathBuf> {
    if crate::test_isolation::spawned_under_test_harness() {
        return Some(std::env::temp_dir().join(format!(
            "ahma-test-history-{}.jsonl",
            crate::test_isolation::test_run_discriminator()
        )));
    }
    crate::daemon_hub::runtime_dir().map(|dir| live_history_path(&dir))
}

/// The history file inside a given runtime directory.
///
/// Split out so the path rule is one expression that a test can exercise
/// without a runtime directory of the machine's choosing.
fn live_history_path(runtime_dir: &Path) -> PathBuf {
    runtime_dir.join("history.jsonl")
}

/// The rotated predecessor of `path`.
fn previous_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".1");
    PathBuf::from(name)
}

/// A single-writer append task. Connection handlers send records; this owns the
/// file handle, so there is exactly one writer and no interleaved half-lines.
pub struct HistoryWriter {
    tx: tokio::sync::mpsc::UnboundedSender<WriterMsg>,
    done: tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}

enum WriterMsg {
    Record(Box<HistoryRecord>),
    Flush(tokio::sync::oneshot::Sender<()>),
}

impl HistoryWriter {
    /// Start the writer task for `path`. Returns `None` when no path is
    /// available (no home directory), in which case history is in-memory only —
    /// a degraded mode, not a failure.
    pub fn start(path: Option<PathBuf>) -> Option<Self> {
        let path = path?;
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<WriterMsg>();
        let handle = tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                match msg {
                    WriterMsg::Record(record) => {
                        if let Err(e) = append_record(&path, &record).await {
                            // A history file that cannot be written is a lost
                            // convenience, never a reason to stop serving.
                            debug!("daemon history: append failed: {e}");
                        }
                    }
                    WriterMsg::Flush(ack) => {
                        let _ = ack.send(());
                    }
                }
            }
        });
        Some(Self {
            tx,
            done: tokio::sync::Mutex::new(Some(handle)),
        })
    }

    /// Queue a record. Never blocks the caller and never fails the operation it
    /// describes.
    pub fn record(&self, record: HistoryRecord) {
        let _ = self.tx.send(WriterMsg::Record(Box::new(record)));
    }

    /// Wait until everything queued so far has been written. Called on the
    /// shutdown path so an exiting daemon does not lose the last few records.
    pub async fn flush(&self) {
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
        if self.tx.send(WriterMsg::Flush(ack_tx)).is_ok() {
            let _ = ack_rx.await;
        }
    }

    /// Stop the writer task after draining what it has.
    pub async fn shutdown(&self) {
        self.flush().await;
        if let Some(handle) = self.done.lock().await.take() {
            handle.abort();
        }
    }
}

/// Append one record, rotating first when the file has grown past its cap.
async fn append_record(path: &Path, record: &HistoryRecord) -> std::io::Result<()> {
    if let Ok(meta) = tokio::fs::metadata(path).await
        && meta.len() >= HISTORY_MAX_BYTES
    {
        // Rename rather than truncate: a reader holding the old file keeps
        // reading a consistent file, and one generation of older history
        // survives the rotation.
        let _ = tokio::fs::rename(path, previous_path(path)).await;
    }
    let mut line = serde_json::to_string(record).map_err(std::io::Error::other)?;
    line.push('\n');
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await?;
    file.write_all(line.as_bytes()).await?;
    // 0600: this file names every command run on the user's behalf.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).await;
    }
    Ok(())
}

/// Read back the records newer than `cutoff_ms`, oldest first.
///
/// Reads the rotated predecessor before the current file so ordering survives a
/// rotation. Tolerant by design: a line that does not parse — the torn last
/// write of a killed daemon, or a record from a newer version — is skipped with
/// a warning. A history file is a convenience; refusing to start because one
/// line of it is malformed would trade a small loss for a total one.
pub async fn load_recent(path: &Path, cutoff_ms: u64) -> Vec<HistoryRecord> {
    let mut out = Vec::new();
    // The rotated predecessor and the current file are independent reads;
    // fetch both concurrently, then process in fixed (previous, then
    // current) order so replay ordering across a rotation is unaffected.
    let (prev_text, cur_text) = tokio::join!(
        tokio::fs::read_to_string(previous_path(path)),
        tokio::fs::read_to_string(path)
    );
    for (candidate, text) in [
        (previous_path(path), prev_text),
        (path.to_path_buf(), cur_text),
    ] {
        let Ok(text) = text else {
            continue;
        };
        let mut skipped = 0usize;
        for line in text.lines() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<HistoryRecord>(line) {
                Ok(record) if record.timestamp() >= cutoff_ms => out.push(record),
                Ok(_) => {}
                Err(_) => skipped += 1,
            }
        }
        if skipped > 0 {
            warn!(
                "daemon history: skipped {skipped} unreadable line(s) in {} \
                 (a torn final write, or records from a newer ahma)",
                candidate.display()
            );
        }
    }
    if out.len() > MAX_LINES_LOADED {
        let start = out.len() - MAX_LINES_LOADED;
        out.drain(..start);
    }
    out
}

/// The cutoff for [`load_recent`]: one replay window ago.
pub fn replay_cutoff_ms(now_ms: u64) -> u64 {
    now_ms.saturating_sub(HISTORY_REPLAY_WINDOW.as_millis() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon_hub::OpStatus;

    fn instance(id: &str) -> InstanceInfo {
        InstanceInfo {
            id: id.into(),
            pid: 7,
            mode: "stdio".into(),
            scope: "/ws".into(),
            label: "ahma".into(),
            client: Some("claude-code".into()),
            session_id: Some("sess".into()),
            client_pid: Some(11),
            sampling: false,
            ended_epoch_ms: None,
        }
    }

    fn started(id: &str) -> DaemonEvent {
        DaemonEvent::OpStarted {
            id: id.into(),
            tool_name: "run_terminal_command".into(),
            description: "d".into(),
            scope: "/ws".into(),
            parent_id: None,
            started_epoch_ms: Some(1_000),
            title: Some("cargo build".into()),
            cwd: Some("/ws".into()),
            command: Some("cargo build".into()),
            origin: Some("claude-code".into()),
            partial: false,
            unsandboxed: false,
        }
    }

    fn finished(id: &str) -> DaemonEvent {
        DaemonEvent::OpFinished {
            id: id.into(),
            status: OpStatus::Completed,
            result_summary: Some("ok".into()),
            duration_ms: 12,
            ended_epoch_ms: Some(2_000),
            exit_code: Some(0),
            denial: None,
            interrupted: false,
        }
    }

    #[tokio::test]
    async fn records_round_trip_one_per_line() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("history.jsonl");
        let writer = HistoryWriter::start(Some(path.clone())).unwrap();

        writer.record(HistoryRecord::Started {
            ts: 1_000,
            instance: instance("i1"),
            event: started("op-1"),
        });
        writer.record(HistoryRecord::Finished {
            ts: 2_000,
            instance_id: "i1".into(),
            event: finished("op-1"),
            tail: vec![("compiling".into(), false)],
        });
        writer.record(HistoryRecord::InstanceEnded {
            ts: 2_100,
            instance_id: "i1".into(),
        });
        writer.flush().await;

        let text = tokio::fs::read_to_string(&path).await.unwrap();
        assert_eq!(text.lines().count(), 3, "one record per line: {text}");

        let loaded = load_recent(&path, 0).await;
        assert_eq!(loaded.len(), 3);
        match &loaded[1] {
            HistoryRecord::Finished { tail, .. } => {
                assert_eq!(tail, &vec![("compiling".to_string(), false)]);
            }
            other => panic!("expected Finished, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn load_recent_skips_a_torn_last_line_and_records_outside_the_window() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("history.jsonl");
        let good = serde_json::to_string(&HistoryRecord::Started {
            ts: 5_000,
            instance: instance("i1"),
            event: started("recent"),
        })
        .unwrap();
        let stale = serde_json::to_string(&HistoryRecord::Started {
            ts: 10,
            instance: instance("i1"),
            event: started("ancient"),
        })
        .unwrap();
        // A crash mid-write leaves exactly this: a half-written final line.
        let torn = &good[..good.len() / 2];
        tokio::fs::write(&path, format!("{stale}\n{good}\n{torn}"))
            .await
            .unwrap();

        let loaded = load_recent(&path, 1_000).await;
        assert_eq!(loaded.len(), 1, "only the in-window, complete record");
        assert_eq!(loaded[0].timestamp(), 5_000);
    }

    #[tokio::test]
    async fn an_unknown_record_kind_is_skipped_not_fatal() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("history.jsonl");
        let good = serde_json::to_string(&HistoryRecord::InstanceEnded {
            ts: 5_000,
            instance_id: "i1".into(),
        })
        .unwrap();
        tokio::fs::write(
            &path,
            format!("{{\"kind\":\"SomethingNewer\",\"ts\":5000}}\n{good}\n"),
        )
        .await
        .unwrap();

        let loaded = load_recent(&path, 0).await;
        assert_eq!(loaded.len(), 1, "the readable record still loads");
    }

    #[tokio::test]
    async fn rotation_keeps_one_generation_and_load_reads_both() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("history.jsonl");
        // Pre-fill past the cap so the next append rotates.
        tokio::fs::write(&path, "x".repeat(HISTORY_MAX_BYTES as usize + 1))
            .await
            .unwrap();

        let record = HistoryRecord::InstanceEnded {
            ts: 9_000,
            instance_id: "i1".into(),
        };
        append_record(&path, &record).await.unwrap();

        assert!(previous_path(&path).exists(), "predecessor is kept");
        let len = tokio::fs::metadata(&path).await.unwrap().len();
        assert!(len < HISTORY_MAX_BYTES, "the live file restarts small");
        let loaded = load_recent(&path, 0).await;
        assert_eq!(loaded.len(), 1, "the rotated junk contributes nothing");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn the_history_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("history.jsonl");
        append_record(
            &path,
            &HistoryRecord::InstanceEnded {
                ts: 1,
                instance_id: "i1".into(),
            },
        )
        .await
        .unwrap();
        let mode = tokio::fs::metadata(&path)
            .await
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            mode, 0o600,
            "history names every command run on the user's behalf"
        );
    }

    /// The history belongs beside the sockets, in the directory whose
    /// ownership and mode the daemon actually checks.
    ///
    /// It used to resolve to `~/.ahma` unconditionally while the sockets
    /// resolved to `$XDG_RUNTIME_DIR/ahma`. On any Linux desktop — where that
    /// variable is set — that split the daemon's state across two directories
    /// and, worse, put the record of every command every client ran into the
    /// one of the two that `verify_runtime_dir_secure` never examines. The
    /// 0700-and-owned guarantee is made about the runtime directory; the file
    /// has to live inside it to inherit it.
    #[test]
    fn the_history_lives_beside_the_sockets() {
        // Under the harness both resolve privately, so compare the shapes the
        // production arm produces instead.
        let dir = tempfile::tempdir().unwrap();
        let live = live_history_path(dir.path());
        assert_eq!(live.parent(), Some(dir.path()), "{}", live.display());
        assert_eq!(live.file_name().unwrap(), "history.jsonl");
    }

    #[test]
    fn history_path_is_private_under_test_harness() {
        // The suite always runs under one, so this is the live expectation.
        let path = history_path().expect("a history path is available");
        assert!(
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("ahma-test-history-"),
            "a test must never read or write the developer's history: {}",
            path.display()
        );
        assert!(path.starts_with(std::env::temp_dir()));
    }
}
