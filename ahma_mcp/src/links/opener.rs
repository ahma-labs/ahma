//! What opens a link once it is allowed (SPEC R-LINK.3): the OS default
//! handler, outside every sandbox, with ahma's own environment — or, under a
//! test harness, a line in the test home and nothing launched (R-ISO.1).
//!
//! Openers run on the broker thread (SPEC R-CRED.9): `tokio::process` forks
//! on the calling thread, which no Landlock domain binds, so the browser
//! starts unconfined. Never on a `spawn_blocking` thread, which may be
//! created after the process restricted itself.

use super::policy::LinkTarget;
use std::path::PathBuf;
use std::time::Duration;

/// How long the system opener may take to hand the link over. `open` and
/// `xdg-open` return once the browser has it; one still running after this
/// is launching the browser itself, which counts as opened.
const OPEN_WAIT: Duration = Duration::from_secs(10);

/// The file a test run's opens are recorded in, under the test home.
pub const RECORDED_OPENS_FILE: &str = "opened-links.jsonl";

/// Opens a link that passed every check.
#[async_trait::async_trait]
pub trait Opener: Send + Sync + std::fmt::Debug {
    /// Open `target` ([`LinkTarget::as_str`], never the raw argument).
    /// `Err` says why it could not.
    async fn open(&self, target: &LinkTarget) -> Result<(), String>;
}

/// The opener for this process: [`RecordingOpener`] under a test harness, in
/// any build, so no test can open a browser; else [`SystemOpener`].
pub fn for_this_process() -> std::sync::Arc<dyn Opener> {
    if ahma_common::test_isolation::spawned_under_test_harness()
        && let Some(home) = ahma_common::config::ahma_home_dir()
    {
        return std::sync::Arc::new(RecordingOpener {
            file: home.join(".ahma").join(RECORDED_OPENS_FILE),
        });
    }
    std::sync::Arc::new(SystemOpener)
}

/// `/usr/bin/open <URL>` on macOS (LaunchServices maps http and https to the
/// default browser), `xdg-open <URL>` elsewhere.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemOpener;

impl SystemOpener {
    fn program() -> &'static str {
        if cfg!(target_os = "macos") {
            "/usr/bin/open"
        } else {
            "xdg-open"
        }
    }
}

#[async_trait::async_trait]
impl Opener for SystemOpener {
    async fn open(&self, target: &LinkTarget) -> Result<(), String> {
        let program = Self::program();
        // The argument begins `http`, never `-`, so it cannot be read as an
        // option (SPEC R-LINK.2).
        let mut child = tokio::process::Command::new(program)
            .arg(target.as_str())
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|e| format!("{program} could not start: {e}"))?;
        match tokio::time::timeout(OPEN_WAIT, child.wait()).await {
            Ok(Ok(status)) if status.success() => Ok(()),
            Ok(Ok(status)) => Err(format!("{program} failed ({status})")),
            Ok(Err(e)) => Err(format!("{program}: {e}")),
            // Still running: it is the browser, or starting one. Tokio reaps
            // it when it exits.
            Err(_) => Ok(()),
        }
    }
}

/// Appends each link to a file and launches nothing: every opener under a
/// test harness, so a test asserts what would have opened.
#[derive(Debug, Clone)]
pub struct RecordingOpener {
    pub file: PathBuf,
}

#[async_trait::async_trait]
impl Opener for RecordingOpener {
    async fn open(&self, target: &LinkTarget) -> Result<(), String> {
        use std::io::Write as _;
        let line = serde_json::json!({ "url": target.as_str(), "host": target.host() });
        // Plain std I/O on the broker thread, not `tokio::fs`: that would run
        // on a blocking-pool thread, which may be created after the process
        // restricted itself (SPEC R-CRED.9). One short append.
        let write = || -> std::io::Result<()> {
            if let Some(dir) = self.file.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.file)?;
            writeln!(f, "{line}")
        };
        write().map_err(|e| format!("could not record it in {}: {e}", self.file.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SPEC R-LINK.3, R-ISO.1: under a test harness the opener records the
    /// serialization it would open and launches nothing.
    #[tokio::test]
    async fn the_recording_opener_records_what_it_would_open() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join(".ahma").join(RECORDED_OPENS_FILE);
        let opener = RecordingOpener { file: file.clone() };
        let target = LinkTarget::parse("HTTPS://Example.COM/a%20b").expect("a link");
        opener.open(&target).await.unwrap();
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(
            text.contains("\"url\":\"https://example.com/a%20b\""),
            "{text}"
        );
    }

    /// A process under a test harness (nextest's `NEXTEST`, or the explicit
    /// `AHMA_TEST_ISOLATION`) gets the recording opener: nothing is launched.
    #[test]
    fn under_a_test_harness_nothing_is_launched() {
        // SAFETY: nextest runs each test in its own process.
        unsafe { std::env::set_var("AHMA_TEST_ISOLATION", "1") };
        let opener = format!("{:?}", for_this_process());
        assert!(opener.starts_with("RecordingOpener"), "{opener}");
    }
}
