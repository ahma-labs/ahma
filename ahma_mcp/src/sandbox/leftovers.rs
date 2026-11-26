//! What a sandboxed command leaves running ends with it (SPEC R-PROC.5).
//!
//! A sandbox is inherited for life by every process a command starts. A helper
//! that detaches (a build daemon, a local server) therefore keeps the command's
//! confinement after the command is gone, and whoever finds it next (another
//! checkout's build, the IDE, the human's own terminal) is served by a process
//! that can write only one workspace. The process-group kill (R-PROC.2) does
//! not reach it: detaching is leaving the group.
//!
//! So every command ahma spawns is one *run*, marked twice, and when the
//! command ends ahma looks at whatever still carries a mark and stops what
//! would go on being found by others: a process still in the command's own
//! group (a background job), and a process that left it and accepts
//! connections (a server). Left to finish: a process that left the group and
//! listens on nothing (git's automatic repack, a tool's update check), and one
//! in the group whose input is a pipe, which is still writing out what the
//! command sent it (`tar cf >(xz > out.tar.xz) dir`) and ends at end of
//! input. The marks:
//!
//! - a stamp in its environment ([`RUN_ENV`]), which descendants inherit
//!   wherever they move. macOS does not show the environment of its own system
//!   binaries (`sh`, `python3`, `ssh`), so there this finds third-party
//!   programs only;
//! - on macOS, a rule only this run's Seatbelt profile has
//!   ([`seatbelt_rule`]), which the kernel answers for any process and which
//!   no process can shed.
//!
//! The rule is about lifetime, not about any one tool. Where only the stamp
//! exists (Linux, or an ahma that defers to an outer sandbox) it is hygiene,
//! not a boundary: a process that clears its environment is not found, and
//! on macOS under an outer sandbox neither is a system binary
//! (`python3 -m http.server`, `nc -l`). Such a process stays as confined as
//! it was, and is what `ahma doctor` reports by shape (R-DOCTOR.7).

pub use ahma_common::process_guard::SANDBOX_RUN_ENV as RUN_ENV;

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

/// A process a command left running, which ahma stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Leftover {
    pub pid: u32,
    pub name: String,
}

/// Stamp `cmd` as one run: every process it starts inherits the stamp.
pub fn stamp(cmd: &mut tokio::process::Command) {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or_default();
    cmd.env(
        RUN_ENV,
        format!(
            "{}-{}-{nanos}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ),
    );
}

/// Let what `cmd` starts stay running after it: a command a human declared
/// long-running (SPEC R2.7.10). It carries no stamp, not even an outer run's.
pub fn keep(cmd: &mut tokio::process::Command) {
    cmd.env_remove(RUN_ENV);
}

/// The stamp `cmd` carries, if it does.
fn run_of(cmd: &tokio::process::Command) -> Option<String> {
    cmd.as_std()
        .get_envs()
        .find(|(name, _)| *name == std::ffi::OsStr::new(RUN_ENV))
        .and_then(|(_, value)| value)
        .map(|value| value.to_string_lossy().into_owned())
}

/// One command, as [`end`] finds its processes again.
#[derive(Debug, Clone, PartialEq, Eq)]
// Read only where processes are looked for: the Job Object does it on Windows.
#[cfg_attr(not(unix), allow(dead_code))]
pub struct Run {
    /// The value of [`RUN_ENV`] its processes inherit.
    stamp: String,
    /// The tag of its Seatbelt profile, when ahma built one for it.
    seatbelt_tag: Option<String>,
    /// Its process group: the pid it was spawned as, a group leader.
    group: Option<u32>,
    /// When it was spawned: only processes started since are looked at.
    since: SystemTime,
}

impl Run {
    /// The run `cmd` is, spawned just now as process `pid`, a group leader.
    /// `None` for a command that carries no stamp: one a human let stay
    /// ([`keep`]).
    pub fn of(cmd: &tokio::process::Command, pid: Option<u32>) -> Option<Self> {
        Some(Self {
            stamp: run_of(cmd)?,
            seatbelt_tag: super::kernel_denials::tag_of(cmd.as_std().get_args()),
            group: pid,
            since: SystemTime::now(),
        })
    }
}

/// The Seatbelt rule that marks a profile as run `tag`'s: leave to register
/// one Mach service name nothing looks up. Registration is otherwise denied,
/// so the kernel answers "may this process register `ahma.run.<tag>`?" with
/// yes only for a process under this profile (`runs_under`).
pub fn seatbelt_rule(tag: &str) -> String {
    format!(
        "(allow mach-register (global-name \"{}\"))\n",
        run_service(tag)
    )
}

fn run_service(tag: &str) -> String {
    format!("ahma.run.{tag}")
}

/// Whether `pid` runs under the Seatbelt profile tagged `tag`, asked of the
/// kernel: it may register this run's name and may not register a name no
/// profile grants. The second question is what tells it from a process in no
/// sandbox, or in one that allows every registration, which are told yes to
/// both.
#[cfg(target_os = "macos")]
fn runs_under(pid: u32, tag: &str) -> bool {
    use ahma_common::seatbelt::pid_may_register_mach_name as may_register;
    may_register(pid, &run_service(tag)) && !may_register(pid, &run_service("none"))
}

#[cfg(all(unix, not(target_os = "macos")))]
fn runs_under(_pid: u32, _tag: &str) -> bool {
    false
}

/// Whether `pid` accepts connections: it holds a listening TCP or Unix socket.
/// That is what makes a leftover a server others can find; `false` when it
/// cannot be told.
#[cfg(target_os = "macos")]
fn listens(pid: u32) -> bool {
    // <sys/proc_info.h>: `proc_pidfdinfo` flavor for `struct socket_fdinfo`,
    // and where `psi.soi_options` lies in it (a 24-byte `proc_fileinfo`, then
    // `socket_info`: a 136-byte `vinfo_stat`, two handles, three ints).
    const PROC_PIDFDSOCKETINFO: libc::c_int = 3;
    const SOI_OPTIONS_AT: usize = 24 + 136 + 8 + 8 + 4 + 4 + 4;
    open_files(pid)
        .iter()
        .filter(|fd| fd.proc_fdtype == libc::PROX_FDTYPE_SOCKET as u32)
        .any(|fd| {
            let mut info = [0u8; 1024];
            // SAFETY: the kernel writes at most `info.len()` bytes.
            let wrote = unsafe {
                libc::proc_pidfdinfo(
                    pid as libc::c_int,
                    fd.proc_fd,
                    PROC_PIDFDSOCKETINFO,
                    info.as_mut_ptr().cast(),
                    info.len() as libc::c_int,
                )
            };
            wrote > 0 && wrote as usize >= SOI_OPTIONS_AT + 2 && {
                let options = i16::from_ne_bytes([info[SOI_OPTIONS_AT], info[SOI_OPTIONS_AT + 1]]);
                options as libc::c_int & libc::SO_ACCEPTCONN != 0
            }
        })
}

/// Whether `pid` reads its input from a pipe: it is consuming what another
/// process sends it and ends at end of input. A background job does not: a
/// shell without job control gives one `/dev/null`.
#[cfg(target_os = "macos")]
fn drains_a_pipe(pid: u32) -> bool {
    open_files(pid)
        .iter()
        .any(|fd| fd.proc_fd == 0 && fd.proc_fdtype == libc::PROX_FDTYPE_PIPE as u32)
}

/// The descriptors `pid` holds open; none when it cannot be told.
#[cfg(target_os = "macos")]
fn open_files(pid: u32) -> Vec<libc::proc_fdinfo> {
    let pid = pid as libc::c_int;
    let fd_size = std::mem::size_of::<libc::proc_fdinfo>();
    // SAFETY: a size query; no buffer is written.
    let needed =
        unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, std::ptr::null_mut(), 0) };
    if needed <= 0 {
        return Vec::new();
    }
    // Room for descriptors opened between the two calls.
    let mut fds = vec![
        libc::proc_fdinfo {
            proc_fd: 0,
            proc_fdtype: 0
        };
        needed as usize / fd_size + 16
    ];
    // SAFETY: the buffer is `fds.len() * fd_size` bytes, as passed.
    let got = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDLISTFDS,
            0,
            fds.as_mut_ptr().cast(),
            (fds.len() * fd_size) as libc::c_int,
        )
    };
    if got <= 0 {
        return Vec::new();
    }
    fds.truncate(got as usize / fd_size);
    fds
}

/// Whether `pid` has exited and only awaits its parent's `wait`: a signal
/// still reaches such a process, so "can be signalled" does not mean alive.
///
/// Measured on macOS 27: `proc_pidinfo` answers "no such process" for one,
/// while signal 0 still finds it. (The `SZOMB` status it documents is checked
/// too, for a system that reports it.)
#[cfg(target_os = "macos")]
pub(crate) fn is_zombie(pid: u32) -> bool {
    // <sys/proc.h>: the `p_stat` of a process that has exited.
    const SZOMB: u32 = 5;
    // SAFETY: `proc_bsdinfo` is plain integers and byte arrays; all-zero is valid.
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    // SAFETY: the kernel writes at most `size` bytes into `info`.
    let got = unsafe {
        libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDTBSDINFO,
            0,
            (&raw mut info).cast(),
            size,
        )
    };
    if got == size {
        return info.pbi_status == SZOMB;
    }
    let no_such_process = std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
    no_such_process && signal(pid, 0)
}

/// As above, from `/proc/<pid>/stat`: the state after the command name.
#[cfg(all(unix, not(target_os = "macos")))]
pub(crate) fn is_zombie(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|stat| {
            stat.rsplit_once(')')
                .map(|(_, rest)| rest.trim_start().starts_with('Z'))
        })
        .unwrap_or(false)
}

/// As above, from `/proc`: what descriptor 0 is.
#[cfg(all(unix, not(target_os = "macos")))]
fn drains_a_pipe(pid: u32) -> bool {
    std::fs::read_link(format!("/proc/{pid}/fd/0"))
        .is_ok_and(|target| target.to_string_lossy().starts_with("pipe:["))
}

/// As above, from `/proc`: the sockets `pid` holds, against the listening
/// sockets of its network namespace.
#[cfg(all(unix, not(target_os = "macos")))]
fn listens(pid: u32) -> bool {
    let held: std::collections::HashSet<String> = std::fs::read_dir(format!("/proc/{pid}/fd"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|fd| std::fs::read_link(fd.path()).ok())
        .filter_map(|target| {
            let target = target.to_string_lossy();
            Some(
                target
                    .strip_prefix("socket:[")?
                    .strip_suffix(']')?
                    .to_string(),
            )
        })
        .collect();
    if held.is_empty() {
        return false;
    }
    let table = |name: &str| std::fs::read_to_string(format!("/proc/{pid}/net/{name}"));
    // tcp, tcp6: `sl local rem st … inode`; state 0A is LISTEN, the inode is
    // the tenth column.
    let tcp = ["tcp", "tcp6"]
        .iter()
        .filter_map(|t| table(t).ok())
        .any(|t| {
            t.lines().skip(1).any(|line| {
                let cols: Vec<&str> = line.split_whitespace().collect();
                cols.get(3) == Some(&"0A") && cols.get(9).is_some_and(|inode| held.contains(*inode))
            })
        });
    // unix: `Num RefCount Protocol Flags Type St Inode Path`; flag 0x10000
    // marks a socket that accepts connections.
    tcp || table("unix").is_ok_and(|t| {
        t.lines().skip(1).any(|line| {
            let cols: Vec<&str> = line.split_whitespace().collect();
            cols.get(3)
                .and_then(|flags| u32::from_str_radix(flags, 16).ok())
                .is_some_and(|flags| flags & 0x10000 != 0)
                && cols.get(6).is_some_and(|inode| held.contains(*inode))
        })
    })
}

/// How long a leftover has to exit on `SIGTERM` before it is killed.
#[cfg(unix)]
const TERM_GRACE: std::time::Duration = std::time::Duration::from_millis(500);

/// Stop what `run` left running that others would go on finding, and return
/// it: a process still in the command's group, and one that left the group
/// and accepts connections. Call once the command itself has ended. Each is
/// asked to stop and given half a second (`TERM_GRACE`) before it is killed, so this may
/// block that long: not for a `Drop` ([`end_at_once`]).
///
/// Left alone: a process that left the group and listens on nothing (it is
/// finishing work of its own), one in the group that is still reading the
/// command's output from a pipe (`drains_a_pipe`), and a helper that serves
/// only its own workspace (`confinement::serves_one_workspace`), which no one
/// outside it finds.
#[cfg(unix)]
pub fn end(run: &Run) -> Vec<Leftover> {
    let leftovers = left_running(run);
    stop(&leftovers);
    leftovers
}

/// [`end`] without the grace: what is found is killed outright. For a command
/// that is itself being killed (a timeout, a cancel, a dropped guard), where
/// there is no result to wait for and no thread to block.
#[cfg(unix)]
pub fn end_at_once(run: &Run) -> Vec<Leftover> {
    let leftovers = left_running(run);
    for l in &leftovers {
        signal(l.pid, libc::SIGKILL);
    }
    leftovers
}

/// What [`end`] stops.
#[cfg(unix)]
fn left_running(run: &Run) -> Vec<Leftover> {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
    let since_secs = run
        .since
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
        // Start times are whole seconds, and clocks are read at different moments.
        .saturating_sub(2);
    let me = std::process::id();
    let mut sys = System::new();
    // Processes, not their threads: on Linux each thread is otherwise listed
    // as a process of its own, with the same environment and group.
    let processes = ProcessRefreshKind::nothing().without_tasks();
    sys.refresh_processes_specifics(ProcessesToUpdate::All, true, processes);
    let started_since: Vec<Pid> = sys
        .processes()
        .iter()
        .filter(|(pid, p)| pid.as_u32() > 1 && pid.as_u32() != me && p.start_time() >= since_secs)
        .map(|(pid, _)| *pid)
        .collect();
    if started_since.is_empty() {
        return Vec::new();
    }
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&started_since),
        false,
        processes
            .with_environ(UpdateKind::Always)
            .with_exe(UpdateKind::OnlyIfNotSet),
    );
    let stamp = format!("{RUN_ENV}={}", run.stamp);
    started_since
        .iter()
        .filter_map(|pid| {
            let p = sys.process(*pid)?;
            let environ: Vec<String> = p
                .environ()
                .iter()
                .map(|e| e.to_string_lossy().into_owned())
                .collect();
            let marked = environ.contains(&stamp)
                || run
                    .seatbelt_tag
                    .as_deref()
                    .is_some_and(|tag| runs_under(pid.as_u32(), tag));
            if !marked {
                return None;
            }
            // SAFETY: `getpgid` only reads; a pid that is gone answers -1.
            let group = unsafe { libc::getpgid(pid.as_u32() as libc::pid_t) };
            let in_group = run.group.is_some_and(|g| group == g as libc::pid_t);
            // A job still in the group, unless it is only writing out what the
            // command piped to it; and whatever accepts connections.
            let a_job = in_group && !drains_a_pipe(pid.as_u32());
            if !a_job && !listens(pid.as_u32()) {
                return None;
            }
            let name = p
                .exe()
                .and_then(|exe| exe.file_name())
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| p.name().to_string_lossy().into_owned());
            (!super::confinement::serves_one_workspace(&name, &environ)).then_some(Leftover {
                pid: pid.as_u32(),
                name,
            })
        })
        .collect()
}

#[cfg(not(unix))]
pub fn end(_run: &Run) -> Vec<Leftover> {
    // The Job Object every command runs in ends its processes with it.
    Vec::new()
}

#[cfg(not(unix))]
pub fn end_at_once(run: &Run) -> Vec<Leftover> {
    end(run)
}

/// Send `sig` to `pid`; whether there was a process to take it.
#[cfg(unix)]
fn signal(pid: u32, sig: libc::c_int) -> bool {
    // SAFETY: `kill` takes no memory from us; a pid that is gone is ESRCH.
    unsafe { libc::kill(pid as libc::pid_t, sig) == 0 }
}

/// `SIGTERM`, a short grace, then `SIGKILL` for what is still there.
#[cfg(unix)]
fn stop(leftovers: &[Leftover]) {
    for l in leftovers {
        signal(l.pid, libc::SIGTERM);
    }
    let deadline = std::time::Instant::now() + TERM_GRACE;
    // Exited and not yet reaped is gone: its parent may be slow to `wait`.
    let alive = |l: &&Leftover| signal(l.pid, 0) && !is_zombie(l.pid);
    while leftovers.iter().any(|l| alive(&l)) && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    for l in leftovers.iter().filter(alive) {
        signal(l.pid, libc::SIGKILL);
    }
}

/// The line a command's result carries for what ahma stopped after it
/// (SPEC R-PROC.5): which processes, why, and the two ways to keep one.
pub fn note(stopped: &[Leftover]) -> Option<String> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    let services = super::profiles::host_services(super::profiles::enabled_profile_names());
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let services = Vec::new();
    note_with(stopped, &services)
}

/// [`note`], given the host services the enabled profiles declare. A stopped
/// process that is one of them is a host service started inside the sandbox
/// (SPEC R-PROC.6): keeping it there is not the fix, so its sentence says
/// where it has to run instead, and the line that puts it there.
fn note_with(stopped: &[Leftover], services: &[super::profiles::HostService]) -> Option<String> {
    let (hosted, plain): (Vec<&Leftover>, Vec<&Leftover>) = stopped
        .iter()
        .partition(|l| services.iter().any(|s| s.program == l.name));
    let mut out: Vec<String> = Vec::new();
    if let Some((which, verb)) = which(&plain) {
        out.push(format!(
            "ahma: stopped {which}, which this command left running. A process started inside \
             the sandbox keeps this command's confinement for life, so it would serve other \
             programs while able to write only this workspace; {verb} ended with the command \
             instead. To keep a server running, start it as its own background command (it \
             lives as long as that command does); for a `run_terminal_command` line a human can \
             instead declare the command in `tools.long_running` (`ahma settings explain \
             tools.long_running`)."
        ));
    }
    let mut said: Vec<&str> = Vec::new();
    for s in services {
        let mine: Vec<&Leftover> = hosted
            .iter()
            .copied()
            .filter(|l| l.name == s.program)
            .collect();
        if mine.is_empty() || said.contains(&s.program.as_str()) {
            continue;
        }
        said.push(&s.program);
        let (which, _) = which(&mine).unwrap_or_default();
        out.push(format!(
            "ahma: stopped {which}, the server `{p}` starts when none is running. It holds {res}, \
             so it has to run outside the sandbox: started inside it, it is confined for life and \
             cannot reach {res}, so it finds none, and every later `{p}` would ask it. A human \
             runs `{line}` once, in a terminal outside the sandbox; this command and every later \
             one then reach that server (SPEC R-PROC.6).",
            p = s.program,
            res = s.resource,
            line = s.restart_line(),
        ));
    }
    (!out.is_empty()).then(|| out.join("\n"))
}

/// The processes named for a note, and the pronoun for them.
fn which(stopped: &[&Leftover]) -> Option<(String, &'static str)> {
    let mut names: Vec<String> = stopped
        .iter()
        .map(|l| format!("`{}` (pid {})", l.name, l.pid))
        .collect();
    names.dedup();
    match names.len() {
        0 => None,
        1 => Some((names.remove(0), "it")),
        _ => Some((names.join(", "), "them")),
    }
}

#[cfg(test)]
mod note_tests {
    use super::*;
    use crate::sandbox::profiles::HostService;

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

    fn left(pid: u32, name: &str) -> Leftover {
        Leftover {
            pid,
            name: name.into(),
        }
    }

    /// Field case 2026-11-26: a sandboxed `adb devices` started adb's server,
    /// which could not reach the phone over USB and listed nothing, and the
    /// result said only that a process was stopped. A host service is named
    /// as one, with the line a human types outside the sandbox (SPEC R-PROC.6).
    #[test]
    fn a_host_service_started_inside_the_sandbox_is_named_with_its_restart_line() {
        let line = note_with(&[left(42, "adb")], &[adb()]).expect("a note");
        assert!(line.contains("`adb` (pid 42)"), "{line}");
        assert!(line.contains("USB devices"), "{line}");
        assert!(line.contains("outside the sandbox"), "{line}");
        assert!(
            line.contains("`adb kill-server && adb start-server`"),
            "{line}"
        );
        assert!(
            !line.contains("tools.long_running"),
            "keeping it inside the sandbox is not the fix: {line}"
        );
    }

    /// Anything else stopped keeps the R-PROC.5 sentence, beside the service's.
    #[test]
    fn other_leftovers_keep_their_own_note() {
        let plain = note_with(&[left(7, "server")], &[adb()]).expect("a note");
        assert!(plain.contains("tools.long_running"), "{plain}");
        assert!(!plain.contains("adb"), "{plain}");

        let both = note_with(&[left(7, "server"), left(42, "adb")], &[adb()]).expect("a note");
        assert!(both.contains("`server` (pid 7)"), "{both}");
        assert!(
            both.contains("`adb kill-server && adb start-server`"),
            "{both}"
        );
        assert!(note_with(&[], &[adb()]).is_none());
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use ahma_common::timeouts::{TestTimeouts, TimeoutCategory};
    use std::os::unix::process::CommandExt;

    /// Set only on a [`waiter`], to what it does while it waits: a run of
    /// every ignored test must not wait.
    const WAITER_ENV: &str = "AHMA_TEST_LEFTOVER_WAITER";

    /// A program macOS shows the environment of: this test binary, running a
    /// test that only waits. (`sleep` and `sh` are system binaries, whose
    /// environment macOS shows to no one.) It prints one line when it is
    /// ready, which [`ready`] waits for.
    fn waiter(does: &str, stamp: &str) -> std::process::Command {
        let mut cmd = std::process::Command::new(std::env::current_exe().expect("test binary"));
        cmd.args([
            "sandbox::leftovers::tests::waits_to_be_stopped",
            "--exact",
            "--ignored",
            "--nocapture",
        ])
        .env(WAITER_ENV, does)
        .env(RUN_ENV, stamp)
        // As a shell gives a background job: no input.
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
        cmd
    }

    /// Not a test: what a [`waiter`] runs. Ends by itself if nothing stops it.
    #[test]
    #[ignore = "helper process for the leftover tests"]
    fn waits_to_be_stopped() {
        let Some(does) = std::env::var_os(WAITER_ENV) else {
            return;
        };
        let _listener = (does == "listen")
            .then(|| std::net::TcpListener::bind("127.0.0.1:0").expect("a loopback port"));
        // A plain write: the reader may be gone, and a broken pipe must not panic.
        let _ = std::io::Write::write_all(&mut std::io::stdout(), b"waiter-ready\n");
        std::thread::sleep(TestTimeouts::get(TimeoutCategory::ProcessSpawn));
    }

    /// `child`, once it has said it is ready (its socket, if any, is open).
    fn ready(mut child: std::process::Child) -> std::process::Child {
        use std::io::BufRead;
        let out = child.stdout.take().expect("piped");
        let said = std::io::BufReader::new(out)
            .lines()
            .map_while(Result::ok)
            .any(|line| line.contains("waiter-ready"));
        assert!(said, "the waiter starts");
        child
    }

    /// A process that left its command's group, as a daemon does.
    fn detached(does: &str, stamp: &str) -> std::process::Child {
        ready(waiter(does, stamp).process_group(0).spawn().expect("spawn"))
    }

    fn run(stamp: &str, group: Option<u32>, since: SystemTime) -> Run {
        Run {
            stamp: stamp.to_string(),
            seatbelt_tag: None,
            group,
            since,
        }
    }

    fn exited(child: &mut std::process::Child) -> bool {
        let deadline = std::time::Instant::now() + TestTimeouts::get(TimeoutCategory::Quick);
        while std::time::Instant::now() < deadline {
            if child.try_wait().expect("try_wait").is_some() {
                return true;
            }
            std::thread::sleep(TestTimeouts::poll_interval() / 10);
        }
        false
    }

    fn end_now(mut child: std::process::Child) {
        let _ = child.kill();
        let _ = child.wait();
    }

    /// SPEC R-PROC.5 (field case 2026-11-18): a Gradle daemon started by an
    /// agent's sandboxed build served Android Studio for days, confined to one
    /// checkout, and the only remedy was the doctor's "stop it" each time. A
    /// server that outlives its command by leaving its group is stopped when
    /// the command ends, and so is a job still in the group. A process that
    /// left the group and listens on nothing is finishing its own work (git's
    /// automatic repack) and is left to it; another run's is not touched.
    #[test]
    fn a_command_ends_the_servers_and_jobs_it_left_running() {
        let since = SystemTime::now();
        let mine = format!("test-{}-mine", std::process::id());
        let other = format!("test-{}-other", std::process::id());
        // The command itself: a group leader, with one job still in its group.
        let leader = detached("wait", &mine);
        let mut job = ready(
            waiter("wait", &mine)
                .process_group(leader.id() as i32)
                .spawn()
                .expect("spawn"),
        );
        // Still in the group too, but reading what the command piped to it:
        // `tar cf >(xz > out.tar.xz) dir`. It ends at end of input.
        let mut drainer = ready(
            waiter("wait", &mine)
                .process_group(leader.id() as i32)
                .stdin(std::process::Stdio::piped())
                .spawn()
                .expect("spawn"),
        );
        let mut server = detached("listen", &mine);
        let mut worker = detached("wait", &mine);
        let mut others_server = detached("listen", &other);
        let group = leader.id();
        // The command ends; what it started is still there.
        end_now(leader);

        let stopped = end(&run(&mine, Some(group), since));

        let mut pids: Vec<u32> = stopped.iter().map(|l| l.pid).collect();
        pids.sort_unstable();
        let mut expected = [job.id(), server.id()];
        expected.sort_unstable();
        assert_eq!(pids, expected, "the job and the server, nothing else");
        assert!(
            exited(&mut job),
            "a job still in the command's group is gone"
        );
        assert!(exited(&mut server), "a server that left the group is gone");
        assert!(
            drainer.try_wait().unwrap().is_none(),
            "a process still writing out what the command piped to it is left to finish"
        );
        assert!(
            worker.try_wait().unwrap().is_none(),
            "a detached process that listens on nothing finishes its own work"
        );
        assert!(
            others_server.try_wait().unwrap().is_none(),
            "another run's server keeps running"
        );
        let line = note(&stopped).expect("a line for the result");
        assert!(line.contains(&stopped[0].name), "{line}");
        assert!(line.contains(&server.id().to_string()), "{line}");
        assert!(line.contains("tools.long_running"), "{line}");

        // A command that is itself being killed takes its leftovers with it at
        // once, with no grace to block on.
        let before = std::time::Instant::now();
        assert_eq!(end_at_once(&run(&other, None, since)).len(), 1);
        assert!(before.elapsed() < TERM_GRACE, "no grace is waited for");
        assert!(exited(&mut others_server));
        end_now(worker);
        end_now(drainer);
    }

    /// A process that has exited and is not yet reaped still takes signal 0;
    /// waiting out the whole grace for it would be waiting for its parent.
    #[test]
    fn an_exited_process_not_yet_reaped_counts_as_gone() {
        let mut child = waiter("wait", "test-zombie").spawn().expect("spawn");
        let pid = child.id();
        assert!(!is_zombie(pid), "running");
        child.kill().expect("kill");
        let deadline = std::time::Instant::now() + TestTimeouts::get(TimeoutCategory::Quick);
        while !is_zombie(pid) && std::time::Instant::now() < deadline {
            std::thread::sleep(TestTimeouts::poll_interval() / 10);
        }
        assert!(is_zombie(pid), "exited, not yet waited for");
        assert!(signal(pid, 0), "and still there to signal");
        child.wait().expect("wait");
        assert!(!is_zombie(pid), "reaped");
    }

    #[test]
    fn nothing_left_running_says_nothing() {
        let stamp = format!("test-{}-none", std::process::id());
        assert!(end(&run(&stamp, None, SystemTime::now())).is_empty());
        assert!(note(&[]).is_none());
    }

    /// A command a human declared long-running carries no stamp, so it is no
    /// run and nothing it starts is looked for.
    #[test]
    fn a_kept_command_is_no_run() {
        let mut cmd = tokio::process::Command::new("true");
        stamp(&mut cmd);
        let first = Run::of(&cmd, Some(7)).expect("stamped");
        assert!(
            first.stamp.starts_with(&std::process::id().to_string()),
            "{first:?}"
        );
        assert_eq!(first.group, Some(7));
        let mut again = tokio::process::Command::new("true");
        stamp(&mut again);
        assert_ne!(
            Run::of(&again, None).map(|r| r.stamp),
            Some(first.stamp),
            "each command is its own run"
        );
        keep(&mut cmd);
        assert_eq!(Run::of(&cmd, Some(7)), None);
    }

    /// The kernel-side mark, where it can be tried: a process under a profile
    /// with this run's rule is the run's, one under another run's profile is
    /// not, and neither is a process in no sandbox. macOS refuses to start a
    /// sandbox inside a sandbox, so a run of this suite that is itself
    /// sandboxed (ahma's own hook) can check only the last two.
    #[cfg(target_os = "macos")]
    #[test]
    fn the_kernel_says_which_run_a_process_belongs_to() {
        let unsandboxed = 1;
        assert!(!runs_under(unsandboxed, "0123456789abcdef"));
        assert!(!runs_under(std::process::id(), "0123456789abcdef"));
        if crate::sandbox::prerequisites::process_is_seatbelt_confined() {
            eprintln!("nested in a sandbox: the positive case runs only outside one");
            return;
        }
        let profile = format!(
            "(version 1)\n(deny default)\n(allow process*)\n(allow file-read*)\n\
             (allow sysctl-read)\n(allow mach-lookup)\n{}",
            seatbelt_rule("0123456789abcdef")
        );
        let mut child = std::process::Command::new("/usr/bin/sandbox-exec")
            .args(["-p", &profile, "/bin/sleep", "300"])
            .process_group(0)
            .spawn()
            .expect("sandbox-exec starts");
        let pid = child.id();
        let deadline = std::time::Instant::now() + TestTimeouts::get(TimeoutCategory::Quick);
        while !runs_under(pid, "0123456789abcdef") && std::time::Instant::now() < deadline {
            std::thread::sleep(TestTimeouts::poll_interval() / 10);
        }
        let mine = runs_under(pid, "0123456789abcdef");
        let anothers = runs_under(pid, "fedcba9876543210");
        let _ = child.kill();
        let _ = child.wait();
        assert!(mine, "a system binary under the run's profile is found");
        assert!(!anothers, "and belongs to no other run");
    }
}
