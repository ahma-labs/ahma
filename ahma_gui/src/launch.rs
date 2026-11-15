//! `ahma web` returns to the prompt (SPEC R-GUI.12).
//!
//! The command the human typed only launches: it checks the scope and makes
//! sure the per-user hub is up, where its warnings can still be read, then
//! starts the same command again as a background server (`--foreground
//! --ready-pipe`), detached from the terminal, and waits for that server's one
//! line on standard output — its address and first pairing code. It copies the
//! code, prints both, opens the browser and exits. The server stops by itself
//! when no tab is left ([`crate::idle`]).
//!
//! The code crosses from server to launcher on a pipe only the two of them
//! hold: never argv, the environment or a file, which other processes can
//! read.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};

/// How long the launcher waits for the background server to say it is up.
/// It binds a port and connects to a hub that is already running, so this is
/// generous.
const READY_WAIT: Duration = Duration::from_secs(30);

/// The background server's one line to its launcher.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ready {
    pub url: String,
    /// The first pairing code; empty for `ahma desktop`, which pairs nothing.
    #[serde(default)]
    pub code: String,
    /// `ahma desktop` only: why no window could open, so the launcher can
    /// fall back to `ahma web`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unavailable: Option<String>,
}

/// The background server's arguments: the launcher's own, so every flag the
/// human gave (global ones included) means the same there, plus the two that
/// make it the server.
pub fn child_args(launcher_args: impl IntoIterator<Item = OsString>) -> Vec<OsString> {
    let mut args: Vec<OsString> = launcher_args.into_iter().collect();
    args.push("--foreground".into());
    args.push("--ready-pipe".into());
    args
}

/// Start the background server and wait for its [`Ready`] line.
pub async fn start_server() -> Result<Ready> {
    let exe = std::env::current_exe().context("cannot find the ahma executable")?;
    let mut cmd = tokio::process::Command::new(exe);
    cmd.args(child_args(std::env::args_os().skip(1)))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        // Its logs go where ahma's logs go (`ahma logs`); nothing else of it
        // should reach a terminal it has left.
        .stderr(Stdio::null())
        .kill_on_drop(false);
    // Its own process group, so the terminal's Ctrl-C and hang-up are not its
    // (the same detachment the per-user hub has, SPEC R-PROC.3).
    #[cfg(unix)]
    cmd.process_group(0);
    #[cfg(windows)]
    {
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    let mut child = cmd
        .spawn()
        .context("cannot start the ahma web server in the background")?;
    let stdout = child
        .stdout
        .take()
        .context("the background server has no output pipe")?;
    let mut line = String::new();
    let read = tokio::time::timeout(READY_WAIT, BufReader::new(stdout).read_line(&mut line)).await;
    match read {
        Ok(Ok(n)) if n > 0 => {}
        Ok(_) => {
            let status = child.wait().await.ok();
            bail!(
                "the ahma web server stopped before it was ready ({}). \
                 Run `ahma web --foreground` to see why.",
                status.map_or_else(|| "no exit status".to_string(), |s| s.to_string())
            );
        }
        Err(_) => {
            let _ = child.start_kill();
            bail!(
                "the ahma web server did not start within {}s. \
                 Run `ahma web --foreground` to see why.",
                READY_WAIT.as_secs()
            );
        }
    }
    parse_ready(&line)
}

/// Read the server's ready line.
pub fn parse_ready(line: &str) -> Result<Ready> {
    serde_json::from_str(line.trim()).context("the ahma web server's ready line was not understood")
}

/// The ready line the server writes.
pub fn ready_line(ready: &Ready) -> String {
    let mut line = serde_json::to_string(ready).unwrap_or_default();
    line.push('\n');
    line
}

/// What the launcher prints once the server is up.
pub fn summary(ready: &Ready, copied: Option<&str>, opened: Result<(), String>) -> String {
    let mut out = format!(
        "\nahma web is running in the background, on this machine only.\n  Address       {}\n  Pairing code  {}   (single use, good for {} minutes)\n",
        ready.url,
        ready.code,
        crate::auth::CODE_TTL.as_secs() / 60
    );
    match copied {
        Some(tool) => out.push_str(&format!(
            "The code is on your clipboard ({tool}): paste it into the page. It is cleared \
             from the clipboard once used.\n"
        )),
        None => out.push_str("Type the code above into the page.\n"),
    }
    if let Err(e) = opened {
        out.push_str(&format!(
            "Could not open a browser ({e}); open the address yourself.\n"
        ));
    }
    out.push_str(
        "The server stops by itself a minute after its last tab closes. The browser you \
         pair, and its extensions, can do whatever the page can: pair from a profile whose \
         extensions you trust.\n",
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The server gets every argument the human gave, global flags and the
    /// path included, then the two that make it the server.
    #[test]
    fn the_server_runs_the_same_command_plus_two_flags() {
        let args = child_args(
            ["--minimize-tokens", "web", "--port", "8765", "/proj"]
                .into_iter()
                .map(OsString::from),
        );
        assert_eq!(
            args,
            [
                "--minimize-tokens",
                "web",
                "--port",
                "8765",
                "/proj",
                "--foreground",
                "--ready-pipe"
            ]
            .map(OsString::from)
        );
    }

    #[test]
    fn the_ready_line_round_trips_and_is_one_line() {
        let ready = Ready {
            url: "http://127.0.0.1:5000/".into(),
            code: "ABCD-1234".into(),
            unavailable: None,
        };
        let line = ready_line(&ready);
        assert!(line.ends_with('\n') && line.matches('\n').count() == 1);
        assert_eq!(parse_ready(&line).unwrap(), ready);
        assert!(parse_ready("not json").is_err());
    }

    /// The summary says where, what to do with the code, how long it lasts,
    /// that the server goes by itself, and that the browser is trusted
    /// (R-GUI.11).
    #[test]
    fn the_summary_says_where_what_to_do_and_when_it_stops() {
        let ready = Ready {
            url: "http://127.0.0.1:5000/".into(),
            code: "ABCD-1234".into(),
            unavailable: None,
        };
        let copied = summary(&ready, Some("pbcopy"), Ok(()));
        assert!(copied.contains("http://127.0.0.1:5000/") && copied.contains("ABCD-1234"));
        assert!(copied.contains("clipboard") && copied.contains("paste"));
        assert!(copied.contains("stops by itself"));
        assert!(copied.contains("extensions"));
        let typed = summary(&ready, None, Err("no browser".into()));
        assert!(typed.contains("Type the code") && typed.contains("no browser"));
    }
}
