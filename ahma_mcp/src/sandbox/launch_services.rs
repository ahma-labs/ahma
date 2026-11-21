//! Launching apps through LaunchServices is never granted (SPEC R6.2.10).
//!
//! `open App.app`, `open -a App` and `open URL` hand the launch to
//! LaunchServices, which starts the app outside every sandbox: ahma has no
//! quarantine, so an app planted in the workspace would run unconfined. The
//! Seatbelt profile therefore denies `lsopen`, and that is permanent. What the
//! agent can do instead is run the app's binary directly, which keeps it
//! inside the sandbox; a web link has its own way out, the link broker (SPEC
//! R-LINK), so a refused link names the exact `ahma open-url` to run, and is
//! not something to run outside the sandbox (no R-ESCAPE).

/// Explain a launch LaunchServices refused, if the output shows one: for a
/// web link, the exact `ahma open-url` that opens it with the human's
/// consent; for anything else, the app's binary.
pub fn launch_services_denial_note(stderr: &str, stdout: &str) -> Option<String> {
    if let Some(url) = refused_link(stderr, stdout) {
        return Some(link_note(&url));
    }
    let hit = stderr
        .lines()
        .chain(stdout.lines())
        .any(looks_like_refused_launch);
    hit.then(|| {
        "The sandbox refused to launch an app through LaunchServices (`open`): an app opened \
         that way would run outside every sandbox, so this is never granted (SPEC R6.2.10). Run \
         the app's binary directly instead (`App.app/Contents/MacOS/App`), which keeps it in the \
         sandbox; a file or another app is the human's to open."
            .to_string()
    })
}

/// The web link a refused LaunchServices launch was for, as `open` names it
/// (`… for the URL https://…`, `… for the file https://…`): `None` when the
/// output shows no refused launch, or one of a file or an app.
pub fn refused_link(stderr: &str, stdout: &str) -> Option<String> {
    stderr
        .lines()
        .chain(stdout.lines())
        .filter(|line| looks_like_refused_launch(line))
        .find_map(link_in_line)
}

/// The note for a refused web link (SPEC R6.2.10, R-LINK.10): the exact
/// helper line, which asks the human when no grant covers the site; never a
/// run outside the sandbox.
pub fn link_note(url: &str) -> String {
    let quoted = crate::links::policy::sh_single_quote(url);
    format!(
        "The sandbox refused to open a web link through LaunchServices (`open`) (SPEC R6.2.10). \
         ahma opens web links itself, in the human's browser, for a site they allowed: run \
         `ahma open-url {quoted}` (or `\"$BROWSER\" {quoted}`). This is not run outside the \
         sandbox, and nothing else opens it."
    )
}

/// LSOpen errors as `open` prints them: `-10810` (kLSUnknownErr), `-10822`
/// (kLSServerCommunicationErr) and `-54` (a permission error), always with
/// "LSOpen" or "launch" in the line.
fn looks_like_refused_launch(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    let launching =
        lower.contains("lsopen") || lower.contains("unable to launch") || lower.contains("launch");
    let code = ["-10810", "-10822", "-54"]
        .iter()
        .any(|c| lower.contains(c));
    launching && code
}

/// The `http`/`https` address in a refused-launch line, without the full
/// stop `open` ends the line with: what follows `for the URL ` or
/// `for the file `.
fn link_in_line(line: &str) -> Option<String> {
    let lower = line.to_ascii_lowercase();
    let at = ["for the url ", "for the file "]
        .iter()
        .find_map(|marker| lower.find(marker).map(|i| i + marker.len()))?;
    let word = line.get(at..)?.split_whitespace().next()?;
    let word = word.strip_suffix('.').unwrap_or(word);
    let head = word.get(..8).unwrap_or(word).to_ascii_lowercase();
    (head.starts_with("http://") || head.starts_with("https://")).then(|| word.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_refused_launch_says_run_the_binary_instead() {
        for line in [
            "The application /tmp/w/Clown.app cannot be opened for an unexpected reason, error=Error Domain=NSOSStatusErrorDomain Code=-10810 \"kLSUnknownErr\" (LSOpen failure)",
            "LSOpenURLsWithRole() failed with error -10822 for the file /tmp/w/Clown.app.",
            "LSOpenURLsWithRole() failed with error -54 for the file /tmp/w/x.html.",
        ] {
            let note = launch_services_denial_note(line, "").unwrap_or_else(|| panic!("{line}"));
            assert!(note.contains("Contents/MacOS"), "{note}");
            assert!(note.contains("never granted"), "{note}");
            assert!(!note.contains("open-url"), "{note}");
            assert_eq!(refused_link(line, ""), None, "{line}");
        }
        assert!(launch_services_denial_note("error -54 reading file", "").is_none());
        assert!(launch_services_denial_note("Launching app…", "").is_none());
    }

    /// SPEC R6.2.10 (amended), R-LINK.10: a refused open of a web link names
    /// the exact `ahma open-url` that opens it, quoted as one shell word, and
    /// says it is not something to run outside the sandbox. Before, it said
    /// opening a URL was "the human's to do".
    #[test]
    fn a_refused_url_open_names_ahma_open_url() {
        for (line, url) in [
            (
                "LSOpenURLsWithRole() failed with error -54 for the URL https://github.com/o/r/pull/1?a=1&b=2.",
                "https://github.com/o/r/pull/1?a=1&b=2",
            ),
            (
                "LSOpenURLsWithRole() failed with error -10822 for the file HTTP://Example.com/x.",
                "HTTP://Example.com/x",
            ),
        ] {
            assert_eq!(refused_link("", line).as_deref(), Some(url), "{line}");
            let note = launch_services_denial_note(line, "").unwrap_or_else(|| panic!("{line}"));
            assert!(note.contains(&format!("`ahma open-url '{url}'`")), "{note}");
            assert!(note.contains("not run outside the sandbox"), "{note}");
            assert!(!note.contains("Contents/MacOS"), "{note}");
            assert!(!note.contains("human's to do"), "{note}");
        }
        // Not a refusal: a URL in ordinary output is nobody's concern.
        assert_eq!(
            refused_link("see https://example.com/ for -54 help", ""),
            None
        );
    }
}
