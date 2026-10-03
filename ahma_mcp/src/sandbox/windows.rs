//! # Windows Sandbox Backend — AppContainer spawn isolation + Job Object lifetime
//!
//! This module is the Windows counterpart to `landlock.rs` (Linux) and
//! `seatbelt.rs` (macOS). It supplies the two halves of SPEC R6.3:
//!
//! * **R6.3.2 — process-tree lifetime.** [`enforce_windows_sandbox`](crate::sandbox::windows::enforce_windows_sandbox) assigns the
//!   server process to a Job Object with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, so
//!   no tool subprocess outlives the server. This restricts *nothing* by path.
//! * **R6.3.3 — the filesystem boundary.** Every tool subprocess is launched into
//!   an **AppContainer** whose SID has been granted access to exactly the locked
//!   sandbox scopes, and to nothing else. An AppContainer token is deny-by-default
//!   against the user's files: unless a DACL names the container SID (or a group it
//!   belongs to), the open fails in the kernel, in both directions.
//!
//! ## Why a launcher process exists
//!
//! Putting a child in an AppContainer means passing
//! `PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES` through a `STARTUPINFOEXW`
//! attribute list to `CreateProcessW`. `std::process::Command` cannot carry a
//! proc-thread attribute on stable Rust — `CommandExt::raw_attribute` and
//! `ProcThreadAttributeList` are both `#[unstable]` (rust-lang/rust#114854), and this
//! workspace pins a stable toolchain — so `tokio::process::Command`, which every
//! caller of [`Sandbox::create_command`](crate::sandbox::Sandbox::create_command)
//! expects back, physically cannot make the call.
//!
//! So Windows does what macOS already does. On macOS the sandboxed command is not
//! spawned directly either: it is wrapped in `sandbox-exec`, which applies the
//! profile and then execs the real program. Here the wrapper is **`ahma.exe`
//! itself**, re-entered with a reserved first argument
//! ([`LAUNCHER_ARGV0`](crate::sandbox::windows::LAUNCHER_ARGV0)). The launcher applies the AppContainer attribute, spawns
//! the real program with `CreateProcessW`, keeps it in a kill-on-close job, waits,
//! and exits with the child's exit code. Standard handles are inherited straight
//! through, so the pipes tokio created are the pipes the real program writes to and
//! the extra process is invisible to the caller.
//!
//! The launcher re-entry is dispatched by [`appcontainer_launcher_hook`], which the
//! `ahma` binary calls as the first statement of `main` — before any CLI parsing,
//! because the reserved argument is deliberately not a clap subcommand.
//!
//! ## What is granted, and what is reverted
//!
//! An AppContainer starts with access to nothing of the user's, so the scopes have
//! to be granted explicitly by adding one inheritable `ACCESS_ALLOWED` ACE naming
//! the container SID to the DACL of each scope root — write scopes get
//! read/write/execute/delete, read scopes get read/execute. This **mutates the
//! user's filesystem ACLs**, so every grant is recorded and reverted:
//!
//! * `AppContainerSession` (Windows-only) holds the grant list and revokes on
//!   [`cleanup_windows_sandbox`](crate::sandbox::windows::cleanup_windows_sandbox) (session teardown).
//! * Before mutating, the grant list is journalled to
//!   `%LOCALAPPDATA%\ahma\appcontainer-grants\<pid>.grants`. [`enforce_windows_sandbox`](crate::sandbox::windows::enforce_windows_sandbox)
//!   sweeps that directory at startup and revokes any journal whose owning process
//!   is gone, so a `SIGKILL`/crash/power-loss leaves at most one stale ACE until the
//!   next run, not forever.
//! * The container name is derived from the scope path, so two concurrent sessions
//!   on the same workspace share one container; the sweep therefore only revokes a
//!   container's grants when no *live* journal still references it.
//! * Grants are revoke-then-grant, so repeated runs never stack duplicate ACEs.
//!
//! System directories are deliberately **not** touched: `%SystemRoot%` and
//! `%ProgramFiles%` already carry a default `ALL APPLICATION PACKAGES` read+execute
//! ACE, and every AppContainer token is a member of that group. Toolchains living
//! under the user profile (`~\.cargo\bin`, `~\.rustup`) are reached through the
//! ordinary read-scope grant.
//!
//! ## Known platform limitations (must be disclosed, not papered over — SPEC R7)
//!
//! * **Loopback is blocked.** AppContainer forbids loopback connections unless the
//!   container is registered with `CheckNetIsolation LoopbackExempt`. The guarded
//!   egress proxy (`--restrict-network`, R-NET) binds `127.0.0.1`, so a sandboxed
//!   child cannot reach it. `--restrict-network` and Windows AppContainer isolation
//!   are therefore mutually exclusive today.
//! * **`%TEMP%` is redirected.** The user's temp directory is outside every scope,
//!   so `TEMP`/`TMP` are pointed at the per-container folder Windows creates
//!   (`GetAppContainerFolderPath`), which the container always owns.
//! * **Only the internet-client capability is granted.** No private-network, no
//!   documents/pictures library, no removable storage.
//!
//! ## Verification status: executed, and **not yet explained**
//!
//! This was written on macOS and type-checked against `windows-sys`. It has since
//! been executed on `windows-latest`, and no run has shown the in-scope write
//! succeeding, so the spawn path is switched off rather than shipped unproven. See
//! [`appcontainer_spawn_enabled`](crate::sandbox::windows::appcontainer_spawn_enabled), which is the single place that verdict lives,
//! and `sandbox/command.rs`, which reads it.
//!
//! What the evidence establishes (`appcontainer_dacl_diagnostics`, CI run 37108502254,
//! 2026-10-03):
//!
//! * **The boundary holds both ways.** Inside the container the shell-free
//!   probe and `cmd.exe` write and read inside the scope, and are denied writing or
//!   reading a sibling directory outside it, with or without a Low integrity label,
//!   under `%TEMP%` and `D:\a\_temp`.
//! * **`NUL` is denied.** The `NUL` device grants application packages nothing, so
//!   any tool that writes to it fails; changing that needs an administrator.
//! * **Ancestors are denied.** stat, list and canonicalize of every ancestor of the
//!   scope fail (they grant the container nothing), so `GetFinalPathNameByHandle`
//!   on an in-scope file fails, and Windows PowerShell 5.1 fails when its working
//!   directory is an 8.3 short path, because .NET Framework expands it with
//!   `GetLongPathName`. PowerShell 7 works.
//!
//! The spawn stays disabled until those two blockers have a design.
//!
//! Everything below therefore compiles and is unit-tested, and none of it is
//! reachable in production. That is deliberate while a fix is in flight; if it
//! stops being in flight, this note is the place to say so.
//!
//! ## The probe: evidence that needs no shell
//!
//! A shell is a poor instrument inside an AppContainer, because the shell itself
//! may be what fails. So `ahma.exe` has a second reserved first argument,
//! [`PROBE_ARGV0`](crate::sandbox::windows::PROBE_ARGV0). Started through the launcher above, it runs inside exactly
//! the AppContainer and Job setup a tool would. It opens each path it is given with
//! `std::fs` (that is `CreateFileW`, and a failure carries the raw `GetLastError`
//! code), and it reports the token it actually holds: the AppContainer flag, the
//! integrity level, the groups and the capabilities.
//!
//! `windows_sandbox_integration_test::appcontainer::appcontainer_dacl_diagnostics`
//! drives it, together with a `cmd.exe` redirect probe and a PowerShell
//! module-load probe. It varies the scope location (`%TEMP%`, `%RUNNER_TEMP%`) and
//! the scope's integrity label, prints one labelled result table, and asserts
//! only what the harness controls: that the probe ran and reported every case.
//! The `AppContainer diagnostics` step in `build.yml` runs it on every Windows
//! leg. Read that table before changing anything here.
//!
//! SPEC R6.3.3 stays open, R6.3.9's disclosure stays as written, and
//! `red_team_command_write_escape_blocked` stays `#[cfg_attr(windows, ignore)]`
//! until a `windows-latest` run shows the in-scope write *and* read succeeding
//! and the out-of-scope pair blocked.

use super::error::SandboxError;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// windows-sys imports
// ---------------------------------------------------------------------------
#[cfg(target_os = "windows")]
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, ERROR_INSUFFICIENT_BUFFER, ERROR_SUCCESS, FALSE, HANDLE,
    HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE, LocalFree, S_OK, SetHandleInformation, TRUE,
    WAIT_OBJECT_0,
};

#[cfg(target_os = "windows")]
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject,
};

#[cfg(target_os = "windows")]
use windows_sys::Win32::System::Threading::{
    CREATE_SUSPENDED, CreateProcessW, DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT,
    GetCurrentProcess, GetExitCodeProcess, INFINITE, InitializeProcThreadAttributeList,
    OpenProcess, PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES, PROCESS_INFORMATION,
    PROCESS_QUERY_LIMITED_INFORMATION, ResumeThread, STARTF_USESTDHANDLES, STARTUPINFOEXW,
    UpdateProcThreadAttribute, WaitForSingleObject,
};

#[cfg(target_os = "windows")]
use windows_sys::Win32::System::Console::{
    GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
};

#[cfg(target_os = "windows")]
use windows_sys::Win32::Security::{
    ACL, CopySid, CreateWellKnownSid, DACL_SECURITY_INFORMATION, FreeSid, GetLengthSid, IsValidSid,
    PSID, SECURITY_CAPABILITIES, SECURITY_MAX_SID_SIZE, SID_AND_ATTRIBUTES,
    SUB_CONTAINERS_AND_OBJECTS_INHERIT, WinCapabilityInternetClientSid,
};

#[cfg(target_os = "windows")]
use windows_sys::Win32::Security::Authorization::{
    EXPLICIT_ACCESS_W, GRANT_ACCESS, GetNamedSecurityInfoW, NO_MULTIPLE_TRUSTEE, REVOKE_ACCESS,
    SE_FILE_OBJECT, SetEntriesInAclW, SetNamedSecurityInfoW, TRUSTEE_IS_GROUP, TRUSTEE_IS_SID,
    TRUSTEE_W,
};

#[cfg(target_os = "windows")]
use windows_sys::Win32::Security::Isolation::{
    CreateAppContainerProfile, DeleteAppContainerProfile,
    DeriveAppContainerSidFromAppContainerName, GetAppContainerFolderPath,
};

#[cfg(target_os = "windows")]
use windows_sys::Win32::Storage::FileSystem::{
    DELETE, FILE_DELETE_CHILD, FILE_GENERIC_EXECUTE, FILE_GENERIC_READ, FILE_GENERIC_WRITE,
};

/// `SE_GROUP_ENABLED`. Lives in `Win32_System_SystemServices`, an enormous feature
/// this crate has no other use for, so the one constant is spelled out here.
#[cfg(target_os = "windows")]
const SE_GROUP_ENABLED: u32 = 0x0000_0004;

// ===========================================================================
// Platform-independent core
//
// Everything in this section is compiled on every platform (see the `pub mod
// windows` declaration in `sandbox/mod.rs`) precisely so that the argv protocol,
// the container-name derivation, the launcher resolution and the crash-recovery
// journal are exercised by `cargo nextest run` on Linux and macOS. The Win32
// calls below cannot be; the logic feeding them can, and is.
// ===========================================================================

/// Reserved first argument that re-enters `ahma.exe` as the AppContainer
/// launcher. Deliberately not a clap subcommand: [`appcontainer_launcher_hook`]
/// consumes it before any CLI parsing happens, so it can never collide with, or
/// be discovered through, the user-facing command surface.
pub const LAUNCHER_ARGV0: &str = "__ahma-appcontainer-exec";

/// Build a deterministic AppContainer profile name from a sandbox scope path.
///
/// Windows requires ≤ 64 characters of alphanumerics, `-` and `.`, so the scope is
/// reduced to an FNV-1a hash. Determinism is load-bearing in three places: the
/// launcher re-derives the SID from the name alone (no state passed across the
/// process boundary), two concurrent sessions on one workspace share a container
/// rather than fighting over its ACEs, and a crashed session's leftover ACE is the
/// exact ACE the next session would add anyway.
pub fn appcontainer_name_for_scope(scope: &Path) -> String {
    let raw = scope.as_os_str().to_string_lossy();
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in raw.as_bytes() {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("ahma-sandbox-{hash:016x}")
}

/// The argv `ahma.exe` is re-entered with to launch `program` inside `container`.
///
/// Everything after `--` is the real command, so a tool argument can never be
/// mistaken for a launcher argument no matter what it looks like.
pub fn launcher_args(container: &str, program: &str, args: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(args.len() + 4);
    out.push(LAUNCHER_ARGV0.to_string());
    out.push(container.to_string());
    out.push("--".to_string());
    out.push(program.to_string());
    out.extend(args.iter().cloned());
    out
}

/// A decoded launcher re-entry: which container, and what to run inside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LauncherRequest {
    pub container: String,
    pub program: String,
    pub args: Vec<String>,
}

/// Decode a process argv (including `argv[0]`, the executable path) into a
/// [`LauncherRequest`], or `None` when this process is not a launcher re-entry.
///
/// Returning `None` rather than an error for a malformed re-entry would let a
/// corrupted invocation fall through to the ordinary CLI and run *unsandboxed*,
/// so a request that starts with the marker but does not parse is an `Err`.
pub fn parse_launcher_args<I>(argv: I) -> Result<Option<LauncherRequest>, String>
where
    I: IntoIterator<Item = String>,
{
    let mut it = argv.into_iter();
    let _exe = it.next();
    match it.next() {
        Some(marker) if marker == LAUNCHER_ARGV0 => {}
        _ => return Ok(None),
    }
    let container = it
        .next()
        .ok_or_else(|| format!("{LAUNCHER_ARGV0}: missing container name"))?;
    if container.is_empty() {
        return Err(format!("{LAUNCHER_ARGV0}: empty container name"));
    }
    match it.next() {
        Some(sep) if sep == "--" => {}
        other => {
            return Err(format!(
                "{LAUNCHER_ARGV0}: expected `--` before the command, found {other:?}"
            ));
        }
    }
    let program = it
        .next()
        .ok_or_else(|| format!("{LAUNCHER_ARGV0}: missing program after `--`"))?;
    Ok(Some(LauncherRequest {
        container,
        program,
        args: it.collect(),
    }))
}

// ---------------------------------------------------------------------------
// The diagnostic probe (SPEC R6.3.3)
// ---------------------------------------------------------------------------

/// Reserved first argument that re-enters `ahma.exe` as the AppContainer
/// **diagnostic probe**. Like [`LAUNCHER_ARGV0`] it is consumed by
/// [`appcontainer_launcher_hook`] before any CLI parsing, so it is not a clap
/// subcommand and cannot collide with one.
///
/// It exists because a shell is a poor instrument inside an AppContainer: on
/// `windows-latest`, Windows PowerShell 5.1 could not load its own modules there,
/// so every PowerShell-based observation was of PowerShell failing, not of the
/// boundary. The probe is ahma's own code. It needs nothing beyond the file
/// system calls under test, and it reports exactly what each call returned.
pub const PROBE_ARGV0: &str = "__ahma-appcontainer-probe";

/// Every line the probe reports starts with this, so the report can be picked
/// out of whatever else shares the stream.
pub const PROBE_LINE_PREFIX: &str = "AHMA-PROBE ";

/// The key the probe reports last. Its presence proves the probe ran to the end
/// rather than dying part way through.
pub const PROBE_DONE_KEY: &str = "done";

/// One file-system operation the probe can attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeOp {
    /// Open for writing, creating the file if absent, then write a few bytes.
    Write,
    /// Read the whole file.
    Read,
    /// Query metadata, which opens the path with no data access.
    Stat,
    /// Enumerate a directory.
    List,
    /// Resolve the final path (`GetFinalPathNameByHandleW` on Windows).
    Canonicalize,
}

impl ProbeOp {
    /// The argv spelling of this operation.
    pub fn as_str(self) -> &'static str {
        match self {
            ProbeOp::Write => "write",
            ProbeOp::Read => "read",
            ProbeOp::Stat => "stat",
            ProbeOp::List => "list",
            ProbeOp::Canonicalize => "canon",
        }
    }

    /// Inverse of [`ProbeOp::as_str`].
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "write" => Some(ProbeOp::Write),
            "read" => Some(ProbeOp::Read),
            "stat" => Some(ProbeOp::Stat),
            "list" => Some(ProbeOp::List),
            "canon" => Some(ProbeOp::Canonicalize),
            _ => None,
        }
    }
}

/// One labelled probe: what to attempt, and on which path. The label becomes the
/// report key `case.<label>` (see [`probe_case_key`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeCase {
    pub label: String,
    pub op: ProbeOp,
    pub path: PathBuf,
}

impl ProbeCase {
    pub fn new(label: impl Into<String>, op: ProbeOp, path: impl Into<PathBuf>) -> Self {
        Self {
            label: label.into(),
            op,
            path: path.into(),
        }
    }
}

/// The report key under which a case's outcome appears.
pub fn probe_case_key(label: &str) -> String {
    format!("case.{label}")
}

/// The argv, after the executable, that re-enters `ahma.exe` as the probe: the
/// marker, then one `<op> <label> <path>` triple per case.
pub fn probe_args(cases: &[ProbeCase]) -> Vec<String> {
    let mut out = Vec::with_capacity(1 + cases.len() * 3);
    out.push(PROBE_ARGV0.to_string());
    for case in cases {
        out.push(case.op.as_str().to_string());
        out.push(case.label.clone());
        out.push(case.path.to_string_lossy().into_owned());
    }
    out
}

/// Decode a process argv (including `argv[0]`) into the probe's cases, or `None`
/// when this process is not a probe re-entry.
///
/// A probe with no cases is valid: it still reports its token. A re-entry that
/// starts with the marker but does not parse is an `Err`, for the same reason as
/// in [`parse_launcher_args`]: it must never fall through to the ordinary CLI.
pub fn parse_probe_args<I>(argv: I) -> Result<Option<Vec<ProbeCase>>, String>
where
    I: IntoIterator<Item = String>,
{
    let mut it = argv.into_iter();
    let _exe = it.next();
    match it.next() {
        Some(marker) if marker == PROBE_ARGV0 => {}
        _ => return Ok(None),
    }
    let rest: Vec<String> = it.collect();
    if !rest.len().is_multiple_of(3) {
        return Err(format!(
            "{PROBE_ARGV0}: expected `<op> <label> <path>` triples, got {} trailing argument(s)",
            rest.len() % 3
        ));
    }
    rest.chunks(3)
        .map(|triple| -> Result<ProbeCase, String> {
            let op = ProbeOp::parse(&triple[0])
                .ok_or_else(|| format!("{PROBE_ARGV0}: unknown operation {:?}", triple[0]))?;
            let label = &triple[1];
            if label.is_empty() || label.contains('=') || label.contains(char::is_whitespace) {
                return Err(format!(
                    "{PROBE_ARGV0}: label {label:?} must be non-empty, without `=` or whitespace"
                ));
            }
            if triple[2].is_empty() {
                return Err(format!("{PROBE_ARGV0}: empty path for {label:?}"));
            }
            Ok(ProbeCase::new(label.clone(), op, triple[2].clone()))
        })
        .collect::<Result<Vec<_>, _>>()
        .map(Some)
}

/// Describe an I/O outcome the way the probe reports it: `ok`, `ok:<detail>`, or
/// `err:<os error code>:<message>`. The code is the raw `GetLastError` value on
/// Windows (`5` is `ERROR_ACCESS_DENIED`), so it can be read without guessing
/// from a localised message.
pub fn describe_probe_outcome(result: std::io::Result<String>) -> String {
    match result {
        Ok(detail) if detail.is_empty() => "ok".to_string(),
        Ok(detail) => format!("ok:{detail}"),
        Err(e) => match e.raw_os_error() {
            Some(code) => format!("err:{code}:{e}"),
            None => format!("err:-:{e}"),
        },
    }
}

/// Attempt one case against the real file system and describe the outcome.
pub fn run_probe_case(case: &ProbeCase) -> String {
    use std::io::Write as _;
    let result: std::io::Result<String> = match case.op {
        ProbeOp::Write => std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(&case.path)
            .and_then(|mut f| f.write_all(b"ahma-probe\n"))
            .map(|()| String::new()),
        ProbeOp::Read => std::fs::read(&case.path).map(|bytes| format!("{} bytes", bytes.len())),
        ProbeOp::Stat => std::fs::metadata(&case.path).map(|m| {
            if m.is_dir() {
                "dir".to_string()
            } else {
                format!("file, {} bytes", m.len())
            }
        }),
        ProbeOp::List => std::fs::read_dir(&case.path).and_then(|entries| {
            let mut n = 0usize;
            for entry in entries {
                entry?;
                n += 1;
            }
            Ok(format!("{n} entries"))
        }),
        ProbeOp::Canonicalize => {
            std::fs::canonicalize(&case.path).map(|p| p.to_string_lossy().into_owned())
        }
    };
    describe_probe_outcome(result)
}

/// Format one report line. Line breaks in the value are flattened, so one report
/// is always one line.
pub fn probe_report_line(key: &str, value: &str) -> String {
    let value: String = value
        .chars()
        .map(|c| if c == '\r' || c == '\n' { ' ' } else { c })
        .collect();
    format!("{PROBE_LINE_PREFIX}{key}={value}")
}

/// Pick the `key=value` reports out of mixed output, in order. Lines without
/// [`PROBE_LINE_PREFIX`] are ignored, and a repeated key keeps every occurrence
/// (token groups repeat). Values are trimmed, because `cmd.exe`'s `echo` keeps
/// the space before a closing parenthesis.
pub fn parse_probe_report(output: &str) -> Vec<(String, String)> {
    output
        .lines()
        .filter_map(|line| {
            let rest = line.trim_start().strip_prefix(PROBE_LINE_PREFIX)?;
            let (key, value) = rest.split_once('=').unwrap_or((rest, ""));
            Some((key.trim().to_string(), value.trim().to_string()))
        })
        .collect()
}

/// Run the probe: report its own context, the token (Windows only), every case
/// in order, and finally [`PROBE_DONE_KEY`]. Each report goes to `emit` as soon as
/// it is known, so a probe that hangs or dies still leaves what it had.
pub fn probe_report(cases: &[ProbeCase], emit: &mut dyn FnMut(&str, &str)) {
    let exe = std::env::current_exe().map(|p| p.to_string_lossy().into_owned());
    emit("exe", &describe_probe_outcome(exe));
    let cwd = std::env::current_dir().map(|p| p.to_string_lossy().into_owned());
    emit("cwd", &describe_probe_outcome(cwd));
    for var in ["TEMP", "TMP", "LOCALAPPDATA", "USERPROFILE"] {
        let value = std::env::var(var).unwrap_or_else(|_| "<unset>".to_string());
        emit(&format!("env.{var}"), &value);
    }
    #[cfg(target_os = "windows")]
    probe_token_report(emit);
    for case in cases {
        emit(&probe_case_key(&case.label), &run_probe_case(case));
    }
    emit(PROBE_DONE_KEY, "1");
}

/// Name a mandatory integrity level from the last sub-authority (RID) of its
/// `S-1-16-*` label SID.
pub fn integrity_level_name(rid: u32) -> &'static str {
    match rid {
        0x0000 => "Untrusted",
        0x1000 => "Low",
        0x2000 => "Medium",
        0x2100 => "Medium Plus",
        0x3000 => "High",
        0x4000 => "System",
        0x5000 => "Protected Process",
        _ => "unrecognised",
    }
}

/// Locate the `ahma` executable that knows how to act as the launcher.
///
/// `current_exe` is usually `ahma.exe` itself, but not always: an integration test
/// binary under `target\debug\deps\` links the same library and builds the same
/// sandboxed commands, and it has no launcher dispatch of its own. Hence the
/// walk outwards, then `PATH`.
///
/// Split from [`resolve_launcher_exe`] so the search order is testable without a
/// Windows filesystem: `exists` is injected, and the candidate list is pure.
pub fn resolve_launcher_exe_from(
    current_exe: &Path,
    path_dirs: &[PathBuf],
    exe_name: &str,
    exists: &dyn Fn(&Path) -> bool,
) -> Option<PathBuf> {
    // 1. We *are* ahma.
    let stem = current_exe
        .file_stem()
        .map(|s| s.to_string_lossy().to_ascii_lowercase());
    if stem.as_deref() == Some("ahma") {
        return Some(current_exe.to_path_buf());
    }
    // 2/3. Alongside, then one directory up (`target\debug\deps\` -> `target\debug\`).
    let mut dir = current_exe.parent();
    for _ in 0..2 {
        let Some(d) = dir else { break };
        let candidate = d.join(exe_name);
        if exists(&candidate) {
            return Some(candidate);
        }
        dir = d.parent();
    }
    // 4. PATH.
    for d in path_dirs {
        let candidate = d.join(exe_name);
        if exists(&candidate) {
            return Some(candidate);
        }
    }
    None
}

/// [`resolve_launcher_exe_from`] against the real process and environment.
///
/// Memoized: this runs on the way to *every* sandboxed spawn, and neither the
/// running executable nor `PATH` changes underneath a process, so repeating the
/// filesystem probes would be pure overhead on a hot path.
pub fn resolve_launcher_exe() -> Result<PathBuf, SandboxError> {
    static CACHED: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    if let Some(cached) = CACHED.get_or_init(|| resolve_launcher_exe_uncached().ok()) {
        return Ok(cached.clone());
    }
    // Not cached as an error: the message is rebuilt so it names the executable
    // and stays actionable, and this path ends in a hard failure anyway.
    resolve_launcher_exe_uncached()
}

fn resolve_launcher_exe_uncached() -> Result<PathBuf, SandboxError> {
    let exe_name = crate::update::AHMA_BINARY_NAME;
    let current = std::env::current_exe().map_err(|e| {
        SandboxError::PrerequisiteFailed(format!("cannot determine the running executable: {e}"))
    })?;
    let path_dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    resolve_launcher_exe_from(&current, &path_dirs, exe_name, &|p| p.is_file()).ok_or_else(|| {
        SandboxError::PrerequisiteFailed(format!(
            "Windows AppContainer isolation needs the `{exe_name}` launcher, which could not be \
             found next to '{}' or on PATH. Install ahma so `{exe_name}` is on PATH, or start the \
             server with --no-sandbox to run without kernel enforcement.",
            current.display()
        ))
    })
}

/// Serialize a grant journal: container name on the first line, one granted path
/// per line after it. Deliberately not JSON — the sweep that reads this back runs
/// during startup on a possibly corrupt file, and a line-oriented format degrades
/// into "skip that line" instead of "the whole journal is unreadable". Windows
/// paths cannot contain `\n`, so the encoding is unambiguous.
pub fn encode_grant_journal(container: &str, paths: &[PathBuf]) -> String {
    let mut s = String::new();
    s.push_str(container);
    s.push('\n');
    for p in paths {
        s.push_str(&p.to_string_lossy());
        s.push('\n');
    }
    s
}

/// Inverse of [`encode_grant_journal`]. `None` when the journal has no container
/// name; a journal with a name and no paths is valid (nothing to revoke).
pub fn decode_grant_journal(text: &str) -> Option<(String, Vec<PathBuf>)> {
    let mut lines = text.lines();
    let container = lines.next()?.trim().to_string();
    if container.is_empty() {
        return None;
    }
    let paths = lines
        .filter(|l| !l.trim().is_empty())
        .map(PathBuf::from)
        .collect();
    Some((container, paths))
}

/// Whether an AppContainer can already read+execute `path` without ahma touching
/// any DACL, because Windows ships a default `ALL APPLICATION PACKAGES` ACE there
/// and every AppContainer token is a member of that group.
///
/// This is what keeps ahma out of the system directories: attempting to add an ACE
/// to `%SystemRoot%` would need administrator rights, would fail, and would take a
/// tool call down with it — for access the container already has.
pub fn appcontainer_has_default_read(path: &Path, system_dirs: &[PathBuf]) -> bool {
    system_dirs.iter().any(|d| {
        // Windows paths are case-insensitive; `Path::starts_with` is not.
        let p = path.to_string_lossy().to_ascii_lowercase();
        let d = d.to_string_lossy().to_ascii_lowercase();
        !d.is_empty() && (p == d || p.starts_with(&format!("{d}\\")))
    })
}

/// The directories [`appcontainer_has_default_read`] treats as pre-granted, read
/// from the environment (`%SystemRoot%`, `%ProgramFiles%`, `%ProgramFiles(x86)%`,
/// `%ProgramW6432%`).
pub fn default_appcontainer_readable_dirs() -> Vec<PathBuf> {
    [
        "SystemRoot",
        "ProgramFiles",
        "ProgramFiles(x86)",
        "ProgramW6432",
    ]
    .iter()
    .filter_map(std::env::var_os)
    .map(PathBuf::from)
    .collect()
}

/// Quote one argument for a Windows command line so `CommandLineToArgvW` — which
/// is what `CreateProcessW` children use to rebuild `argv` — reproduces it byte for
/// byte.
///
/// The rule that trips people up is that a backslash is only an escape *when it
/// precedes a quote*: `a\b` stays `a\b`, but `a\"` needs the backslash doubled.
fn needs_windows_quotes(arg: &str) -> bool {
    arg.is_empty() || arg.contains([' ', '\t', '\n', '\u{b}', '"'])
}

pub fn quote_windows_arg(arg: &str) -> String {
    if !needs_windows_quotes(arg) {
        return arg.to_string();
    }
    let mut out = String::with_capacity(arg.len() + 2);
    out.push('"');
    let mut backslashes = 0usize;
    for c in arg.chars() {
        match c {
            '\\' => {
                backslashes += 1;
                out.push('\\');
            }
            '"' => {
                append_escaped_quote(&mut out, backslashes);
                backslashes = 0;
            }
            _ => {
                backslashes = 0;
                out.push(c);
            }
        }
    }
    append_trailing_backslashes(&mut out, backslashes);
    out.push('"');
    out
}

fn append_escaped_quote(out: &mut String, backslashes: usize) {
    for _ in 0..backslashes {
        out.push('\\');
    }
    out.push('\\');
    out.push('"');
}

fn append_trailing_backslashes(out: &mut String, backslashes: usize) {
    for _ in 0..backslashes {
        out.push('\\');
    }
}

/// Assemble the `lpCommandLine` string for `CreateProcessW`.
pub fn build_command_line(program: &str, args: &[String]) -> String {
    let mut s = quote_windows_arg(program);
    for a in args {
        s.push(' ');
        s.push_str(&quote_windows_arg(a));
    }
    s
}

/// What a Windows-sandboxed spawn needs: the launcher to run, the argv to run it
/// with, and the environment overrides the AppContainer child requires.
///
/// Returned rather than a finished `Command` so `sandbox/command.rs` can build it
/// through `Sandbox::base_command` and pick up the secret/code-injection env scrub
/// (`scrub_secret_env`), the egress-proxy variables and `kill_on_drop` that every
/// other platform's spawn path already gets. The previous Windows arm constructed
/// its own `std::process::Command` and silently skipped all three.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowsSpawnPlan {
    pub launcher: PathBuf,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

// ===========================================================================
// Public API
// ===========================================================================

/// Check whether the Windows sandbox prerequisites are met (SPEC R6.3.1).
///
/// Two things must hold before strict mode can promise a filesystem boundary:
/// the AppContainer API has to exist (Windows 8+), and the `ahma.exe` launcher
/// this backend spawns through has to be findable. Either missing means ahma
/// cannot sandbox, and per SPEC R7 that is a startup failure, not a downgrade.
pub fn check_windows_sandbox_available() -> Result<(), SandboxError> {
    #[cfg(target_os = "windows")]
    {
        probe_appcontainer_api()?;
        resolve_launcher_exe()?;
        Ok(())
    }
    #[cfg(not(target_os = "windows"))]
    {
        Err(SandboxError::PrerequisiteFailed(
            "Windows AppContainer is only available on Windows".into(),
        ))
    }
}

/// Whether Windows command spawns are **actually** routed through AppContainer
/// isolation (SPEC R6.3.3).
///
/// This is the single source of truth for that question, and it exists because the
/// answer was previously re-derived in two places that then disagreed.
/// `sandbox/command.rs` decides whether to build a [`WindowsSpawnPlan`];
/// `shell/modes/server.rs` decides whether the guarded egress proxy can be reached
/// by a sandboxed child (R6.3.3.1a — an AppContainer blocks loopback, so the proxy
/// and the container are mutually exclusive). When `command.rs` disabled the
/// AppContainer path in #558 and `server.rs` did not, Windows lost the filesystem
/// boundary *and* kept refusing `--restrict-network`, while telling the operator
/// that "every command is launched into an AppContainer" — a claim that had stopped
/// being true. Two consumers, one fact: they read it here.
///
/// Currently `false` on every platform. On Windows it stays `false` until a
/// `windows-latest` CI run demonstrates the R6.3.3 boundary in both directions —
/// an in-scope write and read succeeding *and* the out-of-scope pair blocked. A
/// containment layer that fails **broken** rather than **safe** is worse than none,
/// and no run has yet shown the in-scope write succeeding: the shell used to test
/// it could not load its own modules inside the container, so whether the
/// container or the shell was at fault is not known (see the module docs).
///
/// Flipping this to `true` on Windows re-enables the container spawn path and
/// re-disables `--restrict-network` there, together and by construction.
pub const fn appcontainer_spawn_enabled() -> bool {
    false
}

/// Prepare an AppContainer for `write_scopes`/`read_scopes` and describe how to
/// launch `program` inside it.
///
/// Idempotent per session: the first call creates the profile and adds the DACL
/// entries, later calls reuse them. Any failure is returned, never swallowed — a
/// caller that cannot build this plan must not fall back to an unsandboxed spawn
/// (SPEC R7).
pub fn plan_windows_sandboxed_spawn(
    program: &str,
    args: &[String],
    write_scopes: &[PathBuf],
    read_scopes: &[PathBuf],
) -> anyhow::Result<WindowsSpawnPlan> {
    #[cfg(target_os = "windows")]
    {
        let launcher = resolve_launcher_exe()?;
        let (container, container_folder) = ensure_appcontainer(write_scopes, read_scopes)?;
        let mut env = Vec::new();
        if let Some(folder) = container_folder {
            // The user's %TEMP% is outside every scope and unreachable from the
            // container. The per-container folder always is reachable, so point
            // the child's temp at it rather than letting every tool that writes a
            // temp file fail with an unexplained access denial.
            let s = folder.to_string_lossy().into_owned();
            env.push(("TEMP".to_string(), s.clone()));
            env.push(("TMP".to_string(), s));
        }
        Ok(WindowsSpawnPlan {
            args: launcher_args(&container, program, args),
            launcher,
            env,
        })
    }
    #[cfg(not(target_os = "windows"))]
    {
        // Compiled but unreachable off Windows: `sandbox/command.rs` only calls
        // this under `#[cfg(target_os = "windows")]`. It exists so the module
        // type-checks (and its pure half is unit-tested) on every platform.
        let _ = (write_scopes, read_scopes);
        Err(anyhow::anyhow!(
            "AppContainer isolation is only available on Windows (asked to run {program} {args:?})"
        ))
    }
}

/// Apply Job Object restrictions to the current server process (SPEC R6.3.2), and
/// sweep any ACL grants left behind by a previous run that died without cleaning up.
///
/// Sets `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` so all child processes are terminated
/// when the server exits. This is defense-in-depth and process-lifetime only — it
/// does **not** restrict file-system access by path in either direction; that is
/// [`plan_windows_sandboxed_spawn`]'s job.
///
/// The job handle is intentionally kept open for the lifetime of the process so the
/// kill-on-close trigger fires at process exit, not at scope exit. When called
/// inside an existing job (e.g., CI runner or Task Scheduler) the assignment will
/// fail with a warning, which is non-fatal: the outer job still bounds the tree.
pub fn enforce_windows_sandbox(_roots: &[PathBuf]) -> Result<(), SandboxError> {
    #[cfg(target_os = "windows")]
    {
        // Before anything else, undo ACL grants orphaned by a previous crash.
        // Doing it here rather than lazily means a user who stops using ahma on a
        // workspace still gets their ACLs back the next time ahma runs at all.
        sweep_orphaned_grants();

        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job.is_null() {
                let err = std::io::Error::last_os_error();
                return Err(SandboxError::PrerequisiteFailed(format!(
                    "CreateJobObjectW failed: {err}"
                )));
            }

            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;

            if SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                std::ptr::addr_of!(info).cast(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            ) == FALSE
            {
                let err = std::io::Error::last_os_error();
                let _ = CloseHandle(job);
                return Err(SandboxError::PrerequisiteFailed(format!(
                    "SetInformationJobObject failed: {err}"
                )));
            }

            if AssignProcessToJobObject(job, GetCurrentProcess()) == FALSE {
                // Non-fatal: the process may already be assigned to an outer job
                // (CI runner, Task Scheduler, or Docker).  The outer job still
                // limits the process tree; log a warning and continue.
                let err = std::io::Error::last_os_error();
                tracing::warn!(
                    "AssignProcessToJobObject returned false (already in a job?): {err}"
                );
                let _ = CloseHandle(job);
                return Ok(());
            }

            // Intentionally keep the handle open for process lifetime so the
            // JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE trigger fires at exit.
            let _ = job;
            tracing::info!("Windows Job Object enforcement active (kill-on-close)");
            Ok(())
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        Ok(())
    }
}

/// Revoke every DACL entry this session added and drop the AppContainer profile.
///
/// Call once on session teardown. Safe to call when no session was ever
/// established, and safe to call twice.
///
/// Not the only line of defence, deliberately: a process that is killed never runs
/// this, which is why [`enforce_windows_sandbox`] sweeps the journal at startup.
pub fn cleanup_windows_sandbox() {
    #[cfg(target_os = "windows")]
    {
        let session = APPCONTAINER_SESSION.lock().take();
        if let Some(session) = session {
            session.revoke_grants();
            session.delete_profile();
        }
    }
}

/// Windows-only entry point re-entered by [`plan_windows_sandboxed_spawn`]'s
/// launcher argv. **Must be the first statement of `main`**, before any CLI
/// parsing: [`LAUNCHER_ARGV0`] is not a clap subcommand and clap would reject it.
///
/// Returns normally when this process is an ordinary `ahma` invocation. When it is
/// a launcher re-entry it never returns — it runs the real program inside the
/// AppContainer and exits with that program's exit code. When it is a
/// [`PROBE_ARGV0`] re-entry it never returns either: it prints the probe report
/// and exits 0.
pub fn appcontainer_launcher_hook() {
    #[cfg(target_os = "windows")]
    {
        // `args_os` rather than `args`: the latter *panics* on an argument that is
        // not valid Unicode, and this is the first statement of `main`. Ahma's own
        // pipeline carries `String` throughout, so the lossy conversion cannot
        // alter anything ahma itself put here.
        let argv: Vec<String> = std::env::args_os()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        // The diagnostic probe (SPEC R6.3.3). Like the launcher, once its marker
        // is seen it never falls through to the CLI.
        match parse_probe_args(argv.iter().cloned()) {
            Ok(None) => {}
            Ok(Some(cases)) => std::process::exit(run_probe(&cases)),
            Err(message) => {
                eprintln!("ahma: {message}");
                std::process::exit(2);
            }
        }
        let request = match parse_launcher_args(argv) {
            Ok(None) => return,
            Ok(Some(request)) => request,
            Err(message) => {
                // A malformed re-entry must never fall through to the normal CLI:
                // that would run the command *outside* the AppContainer.
                eprintln!("ahma: {message}");
                std::process::exit(126);
            }
        };
        match run_appcontainer_launcher(&request) {
            Ok(code) => std::process::exit(code as i32),
            Err(e) => {
                eprintln!(
                    "ahma: could not launch '{}' inside AppContainer '{}': {e:#}",
                    request.program, request.container
                );
                std::process::exit(126);
            }
        }
    }
}

// ===========================================================================
// Windows implementation
// ===========================================================================

/// Probe the AppContainer API to verify we're on Windows 8+ with a working
/// `CreateAppContainerProfile` implementation.
///
/// We call it with a deliberately invalid name (empty string) — the expected
/// result is `E_INVALIDARG` (0x80070057). Any other Windows error also
/// confirms the API is present.  `ERROR_PROC_NOT_FOUND` would mean the DLL
/// entry point is missing (very old OS).
#[cfg(target_os = "windows")]
fn probe_appcontainer_api() -> Result<(), SandboxError> {
    use windows_sys::Win32::Foundation::E_INVALIDARG;

    unsafe {
        let empty: Vec<u16> = vec![0u16];
        let mut sid: PSID = std::ptr::null_mut();

        let hr = CreateAppContainerProfile(
            empty.as_ptr(),
            empty.as_ptr(),
            empty.as_ptr(),
            std::ptr::null(),
            0,
            &mut sid,
        );

        if hr == S_OK {
            // Unexpectedly succeeded with empty name — clean up and continue.
            if !sid.is_null() {
                FreeSid(sid);
            }
            return Ok(());
        }
        if hr == E_INVALIDARG {
            // Expected: API is present, properly rejected the empty name.
            return Ok(());
        }
        // `ERROR_PROC_NOT_FOUND` as an HRESULT: the entry point is absent, i.e.
        // the OS predates Windows 8 and there is no AppContainer to be had.
        const HRESULT_PROC_NOT_FOUND: i32 = 0x8007_007Fu32 as i32;
        if hr == HRESULT_PROC_NOT_FOUND {
            return Err(SandboxError::PrerequisiteFailed(format!(
                "AppContainer API unavailable (Windows 8+ required). HRESULT: 0x{hr:08X}"
            )));
        }
        // Any other HRESULT means the API is present but something else went
        // wrong; a real spawn will surface the specific failure.
        Ok(())
    }
}

/// A SID we own the storage for. `PSID` is a bare pointer into memory whose owner
/// varies by API (`FreeSid`, `LocalFree`, a caller stack buffer); copying the bytes
/// out once removes every lifetime question at the cost of ≤ 68 bytes.
#[cfg(target_os = "windows")]
#[derive(Clone)]
struct OwnedSid(Vec<u8>);

#[cfg(target_os = "windows")]
impl OwnedSid {
    /// Copy `psid` into owned storage. Does not take ownership of `psid`.
    ///
    /// # Safety
    /// `psid` must point at a valid SID.
    unsafe fn copy_from(psid: PSID) -> anyhow::Result<Self> {
        unsafe {
            if psid.is_null() || IsValidSid(psid) == FALSE {
                anyhow::bail!("received an invalid SID from Windows");
            }
            let len = GetLengthSid(psid) as usize;
            let mut buf = vec![0u8; len];
            if CopySid(len as u32, buf.as_mut_ptr().cast(), psid) == FALSE {
                anyhow::bail!("CopySid failed: {}", std::io::Error::last_os_error());
            }
            Ok(Self(buf))
        }
    }

    /// The Win32 APIs used here (`SetEntriesInAclW`, `SECURITY_CAPABILITIES`) take
    /// `PSID` (`*mut c_void`) but only read through it.
    fn as_psid(&self) -> PSID {
        self.0.as_ptr() as PSID
    }
}

/// A live AppContainer plus the DACL entries added on its behalf, so they can be
/// taken back off again.
#[cfg(target_os = "windows")]
struct AppContainerSession {
    name: String,
    sid: OwnedSid,
    folder: Option<PathBuf>,
    /// Every scope the caller asked for, including ones no ACE was needed for.
    /// Compared against on the next call so a repeat spawn with an unchanged
    /// scope set is a lock-and-return, not a full revoke-and-regrant. Tracked
    /// separately from `granted` precisely because the two differ: a system
    /// directory or a not-yet-created scope is requested but never granted, and
    /// comparing against `granted` would make every single spawn look like a
    /// scope change and rewrite the workspace ACL on every tool call.
    requested: Vec<(PathBuf, u32)>,
    /// The subset of `requested` whose DACL this process actually modified —
    /// exactly what has to be undone.
    granted: Vec<PathBuf>,
    journal: Option<PathBuf>,
}

#[cfg(target_os = "windows")]
impl AppContainerSession {
    /// Remove every ACE this session added and drop the journal.
    ///
    /// Best-effort by design: one unrevokable path (deleted directory, permissions
    /// changed underneath us) must not stop the other paths being cleaned up. The
    /// warning names the `icacls` command that finishes the job by hand, because a
    /// leftover ACE on someone's source tree is not something to leave unexplained.
    fn revoke_grants(&self) {
        for path in &self.granted {
            if let Err(e) = revoke_path_from_sid(path, &self.sid) {
                eprintln!(
                    "ahma: failed to revoke AppContainer access on '{}': {e:#}\n\
                     ahma: remove it manually with: icacls \"{}\" /remove:g *{}",
                    path.display(),
                    path.display(),
                    self.name
                );
            }
        }
        if let Some(journal) = &self.journal {
            let _ = std::fs::remove_file(journal);
        }
        tracing::info!(
            "Revoked AppContainer '{}' access on {} path(s)",
            self.name,
            self.granted.len()
        );
    }

    /// Drop the profile itself. Only done at session teardown: a mid-session
    /// re-grant keeps the same profile so a *concurrent* session sharing this
    /// scope's container is not pulled out from under.
    fn delete_profile(&self) {
        unsafe {
            let name = to_wide(&self.name);
            let _ = DeleteAppContainerProfile(name.as_ptr());
        }
    }
}

#[cfg(target_os = "windows")]
static APPCONTAINER_SESSION: parking_lot::Mutex<Option<AppContainerSession>> =
    parking_lot::Mutex::new(None);

/// Grant `session.sid` access to every scope in `wanted` that is not already
/// covered by AppContainer's default `ALL APPLICATION PACKAGES` read+execute,
/// recording each one actually touched in `session.granted`. Split out of
/// [`ensure_appcontainer`] so that function's fresh-session build path reads
/// as "grant, then handle failure" instead of a loop nested inside it.
#[cfg(target_os = "windows")]
fn grant_scopes(
    session: &mut AppContainerSession,
    wanted: &[(PathBuf, u32)],
    system_dirs: &[PathBuf],
    name: &str,
) -> anyhow::Result<()> {
    let mut seen: Vec<PathBuf> = Vec::new();
    for (path, mask) in wanted {
        if seen.contains(path) {
            continue;
        }
        seen.push(path.clone());
        if appcontainer_has_default_read(path, system_dirs) {
            tracing::debug!(
                "AppContainer already has default read+execute on '{}' (ALL APPLICATION \
                 PACKAGES); not touching its ACL",
                path.display()
            );
            continue;
        }
        if !path.exists() {
            tracing::debug!(
                "sandbox scope '{}' does not exist yet; nothing to grant",
                path.display()
            );
            continue;
        }
        grant_path_to_sid(path, &session.sid, *mask).map_err(|e| {
            // Fail loud (SPEC R7): a scope we could not grant is a scope the
            // tool cannot use, and continuing would look like a sandbox that
            // works.
            anyhow::anyhow!(
                "could not grant AppContainer '{name}' access to sandbox scope '{}': {e:#}. \
                 ahma will not run commands it cannot confine; use --no-sandbox to run \
                 without kernel enforcement.",
                path.display()
            )
        })?;
        session.granted.push(path.clone());
    }
    Ok(())
}

/// Create (or reuse) the AppContainer for these scopes and make sure its SID has
/// exactly the access those scopes describe. Returns the container name and the
/// per-container folder Windows allocated for it.
#[cfg(target_os = "windows")]
fn ensure_appcontainer(
    write_scopes: &[PathBuf],
    read_scopes: &[PathBuf],
) -> anyhow::Result<(String, Option<PathBuf>)> {
    let primary = write_scopes.first().ok_or_else(|| {
        anyhow::anyhow!(
            "cannot build an AppContainer before the sandbox scope is locked (no write scope). \
             This is the R6.3.4 gate: tool calls before scope lock must be refused, not run."
        )
    })?;
    let name = appcontainer_name_for_scope(primary);

    let mut guard = APPCONTAINER_SESSION.lock();

    // Everything the container must reach, with the access it needs. Write scopes
    // come first so that a path appearing in both lists keeps the stronger grant.
    let wanted: Vec<(PathBuf, u32)> = write_scopes
        .iter()
        .map(|p| (p.clone(), write_access_mask()))
        .chain(read_scopes.iter().map(|p| (p.clone(), read_access_mask())))
        .collect();

    if let Some(existing) = guard.as_ref()
        && existing.name == name
        && existing.requested == wanted
    {
        return Ok((existing.name.clone(), existing.folder.clone()));
    }

    // Scope changed (read scopes can still grow after lock) or first use: rebuild.
    // Revoke the previous grants first so a narrowed scope actually narrows.
    if let Some(previous) = guard.take() {
        previous.revoke_grants();
        if previous.name != name {
            previous.delete_profile();
        }
    }

    let sid = create_or_derive_appcontainer(&name)?;
    let folder = appcontainer_folder(&sid);
    let system_dirs = default_appcontainer_readable_dirs();

    let mut session = AppContainerSession {
        name: name.clone(),
        sid,
        folder: folder.clone(),
        requested: wanted.clone(),
        granted: Vec::new(),
        journal: None,
    };

    // Journal *before* the first mutation. A crash between journalling and
    // granting leaves a journal naming a path with no ACE, and revoking an ACE
    // that is not there is a no-op — the harmless direction of the race. The
    // reverse order would leak a real ACE with nothing recording it.
    let planned: Vec<PathBuf> = wanted
        .iter()
        .filter(|(p, _)| !appcontainer_has_default_read(p, &system_dirs))
        .map(|(p, _)| p.clone())
        .collect();
    session.journal = write_grant_journal(&name, &planned);

    if let Err(e) = grant_scopes(&mut session, &wanted, &system_dirs, &name) {
        // A half-granted session is the one state nothing else cleans up: the
        // journal names this live process, so the startup sweep skips it, and no
        // session is stored for teardown to find. Undo the partial work here or
        // the user keeps ACEs for a container that was never used.
        session.revoke_grants();
        session.delete_profile();
        return Err(e);
    }

    tracing::info!(
        "AppContainer '{}' active for {} scope(s) (write: {}, read: {})",
        name,
        session.granted.len(),
        write_scopes.len(),
        read_scopes.len()
    );

    let result = (session.name.clone(), session.folder.clone());
    *guard = Some(session);
    Ok(result)
}

/// Rights an AppContainer needs on a write scope. `FILE_ALL_ACCESS` is
/// deliberately not used: it carries `WRITE_DAC`/`WRITE_OWNER`, which would let a
/// contained process rewrite the very ACL that confines it.
#[cfg(target_os = "windows")]
fn write_access_mask() -> u32 {
    FILE_GENERIC_READ | FILE_GENERIC_WRITE | FILE_GENERIC_EXECUTE | DELETE | FILE_DELETE_CHILD
}

/// Rights an AppContainer needs on a read scope — enough to run a toolchain out of
/// it (`~\.cargo\bin`), not enough to modify one.
#[cfg(target_os = "windows")]
fn read_access_mask() -> u32 {
    FILE_GENERIC_READ | FILE_GENERIC_EXECUTE
}

/// Create the named AppContainer profile, or derive its SID if it already exists.
#[cfg(target_os = "windows")]
fn create_or_derive_appcontainer(name: &str) -> anyhow::Result<OwnedSid> {
    const HRESULT_ALREADY_EXISTS: i32 = (0x8007_0000u32 | ERROR_ALREADY_EXISTS) as i32;

    unsafe {
        let wname = to_wide(name);
        let display = to_wide("ahma sandbox");
        let description = to_wide("ahma kernel-enforced workspace sandbox");
        let mut psid: PSID = std::ptr::null_mut();

        let hr = CreateAppContainerProfile(
            wname.as_ptr(),
            display.as_ptr(),
            description.as_ptr(),
            std::ptr::null(),
            0,
            &mut psid,
        );

        if hr == S_OK {
            let owned = OwnedSid::copy_from(psid)?;
            FreeSid(psid);
            return Ok(owned);
        }
        if hr != HRESULT_ALREADY_EXISTS {
            anyhow::bail!("CreateAppContainerProfile('{name}') failed: HRESULT 0x{hr:08X}");
        }

        // Profile survives across runs by design (the name is derived from the
        // scope), so "already exists" is the common path, not an error.
        let mut psid: PSID = std::ptr::null_mut();
        let hr = DeriveAppContainerSidFromAppContainerName(wname.as_ptr(), &mut psid);
        if hr != S_OK {
            anyhow::bail!(
                "DeriveAppContainerSidFromAppContainerName('{name}') failed: HRESULT 0x{hr:08X}"
            );
        }
        let owned = OwnedSid::copy_from(psid)?;
        FreeSid(psid);
        Ok(owned)
    }
}

/// The per-container folder Windows allocates under
/// `%LOCALAPPDATA%\Packages\<container>\`. The container owns it outright, which
/// makes it the only sane place to point `%TEMP%`.
#[cfg(target_os = "windows")]
fn appcontainer_folder(sid: &OwnedSid) -> Option<PathBuf> {
    use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;

    unsafe {
        let mut string_sid: *mut u16 = std::ptr::null_mut();
        if ConvertSidToStringSidW(sid.as_psid(), &mut string_sid) == FALSE {
            return None;
        }
        let mut folder: *mut u16 = std::ptr::null_mut();
        let hr = GetAppContainerFolderPath(string_sid, &mut folder);
        LocalFree(string_sid.cast());
        if hr != S_OK || folder.is_null() {
            return None;
        }
        let path = PathBuf::from(from_wide(folder));
        // `folder` is CoTaskMem-allocated and is deliberately leaked: the correct
        // free is `CoTaskMemFree`, which lives in `Win32_System_Com`, and enabling
        // that whole feature to reclaim ~520 bytes once per process is the worse
        // trade. Freeing it with `LocalFree` instead would be a heap mismatch, so
        // the one thing not done here is the tempting one.
        Some(path)
    }
}

/// Add one inheritable `ACCESS_ALLOWED` ACE for `sid` to `path`'s DACL.
///
/// Revoke-then-grant, so running twice cannot stack duplicate ACEs and a rerun
/// after a narrowing scope change replaces rather than unions the rights.
///
/// The read-modify-write is the recipe Microsoft documents for this
/// (`GetNamedSecurityInfo` → `SetEntriesInAcl` → `SetNamedSecurityInfo`).
///
/// Neither `PROTECTED_` nor `UNPROTECTED_DACL_SECURITY_INFORMATION` is passed, on
/// purpose. The DACL read back includes the object's *inherited* ACEs, and the
/// system strips those again on the way in only because the object's own
/// protection state is preserved by omitting both flags. Passing `UNPROTECTED_`
/// to force that behaviour would silently switch a deliberately protected
/// directory back to inheriting — a change to the user's security posture that
/// ahma has no business making just to add one ACE.
#[cfg(target_os = "windows")]
fn grant_path_to_sid(path: &Path, sid: &OwnedSid, mask: u32) -> anyhow::Result<()> {
    set_path_ace(path, sid, mask, GRANT_ACCESS)
}

/// Remove every ACE naming `sid` from `path`'s DACL. Precise by construction:
/// the SID is ahma's own per-scope container, so "all ACEs for this trustee" is
/// exactly "all ACEs ahma added" and can never catch a user's entry.
#[cfg(target_os = "windows")]
fn revoke_path_from_sid(path: &Path, sid: &OwnedSid) -> anyhow::Result<()> {
    set_path_ace(path, sid, 0, REVOKE_ACCESS)
}

#[cfg(target_os = "windows")]
fn set_path_ace(
    path: &Path,
    sid: &OwnedSid,
    mask: u32,
    mode: windows_sys::Win32::Security::Authorization::ACCESS_MODE,
) -> anyhow::Result<()> {
    unsafe {
        let wpath = to_wide(&path.to_string_lossy());
        let mut old_dacl: *mut ACL = std::ptr::null_mut();
        let mut security_descriptor = std::ptr::null_mut();

        let rc = GetNamedSecurityInfoW(
            wpath.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut old_dacl,
            std::ptr::null_mut(),
            &mut security_descriptor,
        );
        if rc != ERROR_SUCCESS {
            anyhow::bail!(
                "GetNamedSecurityInfoW('{}') failed: {}",
                path.display(),
                std::io::Error::from_raw_os_error(rc as i32)
            );
        }
        // From here on `security_descriptor` must be freed on every path.
        let result = (|| -> anyhow::Result<()> {
            let mut trustee: TRUSTEE_W = std::mem::zeroed();
            trustee.pMultipleTrustee = std::ptr::null_mut();
            trustee.MultipleTrusteeOperation = NO_MULTIPLE_TRUSTEE;
            trustee.TrusteeForm = TRUSTEE_IS_SID;
            // An AppContainer SID is a group SID, not a user SID.
            trustee.TrusteeType = TRUSTEE_IS_GROUP;
            trustee.ptstrName = sid.as_psid().cast();

            let mut access: EXPLICIT_ACCESS_W = std::mem::zeroed();
            access.grfAccessPermissions = mask;
            access.grfAccessMode = mode;
            // A directory grant has to reach the files inside it, or the boundary
            // would be "the workspace root and nothing under it".
            access.grfInheritance = SUB_CONTAINERS_AND_OBJECTS_INHERIT;
            access.Trustee = trustee;

            let mut new_dacl: *mut ACL = std::ptr::null_mut();
            let rc = SetEntriesInAclW(1, &access, old_dacl, &mut new_dacl);
            if rc != ERROR_SUCCESS {
                anyhow::bail!(
                    "SetEntriesInAclW('{}') failed: {}",
                    path.display(),
                    std::io::Error::from_raw_os_error(rc as i32)
                );
            }

            let rc = SetNamedSecurityInfoW(
                wpath.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                new_dacl,
                std::ptr::null(),
            );
            LocalFree(new_dacl.cast());
            if rc != ERROR_SUCCESS {
                anyhow::bail!(
                    "SetNamedSecurityInfoW('{}') failed: {}",
                    path.display(),
                    std::io::Error::from_raw_os_error(rc as i32)
                );
            }
            Ok(())
        })();

        LocalFree(security_descriptor.cast());
        result
    }
}

// ---------------------------------------------------------------------------
// Crash-recovery journal
// ---------------------------------------------------------------------------

/// `%LOCALAPPDATA%\ahma\appcontainer-grants`.
#[cfg(target_os = "windows")]
fn grant_journal_dir() -> Option<PathBuf> {
    dirs::data_local_dir().map(|d| d.join("ahma").join("appcontainer-grants"))
}

/// Record the paths about to be granted, keyed by this process id.
#[cfg(target_os = "windows")]
fn write_grant_journal(container: &str, paths: &[PathBuf]) -> Option<PathBuf> {
    let dir = grant_journal_dir()?;
    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::warn!("cannot create the AppContainer grant journal directory: {e}");
        return None;
    }
    let file = dir.join(format!("{}.grants", std::process::id()));
    match std::fs::write(&file, encode_grant_journal(container, paths)) {
        Ok(()) => Some(file),
        Err(e) => {
            tracing::warn!("cannot write the AppContainer grant journal: {e}");
            None
        }
    }
}

#[cfg(target_os = "windows")]
struct GrantJournalEntry {
    file: PathBuf,
    container: String,
    paths: Vec<PathBuf>,
    alive: bool,
}

/// Read every `*.grants` journal in `dir`, decoding each into a
/// [`GrantJournalEntry`] and tagging whether the process that wrote it is
/// still alive. Malformed journals (no container name) are deleted on sight
/// rather than surfaced — there is nothing else useful to do with them.
#[cfg(target_os = "windows")]
fn read_grant_journals(dir: &Path) -> Vec<GrantJournalEntry> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut journals = Vec::new();
    for entry in entries.flatten() {
        let file = entry.path();
        if file.extension().and_then(|e| e.to_str()) != Some("grants") {
            continue;
        }
        let Some(pid) = file
            .file_stem()
            .and_then(|s| s.to_str())
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
        let Some((container, paths)) = decode_grant_journal(&text) else {
            let _ = std::fs::remove_file(&file);
            continue;
        };
        let alive = pid != std::process::id() && process_is_running(pid);
        journals.push(GrantJournalEntry {
            file,
            container,
            paths,
            alive,
        });
    }
    journals
}

/// Revoke every ACE a stale journal recorded and delete both the profile and
/// the journal file. Assumes the caller has already confirmed no live
/// journal still shares this container.
#[cfg(target_os = "windows")]
fn revoke_stale_journal(journal: &GrantJournalEntry) {
    let sid = match derive_container_sid(&journal.container) {
        Ok(sid) => sid,
        Err(e) => {
            tracing::warn!(
                "cannot derive SID for stale AppContainer '{}': {e:#}",
                journal.container
            );
            return;
        }
    };
    for path in &journal.paths {
        if let Err(e) = revoke_path_from_sid(path, &sid) {
            tracing::warn!(
                "could not revoke stale AppContainer grant on '{}': {e:#}",
                path.display()
            );
        }
    }
    unsafe {
        let name = to_wide(&journal.container);
        let _ = DeleteAppContainerProfile(name.as_ptr());
    }
    let _ = std::fs::remove_file(&journal.file);
    tracing::info!(
        "swept {} orphaned AppContainer grant(s) left by a previous run ('{}')",
        journal.paths.len(),
        journal.container
    );
}

/// Revoke ACL grants journalled by processes that are no longer running.
///
/// The container name is shared by every session on the same scope, so a journal
/// is only acted on once no *live* journal still names its container — otherwise
/// cleaning up after a crashed session would strip a running session's access.
#[cfg(target_os = "windows")]
fn sweep_orphaned_grants() {
    let Some(dir) = grant_journal_dir() else {
        return;
    };
    let journals = read_grant_journals(&dir);

    let live_containers: Vec<String> = journals
        .iter()
        .filter(|j| j.alive)
        .map(|j| j.container.clone())
        .collect();

    for journal in journals.iter().filter(|j| !j.alive) {
        if live_containers.contains(&journal.container) {
            tracing::debug!(
                "leaving AppContainer '{}' grants in place: another live session shares it",
                journal.container
            );
            continue;
        }
        revoke_stale_journal(journal);
    }
}

/// Derive an AppContainer SID from its name without creating the profile.
#[cfg(target_os = "windows")]
fn derive_container_sid(name: &str) -> anyhow::Result<OwnedSid> {
    unsafe {
        let wname = to_wide(name);
        let mut psid: PSID = std::ptr::null_mut();
        let hr = DeriveAppContainerSidFromAppContainerName(wname.as_ptr(), &mut psid);
        if hr != S_OK {
            anyhow::bail!("DeriveAppContainerSidFromAppContainerName failed: HRESULT 0x{hr:08X}");
        }
        let owned = OwnedSid::copy_from(psid)?;
        FreeSid(psid);
        Ok(owned)
    }
}

/// Whether a process id currently belongs to a running process.
///
/// PID reuse can make a dead journal look alive; the consequence is that its
/// grants are swept one run later, never that a live session is stripped.
#[cfg(target_os = "windows")]
fn process_is_running(pid: u32) -> bool {
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, FALSE, pid);
        if handle.is_null() {
            return false;
        }
        let mut code: u32 = 0;
        let running = GetExitCodeProcess(handle, &mut code) != FALSE && code == STILL_ACTIVE;
        let _ = CloseHandle(handle);
        running
    }
}

/// `STILL_ACTIVE` (`STATUS_PENDING`) — the exit code Windows reports for a process
/// that has not exited.
#[cfg(target_os = "windows")]
const STILL_ACTIVE: u32 = 259;

// ---------------------------------------------------------------------------
// The launcher
// ---------------------------------------------------------------------------

/// Run `request.program` inside `request.container` and return its exit code.
///
/// This runs in the short-lived `ahma.exe` re-entry, not in the server.
#[cfg(target_os = "windows")]
fn run_appcontainer_launcher(request: &LauncherRequest) -> anyhow::Result<u32> {
    unsafe {
        let container_sid = derive_container_sid(&request.container)?;

        let internet_client = well_known_sid(WinCapabilityInternetClientSid)?;
        let mut capabilities = [SID_AND_ATTRIBUTES {
            Sid: internet_client.as_psid(),
            Attributes: SE_GROUP_ENABLED,
        }];

        let mut security_capabilities = SECURITY_CAPABILITIES {
            AppContainerSid: container_sid.as_psid(),
            Capabilities: capabilities.as_mut_ptr(),
            CapabilityCount: capabilities.len() as u32,
            Reserved: 0,
        };

        let (mut attribute_buffer, attribute_list) =
            init_security_attribute_list(&mut security_capabilities)?;

        let attribute_result = spawn_and_wait_appcontainer_child(request, attribute_list);

        DeleteProcThreadAttributeList(attribute_list);
        let _ = &mut attribute_buffer;
        let _ = &mut capabilities;
        let _ = &mut security_capabilities;
        attribute_result
    }
}

#[cfg(target_os = "windows")]
unsafe fn init_security_attribute_list(
    security_capabilities: &mut SECURITY_CAPABILITIES,
) -> anyhow::Result<(Vec<u8>, *mut std::ffi::c_void)> {
    let mut size: usize = 0;
    if InitializeProcThreadAttributeList(std::ptr::null_mut(), 1, 0, &mut size) != FALSE {
        anyhow::bail!("InitializeProcThreadAttributeList unexpectedly succeeded when sizing");
    }
    let last = std::io::Error::last_os_error();
    if last.raw_os_error() != Some(ERROR_INSUFFICIENT_BUFFER as i32) {
        anyhow::bail!("InitializeProcThreadAttributeList sizing failed: {last}");
    }
    let mut attribute_buffer = vec![0u8; size];
    let attribute_list = attribute_buffer.as_mut_ptr().cast();
    if InitializeProcThreadAttributeList(attribute_list, 1, 0, &mut size) == FALSE {
        anyhow::bail!(
            "InitializeProcThreadAttributeList failed: {}",
            std::io::Error::last_os_error()
        );
    }
    if UpdateProcThreadAttribute(
        attribute_list,
        0,
        PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES as usize,
        std::ptr::addr_of!(*security_capabilities).cast(),
        std::mem::size_of::<SECURITY_CAPABILITIES>(),
        std::ptr::null_mut(),
        std::ptr::null(),
    ) == FALSE
    {
        anyhow::bail!(
            "UpdateProcThreadAttribute(SECURITY_CAPABILITIES) failed: {}",
            std::io::Error::last_os_error()
        );
    }
    Ok((attribute_buffer, attribute_list))
}

#[cfg(target_os = "windows")]
unsafe fn setup_inherited_stdio_handles(
    startup: &mut STARTUPINFOEXW,
    attribute_list: *mut std::ffi::c_void,
) {
    let stdin = GetStdHandle(STD_INPUT_HANDLE);
    let stdout = GetStdHandle(STD_OUTPUT_HANDLE);
    let stderr = GetStdHandle(STD_ERROR_HANDLE);
    for h in [stdin, stdout, stderr] {
        if !h.is_null() && h != INVALID_HANDLE_VALUE {
            let _ = SetHandleInformation(h, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT);
        }
    }

    startup.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
    startup.lpAttributeList = attribute_list.cast();
    startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    startup.StartupInfo.hStdInput = stdin;
    startup.StartupInfo.hStdOutput = stdout;
    startup.StartupInfo.hStdError = stderr;
}

#[cfg(target_os = "windows")]
unsafe fn assign_child_to_kill_on_close_job(process_handle: HANDLE) {
    let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
    if !job.is_null() {
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            std::ptr::addr_of!(info).cast(),
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        ) == FALSE
            || AssignProcessToJobObject(job, process_handle) == FALSE
        {
            eprintln!(
                "ahma: could not place the AppContainer child in a kill-on-close job: {}",
                std::io::Error::last_os_error()
            );
        }
    }
}

#[cfg(target_os = "windows")]
unsafe fn wait_and_get_exit_code(process_handle: HANDLE) -> anyhow::Result<u32> {
    let wait = WaitForSingleObject(process_handle, INFINITE);
    if wait != WAIT_OBJECT_0 {
        let _ = CloseHandle(process_handle);
        anyhow::bail!(
            "WaitForSingleObject on the AppContainer child failed: {}",
            std::io::Error::last_os_error()
        );
    }
    let mut exit_code: u32 = 0;
    if GetExitCodeProcess(process_handle, &mut exit_code) == FALSE {
        let _ = CloseHandle(process_handle);
        anyhow::bail!(
            "GetExitCodeProcess failed: {}",
            std::io::Error::last_os_error()
        );
    }
    let _ = CloseHandle(process_handle);
    Ok(exit_code)
}

#[cfg(target_os = "windows")]
unsafe fn spawn_and_wait_appcontainer_child(
    request: &LauncherRequest,
    attribute_list: *mut std::ffi::c_void,
) -> anyhow::Result<u32> {
    let mut startup: STARTUPINFOEXW = std::mem::zeroed();
    setup_inherited_stdio_handles(&mut startup, attribute_list);

    let mut command_line = to_wide(&build_command_line(&request.program, &request.args));
    let mut process_info: PROCESS_INFORMATION = std::mem::zeroed();

    let created = CreateProcessW(
        std::ptr::null(),
        command_line.as_mut_ptr(),
        std::ptr::null(),
        std::ptr::null(),
        TRUE,
        EXTENDED_STARTUPINFO_PRESENT | CREATE_SUSPENDED,
        std::ptr::null(),
        std::ptr::null(),
        std::ptr::addr_of!(startup.StartupInfo),
        &mut process_info,
    );
    if created == FALSE {
        anyhow::bail!(
            "CreateProcessW('{}') into AppContainer failed: {}",
            request.program,
            std::io::Error::last_os_error()
        );
    }

    assign_child_to_kill_on_close_job(process_info.hProcess);

    ResumeThread(process_info.hThread);
    let _ = CloseHandle(process_info.hThread);

    wait_and_get_exit_code(process_info.hProcess)
}

/// Materialise a well-known capability SID (`S-1-15-3-*`).
#[cfg(target_os = "windows")]
fn well_known_sid(
    kind: windows_sys::Win32::Security::WELL_KNOWN_SID_TYPE,
) -> anyhow::Result<OwnedSid> {
    unsafe {
        let mut buf = vec![0u8; SECURITY_MAX_SID_SIZE as usize];
        let mut len = SECURITY_MAX_SID_SIZE;
        if CreateWellKnownSid(
            kind,
            std::ptr::null_mut(),
            buf.as_mut_ptr().cast(),
            &mut len,
        ) == FALSE
        {
            anyhow::bail!(
                "CreateWellKnownSid({kind}) failed: {}",
                std::io::Error::last_os_error()
            );
        }
        buf.truncate(len as usize);
        Ok(OwnedSid(buf))
    }
}

// ---------------------------------------------------------------------------
// The diagnostic probe, Windows half: running it and reading the token
// ---------------------------------------------------------------------------

/// Print the probe report to stdout and return the exit code.
///
/// A closed stdout ends the probe quietly with exit code 1 rather than a panic:
/// the standard print macro panics on a write error (SPEC R5.6.1).
#[cfg(target_os = "windows")]
fn run_probe(cases: &[ProbeCase]) -> i32 {
    use std::io::Write as _;
    let mut stdout = std::io::stdout().lock();
    let mut broken = false;
    probe_report(cases, &mut |key, value| {
        if !broken {
            // Flushed per line: a probe that hangs on a later case must still
            // have shown everything before it.
            broken = writeln!(stdout, "{}", probe_report_line(key, value))
                .and_then(|()| stdout.flush())
                .is_err();
        }
    });
    i32::from(broken)
}

/// Report the token this process actually runs under: whether it is an
/// AppContainer token, its integrity level, its AppContainer SID, and every
/// group and capability SID with its attributes. This replaces `whoami /groups`,
/// which printed nothing inside the container.
#[cfg(target_os = "windows")]
fn probe_token_report(emit: &mut dyn FnMut(&str, &str)) {
    use windows_sys::Win32::Security::{
        TOKEN_APPCONTAINER_INFORMATION, TOKEN_GROUPS, TOKEN_MANDATORY_LABEL, TOKEN_QUERY,
        TokenAppContainerSid, TokenCapabilities, TokenGroups, TokenIntegrityLevel,
        TokenIsAppContainer,
    };
    use windows_sys::Win32::System::Threading::OpenProcessToken;

    let mut token: HANDLE = std::ptr::null_mut();
    // SAFETY: `GetCurrentProcess` returns a pseudo-handle that needs no closing,
    // and `token` is a valid out-pointer.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == FALSE {
        emit(
            "token.error",
            &format!("OpenProcessToken: {}", std::io::Error::last_os_error()),
        );
        return;
    }

    match token_information(token, TokenIsAppContainer) {
        Ok(buf) => {
            // SAFETY: `TokenIsAppContainer` fills a DWORD, and the buffer is at
            // least 8 bytes and 8-byte aligned.
            let flag = unsafe { *buf.as_ptr().cast::<u32>() };
            emit(
                "token.is_appcontainer",
                if flag != 0 { "true" } else { "false" },
            );
        }
        Err(e) => emit("token.is_appcontainer", &format!("err:{e}")),
    }

    match token_information(token, TokenIntegrityLevel) {
        Ok(buf) => {
            // SAFETY: `TokenIntegrityLevel` fills a `TOKEN_MANDATORY_LABEL` whose
            // SID points into the same buffer, which outlives this use.
            let sid = unsafe { (*buf.as_ptr().cast::<TOKEN_MANDATORY_LABEL>()).Label.Sid };
            emit("token.integrity", &describe_integrity(sid));
        }
        Err(e) => emit("token.integrity", &format!("err:{e}")),
    }

    match token_information(token, TokenAppContainerSid) {
        Ok(buf) => {
            // SAFETY: as above, for `TOKEN_APPCONTAINER_INFORMATION`. The SID is
            // null for a token that is not an AppContainer token.
            let sid = unsafe {
                (*buf.as_ptr().cast::<TOKEN_APPCONTAINER_INFORMATION>()).TokenAppContainer
            };
            emit("token.appcontainer_sid", &sid_string(sid));
        }
        Err(e) => emit("token.appcontainer_sid", &format!("err:{e}")),
    }

    for (class, key) in [
        (TokenGroups, "token.group"),
        (TokenCapabilities, "token.capability"),
    ] {
        match token_information(token, class) {
            Ok(buf) => {
                let groups = buf.as_ptr().cast::<TOKEN_GROUPS>();
                // SAFETY: both classes fill a `TOKEN_GROUPS` whose variable-length
                // `Groups` array holds `GroupCount` entries inside this buffer.
                // The element pointer is derived from the buffer pointer, not
                // from a reference to the one-element array, so it may range
                // over all of them.
                let entries = unsafe {
                    let count = (*groups).GroupCount as usize;
                    let first = std::ptr::addr_of!((*groups).Groups).cast::<SID_AND_ATTRIBUTES>();
                    std::slice::from_raw_parts(first, count)
                };
                if entries.is_empty() {
                    emit(key, "<none>");
                }
                for entry in entries {
                    emit(
                        key,
                        &format!("{} attrs=0x{:08x}", sid_string(entry.Sid), entry.Attributes),
                    );
                }
            }
            Err(e) => emit(key, &format!("err:{e}")),
        }
    }

    // SAFETY: `token` was opened above and is closed exactly once.
    unsafe {
        let _ = CloseHandle(token);
    }
}

/// `GetTokenInformation` with the usual size-then-fill dance. The buffer is
/// `u64`-backed because the structures it receives hold pointers.
#[cfg(target_os = "windows")]
fn token_information(
    token: HANDLE,
    class: windows_sys::Win32::Security::TOKEN_INFORMATION_CLASS,
) -> std::io::Result<Vec<u64>> {
    use windows_sys::Win32::Security::GetTokenInformation;

    let mut needed: u32 = 0;
    // SAFETY: a null buffer of length 0 is the documented way to ask for the
    // size; the call fails and sets `needed`.
    unsafe {
        GetTokenInformation(token, class, std::ptr::null_mut(), 0, &mut needed);
    }
    if needed == 0 {
        return Err(std::io::Error::last_os_error());
    }
    let mut buf = vec![0u64; (needed as usize).div_ceil(8).max(1)];
    let len = (buf.len() * std::mem::size_of::<u64>()) as u32;
    // SAFETY: `buf` is writable for `len` bytes.
    let ok =
        unsafe { GetTokenInformation(token, class, buf.as_mut_ptr().cast(), len, &mut needed) };
    if ok == FALSE {
        return Err(std::io::Error::last_os_error());
    }
    Ok(buf)
}

/// A SID in `S-1-...` form, or a placeholder saying why there is none.
#[cfg(target_os = "windows")]
fn sid_string(sid: PSID) -> String {
    use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;

    if sid.is_null() {
        return "<null>".to_string();
    }
    // SAFETY: `sid` is non-null and came from Windows; the returned string is
    // `LocalAlloc`ed, read once, and freed with `LocalFree`.
    unsafe {
        let mut string_sid: *mut u16 = std::ptr::null_mut();
        if ConvertSidToStringSidW(sid, &mut string_sid) == FALSE {
            return format!("<unconvertible: {}>", std::io::Error::last_os_error());
        }
        let out = from_wide(string_sid);
        LocalFree(string_sid.cast());
        out
    }
}

/// `Low (S-1-16-4096)` and the like, from a mandatory label SID.
#[cfg(target_os = "windows")]
fn describe_integrity(sid: PSID) -> String {
    use windows_sys::Win32::Security::{GetSidSubAuthority, GetSidSubAuthorityCount};

    if sid.is_null() {
        return "<null>".to_string();
    }
    // SAFETY: `sid` is a valid label SID from `GetTokenInformation`; the count is
    // checked before the last sub-authority is read.
    let rid = unsafe {
        let count = *GetSidSubAuthorityCount(sid);
        if count == 0 {
            return sid_string(sid);
        }
        *GetSidSubAuthority(sid, u32::from(count - 1))
    };
    format!("{} ({})", integrity_level_name(rid), sid_string(sid))
}

// ---------------------------------------------------------------------------
// Wide-string helpers
// ---------------------------------------------------------------------------

/// A NUL-terminated UTF-16 wide string.
#[cfg(target_os = "windows")]
fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Read a NUL-terminated UTF-16 string back out of Win32.
///
/// # Safety
/// `ptr` must point at a NUL-terminated UTF-16 string.
#[cfg(target_os = "windows")]
unsafe fn from_wide(ptr: *const u16) -> String {
    unsafe {
        let mut len = 0usize;
        while *ptr.add(len) != 0 {
            len += 1;
        }
        String::from_utf16_lossy(std::slice::from_raw_parts(ptr, len))
    }
}

// ===========================================================================
// R6.3.3 EXPERIMENT support (not production; never reached by a tool call)
// ===========================================================================

/// Win32 helpers that the R6.3.3 **experiment** round of
/// `appcontainer_dacl_diagnostics` drives. They exist to measure candidate fixes
/// for the two blockers that keep AppContainer spawn isolation disabled (`NUL` is
/// denied; the scope's ancestors grant the container nothing), so a design can be
/// chosen from evidence. Nothing here changes production behaviour —
/// [`super::appcontainer_spawn_enabled`] stays `false`, and none of this runs on a
/// tool path. It is `#[doc(hidden)]` because it is a diagnostic instrument, not API.
///
/// Everything mutating is bounded to directories the **current user owns** and is
/// reverted through a guard that also verifies the restore. It never touches
/// `C:\`, `C:\Users`, a device object, or anything needing administrator rights:
/// reads of `\\.\NUL` and of ancestor DACLs are the only contact with those, and
/// they are read-only.
#[cfg(target_os = "windows")]
#[doc(hidden)]
pub mod experiment {
    use super::{OwnedSid, appcontainer_name_for_scope, derive_container_sid, from_wide, to_wide};
    use std::path::{Path, PathBuf};
    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_SUCCESS, FALSE, GENERIC_READ, HANDLE, INVALID_HANDLE_VALUE, LocalFree,
    };
    use windows_sys::Win32::Security::Authorization::{
        ConvertSecurityDescriptorToStringSecurityDescriptorW, ConvertStringSidToSidW,
        EXPLICIT_ACCESS_W, GRANT_ACCESS, GetNamedSecurityInfoW, GetSecurityInfo,
        NO_MULTIPLE_TRUSTEE, SDDL_REVISION_1, SE_FILE_OBJECT, SetEntriesInAclW, SetSecurityInfo,
        TRUSTEE_IS_GROUP, TRUSTEE_IS_SID, TRUSTEE_W,
    };
    use windows_sys::Win32::Security::{
        ACL, DACL_SECURITY_INFORMATION, EqualSid, NO_INHERITANCE, OWNER_SECURITY_INFORMATION,
        PSECURITY_DESCRIPTOR, PSID, TOKEN_QUERY, TOKEN_USER, TokenUser,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE,
        FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_TRAVERSE, OPEN_EXISTING, READ_CONTROL, SYNCHRONIZE,
        WRITE_DAC,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    /// The `ALL APPLICATION PACKAGES` group. Every AppContainer token is a member,
    /// so an ACE for it covers the container without naming the per-scope SID.
    pub const ALL_APPLICATION_PACKAGES_SID: &str = "S-1-15-2-1";

    /// The minimal rights the experiment grants on an ancestor: enough to walk
    /// through it and stat it, nothing else. `FILE_TRAVERSE` is directory "pass
    /// through", `FILE_READ_ATTRIBUTES` lets a stat succeed, `SYNCHRONIZE` is the
    /// standard right every open needs.
    #[must_use]
    pub fn traverse_mask() -> u32 {
        FILE_TRAVERSE | FILE_READ_ATTRIBUTES | SYNCHRONIZE
    }

    /// Which trustee an experiment ACE names.
    #[derive(Debug, Clone)]
    pub enum Trustee {
        /// The per-scope AppContainer SID (derived from the scope's container name).
        ContainerForScope(PathBuf),
        /// A literal SID string, e.g. [`ALL_APPLICATION_PACKAGES_SID`].
        StringSid(String),
    }

    impl Trustee {
        fn resolve(&self) -> anyhow::Result<OwnedSid> {
            match self {
                Trustee::ContainerForScope(scope) => {
                    derive_container_sid(&appcontainer_name_for_scope(scope))
                }
                Trustee::StringSid(s) => sid_from_string(s),
            }
        }
    }

    /// One ancestor of the scope and what the experiment decided to do with it.
    #[derive(Debug, Clone)]
    pub struct AncestorInfo {
        pub path: PathBuf,
        /// `Some(true)`/`Some(false)` from the owner check, `None` if it failed.
        pub owned: Option<bool>,
        /// Whether it is safe to add and then restore an ACE here (owned by the
        /// current user, within the profile root).
        pub eligible: bool,
        /// A short human note: `included`, `skip: not owner`, `stop: profile
        /// root`, `stop: outside profile`, or `err: ...`.
        pub note: String,
    }

    /// Build a SID from its string form (`S-1-15-2-1`). The storage is copied out,
    /// so the `LocalAlloc`ed original is freed before returning.
    fn sid_from_string(s: &str) -> anyhow::Result<OwnedSid> {
        // SAFETY: `w` is NUL-terminated; `psid` is a valid out-pointer; on success
        // the returned SID is `LocalAlloc`ed and is freed once below.
        unsafe {
            let w = to_wide(s);
            let mut psid: PSID = std::ptr::null_mut();
            if ConvertStringSidToSidW(w.as_ptr(), &mut psid) == FALSE {
                anyhow::bail!(
                    "ConvertStringSidToSidW('{s}') failed: {}",
                    std::io::Error::last_os_error()
                );
            }
            let owned = OwnedSid::copy_from(psid);
            LocalFree(psid.cast());
            owned
        }
    }

    /// The current process user's SID.
    fn current_user_sid() -> anyhow::Result<OwnedSid> {
        // SAFETY: `GetCurrentProcess` is a pseudo-handle; `token` is a valid
        // out-pointer and is closed once; the `TOKEN_USER` lives in `buf`, which
        // outlives the copy.
        unsafe {
            let mut token: HANDLE = std::ptr::null_mut();
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == FALSE {
                anyhow::bail!(
                    "OpenProcessToken failed: {}",
                    std::io::Error::last_os_error()
                );
            }
            let buf = super::token_information(token, TokenUser);
            let _ = CloseHandle(token);
            let buf = buf.map_err(|e| anyhow::anyhow!("GetTokenInformation(TokenUser): {e}"))?;
            let sid = (*buf.as_ptr().cast::<TOKEN_USER>()).User.Sid;
            OwnedSid::copy_from(sid)
        }
    }

    /// Case-insensitive "is `path` within `root`" (Windows paths are
    /// case-insensitive; `Path::starts_with` is not).
    fn within(path: &Path, root: &Path) -> bool {
        let p = path.to_string_lossy().to_ascii_lowercase();
        let r = root.to_string_lossy().to_ascii_lowercase();
        let r = r.trim_end_matches(['\\', '/']);
        !r.is_empty() && (p == r || p.starts_with(&format!("{r}\\")))
    }

    /// Whether `path`'s owner SID equals `user`. Read-only (`GetNamedSecurityInfoW`
    /// with `OWNER_SECURITY_INFORMATION`).
    fn path_owner_is(path: &Path, user: &OwnedSid) -> anyhow::Result<bool> {
        // SAFETY: `wpath` is NUL-terminated; the out-pointers are valid; the
        // returned security descriptor is freed on both exits.
        unsafe {
            let wpath = to_wide(&path.to_string_lossy());
            let mut owner: PSID = std::ptr::null_mut();
            let mut psd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
            let rc = GetNamedSecurityInfoW(
                wpath.as_ptr(),
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION,
                &mut owner,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut psd,
            );
            if rc != ERROR_SUCCESS {
                anyhow::bail!(
                    "GetNamedSecurityInfoW(owner, '{}') failed: {}",
                    path.display(),
                    std::io::Error::from_raw_os_error(rc as i32)
                );
            }
            let eq = !owner.is_null() && EqualSid(owner, user.as_psid()) != FALSE;
            LocalFree(psd.cast());
            Ok(eq)
        }
    }

    /// The ancestors of `scope`, nearest first, up to (and excluding) `profile_root`,
    /// each tagged with whether the current user owns it and whether it is safe to
    /// experiment on. Stops at the profile root or the first ancestor outside it, so
    /// `C:\`, `C:\Users` and anything above the profile are never considered.
    pub fn user_owned_ancestors(
        scope: &Path,
        profile_root: &Path,
    ) -> anyhow::Result<Vec<AncestorInfo>> {
        let user = current_user_sid()?;
        // Compare long-form paths: %TEMP% on a runner is the 8.3 short form
        // (`C:\Users\RUNNER~1\...`) while the profile root is long
        // (`C:\Users\runneradmin`), so a raw prefix test calls every ancestor
        // "outside profile" and the experiment grants nothing.
        let scope = dunce::canonicalize(scope).unwrap_or_else(|_| scope.to_path_buf());
        let profile_root =
            dunce::canonicalize(profile_root).unwrap_or_else(|_| profile_root.to_path_buf());
        let profile_root = profile_root.as_path();
        let mut out = Vec::new();
        for ancestor in scope.ancestors().skip(1) {
            if ancestor == profile_root {
                out.push(AncestorInfo {
                    path: ancestor.to_path_buf(),
                    owned: None,
                    eligible: false,
                    note: "stop: profile root".to_string(),
                });
                break;
            }
            if !within(ancestor, profile_root) {
                out.push(AncestorInfo {
                    path: ancestor.to_path_buf(),
                    owned: None,
                    eligible: false,
                    note: "stop: outside profile".to_string(),
                });
                break;
            }
            match path_owner_is(ancestor, &user) {
                Ok(true) => out.push(AncestorInfo {
                    path: ancestor.to_path_buf(),
                    owned: Some(true),
                    eligible: true,
                    note: "included".to_string(),
                }),
                Ok(false) => out.push(AncestorInfo {
                    path: ancestor.to_path_buf(),
                    owned: Some(false),
                    eligible: false,
                    note: "skip: not owner".to_string(),
                }),
                Err(e) => out.push(AncestorInfo {
                    path: ancestor.to_path_buf(),
                    owned: None,
                    eligible: false,
                    note: format!("err: {e}"),
                }),
            }
        }
        Ok(out)
    }

    /// A directory handle that closes on drop.
    struct HandleGuard(HANDLE);

    impl Drop for HandleGuard {
        fn drop(&mut self) {
            // SAFETY: the handle came from `CreateFileW` and is closed once.
            unsafe {
                if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
                    let _ = CloseHandle(self.0);
                }
            }
        }
    }

    /// Open a directory for reading and rewriting its DACL (`FILE_FLAG_BACKUP_SEMANTICS`
    /// is required to get a handle to a directory).
    fn open_dir(path: &Path, access: u32) -> anyhow::Result<HandleGuard> {
        // SAFETY: `wpath` is NUL-terminated; all optional pointers are null; the
        // returned handle is owned by `HandleGuard`.
        unsafe {
            let wpath = to_wide(&path.to_string_lossy());
            let h = CreateFileW(
                wpath.as_ptr(),
                access,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS,
                std::ptr::null_mut(),
            );
            if h.is_null() || h == INVALID_HANDLE_VALUE {
                anyhow::bail!(
                    "CreateFileW('{}') failed: {}",
                    path.display(),
                    std::io::Error::last_os_error()
                );
            }
            Ok(HandleGuard(h))
        }
    }

    /// The DACL of an open object as an SDDL string (DACL only — this is for
    /// before/after comparison, not a full descriptor).
    fn dacl_sddl(handle: HANDLE) -> anyhow::Result<String> {
        // SAFETY: `handle` is a live object handle; the out-pointers are valid; the
        // descriptor and the SDDL string are each freed exactly once.
        unsafe {
            let mut owner: PSID = std::ptr::null_mut();
            let mut group: PSID = std::ptr::null_mut();
            let mut dacl: *mut ACL = std::ptr::null_mut();
            let mut sacl: *mut ACL = std::ptr::null_mut();
            let mut psd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
            let rc = GetSecurityInfo(
                handle,
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                &mut owner,
                &mut group,
                &mut dacl,
                &mut sacl,
                &mut psd,
            );
            if rc != ERROR_SUCCESS {
                anyhow::bail!(
                    "GetSecurityInfo failed: {}",
                    std::io::Error::from_raw_os_error(rc as i32)
                );
            }
            let mut s: *mut u16 = std::ptr::null_mut();
            let mut len: u32 = 0;
            let ok = ConvertSecurityDescriptorToStringSecurityDescriptorW(
                psd,
                SDDL_REVISION_1,
                DACL_SECURITY_INFORMATION,
                &mut s,
                &mut len,
            );
            let out = if ok == FALSE {
                Err(anyhow::anyhow!(
                    "ConvertSecurityDescriptorToStringSecurityDescriptorW failed: {}",
                    std::io::Error::last_os_error()
                ))
            } else {
                let o = from_wide(s);
                LocalFree(s.cast());
                Ok(o)
            };
            LocalFree(psd.cast());
            out
        }
    }

    /// Copy an open object's DACL out into an aligned, owned blob. A `u64` backing
    /// store guarantees the 4-byte alignment `ACL` wants; the slice is the single
    /// contiguous structure `SetSecurityInfo` will later copy back in.
    fn copy_dacl(handle: HANDLE) -> anyhow::Result<Vec<u64>> {
        // SAFETY: `handle` is live; the out-pointers are valid; `dacl` (when
        // non-null) points at `AclSize` contiguous bytes inside `psd`, which is
        // freed once.
        unsafe {
            let mut owner: PSID = std::ptr::null_mut();
            let mut group: PSID = std::ptr::null_mut();
            let mut dacl: *mut ACL = std::ptr::null_mut();
            let mut sacl: *mut ACL = std::ptr::null_mut();
            let mut psd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
            let rc = GetSecurityInfo(
                handle,
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                &mut owner,
                &mut group,
                &mut dacl,
                &mut sacl,
                &mut psd,
            );
            if rc != ERROR_SUCCESS {
                anyhow::bail!(
                    "GetSecurityInfo failed: {}",
                    std::io::Error::from_raw_os_error(rc as i32)
                );
            }
            let bytes = if dacl.is_null() {
                Vec::new()
            } else {
                let size = (*dacl).AclSize as usize;
                let src = std::slice::from_raw_parts(dacl.cast::<u8>(), size);
                let mut buf = vec![0u64; size.div_ceil(8).max(1)];
                std::ptr::copy_nonoverlapping(src.as_ptr(), buf.as_mut_ptr().cast::<u8>(), size);
                buf
            };
            LocalFree(psd.cast());
            Ok(bytes)
        }
    }

    /// Replace an open object's DACL with a previously captured blob (empty = a
    /// NULL DACL, which is what the original had if `copy_dacl` returned empty).
    fn set_dacl(handle: HANDLE, dacl: &[u64]) -> anyhow::Result<()> {
        // SAFETY: `handle` is live; `dacl`, when non-empty, is a valid contiguous
        // `ACL` captured by `copy_dacl` from the same object.
        unsafe {
            let p: *const ACL = if dacl.is_empty() {
                std::ptr::null()
            } else {
                dacl.as_ptr().cast::<ACL>()
            };
            let rc = SetSecurityInfo(
                handle,
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                p,
                std::ptr::null(),
            );
            if rc != ERROR_SUCCESS {
                anyhow::bail!(
                    "SetSecurityInfo (restore) failed: {}",
                    std::io::Error::from_raw_os_error(rc as i32)
                );
            }
            Ok(())
        }
    }

    /// Add one non-inheritable allow ACE for `sid` to an open object's DACL,
    /// merging it with the existing entries (`GRANT_ACCESS`).
    fn add_ace(handle: HANDLE, sid: &OwnedSid, mask: u32) -> anyhow::Result<()> {
        // SAFETY: `handle` is live; `sid` is valid for the call's duration; the
        // old DACL comes from `GetSecurityInfo` inside the same descriptor, which
        // is freed once, and the new DACL from `SetEntriesInAclW` is freed once.
        unsafe {
            let mut owner: PSID = std::ptr::null_mut();
            let mut group: PSID = std::ptr::null_mut();
            let mut old_dacl: *mut ACL = std::ptr::null_mut();
            let mut sacl: *mut ACL = std::ptr::null_mut();
            let mut psd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
            let rc = GetSecurityInfo(
                handle,
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                &mut owner,
                &mut group,
                &mut old_dacl,
                &mut sacl,
                &mut psd,
            );
            if rc != ERROR_SUCCESS {
                anyhow::bail!(
                    "GetSecurityInfo failed: {}",
                    std::io::Error::from_raw_os_error(rc as i32)
                );
            }
            let result = (|| -> anyhow::Result<()> {
                let mut trustee: TRUSTEE_W = std::mem::zeroed();
                trustee.pMultipleTrustee = std::ptr::null_mut();
                trustee.MultipleTrusteeOperation = NO_MULTIPLE_TRUSTEE;
                trustee.TrusteeForm = TRUSTEE_IS_SID;
                // Both a container SID and ALL APPLICATION PACKAGES are group SIDs.
                trustee.TrusteeType = TRUSTEE_IS_GROUP;
                trustee.ptstrName = sid.as_psid().cast();

                let mut access: EXPLICIT_ACCESS_W = std::mem::zeroed();
                access.grfAccessPermissions = mask;
                access.grfAccessMode = GRANT_ACCESS;
                // No inheritance: this grants traverse on exactly this directory,
                // never on the user's files underneath it.
                access.grfInheritance = NO_INHERITANCE;
                access.Trustee = trustee;

                let mut new_dacl: *mut ACL = std::ptr::null_mut();
                let rc = SetEntriesInAclW(1, &access, old_dacl, &mut new_dacl);
                if rc != ERROR_SUCCESS {
                    anyhow::bail!(
                        "SetEntriesInAclW failed: {}",
                        std::io::Error::from_raw_os_error(rc as i32)
                    );
                }
                let rc = SetSecurityInfo(
                    handle,
                    SE_FILE_OBJECT,
                    DACL_SECURITY_INFORMATION,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    new_dacl,
                    std::ptr::null(),
                );
                LocalFree(new_dacl.cast());
                if rc != ERROR_SUCCESS {
                    anyhow::bail!(
                        "SetSecurityInfo (add) failed: {}",
                        std::io::Error::from_raw_os_error(rc as i32)
                    );
                }
                Ok(())
            })();
            LocalFree(psd.cast());
            result
        }
    }

    /// One temporary traverse ACE on a user-owned directory, guaranteed to be
    /// removed. The original DACL is captured before the ACE is added and is
    /// restored by [`TraverseAceGuard::restore_and_verify`]; if that is never
    /// called (early return or panic), `Drop` restores it best-effort. The handle
    /// stays open for the guard's life so add and restore act on the same object.
    pub struct TraverseAceGuard {
        handle: HandleGuard,
        path: PathBuf,
        original_dacl: Vec<u64>,
        original_sddl: String,
        restored: bool,
    }

    impl TraverseAceGuard {
        /// Capture `path`'s DACL, then add one non-inheritable `mask` ACE for the
        /// resolved `trustee`. `path` must be a user-owned directory (the caller
        /// checks via [`user_owned_ancestors`]); the open needs `WRITE_DAC`, which
        /// the owner always holds.
        pub fn add(path: &Path, trustee: &Trustee, mask: u32) -> anyhow::Result<Self> {
            let sid = trustee.resolve()?;
            let handle = open_dir(path, READ_CONTROL | WRITE_DAC)?;
            let original_dacl = copy_dacl(handle.0)?;
            let original_sddl = dacl_sddl(handle.0)?;
            add_ace(handle.0, &sid, mask)?;
            Ok(Self {
                handle,
                path: path.to_path_buf(),
                original_dacl,
                original_sddl,
                restored: false,
            })
        }

        /// The DACL as it stands now (with the experiment ACE), for the report.
        pub fn current_sddl(&self) -> anyhow::Result<String> {
            dacl_sddl(self.handle.0)
        }

        fn restore_inner(&self) -> String {
            match set_dacl(self.handle.0, &self.original_dacl) {
                Err(e) => format!("restore FAILED: {e:#}"),
                Ok(()) => match dacl_sddl(self.handle.0) {
                    Ok(now) if now == self.original_sddl => "restored:verified".to_string(),
                    Ok(now) => format!(
                        "restored, SDDL DIFFERS: before={} after={now}",
                        self.original_sddl
                    ),
                    Err(e) => format!("restored, verify-read failed: {e:#}"),
                },
            }
        }

        /// Restore the original DACL and confirm its SDDL matches what was captured.
        /// Returns a one-line outcome for the report. Consumes the guard, so `Drop`
        /// afterwards is a no-op.
        pub fn restore_and_verify(mut self) -> String {
            let outcome = self.restore_inner();
            self.restored = true;
            outcome
        }
    }

    impl Drop for TraverseAceGuard {
        fn drop(&mut self) {
            if !self.restored {
                let outcome = self.restore_inner();
                eprintln!(
                    "ahma: TraverseAceGuard drop-restore on '{}': {outcome}",
                    self.path.display()
                );
            }
        }
    }

    /// The security descriptor of the `\\.\NUL` device as SDDL (owner + group +
    /// DACL). **Read-only**: it opens the device for `READ_CONTROL | GENERIC_READ`,
    /// reads via `GetSecurityInfo`, and never writes. This answers whether
    /// `ALL APPLICATION PACKAGES` is missing from the device's DACL.
    pub fn nul_device_sddl() -> anyhow::Result<String> {
        use windows_sys::Win32::Security::GROUP_SECURITY_INFORMATION;
        let handle = open_nul_readonly()?;
        // SAFETY: `handle` is a live device handle; out-pointers are valid; both
        // the descriptor and the SDDL string are freed once.
        unsafe {
            let info =
                OWNER_SECURITY_INFORMATION | GROUP_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION;
            let mut owner: PSID = std::ptr::null_mut();
            let mut group: PSID = std::ptr::null_mut();
            let mut dacl: *mut ACL = std::ptr::null_mut();
            let mut sacl: *mut ACL = std::ptr::null_mut();
            let mut psd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
            let rc = GetSecurityInfo(
                handle.0,
                SE_FILE_OBJECT,
                info,
                &mut owner,
                &mut group,
                &mut dacl,
                &mut sacl,
                &mut psd,
            );
            if rc != ERROR_SUCCESS {
                anyhow::bail!(
                    "GetSecurityInfo(\\\\.\\NUL) failed: {}",
                    std::io::Error::from_raw_os_error(rc as i32)
                );
            }
            let mut s: *mut u16 = std::ptr::null_mut();
            let mut len: u32 = 0;
            let ok = ConvertSecurityDescriptorToStringSecurityDescriptorW(
                psd,
                SDDL_REVISION_1,
                info,
                &mut s,
                &mut len,
            );
            let out = if ok == FALSE {
                Err(anyhow::anyhow!(
                    "ConvertSecurityDescriptorToStringSecurityDescriptorW(\\\\.\\NUL) failed: {}",
                    std::io::Error::last_os_error()
                ))
            } else {
                let o = from_wide(s);
                LocalFree(s.cast());
                Ok(o)
            };
            LocalFree(psd.cast());
            out
        }
    }

    /// Open `\\.\NUL` for a read-only security query. Separate so the one device
    /// contact in this module is visibly read-only.
    fn open_nul_readonly() -> anyhow::Result<HandleGuard> {
        // SAFETY: a constant NUL-terminated device name; no optional pointers; the
        // handle is owned by `HandleGuard`.
        unsafe {
            let wpath = to_wide(r"\\.\NUL");
            let h = CreateFileW(
                wpath.as_ptr(),
                READ_CONTROL | GENERIC_READ,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            );
            if h.is_null() || h == INVALID_HANDLE_VALUE {
                anyhow::bail!(
                    "CreateFileW(\\\\.\\NUL) failed: {}",
                    std::io::Error::last_os_error()
                );
            }
            Ok(HandleGuard(h))
        }
    }
}

// ===========================================================================
// Tests — the platform-independent half, which is why it is written that way
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn container_name_is_deterministic_and_windows_legal() {
        let a = appcontainer_name_for_scope(Path::new("/workspace/one"));
        let b = appcontainer_name_for_scope(Path::new("/workspace/one"));
        let c = appcontainer_name_for_scope(Path::new("/workspace/two"));
        assert_eq!(a, b, "the launcher re-derives the SID from the name alone");
        assert_ne!(a, c, "different scopes must not share a container");
        assert!(a.len() <= 64, "Windows caps container names at 64 chars");
        assert!(
            a.chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '.'),
            "container name must be alphanumeric plus `-`/`.`: {a}"
        );
    }

    #[test]
    fn launcher_argv_round_trips() {
        let args = vec![
            "-NoProfile".to_string(),
            "-Command".to_string(),
            "echo hi".to_string(),
        ];
        let argv = launcher_args("ahma-sandbox-dead", "powershell", &args);
        // Prepend a fake argv[0] the way the OS would.
        let mut full = vec!["C:\\ahma.exe".to_string()];
        full.extend(argv);
        let parsed = parse_launcher_args(full).unwrap().unwrap();
        assert_eq!(parsed.container, "ahma-sandbox-dead");
        assert_eq!(parsed.program, "powershell");
        assert_eq!(parsed.args, args);
    }

    /// A tool argument that looks like a launcher argument must not be able to
    /// steer the launcher — that is what the `--` separator is for.
    #[test]
    fn launcher_argv_is_not_confusable_by_tool_arguments() {
        let args = vec![
            "--".to_string(),
            LAUNCHER_ARGV0.to_string(),
            "evil".to_string(),
        ];
        let argv = launcher_args("ahma-sandbox-beef", "cmd", &args);
        let mut full = vec!["C:\\ahma.exe".to_string()];
        full.extend(argv);
        let parsed = parse_launcher_args(full).unwrap().unwrap();
        assert_eq!(parsed.container, "ahma-sandbox-beef");
        assert_eq!(parsed.program, "cmd");
        assert_eq!(parsed.args, args);
    }

    #[test]
    fn ordinary_argv_is_not_a_launcher_reentry() {
        let argv = vec![
            "C:\\ahma.exe".to_string(),
            "serve".to_string(),
            "stdio".to_string(),
        ];
        assert_eq!(parse_launcher_args(argv).unwrap(), None);
    }

    /// A malformed re-entry must be an error, never a silent fall-through: falling
    /// through would run the command outside the AppContainer.
    #[test]
    fn malformed_launcher_reentry_is_an_error() {
        for argv in [
            vec!["ahma".to_string(), LAUNCHER_ARGV0.to_string()],
            vec![
                "ahma".to_string(),
                LAUNCHER_ARGV0.to_string(),
                "container".to_string(),
            ],
            vec![
                "ahma".to_string(),
                LAUNCHER_ARGV0.to_string(),
                "container".to_string(),
                "notdashdash".to_string(),
                "cmd".to_string(),
            ],
            vec![
                "ahma".to_string(),
                LAUNCHER_ARGV0.to_string(),
                "container".to_string(),
                "--".to_string(),
            ],
            vec![
                "ahma".to_string(),
                LAUNCHER_ARGV0.to_string(),
                String::new(),
                "--".to_string(),
                "cmd".to_string(),
            ],
        ] {
            assert!(
                parse_launcher_args(argv.clone()).is_err(),
                "malformed re-entry must not parse as a normal invocation: {argv:?}"
            );
        }
    }

    /// `\` is only a separator on Windows, so the fixture is built with `join`
    /// rather than spelled out — a literal `C:\...` string is a *single*
    /// component on this host and would test nothing.
    #[test]
    fn launcher_resolution_prefers_the_running_ahma() {
        let never = |_: &Path| false;
        let install = if cfg!(target_os = "windows") {
            PathBuf::from("C:\\Program Files\\ahma")
        } else {
            PathBuf::from("/opt/ahma")
        };
        let exe = install.join("ahma.exe");
        let resolved = resolve_launcher_exe_from(&exe, &[], "ahma.exe", &never);
        assert_eq!(
            resolved,
            Some(exe),
            "when the running binary *is* ahma, nothing else needs looking up"
        );
    }

    /// An integration-test binary lives in `target\debug\deps\`; the launcher it
    /// needs is two directories up in `target\debug\`.
    #[test]
    fn launcher_resolution_walks_out_of_the_deps_directory() {
        let td = tempfile::tempdir().unwrap();
        let debug = td.path().join("debug");
        let deps = debug.join("deps");
        std::fs::create_dir_all(&deps).unwrap();
        let launcher = debug.join("ahma");
        std::fs::write(&launcher, b"").unwrap();

        let resolved = resolve_launcher_exe_from(
            &deps.join("sandbox_test-abc123"),
            &[],
            "ahma",
            &|p: &Path| p.is_file(),
        );
        assert_eq!(resolved, Some(launcher));
    }

    #[test]
    fn launcher_resolution_falls_back_to_path_then_gives_up() {
        let td = tempfile::tempdir().unwrap();
        let bin = td.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let launcher = bin.join("ahma");
        std::fs::write(&launcher, b"").unwrap();

        let elsewhere = td.path().join("other").join("some-test-binary");
        let resolved = resolve_launcher_exe_from(
            &elsewhere,
            std::slice::from_ref(&bin),
            "ahma",
            &|p: &Path| p.is_file(),
        );
        assert_eq!(resolved, Some(launcher));

        let missing = resolve_launcher_exe_from(&elsewhere, &[], "ahma", &|p: &Path| p.is_file());
        assert_eq!(missing, None, "no launcher anywhere must be a hard failure");
    }

    #[test]
    fn grant_journal_round_trips() {
        let paths = vec![
            PathBuf::from("C:\\Users\\dev\\project"),
            PathBuf::from("C:\\Users\\dev\\.cargo"),
        ];
        let encoded = encode_grant_journal("ahma-sandbox-0123456789abcdef", &paths);
        let (container, decoded) = decode_grant_journal(&encoded).unwrap();
        assert_eq!(container, "ahma-sandbox-0123456789abcdef");
        assert_eq!(decoded, paths);
    }

    #[test]
    fn grant_journal_tolerates_a_container_with_no_paths_and_rejects_an_empty_file() {
        let encoded = encode_grant_journal("ahma-sandbox-abc", &[]);
        let (container, paths) = decode_grant_journal(&encoded).unwrap();
        assert_eq!(container, "ahma-sandbox-abc");
        assert!(paths.is_empty());

        assert_eq!(decode_grant_journal(""), None);
        assert_eq!(decode_grant_journal("\n\n"), None);
    }

    /// System directories must be left alone: ahma has no right to rewrite them,
    /// and the AppContainer can already read them.
    #[test]
    fn system_directories_are_recognised_as_already_readable() {
        let system_dirs = vec![
            PathBuf::from("C:\\Windows"),
            PathBuf::from("C:\\Program Files"),
        ];
        assert!(appcontainer_has_default_read(
            Path::new("C:\\Windows\\System32\\WindowsPowerShell\\v1.0"),
            &system_dirs
        ));
        // Case-insensitive, like the filesystem.
        assert!(appcontainer_has_default_read(
            Path::new("c:\\program files\\Git\\cmd"),
            &system_dirs
        ));
        assert!(appcontainer_has_default_read(
            Path::new("C:\\Windows"),
            &system_dirs
        ));
        // A user path that merely starts with the same characters must not match.
        assert!(!appcontainer_has_default_read(
            Path::new("C:\\WindowsProjects\\app"),
            &system_dirs
        ));
        assert!(!appcontainer_has_default_read(
            Path::new("C:\\Users\\dev\\project"),
            &system_dirs
        ));
        assert!(
            !appcontainer_has_default_read(Path::new("C:\\Users\\dev"), &[]),
            "with no system dirs known, nothing is assumed pre-granted"
        );
    }

    /// `CommandLineToArgvW` must rebuild exactly what we put in — a mis-quoted
    /// argument silently changes the command the sandbox runs.
    #[test]
    fn windows_argument_quoting_follows_commandlinetoargvw_rules() {
        assert_eq!(quote_windows_arg("simple"), "simple");
        assert_eq!(quote_windows_arg("has space"), "\"has space\"");
        assert_eq!(quote_windows_arg(""), "\"\"");
        // A backslash is only an escape in front of a quote.
        assert_eq!(quote_windows_arg("C:\\path\\file"), "C:\\path\\file");
        assert_eq!(quote_windows_arg("C:\\a b\\c"), "\"C:\\a b\\c\"");
        assert_eq!(quote_windows_arg("dir\\ "), "\"dir\\ \"");
        // A trailing backslash needs no escaping when the argument was not quoted
        // in the first place — quoting it "just in case" would change the value.
        assert_eq!(quote_windows_arg("C:\\dir\\"), "C:\\dir\\");
        // ...but once quotes are required, the run before the closing quote must
        // be doubled or it escapes the quote and swallows the next argument.
        assert_eq!(
            quote_windows_arg("C:\\Program Files\\dir\\"),
            "\"C:\\Program Files\\dir\\\\\""
        );
        // An embedded quote is escaped, and the run of backslashes before it doubled.
        assert_eq!(quote_windows_arg("say \"hi\""), "\"say \\\"hi\\\"\"");
        assert_eq!(quote_windows_arg("a\\\"b"), "\"a\\\\\\\"b\"");
    }

    #[test]
    fn command_line_joins_program_and_arguments() {
        let line = build_command_line(
            "C:\\Program Files\\PowerShell\\pwsh.exe",
            &["-Command".to_string(), "echo hello".to_string()],
        );
        assert_eq!(
            line,
            "\"C:\\Program Files\\PowerShell\\pwsh.exe\" -Command \"echo hello\""
        );
    }

    /// Off Windows the backend must refuse rather than hand back something that
    /// looks like a sandboxed command (SPEC R7: never silently unsandboxed).
    #[cfg(not(target_os = "windows"))]
    #[test]
    fn planning_a_spawn_off_windows_is_an_error() {
        let result = plan_windows_sandboxed_spawn(
            "echo",
            &["hi".to_string()],
            &[PathBuf::from("/workspace")],
            &[],
        );
        assert!(result.is_err(), "must not pretend to sandbox off Windows");
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn availability_check_off_windows_is_an_error() {
        assert!(check_windows_sandbox_available().is_err());
    }

    /// The launcher hook must be a no-op for an ordinary invocation on every
    /// platform, since `main` calls it unconditionally.
    #[test]
    fn launcher_hook_is_inert_for_a_normal_invocation() {
        appcontainer_launcher_hook();
    }

    // -- the diagnostic probe (SPEC R6.3.3) ----------------------------------

    fn sample_probe_cases() -> Vec<ProbeCase> {
        vec![
            ProbeCase::new("in_write", ProbeOp::Write, "C:\\ws\\a b\\probe.txt"),
            ProbeCase::new("nul_write", ProbeOp::Write, "\\\\.\\NUL"),
            ProbeCase::new("anc0_stat", ProbeOp::Stat, "C:\\"),
            ProbeCase::new("in_list", ProbeOp::List, "C:\\ws"),
            ProbeCase::new("in_canon", ProbeOp::Canonicalize, "C:\\ws"),
            ProbeCase::new("in_read", ProbeOp::Read, "C:\\ws\\seed.txt"),
        ]
    }

    /// The probe argv is a wire format between the diagnostic test and an
    /// `ahma.exe` running inside the container. It must round-trip, and neither
    /// re-entry may be mistaken for the other.
    #[test]
    fn probe_argv_round_trips_and_is_distinct_from_the_launcher() {
        let cases = sample_probe_cases();
        let mut argv = vec!["C:\\ahma.exe".to_string()];
        argv.extend(probe_args(&cases));
        assert_eq!(parse_probe_args(argv.clone()).unwrap(), Some(cases));
        assert_eq!(
            parse_launcher_args(argv).unwrap(),
            None,
            "a probe re-entry must not be read as a launcher re-entry"
        );

        let mut launcher = vec!["C:\\ahma.exe".to_string()];
        launcher.extend(launcher_args("ahma-sandbox-beef", "cmd", &[]));
        assert_eq!(parse_probe_args(launcher).unwrap(), None);
        assert_eq!(
            parse_probe_args(vec!["ahma".to_string(), "serve".to_string()]).unwrap(),
            None
        );
    }

    /// A probe with no cases still reports its token, so it is a valid request.
    #[test]
    fn a_probe_with_no_cases_parses() {
        let argv = vec!["ahma".to_string(), PROBE_ARGV0.to_string()];
        assert_eq!(parse_probe_args(argv).unwrap(), Some(Vec::new()));
    }

    /// Like the launcher, a malformed probe re-entry must be an error and never
    /// fall through to the ordinary CLI.
    #[test]
    fn malformed_probe_reentry_is_an_error() {
        let bad: [&[&str]; 6] = [
            // An incomplete triple.
            &["write", "label"],
            // An unknown operation.
            &["delete", "label", "C:\\x"],
            // An empty label.
            &["write", "", "C:\\x"],
            // `=` or whitespace in a label would corrupt the `key=value` report.
            &["write", "a=b", "C:\\x"],
            &["write", "a b", "C:\\x"],
            // An empty path.
            &["write", "label", ""],
        ];
        for tail in bad {
            let mut argv = vec!["ahma".to_string(), PROBE_ARGV0.to_string()];
            argv.extend(tail.iter().map(|s| s.to_string()));
            assert!(
                parse_probe_args(argv.clone()).is_err(),
                "must not parse: {argv:?}"
            );
        }
    }

    /// The probe's file-system half runs on every platform, so it is checked
    /// here against outcomes that are known. Inside the container the outcomes
    /// are the unknown; the reporting must not be.
    #[test]
    fn probe_report_covers_every_case_and_finishes() {
        let td = tempfile::tempdir().unwrap();
        let seed = td.path().join("seed.txt");
        std::fs::write(&seed, b"seed").unwrap();
        let cases = vec![
            ProbeCase::new("write", ProbeOp::Write, td.path().join("new.txt")),
            ProbeCase::new("read", ProbeOp::Read, seed.clone()),
            ProbeCase::new("missing", ProbeOp::Read, td.path().join("absent.txt")),
            ProbeCase::new("stat", ProbeOp::Stat, td.path()),
            ProbeCase::new("list", ProbeOp::List, td.path()),
            ProbeCase::new("canon", ProbeOp::Canonicalize, td.path()),
        ];
        let mut lines = Vec::new();
        probe_report(&cases, &mut |key, value| {
            lines.push(probe_report_line(key, value))
        });
        let report = parse_probe_report(&lines.join("\n"));
        let value = |key: &str| {
            report
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.clone())
                .unwrap_or_else(|| panic!("{key} not reported: {report:?}"))
        };

        assert_eq!(value("case.write"), "ok");
        assert!(td.path().join("new.txt").is_file());
        assert_eq!(value("case.read"), "ok:4 bytes");
        // ENOENT and ERROR_FILE_NOT_FOUND are both 2, so this holds everywhere.
        let missing = value("case.missing");
        assert!(missing.starts_with("err:2:"), "{missing}");
        assert_eq!(value("case.stat"), "ok:dir");
        assert_eq!(value("case.list"), "ok:2 entries");
        assert!(value("case.canon").starts_with("ok:"));
        assert!(value("cwd").starts_with("ok:"));
        assert_eq!(
            report.last().map(|(k, _)| k.as_str()),
            Some(PROBE_DONE_KEY),
            "the done marker must come last"
        );
    }

    #[test]
    fn probe_report_lines_stay_on_one_line_and_parse_back() {
        let line = probe_report_line("case.x", "err:5:Access is denied.\r\nmore");
        assert!(!line.contains('\n') && !line.contains('\r'), "{line:?}");
        let mixed = format!("noise\n{line}\r\n  AHMA-PROBE cmd.in_write=ok \nAHMA-PROBE done=1");
        assert_eq!(
            parse_probe_report(&mixed),
            vec![
                (
                    "case.x".to_string(),
                    "err:5:Access is denied.  more".to_string()
                ),
                ("cmd.in_write".to_string(), "ok".to_string()),
                ("done".to_string(), "1".to_string()),
            ]
        );
    }

    #[test]
    fn integrity_levels_are_named() {
        assert_eq!(integrity_level_name(0x1000), "Low");
        assert_eq!(integrity_level_name(0x2000), "Medium");
        assert_eq!(integrity_level_name(0x3000), "High");
        assert_eq!(integrity_level_name(0x1234), "unrecognised");
    }
}
