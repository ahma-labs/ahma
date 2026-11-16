//! The controlling terminal (SPEC R-GUI.2).
//!
//! The pairing code is the proof that whoever opens the page is at the keyboard
//! that started `ahma web`. A command running in ahma's sandbox can read the
//! process list, the environment of other processes on some systems, and the
//! browser's own files — but not this terminal. So the code is written here and
//! nowhere else: not standard output, which a harness may capture and a pipe may
//! carry, and not a file. With no terminal there is nowhere safe to put it, and
//! `ahma web --foreground` does not start (a plain `ahma web` copies the code to
//! the clipboard instead, see `crate::launch`).

use anyhow::{Context, Result};
use std::io::{BufRead, BufReader, Write};
use std::sync::{Arc, Mutex};

/// The controlling terminal, opened for writing and for reading Enter.
pub struct Terminal {
    out: Mutex<std::fs::File>,
    input: std::fs::File,
}

impl Terminal {
    /// Open the controlling terminal. `/dev/tty` is the process's own terminal
    /// even when standard output is redirected; Windows has `CONOUT$` and
    /// `CONIN$`.
    pub fn open() -> Result<Arc<Self>> {
        #[cfg(unix)]
        let (out_path, in_path) = ("/dev/tty", "/dev/tty");
        #[cfg(windows)]
        let (out_path, in_path) = ("CONOUT$", "CONIN$");
        let out = std::fs::OpenOptions::new()
            .write(true)
            .open(out_path)
            .with_context(|| {
                "ahma web needs a terminal: it shows a pairing code there that you type \
                 into the page, which proves the page was opened by whoever started it. \
                 Run it from a terminal, not from a script or a pipe."
            })?;
        let input = std::fs::OpenOptions::new()
            .read(true)
            .open(in_path)
            .context("cannot read the controlling terminal")?;
        Ok(Arc::new(Self {
            out: Mutex::new(out),
            input,
        }))
    }

    /// Write `text` to the terminal.
    pub fn say(&self, text: &str) {
        // A writer that panicked mid-line leaves a usable file: keep writing.
        let mut out = self.out.lock().unwrap_or_else(|p| p.into_inner());
        let _ = out.write_all(text.as_bytes());
        let _ = out.flush();
    }

    /// Call `on_enter` each time the human presses Enter here, on a thread of its
    /// own: the one way to unlock pairing, because it is the one thing a command
    /// in the sandbox cannot do.
    pub fn on_enter(&self, on_enter: impl Fn() + Send + 'static) -> Result<()> {
        let input = self.input.try_clone().context("cannot read the terminal")?;
        std::thread::Builder::new()
            .name("ahma-web-tty".into())
            .spawn(move || {
                let mut lines = BufReader::new(input).lines();
                while let Some(Ok(_)) = lines.next() {
                    on_enter();
                }
            })
            .context("cannot start the terminal reader")?;
        Ok(())
    }
}

/// What is shown when the server is up.
pub fn banner(url: &str) -> String {
    format!(
        "\nahma web is listening on this machine only.\n  Address  {url}\n\
         Type the pairing code below into the page. Press Enter here if pairing locks; \
         Ctrl-C stops the server.\n\
         The browser you pair, and its extensions, can do whatever the page can: \
         pair from a profile whose extensions you trust.\n"
    )
}

/// What is shown each time a pairing code is made.
pub fn code_line(code: &str, first: bool) -> String {
    if first {
        format!("  Pairing code  {code}   (single use)\n")
    } else {
        format!("  Pairing code for another tab  {code}   (single use)\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_banner_says_where_and_what_to_do_and_the_code_line_says_it_is_single_use() {
        let b = banner("http://127.0.0.1:5000/");
        assert!(b.contains("http://127.0.0.1:5000/") && b.contains("this machine only"));
        assert!(b.contains("Press Enter") && b.contains("Ctrl-C"));
        assert!(code_line("ABCD-1234", true).contains("ABCD-1234"));
        assert!(code_line("ABCD-1234", true).contains("single use"));
        assert!(code_line("WXYZ-5678", false).contains("another tab"));
    }

    /// SPEC R-GUI.11: the browser and its extensions are inside what is trusted,
    /// and the banner says so on every start.
    #[test]
    fn the_banner_says_the_browser_and_its_extensions_are_trusted() {
        let b = banner("http://127.0.0.1:5000/");
        assert!(b.contains("browser") && b.contains("extensions"), "{b}");
    }
}
