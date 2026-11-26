//! Host services start outside the sandbox (SPEC R-PROC.6).
//!
//! A host service is a server one user's clients share that holds something
//! only the machine can give: adb's server holds the USB devices. Its client
//! starts it whenever none is running, so a sandboxed command would start it
//! confined for life, unable to reach what it holds. When a human names the
//! program in `[sandbox] host_services` and an enabled profile declares it,
//! ahma (which is not confined) starts the server outside the sandbox before a
//! sandboxed command that runs its client, or restarts one it finds confined.
//!
//! What the agent cannot choose: the program is found on ahma's own `PATH`,
//! never in a folder a sandboxed command can write; its arguments are the
//! profile's; its environment is ahma's own, not the command's.

use super::profiles::HostService;
use std::path::{Path, PathBuf};

/// How a host service's server stands before a command runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Server {
    /// Nothing listens on its port.
    None,
    /// Processes inside a sandbox listen on it.
    Confined(Vec<u32>),
    /// Something outside any sandbox listens on it (or this platform cannot
    /// tell, in which case it is left alone).
    Unconfined,
}

/// Why ahma does not start a server it would otherwise start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Withheld {
    /// This ahma is itself inside a sandbox: what it started would be too.
    SelfConfined,
    /// `--restrict-network` is on: the server reaches hosts the sandbox does
    /// not, and a sandboxed client cannot reach its loopback port either.
    NetworkRestricted,
}

/// What ahma does before the command runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Step {
    Nothing,
    Start,
    Restart(Vec<u32>),
    Withhold(Withheld),
}

/// The decision, from the server's state and ahma's own.
pub(crate) fn decide(server: &Server, network_restricted: bool, self_confined: bool) -> Step {
    match server {
        Server::Unconfined => Step::Nothing,
        _ if self_confined => Step::Withhold(Withheld::SelfConfined),
        _ if network_restricted => Step::Withhold(Withheld::NetworkRestricted),
        Server::None => Step::Start,
        Server::Confined(pids) => Step::Restart(pids.clone()),
    }
}

/// The services a command line runs the client of, among `enabled`.
pub(crate) fn wanted<'a>(line: &str, enabled: &'a [HostService]) -> Vec<&'a HostService> {
    let programs = ahma_common::harness_asks::command_programs(line);
    enabled
        .iter()
        .filter(|s| programs.iter().any(|p| p == &s.program))
        .collect()
}

/// The declared services a human turned on: an enabled profile declares it,
/// and `[sandbox] host_services` names its program.
pub(crate) fn enabled(declared: Vec<HostService>, turned_on: &[String]) -> Vec<HostService> {
    declared
        .into_iter()
        .filter(|s| turned_on.iter().any(|n| n == &s.program))
        .collect()
}

/// The program's executable on `path_var`, skipping every folder `refused`
/// says a sandboxed command can write, and any match whose real file is in
/// one. A relative `PATH` entry never counts: it names a folder relative to
/// wherever the command runs.
pub(crate) fn resolve_program(
    program: &str,
    path_var: Option<&std::ffi::OsStr>,
    refused: impl Fn(&Path) -> bool,
) -> Option<PathBuf> {
    if program.contains(std::path::MAIN_SEPARATOR) || program.contains('/') {
        return None;
    }
    std::env::split_paths(path_var?)
        .filter(|dir| dir.is_absolute() && !refused(dir))
        .map(|dir| dir.join(program))
        .filter(|candidate| is_executable(candidate))
        .find_map(|candidate| {
            let real = dunce::canonicalize(&candidate).ok()?;
            let parent = real.parent().map(Path::to_path_buf).unwrap_or_default();
            (!refused(&real) && !refused(&parent)).then_some(real)
        })
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

/// The line a command's result carries for what ahma did, or would not do.
pub(crate) fn note(service: &HostService, step: &Step, outcome: Option<&str>) -> Option<String> {
    let p = &service.program;
    let res = &service.resource;
    let line = service.restart_line();
    match step {
        Step::Nothing => None,
        Step::Start => Some(format!(
            "ahma: started `{p}`'s server outside the sandbox before this command, because \
             `[sandbox] host_services` names it: it holds {res}, which a server started inside \
             the sandbox cannot reach (SPEC R-PROC.6).{}",
            outcome.map(|o| format!(" {o}")).unwrap_or_default()
        )),
        Step::Restart(pids) => Some(format!(
            "ahma: restarted `{p}`'s server outside the sandbox before this command: the one \
             running (pid {}) was inside a sandbox and could not reach {res} (SPEC R-PROC.6).{}",
            pids.iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(", "),
            outcome.map(|o| format!(" {o}")).unwrap_or_default()
        )),
        Step::Withhold(Withheld::SelfConfined) => Some(format!(
            "ahma: did not start `{p}`'s server: this ahma runs inside another sandbox, so a \
             server it started would be confined too and could not reach {res}. A human runs \
             `{line}` in a terminal outside any sandbox (SPEC R-PROC.6)."
        )),
        Step::Withhold(Withheld::NetworkRestricted) => Some(format!(
            "ahma: did not start `{p}`'s server: `--restrict-network` is on, and a server \
             outside the sandbox reaches hosts the sandbox does not (for adb, through \
             `connect`, `forward` and `reverse`); a sandboxed command cannot reach its loopback \
             port either (SPEC R-PROC.6)."
        )),
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod live {
    use super::*;

    /// The services this process has turned on, read once: settings are fixed
    /// for a session (R5).
    fn turned_on() -> &'static [HostService] {
        static ON: std::sync::OnceLock<Vec<HostService>> = std::sync::OnceLock::new();
        ON.get_or_init(|| {
            let names = ahma_common::config::AhmaSettings::load()
                .sandbox
                .host_services;
            if names.is_empty() {
                return Vec::new();
            }
            enabled(
                crate::sandbox::profiles::host_services(
                    crate::sandbox::profiles::enabled_profile_names(),
                ),
                &names,
            )
        })
    }

    /// Whether this ahma is inside a sandbox: the kernel says so, or it is
    /// itself a process a sandboxed command started (SPEC R-PROC.5's stamp).
    fn self_confined() -> bool {
        ahma_common::seatbelt::this_process_is_confined()
            || std::env::var_os(crate::sandbox::leftovers::RUN_ENV).is_some()
    }

    /// How the server on `port` stands. macOS asks `lsof` who listens and the
    /// kernel whether each is confined; elsewhere a listener is left alone.
    async fn server_on(port: u16) -> Server {
        #[cfg(target_os = "macos")]
        {
            let out = tokio::process::Command::new("/usr/sbin/lsof")
                .args([
                    "-nP",
                    &format!("-iTCP@127.0.0.1:{port}"),
                    "-sTCP:LISTEN",
                    "-t",
                ])
                .stdin(std::process::Stdio::null())
                .kill_on_drop(true)
                .output()
                .await;
            if let Ok(out) = out {
                let pids: Vec<u32> = String::from_utf8_lossy(&out.stdout)
                    .lines()
                    .filter_map(|l| l.trim().parse().ok())
                    .collect();
                if pids.is_empty() {
                    return Server::None;
                }
                let confined: Vec<u32> = pids
                    .into_iter()
                    .filter(|p| crate::sandbox::confinement::pid_is_seatbelt_confined(*p))
                    .collect();
                return if confined.is_empty() {
                    Server::Unconfined
                } else {
                    Server::Confined(confined)
                };
            }
        }
        match tokio::net::TcpStream::connect(("127.0.0.1", port)).await {
            Ok(_) => Server::Unconfined,
            Err(_) => Server::None,
        }
    }

    /// Run `exe args…` outside the sandbox, with ahma's own environment less
    /// the marks of a sandboxed run. The client returns once the server is up;
    /// the server it forks detaches by itself and is meant to outlive this
    /// process (R-PROC.3: deliberately not reaped with us).
    async fn run_outside(exe: &Path, args: &[String]) -> Result<(), String> {
        let mut cmd = tokio::process::Command::new(exe);
        cmd.args(args)
            .env_remove(crate::sandbox::leftovers::RUN_ENV)
            .env_remove("AHMA_SERVER_CHILD")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            // The client is ours and ends by itself; the server it forks
            // leaves our process group, which is the point.
            .kill_on_drop(true)
            .process_group(0);
        let limit = std::time::Duration::from_secs(30);
        match tokio::time::timeout(limit, cmd.output()).await {
            Ok(Ok(out)) if out.status.success() => Ok(()),
            Ok(Ok(out)) => Err(String::from_utf8_lossy(&out.stderr).trim().to_string()),
            Ok(Err(e)) => Err(e.to_string()),
            Err(_) => Err(format!("it did not return within {}s", limit.as_secs())),
        }
    }

    #[cfg(test)]
    pub(super) async fn server_on_for_test(port: u16) -> Server {
        server_on(port).await
    }

    /// Before a sandboxed command runs: start, or restart, the server of each
    /// host service it uses, outside the sandbox. Returns the lines its result
    /// carries.
    pub(crate) async fn prepare_for(line: &str, sandbox: &crate::sandbox::Sandbox) -> Vec<String> {
        let services = turned_on();
        if services.is_empty() {
            return Vec::new();
        }
        let enforcement = sandbox.enforcement();
        if matches!(enforcement, crate::sandbox::Enforcement::Off) {
            // The client runs unconfined, and starts its server unconfined.
            return Vec::new();
        }
        let self_confined =
            self_confined() || matches!(enforcement, crate::sandbox::Enforcement::DeferredTo(_));
        let restricted = crate::sandbox::command::egress_restricted();
        let mut notes = Vec::new();
        for service in wanted(line, services) {
            let server = server_on(service.port()).await;
            let step = decide(&server, restricted, self_confined);
            let outcome = match &step {
                Step::Nothing | Step::Withhold(_) => None,
                Step::Start | Step::Restart(_) => Some(start(service, sandbox).await),
            };
            if let Some(n) = note(service, &step, outcome.as_ref().and_then(|o| o.as_deref())) {
                notes.push(n);
            }
        }
        notes
    }

    /// Stop (for a restart) and start the server; `None` when it started, or
    /// what went wrong.
    async fn start(service: &HostService, sandbox: &crate::sandbox::Sandbox) -> Option<String> {
        let writable_by_profile = crate::sandbox::profiles::resolved_rules(
            crate::sandbox::profiles::enabled_profile_names(),
            sandbox.package_cache_write(),
        );
        let refused = |p: &Path| {
            sandbox.is_path_in_scope(p)
                || writable_by_profile
                    .iter()
                    .any(|r| r.access.is_write() && r.kind.covers(&r.path, p))
        };
        let Some(exe) = resolve_program(
            &service.program,
            std::env::var_os("PATH").as_deref(),
            refused,
        ) else {
            return Some(format!(
                "But no `{}` was found on ahma's PATH outside the folders sandboxed commands can \
                 write, so nothing was started: a human runs `{}` in their own terminal.",
                service.program,
                service.restart_line()
            ));
        };
        let _ = run_outside(&exe, &service.stop).await;
        match run_outside(&exe, &service.start).await {
            Ok(()) => match server_on(service.port()).await {
                Server::Unconfined => None,
                _ => Some(format!(
                    "But `{} {}` returned and no unconfined server listens on port {}: a human \
                     runs `{}` in their own terminal.",
                    exe.display(),
                    service.start.join(" "),
                    service.port(),
                    service.restart_line()
                )),
            },
            Err(why) => Some(format!(
                "But `{} {}` failed ({why}): a human runs `{}` in their own terminal.",
                exe.display(),
                service.start.join(" "),
                service.restart_line()
            )),
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) use live::prepare_for;

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod live_tests {
    /// The probe tells an empty port from one this (unconfined) process
    /// listens on; on macOS that goes through `lsof` and the kernel's
    /// confinement check.
    #[tokio::test]
    async fn a_listener_outside_any_sandbox_is_left_alone() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let state = super::live::server_on_for_test(port).await;
        if ahma_common::seatbelt::this_process_is_confined() {
            // Under an outer sandbox (a confined test run) this process is
            // confined too, and says so.
            assert!(matches!(state, super::Server::Confined(_)), "{state:?}");
        } else {
            assert_eq!(state, super::Server::Unconfined);
        }
        drop(listener);
        assert_eq!(
            super::live::server_on_for_test(port).await,
            super::Server::None
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn adb() -> HostService {
        HostService {
            profile: "android".into(),
            program: "adb".into(),
            start: vec!["start-server".into()],
            stop: vec!["kill-server".into()],
            default_port: 5037,
            port_env: Some("ANDROID_ADB_SERVER_PORT".into()),
            resource: "USB devices".into(),
            cost: "the agent drives every attached device".into(),
        }
    }

    /// The whole table: a server outside the sandbox is used as it is; none,
    /// or a confined one, is started outside it, unless ahma is itself
    /// confined or the network is restricted, which it says instead.
    #[test]
    fn a_host_service_is_started_only_where_it_helps_and_can_be_reached() {
        let confined = Server::Confined(vec![42]);
        assert_eq!(decide(&Server::Unconfined, false, false), Step::Nothing);
        assert_eq!(decide(&Server::Unconfined, true, true), Step::Nothing);
        assert_eq!(decide(&Server::None, false, false), Step::Start);
        assert_eq!(decide(&confined, false, false), Step::Restart(vec![42]));
        for server in [&Server::None, &confined] {
            assert_eq!(
                decide(server, false, true),
                Step::Withhold(Withheld::SelfConfined)
            );
            assert_eq!(
                decide(server, true, false),
                Step::Withhold(Withheld::NetworkRestricted)
            );
        }
    }

    /// Only a line that runs the client counts, and only for a service a human
    /// turned on and a profile declares.
    #[test]
    fn a_service_is_wanted_by_the_lines_that_run_its_client() {
        let on = enabled(vec![adb()], &["adb".to_string()]);
        assert_eq!(on.len(), 1);
        assert!(enabled(vec![adb()], &[]).is_empty());
        assert!(enabled(vec![adb()], &["docker".to_string()]).is_empty());
        assert_eq!(wanted("adb devices -l", &on).len(), 1);
        assert_eq!(
            wanted("cd app && ./gradlew build && adb install x.apk", &on).len(),
            1
        );
        assert!(wanted("cargo build", &on).is_empty());
        assert!(wanted("echo adb", &on).is_empty());
    }

    /// The program comes from a folder no sandboxed command can write, and its
    /// real file is in one too: an `adb` the agent could have put first on the
    /// PATH, or linked to from a folder it cannot write, is never run.
    #[cfg(unix)]
    #[test]
    fn the_program_is_never_one_a_sandboxed_command_could_have_written() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let tmp = dunce::canonicalize(tmp.path()).unwrap();
        let (planted, real, linked) = (tmp.join("ws/bin"), tmp.join("sdk"), tmp.join("links"));
        for d in [&planted, &real, &linked] {
            std::fs::create_dir_all(d).unwrap();
        }
        for exe in [planted.join("adb"), real.join("adb")] {
            std::fs::write(&exe, "#!/bin/sh\n").unwrap();
            std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let ws = tmp.join("ws");
        let refused = |p: &Path| p.starts_with(&ws);
        let path = std::env::join_paths([&planted, &real]).unwrap();
        assert_eq!(
            resolve_program("adb", Some(&path), refused),
            Some(real.join("adb")),
            "the planted adb in the workspace is skipped"
        );

        // A link outside the workspace to an adb inside it is refused too.
        std::os::unix::fs::symlink(planted.join("adb"), linked.join("adb")).unwrap();
        let path = std::env::join_paths([&linked]).unwrap();
        assert_eq!(resolve_program("adb", Some(&path), refused), None);

        // A relative PATH entry, or a program given as a path, never counts.
        let path = std::env::join_paths([Path::new("ws/bin")]).unwrap();
        assert_eq!(resolve_program("adb", Some(&path), |_| false), None);
        let path = std::env::join_paths([&real]).unwrap();
        assert_eq!(resolve_program("./adb", Some(&path), |_| false), None);
        assert_eq!(resolve_program("adb", None, |_| false), None);
    }

    /// Each line names what it did and why; a withheld one gives the line a
    /// human types instead.
    #[test]
    fn each_step_says_what_ahma_did_and_why() {
        let s = adb();
        assert_eq!(note(&s, &Step::Nothing, None), None);
        let started = note(&s, &Step::Start, None).unwrap();
        assert!(
            started.contains("started `adb`'s server outside the sandbox"),
            "{started}"
        );
        assert!(started.contains("host_services"), "{started}");
        let restarted = note(&s, &Step::Restart(vec![42]), None).unwrap();
        assert!(restarted.contains("pid 42"), "{restarted}");
        let failed = note(&s, &Step::Start, Some("But it failed.")).unwrap();
        assert!(failed.ends_with("But it failed."), "{failed}");
        let inside = note(&s, &Step::Withhold(Withheld::SelfConfined), None).unwrap();
        assert!(
            inside.contains("`adb kill-server && adb start-server`"),
            "{inside}"
        );
        let net = note(&s, &Step::Withhold(Withheld::NetworkRestricted), None).unwrap();
        assert!(net.contains("--restrict-network"), "{net}");
    }
}
