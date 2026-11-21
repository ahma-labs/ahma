//! Append one JSON value as one line to a log file: the one way ahma's
//! JSON-lines logs (audit trails, histories, transcripts) are written.
//!
//! Each line is serialized whole, newline included, and handed to the file in
//! a single write on a file opened with `O_APPEND`, so concurrent writers never
//! interleave inside a line, and a crash costs at most the last record.
//! Creating the directory, rotation and file permissions are the caller's:
//! they differ per log (a missing task-vault directory is an error, not
//! something to create).

use std::io;
use std::path::Path;

use serde::Serialize;

fn line_of(value: &impl Serialize) -> io::Result<Vec<u8>> {
    let mut line = serde_json::to_vec(value).map_err(io::Error::other)?;
    line.push(b'\n');
    Ok(line)
}

/// Append `value` to `path` as one JSON line.
pub async fn append(path: &Path, value: &impl Serialize) -> io::Result<()> {
    use tokio::io::AsyncWriteExt as _;
    let line = line_of(value)?;
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await?;
    file.write_all(&line).await?;
    // A tokio `File` hands the write to the blocking pool, and `write_all` can
    // return before the bytes reach the OS: a reader told the record is
    // written could otherwise miss it.
    file.flush().await
}

/// [`append`], for code that is not async.
pub fn append_blocking(path: &Path, value: &impl Serialize) -> io::Result<()> {
    use std::io::Write as _;
    let line = line_of(value)?;
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?
        .write_all(&line)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Writers appending together never interleave inside a line, and every
    /// record lands, through either entry point.
    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_appends_keep_every_line_whole() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log.jsonl");
        let payload = "x".repeat(4096);
        let tasks: Vec<_> = (0..16)
            .map(|i| {
                let path = path.clone();
                let payload = payload.clone();
                tokio::spawn(async move {
                    let record = serde_json::json!({ "n": i, "payload": payload });
                    if i % 2 == 0 {
                        append(&path, &record).await.unwrap();
                    } else {
                        tokio::task::spawn_blocking(move || append_blocking(&path, &record))
                            .await
                            .unwrap()
                            .unwrap();
                    }
                })
            })
            .collect();
        for t in tasks {
            t.await.unwrap();
        }
        let text = std::fs::read_to_string(&path).unwrap();
        let mut seen: Vec<i64> = text
            .lines()
            .map(|l| {
                serde_json::from_str::<serde_json::Value>(l).unwrap()["n"]
                    .as_i64()
                    .unwrap()
            })
            .collect();
        seen.sort();
        assert_eq!(seen, (0..16).collect::<Vec<_>>());
    }
}
