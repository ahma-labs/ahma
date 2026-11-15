//! The system clipboard, through the tools the system already has (SPEC
//! R-GUI.2).
//!
//! `ahma web` copies the pairing code so the human can paste it into the page.
//! The clipboard is readable by other programs of the same user, so the code
//! put there is short-lived ([`crate::auth::CODE_TTL`]), its reuse burns the
//! server ([`crate::auth::PairingPolicy::burn_on_reuse`]), and it is cleared
//! once used or when the server stops — but only while the clipboard still
//! holds it, so nothing the human copied since is lost. ahma's own macOS
//! sandbox denies the pasteboard to the commands it runs (SPEC R6.2.15).
//!
//! No crate: `pbcopy`/`pbpaste` on macOS, `clip.exe` and PowerShell on
//! Windows, `wl-copy`/`wl-paste`, `xclip` or `xsel` on Linux. When none works
//! the caller says so and the human types the code from the terminal.

use std::io::Write;
use std::process::{Command, Stdio};

/// Which system's tools to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Os {
    Mac,
    Windows,
    /// Linux and the other Unixes, under Wayland or X11.
    Unix {
        wayland: bool,
    },
}

impl Os {
    /// The system this is.
    pub fn current() -> Self {
        if cfg!(target_os = "macos") {
            Self::Mac
        } else if cfg!(windows) {
            Self::Windows
        } else {
            Self::Unix {
                wayland: std::env::var_os("WAYLAND_DISPLAY").is_some_and(|v| !v.is_empty()),
            }
        }
    }
}

/// A program and its arguments.
pub type Tool = (&'static str, &'static [&'static str]);

/// Programs that copy their standard input to the clipboard, in the order to
/// try them.
pub fn copy_tools(os: Os) -> Vec<Tool> {
    match os {
        Os::Mac => vec![("pbcopy", &[])],
        Os::Windows => vec![("clip.exe", &[])],
        Os::Unix { wayland } => {
            let mut tools: Vec<Tool> = Vec::new();
            if wayland {
                tools.push(("wl-copy", &[]));
            }
            tools.push(("xclip", &["-selection", "clipboard"]));
            tools.push(("xsel", &["--clipboard", "--input"]));
            tools
        }
    }
}

/// Programs that print the clipboard, in the order to try them.
pub fn paste_tools(os: Os) -> Vec<Tool> {
    match os {
        Os::Mac => vec![("pbpaste", &[])],
        Os::Windows => vec![(
            "powershell.exe",
            &["-NoProfile", "-NonInteractive", "-Command", "Get-Clipboard"],
        )],
        Os::Unix { wayland } => {
            let mut tools: Vec<Tool> = Vec::new();
            if wayland {
                tools.push(("wl-paste", &["--no-newline"]));
            }
            tools.push(("xclip", &["-selection", "clipboard", "-o"]));
            tools.push(("xsel", &["--clipboard", "--output"]));
            tools
        }
    }
}

fn run_copy((program, args): Tool, text: &str) -> bool {
    let Ok(mut child) = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    let wrote = child
        .stdin
        .take()
        .is_some_and(|mut stdin| stdin.write_all(text.as_bytes()).is_ok());
    child.wait().is_ok_and(|status| status.success()) && wrote
}

/// Put `text` on the clipboard. Returns the program that did it, or `None`
/// when no tool could.
pub fn copy(text: &str) -> Option<&'static str> {
    copy_tools(Os::current())
        .into_iter()
        .find(|tool| run_copy(*tool, text))
        .map(|(program, _)| program)
}

/// What the clipboard holds, if it can be read.
pub fn read() -> Option<String> {
    paste_tools(Os::current())
        .into_iter()
        .find_map(|(program, args)| {
            let out = Command::new(program)
                .args(args)
                .stdin(Stdio::null())
                .stderr(Stdio::null())
                .output()
                .ok()?;
            out.status
                .success()
                .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
        })
}

/// Whether what the clipboard holds is `text` (ignoring a trailing newline a
/// tool may add).
pub fn holds(clipboard: &str, text: &str) -> bool {
    clipboard.trim_end_matches(['\r', '\n']) == text
}

/// Empty the clipboard if it still holds `text`; leave it alone otherwise, so
/// nothing the human copied since is lost. Returns whether it was cleared.
pub fn clear_if_holds(text: &str) -> bool {
    match read() {
        Some(current) if holds(&current, text) => copy("").is_some(),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each system's own tools, Wayland's first where it is running, and X11's
    /// as the fallback.
    #[test]
    fn each_system_uses_its_own_tools() {
        assert_eq!(copy_tools(Os::Mac), vec![("pbcopy", &[][..])]);
        assert_eq!(paste_tools(Os::Mac)[0].0, "pbpaste");
        assert_eq!(copy_tools(Os::Windows)[0].0, "clip.exe");
        assert!(paste_tools(Os::Windows)[0].1.contains(&"Get-Clipboard"));
        let wayland: Vec<_> = copy_tools(Os::Unix { wayland: true })
            .into_iter()
            .map(|t| t.0)
            .collect();
        assert_eq!(wayland, ["wl-copy", "xclip", "xsel"]);
        let x11: Vec<_> = paste_tools(Os::Unix { wayland: false })
            .into_iter()
            .map(|t| t.0)
            .collect();
        assert_eq!(x11, ["xclip", "xsel"]);
    }

    /// A tool's trailing newline does not stop the clipboard being recognised
    /// as still holding the code; anything else does.
    #[test]
    fn the_code_is_recognised_only_when_it_is_what_the_clipboard_holds() {
        assert!(holds("ABCD-1234", "ABCD-1234"));
        assert!(holds("ABCD-1234\r\n", "ABCD-1234"));
        assert!(!holds("ABCD-1234 and more", "ABCD-1234"));
        assert!(!holds("", "ABCD-1234"));
    }
}
