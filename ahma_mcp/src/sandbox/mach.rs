//! A Mach service registration the sandbox refused (SPEC R6.2.13).
//!
//! Chromium-based programs (Chrome, Edge, Electron apps, Playwright's
//! browsers) register a Mach service with launchd as they start
//! (`bootstrap_check_in`), and stop when it is refused. ahma's Seatbelt
//! profile allows Mach lookups but no registration, so such a program cannot
//! run inside it, and no path grant changes that. Field case: headless Chrome
//! died on `bootstrap_check_in com.google.Chrome.MachPortRendezvousServer…:
//! Permission denied (1100)`, ahma read a path out of a neighbouring line
//! instead, and the human approved a grant on a folder that does not exist.
//!
//! The kernel's own record of the refusal (`mach-register`) is preferred; the
//! line the program prints is the evidence when no record can be read (SPEC
//! R-ESCAPE.1). A program that prints such a line falsely gains nothing but a
//! question to the human, who sees the command.

/// The Mach service a program was refused registering, from its output:
/// `bootstrap_check_in <service>: Permission denied (1100)`. The trailing
/// process id Chromium appends to the name is dropped. The refusal the
/// program died of (its `FATAL` line) is preferred over one it logged and
/// went on from (Crashpad's handshake).
pub fn refused_mach_service(stderr: &str, stdout: &str) -> Option<String> {
    let lines = || stderr.lines().chain(stdout.lines());
    lines()
        .filter(|l| l.contains("FATAL"))
        .find_map(refused_service_on_line)
        .or_else(|| lines().find_map(refused_service_on_line))
}

fn refused_service_on_line(line: &str) -> Option<String> {
    let (_, rest) = line.split_once("bootstrap_check_in ")?;
    // launchd's BOOTSTRAP_NOT_PRIVILEGED is 1100; Chromium prints it after
    // the reason. A different code (a name already taken) is no refusal.
    if !rest.contains("(1100)") {
        return None;
    }
    let name = rest.split([':', ' ']).next()?.trim();
    (!name.is_empty()).then(|| strip_pid_suffix(name).to_string())
}

/// `com.google.Chrome.MachPortRendezvousServer.18339` names one process;
/// the service is the part before its pid.
fn strip_pid_suffix(name: &str) -> &str {
    match name.rsplit_once('.') {
        Some((head, tail)) if !tail.is_empty() && tail.bytes().all(|b| b.is_ascii_digit()) => head,
        _ => name,
    }
}

/// The program a refused service belongs to, in the words its user knows it
/// by, when the name says so.
pub fn program_for_service(service: &str) -> Option<&'static str> {
    [
        ("com.google.Chrome", "Google Chrome"),
        ("com.microsoft.edgemac", "Microsoft Edge"),
        ("com.brave.Browser", "Brave"),
        ("org.chromium.crashpad", "a Chromium-based program"),
        ("org.chromium", "Chromium"),
    ]
    .into_iter()
    .find(|(prefix, _)| service.starts_with(prefix))
    .map(|(_, program)| program)
}

/// The first line of the [`super::SandboxError::Unfixable`] a refused Mach
/// registration becomes. The hook quotes the `Blocked: ` line in the escape
/// dialog (SPEC R-ESCAPE.1), so it says what was refused in plain words.
pub fn blocked_line(service: &str) -> String {
    let who = program_for_service(service).unwrap_or("This program");
    format!(
        "Blocked: {who} can't run inside ahma's sandbox: macOS refused it the Mach service \
         `{service}` it registers at startup."
    )
}

/// The explanation that follows [`blocked_line`]: why no grant helps, and
/// what to do instead of handing the human a script.
pub fn remediation(service: &str) -> String {
    format!(
        "{}\n\nNo folder grant can fix this: ahma's sandbox allows no Mach service \
         registration, and Chromium-based programs (Chrome, Edge, Electron, Playwright's \
         browsers) need one. Any path also refused in this run did not cause the failure. Do \
         not write a script for the human to run: run this exact command again and, in Claude \
         Code, ahma asks the human once whether to run it outside the sandbox.",
        blocked_line(service)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The lines headless Chrome printed in the field case, verbatim.
    const CHROME: &str = "\
[1107/084514.295772:ERROR:third_party/crashpad/crashpad/util/mach/bootstrap.cc:65] bootstrap_check_in org.chromium.crashpad.child_port_handshake.18343.13373546.WEUUBRAMWHDXRTOW: Permission denied (1100)
[1107/084514.297003:ERROR:third_party/crashpad/crashpad/util/file/file_io_posix.cc:208] open /Users/u/Library/Application Support/Google/Chrome/Crashpad/settings.dat: Operation not permitted (1)
[18339:13373613:1107/084514.398635:FATAL:base/apple/mach_port_rendezvous_mac.cc:159] Check failed: kr == KERN_SUCCESS. bootstrap_check_in com.google.Chrome.MachPortRendezvousServer.18339: Permission denied (1100)";

    #[test]
    fn chromes_refused_registration_is_recognised() {
        // The one it died of, not Crashpad's, which it logged and went on from.
        let fatal = refused_mach_service(CHROME, "").expect("a refused registration");
        assert_eq!(fatal, "com.google.Chrome.MachPortRendezvousServer");
        let crashpad = refused_mach_service(CHROME.lines().next().unwrap(), "").unwrap();
        assert_eq!(
            crashpad,
            "org.chromium.crashpad.child_port_handshake.18343.13373546.WEUUBRAMWHDXRTOW"
        );
        assert_eq!(program_for_service(&fatal), Some("Google Chrome"));
        assert!(blocked_line(&fatal).starts_with("Blocked: Google Chrome can't run"));
    }

    #[test]
    fn found_in_stdout_too() {
        assert!(refused_mach_service("", CHROME).is_some());
    }

    #[test]
    fn other_bootstrap_failures_are_not_refusals() {
        assert!(
            refused_mach_service(
                "bootstrap_check_in com.example.svc: Service name already exists (1103)",
                ""
            )
            .is_none()
        );
        assert!(refused_mach_service("Permission denied (1100)", "").is_none());
        assert!(refused_mach_service("cat: /x: Permission denied", "").is_none());
    }

    #[test]
    fn remediation_steers_the_agent_away_from_a_script() {
        let text = remediation("com.google.Chrome.MachPortRendezvousServer");
        assert!(text.contains("No folder grant can fix this"), "{text}");
        assert!(text.contains("run this exact command again"), "{text}");
    }
}
