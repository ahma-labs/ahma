//! Launching apps through LaunchServices is never granted (SPEC R6.2.10).
//!
//! `open App.app`, `open -a App` and `open URL` hand the launch to
//! LaunchServices, which starts the app outside every sandbox: ahma has no
//! quarantine, so an app planted in the workspace would run unconfined. The
//! Seatbelt profile therefore denies `lsopen`, and that is permanent. What the
//! agent can do instead is run the app's binary directly, which keeps it
//! inside the sandbox.

/// Explain a launch LaunchServices refused, if the output shows one.
pub fn launch_services_denial_note(stderr: &str, stdout: &str) -> Option<String> {
    let hit = stderr
        .lines()
        .chain(stdout.lines())
        .any(looks_like_refused_launch);
    hit.then(|| {
        "The sandbox refused to launch an app through LaunchServices (`open`): an app opened \
         that way would run outside every sandbox, so this is never granted (SPEC R6.2.10). Run \
         the app's binary directly instead (`App.app/Contents/MacOS/App`), which keeps it in the \
         sandbox; opening a URL or another app is the human's to do."
            .to_string()
    })
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
        }
        assert!(launch_services_denial_note("error -54 reading file", "").is_none());
        assert!(launch_services_denial_note("Launching app…", "").is_none());
    }
}
