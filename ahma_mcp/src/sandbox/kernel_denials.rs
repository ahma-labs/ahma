//! What the kernel refused, read from its own records (SPEC R-DENY).
//!
//! A command's output says what the *tool* thought went wrong, in the tool's
//! words. Scraping it for "Permission denied" found a refusal in a `grep` of
//! a log, missed every tool that reports none, and could not tell a read from
//! a write on macOS. The kernel says exactly what it refused: on macOS each
//! Seatbelt denial is a unified-log record `Sandbox: <process>(<pid>)
//! deny(<n>) <operation> <target>`. This module reads such a record and sorts
//! the denial by what can change it — a grant, a capability, a host, or
//! nothing — so a tool ahma has never heard of gets the same answer as one it
//! has, with no tool-specific code.

use std::path::{Path, PathBuf};

use ahma_common::config::ScopeAccess;

/// One denial as the kernel recorded it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KernelDenial {
    /// The process the kernel refused.
    pub process: String,
    pub pid: u32,
    /// The operation, as Seatbelt names it (`file-write-create`, `iokit-open`).
    pub operation: String,
    /// What it was refused on: a path, a service, a host, a process.
    pub target: String,
}

impl KernelDenial {
    /// Parse a Seatbelt record: `… Sandbox: touch(4242) deny(1)
    /// file-write-create /tmp/x …`. `None` for anything else.
    pub fn parse_seatbelt(line: &str) -> Option<Self> {
        let rest = &line[line.find("Sandbox: ")? + "Sandbox: ".len()..];
        let open = rest.find('(')?;
        let close = open + rest[open..].find(')')?;
        let process = rest[..open].to_string();
        let pid = rest[open + 1..close].parse().ok()?;
        let rest = rest[close + 1..].trim_start().strip_prefix("deny(")?;
        let rest = rest[rest.find(')')? + 1..].trim_start();
        let (operation, target) = rest.split_once(' ').unwrap_or((rest, ""));
        Some(Self {
            process,
            pid,
            operation: operation.to_string(),
            target: target.trim().to_string(),
        })
    }
}

/// What can change a denial.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DenialClass {
    /// A path a human may grant, with the access that was refused.
    Grant { path: PathBuf, access: ScopeAccess },
    /// A path no grant can open (credentials, ahma's own settings, the
    /// system): the reason, never a question (SPEC R-PERM.4.3).
    NeverGranted { path: PathBuf, why: String },
    /// A capability a setting enables, never a path (`allow_gpu`,
    /// `signal_other_processes`).
    Capability { setting: &'static str },
    /// A network destination: a host approval under `--restrict-network`.
    Network { target: String },
    /// Something no grant or setting allows (opening an app, a setuid
    /// program, a service the sandbox withholds).
    Unfixable { why: String },
}

/// Sort a denial by what can change it.
pub fn classify(denial: &KernelDenial) -> DenialClass {
    let op = denial.operation.as_str();
    let path_access = if op.starts_with("file-read") {
        Some(ScopeAccess::Ro)
    } else if op.starts_with("file-write") || op.starts_with("file-create") {
        Some(ScopeAccess::Rw)
    } else {
        None
    };
    if let Some(access) = path_access {
        let path = PathBuf::from(&denial.target);
        return match ahma_common::scope_grant::refusal_reason(&path) {
            Some(why) => DenialClass::NeverGranted { path, why },
            None => DenialClass::Grant { path, access },
        };
    }
    match op {
        "iokit-open" | "iokit-open-user-client"
            if super::gpu::GPU_USER_CLIENTS
                .iter()
                .any(|c| denial.target.contains(c)) =>
        {
            DenialClass::Capability {
                setting: "allow_gpu",
            }
        }
        "signal" => DenialClass::Capability {
            setting: "signal_other_processes",
        },
        op if op.starts_with("network-outbound") => DenialClass::Network {
            target: denial.target.clone(),
        },
        "lsopen" => DenialClass::Unfixable {
            why: "opening an app hands it to launchd, which runs it outside the sandbox".into(),
        },
        "forbidden-exec-sugid" => DenialClass::Unfixable {
            why: "a setuid program runs with privileges the sandbox will not pass on".into(),
        },
        _ => DenialClass::Unfixable {
            why: format!("the sandbox does not allow `{op}`"),
        },
    }
}

/// The one line a denial leaves in a command's result (SPEC R-PERM.9): what
/// the kernel refused, and the one thing that would change it.
pub fn one_line(denial: &KernelDenial, class: &DenialClass) -> String {
    let what = format!(
        "ahma's kernel sandbox refused `{}` {} by {} ({})",
        denial.operation,
        if denial.target.is_empty() {
            String::new()
        } else {
            format!("on {}", denial.target)
        },
        denial.process,
        denial.pid
    );
    match class {
        DenialClass::Grant { path, access } => format!(
            "Blocked until a human grants it: {what}. A human runs `ahma sandbox grant {}{}`, \
             or answers the question ahma raised; then re-run.",
            super::grant_channel::grant_dir_for(path).display(),
            if access.is_write() {
                ""
            } else {
                " --read-only"
            }
        ),
        DenialClass::NeverGranted { why, .. } => {
            format!("Blocked, and no grant can change it: {what}. {why}")
        }
        DenialClass::Capability { setting } => format!(
            "Blocked until a human enables it: {what}. This is a capability, not a path: a \
             human sets `[sandbox] {setting} = true` in ~/.ahma/settings.toml."
        ),
        DenialClass::Network { target } => format!(
            "Blocked until a human allows the host: {what}. A human runs `ahma network allow \
             {}`.",
            target.split(':').next().unwrap_or(target)
        ),
        DenialClass::Unfixable { why } => format!(
            "Blocked: {what}. No grant or setting changes this: {why}. If the command must do \
             it, a human runs it in their own terminal."
        ),
    }
}

/// Denials every process makes that are nobody's concern: the dyld DTrace
/// helper each process opens at start (seen on every `sandbox-exec` run).
fn is_noise(denial: &KernelDenial) -> bool {
    denial.target == "/dev/dtracehelper"
}

/// The denials among `lines` (a log excerpt) that `pids` made, in order,
/// without the noise every process makes.
pub fn denials_of(lines: &str, pids: &dyn Fn(u32) -> bool) -> Vec<KernelDenial> {
    lines
        .lines()
        .filter_map(KernelDenial::parse_seatbelt)
        .filter(|d| pids(d.pid) && !is_noise(d))
        .collect()
}

/// Whether `path` is the target of a file denial in `denials`.
pub fn refused(denials: &[KernelDenial], path: &Path) -> bool {
    denials.iter().any(|d| Path::new(&d.target) == path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(line: &str) -> KernelDenial {
        KernelDenial::parse_seatbelt(line).expect("a Seatbelt record")
    }

    #[test]
    fn a_seatbelt_record_parses_into_process_operation_and_target() {
        let r = d(
            "2026-10-04 12:00:01.234 Df kernel[0:abc] (Sandbox) Sandbox: touch(4242) \
                   deny(1) file-write-create /Users/me/.cache/tool/x.lock",
        );
        assert_eq!(r.process, "touch");
        assert_eq!(r.pid, 4242);
        assert_eq!(r.operation, "file-write-create");
        assert_eq!(r.target, "/Users/me/.cache/tool/x.lock");
        let spaced = d("Sandbox: my tool(7) deny(1) file-read-data /a dir/with spaces");
        assert_eq!(spaced.process, "my tool");
        assert_eq!(spaced.target, "/a dir/with spaces");
        // As recorded on macOS 15 (CI, 2026-10): the rule's operation name,
        // and a tagged rule's message on the line after the record.
        let real = d("2026-10-04 11:16:25.855 E  kernel[0:1bf69] \
                      [com.apple.sandbox.reporting:violation] Sandbox: touch(49530) deny(1) \
                      file-write* /private/var/folders/36/T/.tmpunEsrf/denied.txt");
        assert_eq!(real.operation, "file-write*");
        assert_eq!(
            real.target,
            "/private/var/folders/36/T/.tmpunEsrf/denied.txt"
        );
        let tagged = "Sandbox: nc(49511) deny(1) network-outbound remote:*:9\nahma-op-7";
        assert_eq!(d(tagged.lines().next().unwrap()).target, "remote:*:9");
        let bare = d("Sandbox: kill(9) deny(1) signal");
        assert_eq!(
            (bare.operation.as_str(), bare.target.as_str()),
            ("signal", "")
        );
        for not_one in [
            "kernel: hello",
            "Sandbox: x deny(1) y",
            "Sandbox: x(1) allow file-read",
        ] {
            assert_eq!(KernelDenial::parse_seatbelt(not_one), None, "{not_one}");
        }
    }

    /// Every class, from the kernel's word alone: no tool names, no output
    /// text. A tool ahma has never seen gets the same answer.
    #[test]
    fn every_denial_is_sorted_by_what_can_change_it() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path().join("cache.lock");
        let home = ahma_common::config::ahma_home_dir().unwrap();
        let cases = [
            (
                format!(
                    "Sandbox: t(1) deny(1) file-write-create {}",
                    cache.display()
                ),
                "grant-rw",
            ),
            (
                format!("Sandbox: t(1) deny(1) file-read-data {}", cache.display()),
                "grant-ro",
            ),
            (
                format!(
                    "Sandbox: ssh(1) deny(1) file-read-data {}",
                    home.join(".ssh/github_ed25519").display()
                ),
                "never",
            ),
            (
                "Sandbox: llama(1) deny(1) iokit-open AGXDeviceUserClient".to_string(),
                "gpu",
            ),
            ("Sandbox: kill(1) deny(1) signal".to_string(), "signal"),
            (
                "Sandbox: curl(1) deny(1) network-outbound 140.82.121.4:443".to_string(),
                "net",
            ),
            ("Sandbox: open(1) deny(1) lsopen".to_string(), "unfixable"),
            (
                "Sandbox: ps(1) deny(1) forbidden-exec-sugid".to_string(),
                "unfixable",
            ),
            (
                "Sandbox: t(1) deny(1) iokit-open SomeCameraClient".to_string(),
                "unfixable",
            ),
        ];
        for (line, want) in cases {
            let denial = d(&line);
            let class = classify(&denial);
            let got = match &class {
                DenialClass::Grant { access, .. } if access.is_write() => "grant-rw",
                DenialClass::Grant { .. } => "grant-ro",
                DenialClass::NeverGranted { .. } => "never",
                DenialClass::Capability {
                    setting: "allow_gpu",
                } => "gpu",
                DenialClass::Capability { .. } => "signal",
                DenialClass::Network { .. } => "net",
                DenialClass::Unfixable { .. } => "unfixable",
            };
            assert_eq!(got, want, "{line}");
            let said = one_line(&denial, &class);
            assert!(said.starts_with("Blocked"), "{said}");
            if want == "never" || want == "unfixable" {
                assert!(!said.contains("sandbox grant"), "{said}");
            }
        }
    }

    #[test]
    fn only_the_commands_own_processes_count() {
        let log = "Sandbox: a(10) deny(1) file-write-create /x\n\
                   Sandbox: a(10) deny(1) file-write-data /dev/dtracehelper\n\
                   Sandbox: b(20) deny(1) file-write-create /y\n\
                   unrelated line";
        let mine = denials_of(log, &|pid| pid == 10);
        assert_eq!(mine.len(), 1);
        assert!(refused(&mine, Path::new("/x")));
        assert!(!refused(&mine, Path::new("/y")));
    }
}
