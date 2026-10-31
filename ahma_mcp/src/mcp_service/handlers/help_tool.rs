//! The `ahma_help` tool: short, copy-pasteable guidance for an AI agent working
//! inside ahma's sandbox (SPEC R-HELP).
//!
//! An agent that hits `Operation not permitted` has three bad options: retry the
//! same command, copy caches into the workspace, or stop and ask a human. Each
//! costs a round trip, and the first two make a mess. This tool says what to
//! try first — in the agent's own terms (flags, environment variables, a place
//! to put files) — and when a human really is needed, the one command to ask for.
//!
//! The text is generated from the live session ([`HelpFacts`]), not recited:
//! whether temp space is available, and where it is, differ by platform and by
//! `--no-temp-files`, and a help text that is wrong about that is worse than none.

use super::common;
use crate::AhmaMcpService;
use crate::mcp_service::schema;
use rmcp::model::{CallToolResult, ErrorData as McpError};
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Every topic: its name and a one-line summary for the index.
pub const TOPICS: &[(&str, &str)] = &[
    (
        "start",
        "what ahma does to your commands, and the three places you can write",
    ),
    (
        "temp",
        "the temp directory: whether it is available, where, and how it helps your tools",
    ),
    (
        "blocked",
        "a command failed with 'Operation not permitted' or a write was denied",
    ),
    ("slow", "commands that run long or wait for a person"),
    ("readonly", "which commands skip the write queue"),
    (
        "roots",
        "'Sandbox initializing from client roots' and 'no sandbox scope'",
    ),
];

/// Whether sandboxed commands can use the system temp directory, and why not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TempAccess {
    /// The kernel sandbox lets commands write these directories.
    Open(Vec<PathBuf>),
    /// The session was started with `--no-temp-files`: every temp location is refused.
    Refused,
    /// ahma is not enforcing a filesystem boundary here (`--no-sandbox`, or a
    /// platform without one), so temp is writable and nothing is confining it.
    NotEnforced,
}

/// What the help text says about this session. Gathered once from the live
/// sandbox so the rendering is a pure function the tests can drive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HelpFacts {
    pub temp: TempAccess,
    /// The directory the platform calls temp (`std::env::temp_dir()`), canonical.
    pub temp_dir: PathBuf,
    /// The caller's `TMPDIR`, when set.
    pub tmpdir_env: Option<PathBuf>,
    /// The first (primary) write scope, when one is established.
    pub workspace: Option<PathBuf>,
}

impl HelpFacts {
    /// The directories the kernel sandbox lets a command write for temp files,
    /// on this platform. Mirrors the rules in `sandbox/landlock.rs`
    /// (`add_landlock_temp_rules`) and `sandbox/seatbelt.rs`
    /// (`get_macos_temp_rules`); keep them in step.
    pub fn kernel_temp_roots() -> Vec<PathBuf> {
        if cfg!(target_os = "macos") {
            vec![
                PathBuf::from("/private/tmp"),
                PathBuf::from("/private/var/folders"),
            ]
        } else if cfg!(target_os = "linux") {
            vec![PathBuf::from("/tmp")]
        } else {
            Vec::new()
        }
    }

    /// Read the facts from the live sandbox and environment.
    pub fn from_sandbox(sandbox: &crate::sandbox::Sandbox) -> Self {
        let canon = |p: PathBuf| dunce::canonicalize(&p).unwrap_or(p);
        let roots = Self::kernel_temp_roots();
        let temp = if sandbox.is_no_temp_files() {
            TempAccess::Refused
        } else if !sandbox.is_enforced() || roots.is_empty() {
            TempAccess::NotEnforced
        } else {
            TempAccess::Open(roots)
        };
        Self {
            temp,
            temp_dir: canon(std::env::temp_dir()),
            tmpdir_env: std::env::var_os("TMPDIR")
                .filter(|v| !v.is_empty())
                .map(|v| canon(PathBuf::from(v))),
            workspace: sandbox.scopes().first().cloned(),
        }
    }

    /// Whether `path` is inside a directory the kernel lets commands write for temp.
    fn temp_writable(&self, path: &Path) -> bool {
        match &self.temp {
            TempAccess::Open(roots) => roots.iter().any(|r| path.starts_with(r)),
            TempAccess::NotEnforced => true,
            TempAccess::Refused => false,
        }
    }
}

/// The text for `topic` (the index when `None`). An unknown topic gets a short
/// error followed by the index, never a failure: the caller is mid-task.
pub fn render(topic: Option<&str>, facts: &HelpFacts) -> String {
    let Some(raw) = topic.map(str::trim).filter(|t| !t.is_empty()) else {
        return index();
    };
    match raw.to_ascii_lowercase().as_str() {
        "start" => start(facts),
        "temp" => temp(facts),
        "blocked" => blocked(facts),
        "slow" => slow(),
        "readonly" => readonly(),
        "roots" => roots(),
        _ => format!("Unknown topic '{raw}'.\n\n{}", index()),
    }
}

fn index() -> String {
    let width = TOPICS.iter().map(|(n, _)| n.len()).max().unwrap_or(0);
    let mut out = String::from("ahma_help topics (call ahma_help with topic=<name>):\n");
    for (name, summary) in TOPICS {
        out.push_str(&format!("  {name:<width$}  {summary}\n"));
    }
    out.push_str(
        "\nStuck on a failure? Start with topic=blocked. Want somewhere to put files? topic=temp.\n",
    );
    out
}

/// The one-line temp verdict used by `start` and `temp`.
fn temp_headline(facts: &HelpFacts) -> String {
    match &facts.temp {
        TempAccess::Open(_) | TempAccess::NotEnforced => {
            format!("Temp space: AVAILABLE at {}", facts.temp_dir.display())
        }
        TempAccess::Refused => {
            "Temp space: NOT AVAILABLE (this session was started with --no-temp-files)".to_string()
        }
    }
}

fn start(facts: &HelpFacts) -> String {
    let workspace = facts
        .workspace
        .as_ref()
        .map(|w| w.display().to_string())
        .unwrap_or_else(|| "(not established yet)".to_string());
    format!(
        "ahma runs your shell commands inside a kernel sandbox. You may write the workspace, \
         and very little else.\n\
         \n\
         Workspace: {workspace}\n\
         {temp}, see ahma_help topic=temp\n\
         \n\
         Three places a command can write:\n\
         1. the workspace: your work\n\
         2. the temp directory: throwaway files and caches you will not reuse\n\
         3. a gitignored folder in the workspace: caches worth keeping between sessions\n\
         \n\
         Commands that may write run one at a time, in the order sent (ahma_help topic=readonly). \
         A command still running after a few seconds returns an operation_id: `await` it \
         (ahma_help topic=slow).\n\
         A command fails with 'Operation not permitted': ahma_help topic=blocked.\n",
        temp = temp_headline(facts),
    )
}

fn temp(facts: &HelpFacts) -> String {
    match &facts.temp {
        TempAccess::Refused => format!(
            "{headline}\n\
             \n\
             The user chose --no-temp-files, which refuses every temp location, so do not try \
             /tmp, $TMPDIR or mktemp. Use a gitignored folder in the workspace for anything you \
             need to put somewhere (check .gitignore; a folder such as target/ or node_modules/ \
             is already covered for the tools that own it), and say so to the user if you need \
             one added.\n\
             \n\
             Caches the sandbox blocks can still be redirected: see ahma_help topic=blocked.\n",
            headline = temp_headline(facts),
        ),
        TempAccess::Open(_) | TempAccess::NotEnforced => temp_open(facts),
    }
}

fn temp_open(facts: &HelpFacts) -> String {
    let mut out = format!("{}\n\n", temp_headline(facts));
    match &facts.temp {
        TempAccess::Open(roots) => {
            let list: Vec<String> = roots.iter().map(|r| r.display().to_string()).collect();
            out.push_str(&format!(
                "Commands you run can write under: {}.\n",
                list.join(", ")
            ));
            if let Some(env) = &facts.tmpdir_env
                && !facts.temp_writable(env)
            {
                let fix = roots
                    .first()
                    .map(|r| r.display().to_string())
                    .unwrap_or_default();
                out.push_str(&format!(
                    "Warning: your TMPDIR is {env}, which is NOT writable for sandboxed commands \
                     here. Run with TMPDIR={fix} (for example `TMPDIR={fix} mytool`), or tools \
                     that honour TMPDIR will fail.\n",
                    env = env.display(),
                ));
            }
        }
        _ => out.push_str(
            "ahma is not enforcing a filesystem boundary in this session, so temp is writable \
             and nothing confines it; the advice below still keeps your work tidy.\n",
        ),
    }
    out.push_str(
        "\n\
         Make a folder of your own first. The directory is shared with every process on this \
         machine, so a fixed name can collide with someone else's:\n\
         \x20 d=$(mktemp -d) && echo $d\n\
         \n\
         Then use it to get tools working when they want to write somewhere the sandbox refuses \
         (their default cache or home):\n\
         \x20 any tool that follows XDG   XDG_CACHE_HOME=$d/cache\n\
         \x20 pip / uv                    pip install --cache-dir $d/pip   (or --no-cache-dir)   uv --cache-dir $d/uv\n\
         \x20 npm / yarn / pnpm           npm install --cache $d/npm\n\
         \x20 cargo build output          CARGO_TARGET_DIR=$d/target   (keeps the workspace clean)\n\
         \x20 go                          GOCACHE=$d/go-build   GOMODCACHE=$d/go-mod\n\
         \x20 python bytecode             PYTHONPYCACHEPREFIX=$d/pycache   (or PYTHONDONTWRITEBYTECODE=1)\n\
         \x20 gradle                      GRADLE_USER_HOME=$d/gradle\n\
         \x20 scripts and compilers       TMPDIR=$d  (python tempfile, mktemp, gcc and git honour it)\n\
         \n\
         If a tool is not listed, run `<tool> --help` and look for an option naming a cache, temp \
         or output directory.\n\
         \n\
         What temp is not: it is shared with every process on this machine, the operating system \
         cleans it on its own schedule, and ahma's own file tools and working_directory still \
         treat it as outside the workspace. Use it for what you can lose. A cache you want to \
         reuse next session belongs in a gitignored folder in the workspace, not here.\n",
    );
    out
}

fn blocked(facts: &HelpFacts) -> String {
    let step2 = match &facts.temp {
        TempAccess::Refused => "2. Point it somewhere writable. This session was started with \
             --no-temp-files, so temp is not an option: use a gitignored folder in the workspace.\n"
            .to_string(),
        _ => format!(
            "2. Point it somewhere writable. Throwaway files and caches you will not reuse go in \
             the temp directory ({}; ahma_help topic=temp has the recipes). Anything worth keeping \
             between sessions goes in a gitignored folder in the workspace.\n",
            facts.temp_dir.display()
        ),
    };
    format!(
        "Blocked: 'Operation not permitted', or a write was denied.\n\
         \n\
         Do not retry the same command, and do not copy caches or toolchains into the \
         workspace: it bloats the project and goes stale. Work down this list, and keep working \
         at step 1 and 2 until a human has answered step 3.\n\
         \n\
         1. Find the tool's own switch. Run `<tool> --help` (and `<tool> <subcommand> --help`) \
         and look for an option that names a cache, temp, output, config or home directory \
         (--cache-dir, --target-dir, --output, --prefix, --offline, --no-cache), or an \
         environment variable for it (XDG_CACHE_HOME, CARGO_TARGET_DIR, GOCACHE, PIP_CACHE_DIR, \
         npm_config_cache, GRADLE_USER_HOME). Most tools have one.\n\
         {step2}\
         3. Only when the tool cannot be redirected, or it really needs the real location \
         (installing a global binary, a credential store), ask the human, once, for the \
         narrowest directory: `ahma sandbox grant <dir>` (add --read-only when only a read was \
         refused). Say what you need it for. A grant applies from the next command; nothing to \
         restart.\n\
         \n\
         Some things no grant can unlock (credentials, ahma's own settings). The refusal says \
         so; do not look for a way around it.\n"
    )
}

fn slow() -> String {
    "Long-running commands.\n\
     \n\
     A command still running after a few seconds returns an operation_id and keeps running. \
     Do other work, then `await` that id when you need the result; `cancel` stops it. Do not \
     poll `status` in a loop, and do not resend a command that says 'NOT started — queued \
     behind': it will run when its turn comes.\n\
     \n\
     Set `timeout_seconds` on run_terminal_command to kill a command that overruns; \
     without it the configured tool timeout applies (30 minutes by default).\n\
     \n\
     A command that waits for a person (a local server that blocks until someone answers, an \
     interactive prompt) holds the workspace write queue while it waits, so every later build \
     queues behind it, and it is killed at the tool timeout. Make such a command print what the \
     person needs (a URL, a question) and exit, then ask them in your reply and start the next \
     step when they answer.\n"
        .to_string()
}

fn readonly() -> String {
    "Which commands skip the write queue.\n\
     \n\
     Commands that may write the workspace run one at a time, in the order sent, shared with \
     every other ahma session on this machine. A later command waits for earlier ones and its \
     result says 'NOT started — queued behind' another operation; it will run on its own.\n\
     \n\
     Commands ahma recognises as read-only (`git status`/`diff`/`log`, `rg`, `grep`, `ls`, \
     `cat`) skip the queue and answer at once, even during a build. ahma classifies by the \
     command, so a script or interpreter run (python3 script.py, awk) is treated as a possible \
     writer and waits its turn even if it only reads. When you only need to look, prefer the \
     recognised read-only commands.\n"
        .to_string()
}

fn roots() -> String {
    "'Sandbox initializing from client roots' / 'no sandbox scope'.\n\
     \n\
     ahma will not run a command until it knows which folder it may write. It learns that from \
     the client's workspace roots (roots/list) or from a flag. 'Sandbox initializing from client \
     roots' means it is still waiting for them: wait a moment and call again.\n\
     \n\
     If it never settles, or you see 'no sandbox scope', the client is not providing roots. \
     That is a setup step only the user can do: start ahma with `--sandbox-scope <dir>` (or \
     `--working-directories <dir>`), for example in the `args` of the MCP server entry. Tell \
     them that exact flag; do not try to run the command another way.\n"
        .to_string()
}

/// JSON schema for the `ahma_help` tool.
pub fn ahma_help_schema() -> Arc<Map<String, Value>> {
    let mut props = Map::new();
    props.insert(
        "topic".to_string(),
        schema::string_property(
            "What you need help with: start, temp, blocked, slow, readonly or roots. Omit for the index.",
        ),
    );
    schema::object_input_schema(props, &[])
}

impl AhmaMcpService {
    /// Handles the `ahma_help` tool call.
    pub async fn handle_ahma_help(
        &self,
        args: Map<String, Value>,
    ) -> Result<CallToolResult, McpError> {
        let facts = HelpFacts::from_sandbox(self.adapter.sandbox());
        let topic = common::opt_str(&args, "topic");
        Ok(common::text_result(render(topic.as_deref(), &facts)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_facts() -> HelpFacts {
        HelpFacts {
            temp: TempAccess::Open(vec![
                PathBuf::from("/private/tmp"),
                PathBuf::from("/private/var/folders"),
            ]),
            temp_dir: PathBuf::from("/private/var/folders/ab/cd123/T"),
            tmpdir_env: Some(PathBuf::from("/private/var/folders/ab/cd123/T")),
            workspace: Some(PathBuf::from("/work/project")),
        }
    }

    fn refused_facts() -> HelpFacts {
        HelpFacts {
            temp: TempAccess::Refused,
            ..open_facts()
        }
    }

    fn unenforced_facts() -> HelpFacts {
        HelpFacts {
            temp: TempAccess::NotEnforced,
            ..open_facts()
        }
    }

    #[test]
    fn the_index_lists_every_topic() {
        let text = render(None, &open_facts());
        for (name, summary) in TOPICS {
            assert!(
                text.contains(name),
                "{name} missing from the index:\n{text}"
            );
            assert!(text.contains(summary), "{summary} missing:\n{text}");
        }
        assert!(text.contains("ahma_help"), "says how to ask: {text}");
    }

    #[test]
    fn every_topic_says_something_in_every_state() {
        for facts in [open_facts(), refused_facts(), unenforced_facts()] {
            for (name, _) in TOPICS {
                let text = render(Some(name), &facts);
                assert!(
                    text.len() > 80,
                    "{name} is nearly empty in {facts:?}: {text}"
                );
            }
        }
    }

    #[test]
    fn an_unknown_topic_gets_a_short_error_and_the_index() {
        let text = render(Some("frobnicate"), &open_facts());
        assert!(text.starts_with("Unknown topic 'frobnicate'"), "{text}");
        assert!(text.contains("blocked"), "the index follows: {text}");
    }

    #[test]
    fn topic_names_are_matched_without_regard_to_case_or_padding() {
        let facts = open_facts();
        assert_eq!(render(Some(" TEMP "), &facts), render(Some("temp"), &facts));
    }

    /// The three things the caller most needs: that temp is there, where, and
    /// what it is good for.
    #[test]
    fn temp_says_it_is_available_where_it_is_and_how_it_helps_tools() {
        let text = render(Some("temp"), &open_facts());
        assert!(text.contains("AVAILABLE"), "{text}");
        assert!(
            text.contains("/private/var/folders/ab/cd123/T"),
            "the path: {text}"
        );
        assert!(text.contains("/private/tmp"), "every writable root: {text}");
        for help in [
            "mktemp -d",
            "XDG_CACHE_HOME",
            "--cache-dir",
            "CARGO_TARGET_DIR",
            "PYTHONPYCACHEPREFIX",
            "GOCACHE",
        ] {
            assert!(text.contains(help), "{help} missing:\n{text}");
        }
    }

    #[test]
    fn temp_is_honest_about_what_it_is_not() {
        let text = render(Some("temp"), &open_facts());
        assert!(text.contains("shared with every process"), "{text}");
        assert!(
            text.contains("gitignored folder in the workspace"),
            "durable caches go elsewhere: {text}"
        );
    }

    /// On Linux only `/tmp` is writable; a `TMPDIR` elsewhere is a trap.
    #[test]
    fn a_tmpdir_outside_the_writable_roots_is_called_out() {
        let facts = HelpFacts {
            temp: TempAccess::Open(vec![PathBuf::from("/tmp")]),
            temp_dir: PathBuf::from("/run/user/1000"),
            tmpdir_env: Some(PathBuf::from("/run/user/1000")),
            workspace: None,
        };
        let text = render(Some("temp"), &facts);
        assert!(text.contains("/run/user/1000"), "{text}");
        assert!(text.contains("NOT writable"), "{text}");
        assert!(text.contains("TMPDIR=/tmp"), "the fix: {text}");
    }

    #[test]
    fn a_tmpdir_inside_the_writable_roots_raises_no_alarm() {
        let text = render(Some("temp"), &open_facts());
        assert!(!text.contains("NOT writable"), "{text}");
    }

    #[test]
    fn no_temp_files_is_stated_plainly_with_a_way_forward() {
        let text = render(Some("temp"), &refused_facts());
        assert!(text.contains("NOT AVAILABLE"), "{text}");
        assert!(text.contains("--no-temp-files"), "{text}");
        assert!(
            text.contains("gitignored folder in the workspace"),
            "{text}"
        );
        assert!(
            !text.contains("mktemp -d"),
            "no recipe for a place the user has ruled out: {text}"
        );
    }

    #[test]
    fn with_no_boundary_enforced_temp_is_available_and_says_nothing_confines_it() {
        let text = render(Some("temp"), &unenforced_facts());
        assert!(text.contains("AVAILABLE"), "{text}");
        assert!(text.contains("not enforcing"), "{text}");
    }

    #[test]
    fn start_puts_the_temp_availability_up_front() {
        let open = render(Some("start"), &open_facts());
        assert!(
            open.contains("Temp space: AVAILABLE at /private/var/folders/ab/cd123/T"),
            "{open}"
        );
        let refused = render(Some("start"), &refused_facts());
        assert!(refused.contains("Temp space: NOT AVAILABLE"), "{refused}");
    }

    #[test]
    fn blocked_is_the_ladder_a_stuck_agent_needs() {
        let text = render(Some("blocked"), &open_facts());
        assert!(
            text.contains("--help"),
            "look for the tool's own option first: {text}"
        );
        assert!(
            text.contains("ahma sandbox grant"),
            "the one command to ask for, last: {text}"
        );
        assert!(
            text.contains("do not copy"),
            "no copying caches into the workspace: {text}"
        );
        assert!(
            text.contains("ahma_help topic=temp"),
            "points at the temp topic: {text}"
        );
        let help = text.find("--help").unwrap();
        let grant = text.find("ahma sandbox grant").unwrap();
        assert!(help < grant, "ask a human last: {text}");
    }

    #[test]
    fn blocked_in_a_no_temp_files_session_does_not_send_the_agent_to_temp() {
        let text = render(Some("blocked"), &refused_facts());
        assert!(!text.contains("mktemp -d"), "{text}");
        assert!(text.contains("--no-temp-files"), "{text}");
    }

    #[test]
    fn roots_explains_the_two_scope_errors() {
        let text = render(Some("roots"), &open_facts());
        assert!(
            text.contains("Sandbox initializing from client roots"),
            "{text}"
        );
        assert!(text.contains("--sandbox-scope"), "{text}");
    }
}
