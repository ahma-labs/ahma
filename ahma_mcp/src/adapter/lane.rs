//! Which workspace-queue lane a shell command belongs in (SPEC R2.7.4).
//!
//! The read-only lane lets `git status` or `rg foo` answer *during* a
//! ten-minute `cargo nextest run` instead of queueing behind it. That is only
//! safe because the lane is enforced by the kernel, not by this classifier: a
//! read-lane command is spawned with a sandbox profile that grants **no write
//! access to the workspace at all** (`Sandbox::create_read_only_command`). A
//! command this module wrongly calls read-only therefore cannot corrupt
//! anything — it fails with a permission error, and the result says to rerun
//! it. The allowlist is conservative for the model's sake (a failed command is
//! a wasted turn), not for safety's.
//!
//! Anything the classifier cannot parse with certainty — shell operators,
//! substitutions, redirections, escapes, an unknown program — is exclusive.
//!
//! Two verdicts take no lease *without* the kernel's read-only lane, and are
//! held to a stricter test because a mistake there costs ordering, not just a
//! turn (the command still runs in the ordinary sandbox, so it is never an
//! escape): a line of readers that writes files only **outside every
//! workspace** (`gh pr checks 87 --watch > /tmp/ci.log`), which the read-only
//! lane would refuse to open, and — where the kernel has no read-only lane at
//! all — a line of readers that **watches** remote or log state until it ends
//! (`gh run watch`, `gh pr checks --watch`, `tail -f`). Both are the service
//! lane, and both require that no program in the line can write a file through
//! its own arguments (`writes_through_args`). See [`classify_shell_command_at`].

use super::workspace_queue::{Lane, SourceEffect};
use std::path::{Path, PathBuf};

/// Programs that only read, whatever their (plain) arguments.
const READ_ONLY_PROGRAMS: &[&str] = &[
    "ls",
    "cat",
    "head",
    // `tail -f` included: a follower never ends, so in the exclusive lane it
    // would hold the workspace — and stall every later writer — for its whole
    // life. It never writes, which is exactly what the read-only lane enforces.
    "tail",
    "wc",
    "stat",
    "file",
    "pwd",
    "which",
    "whereis",
    "du",
    "df",
    "tree",
    "diff",
    "cmp",
    "realpath",
    "readlink",
    "basename",
    "dirname",
    "sha256sum",
    "sha1sum",
    "md5sum",
    "b3sum",
    "echo",
    "true",
    "date",
    "uname",
    "whoami",
    "id",
    "hostname",
    "printenv",
    "grep",
    "egrep",
    "fgrep",
    "nl",
    "sort",
    "uniq",
    "cut",
    "column",
    "less",
    "more",
    "ps",
    "pgrep",
    "env",
    "printf",
    "tr",
    "jq",
    "base64",
    "od",
    "hexdump",
    "strings",
    "nproc",
    "uptime",
    "vm_stat",
    "iostat",
    "free",
    // `top` only prints; `top -l 1` is how an agent samples it.
    "top",
    "sw_vers",
    "lsof",
    "netstat",
    "dig",
    "host",
    "nslookup",
    "test",
    "false",
    "xxd",
    "cal",
    "seq",
    // Waiting reads nothing and writes nothing; classified as a writer, a
    // poll loop held the workspace's write lease for its whole life.
    "sleep",
];

/// `git` subcommands that only read. `git status` would opportunistically
/// refresh the index; the read lane sets `GIT_OPTIONAL_LOCKS=0`, the switch git
/// provides for exactly this ("don't take optional locks or write the index").
const READ_ONLY_GIT: &[&str] = &[
    "status",
    "diff",
    "log",
    "show",
    "blame",
    "rev-parse",
    "ls-files",
    "ls-tree",
    "cat-file",
    "describe",
    "shortlog",
    "grep",
    "whatchanged",
    "rev-list",
    "merge-base",
    "name-rev",
];

/// Characters that make a command line mean more than "run some programs with
/// these words, piped or listed": substitutions, grouping, escapes, comments,
/// history expansion, heredocs. Pipes, lists and redirections are parsed
/// explicitly below.
const SHELL_OPERATORS: &[char] = &['`', '(', ')', '{', '}', '\\', '\n', '\r', '#', '!'];

/// A simple command: the words of one pipeline segment.
type Segment = Vec<String>;

/// A command line split into pipeline/list segments, with the files its
/// redirections write taken out of the words.
struct Line {
    segs: Vec<Segment>,
    /// Targets of the writing redirections (`>`, `>>`, `>|`, `2>`, `&>`, …),
    /// exactly as written. `/dev/null` and stream merges (`2>&1`) are not
    /// here: they write no file.
    writes: Vec<String>,
}

/// What one redirection word does.
enum Redirection {
    /// `2>&1`, `>&2`, `2>&-`: streams merged or closed, no file opened.
    Stream,
    /// Output to a file; `None` when the file is the next word (`> f`).
    Write(Option<String>),
    /// Input from a file; `None` when the file is the next word (`< f`).
    Read(Option<String>),
}

/// Read a word with an unquoted `<` or `>` as a redirection. `None` for
/// anything else — `a>b`, `>&file`, `&>&2` — which the caller cannot model.
fn redirection(word: &str) -> Option<Redirection> {
    let file = |rest: &str| (!rest.is_empty()).then(|| rest.to_string());
    if let Some(rest) = word.strip_prefix('<') {
        return (!rest.contains(['<', '>', '&'])).then(|| Redirection::Read(file(rest)));
    }
    // `&>` and `&>>` redirect stdout and stderr; `2>`, `1>>` one descriptor.
    let (both, rest) = match word.strip_prefix('&') {
        Some(rest) => (true, rest),
        None => (false, word.trim_start_matches(|c: char| c.is_ascii_digit())),
    };
    let rest = rest.strip_prefix('>')?;
    let rest = rest.strip_prefix(['>', '|']).unwrap_or(rest);
    if let Some(fd) = rest.strip_prefix('&') {
        let to_fd = fd == "-" || (!fd.is_empty() && fd.chars().all(|c| c.is_ascii_digit()));
        return (!both && to_fd).then_some(Redirection::Stream);
    }
    (!rest.contains(['<', '>'])).then(|| Redirection::Write(file(rest)))
}

/// Split a command line into pipeline/list segments of words, honouring plain
/// single and double quotes. `None` for anything with a shell operator this
/// module does not model, an unbalanced quote, a writing redirection, or a
/// substitution — the caller treats that as "cannot tell", i.e. exclusive.
fn segments(command: &str) -> Option<Vec<Segment>> {
    let line = parse_line(command)?;
    line.writes.is_empty().then_some(line.segs)
}

/// [`segments`], with writing redirections parsed out into [`Line::writes`]
/// instead of refused. Input redirections other than `< /dev/null` are still
/// refused: they are reads, but not ones this module models.
fn parse_line(command: &str) -> Option<Line> {
    if command.contains("<<") {
        return None;
    }
    // Tokenize: words, quoted strings, and the operators `|`, `||`, `&&`, `;`.
    // Any other `&` (background, `>&2` outside a redirection) is unknown.
    // Each word carries whether a `<` or `>` in it was *quoted*: that is an
    // argument (`grep '>' f`), never a redirection.
    type Word = (String, bool);
    let mut segs: Vec<Vec<Word>> = vec![Vec::new()];
    let mut cur = String::new();
    let mut quoted_angle = false;
    let mut in_word = false;
    let mut quote: Option<char> = None;
    let chars: Vec<char> = command.chars().collect();
    let mut i = 0;
    let flush = |cur: &mut String,
                 quoted_angle: &mut bool,
                 in_word: &mut bool,
                 segs: &mut Vec<Vec<Word>>| {
        if *in_word {
            segs.last_mut()
                .unwrap()
                .push((std::mem::take(cur), std::mem::take(quoted_angle)));
            *in_word = false;
        }
    };
    while i < chars.len() {
        let c = chars[i];
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => {
                quoted_angle |= c == '<' || c == '>';
                cur.push(c);
            }
            None => match c {
                // `!` standing alone negates the command after it: a word of
                // its own, which the classifier skips like a loop keyword.
                '!' if !in_word && chars.get(i + 1).is_none_or(|n| n.is_whitespace()) => {
                    cur.push('!');
                    in_word = true;
                }
                // An operator this module does not model, outside quotes only:
                // a `}` inside a sed script is just a character.
                c if SHELL_OPERATORS.contains(&c) => return None,
                '\'' | '"' => {
                    quote = Some(c);
                    in_word = true;
                }
                '&' if cur.ends_with('>') || cur.ends_with('<') => {
                    // `2>&1`, `>&2`: part of a redirection word, not an operator.
                    cur.push(c);
                    in_word = true;
                }
                '|' if cur.ends_with('>') => {
                    // `>|file`: write even under `noclobber`, not a pipe.
                    cur.push(c);
                    in_word = true;
                }
                '&' if chars.get(i + 1) == Some(&'>') => {
                    // `&>file`, `&>>file`: a redirection word of its own.
                    flush(&mut cur, &mut quoted_angle, &mut in_word, &mut segs);
                    cur.push(c);
                    in_word = true;
                }
                '|' | ';' | '&' => {
                    let next = chars.get(i + 1).copied();
                    let op_len = match (c, next) {
                        ('|', Some('|')) | ('&', Some('&')) => 2,
                        ('|', _) | (';', _) => 1,
                        // A bare `&`: background or a redirection we do not model.
                        ('&', _) => return None,
                        _ => 1,
                    };
                    flush(&mut cur, &mut quoted_angle, &mut in_word, &mut segs);
                    if segs.last().is_some_and(Vec::is_empty) {
                        return None; // `| cmd`, `cmd ||  | cmd`: not a command line we read
                    }
                    segs.push(Vec::new());
                    i += op_len;
                    continue;
                }
                c if c.is_whitespace() => {
                    flush(&mut cur, &mut quoted_angle, &mut in_word, &mut segs)
                }
                _ => {
                    cur.push(c);
                    in_word = true;
                }
            },
        }
        i += 1;
    }
    if quote.is_some() {
        return None;
    }
    flush(&mut cur, &mut quoted_angle, &mut in_word, &mut segs);
    if segs.last().is_some_and(Vec::is_empty) {
        return None; // trailing operator
    }
    // Redirections: drop the ones that open no file, collect the files the
    // writing ones open, refuse anything else with `<`/`>`.
    let mut writes = Vec::new();
    let mut cleaned_segs = Vec::with_capacity(segs.len());
    for seg in segs {
        let mut cleaned = Vec::new();
        let mut words = seg.into_iter();
        while let Some((w, quoted_angle)) = words.next() {
            if !w.contains(['<', '>']) {
                // `$(…)` is a substitution; a plain `$VAR` reference only reads
                // the environment and is left to the shell.
                if w.contains("$(") {
                    return None;
                }
                cleaned.push(w);
                continue;
            }
            if quoted_angle {
                return None;
            }
            let (writing, target) = match redirection(&w)? {
                Redirection::Stream => continue,
                Redirection::Write(target) => (true, target),
                Redirection::Read(target) => (false, target),
            };
            // `> file`: the file is the next word, which must be a plain one.
            let target = match target {
                Some(target) => target,
                None => match words.next() {
                    Some((next, _)) if !next.is_empty() && !next.contains(['<', '>']) => next,
                    _ => return None,
                },
            };
            if target == "/dev/null" {
                continue;
            }
            if !writing {
                return None;
            }
            writes.push(target);
        }
        if cleaned.is_empty() {
            return None;
        }
        cleaned_segs.push(cleaned);
    }
    Some(Line {
        segs: cleaned_segs,
        writes,
    })
}

/// Programs that only read, beyond [`READ_ONLY_PROGRAMS`], whose arguments
/// need a look: a writing option makes them exclusive.
fn program_is_read_only(program: &str, args: &[String]) -> bool {
    let has = |needle: &str| args.iter().any(|a| a == needle);
    let starts = |prefix: &str| args.iter().any(|a| a.starts_with(prefix));
    match program {
        "git" => git_is_read_only(args),
        "find" => !args.iter().any(|a| {
            matches!(
                a.as_str(),
                "-delete"
                    | "-exec"
                    | "-execdir"
                    | "-ok"
                    | "-okdir"
                    | "-fls"
                    | "-fprint"
                    | "-fprint0"
                    | "-fprintf"
            )
        }),
        // `rg --pre` runs an arbitrary preprocessor per file.
        "rg" => !starts("--pre"),
        // `sed -i` edits in place; `w` inside a script would write too, and the
        // kernel catches that one.
        "sed" | "gsed" => !starts("-i") && !starts("--in-place"),
        // `awk` can redirect inside its program text or shell out.
        "awk" | "gawk" | "mawk" => !args
            .iter()
            .any(|a| a.contains('>') || a.contains("system(")),
        // `curl` writes a file only when told to.
        "curl" => !args.iter().any(|a| {
            a == "-o"
                || a == "-O"
                || a.starts_with("--output")
                || a == "--remote-name"
                || a.starts_with("-o") && a.len() > 2 && !a.starts_with("--")
        }),
        // `gh`: the viewing subcommands, and `api` without a mutating method.
        // Each writes only under its own config and cache directories, never
        // the working directory. `gh run watch` follows a run until it ends.
        "gh" => {
            let mut it = args.iter().map(String::as_str);
            match (it.next(), it.next()) {
                (Some("pr"), Some("view" | "checks" | "list" | "status" | "diff")) => true,
                (Some("run"), Some("view" | "list" | "watch")) => true,
                (Some("issue"), Some("view" | "list")) => true,
                (Some("repo"), Some("view")) => true,
                (Some("api"), Some(_)) => gh_api_reads(&args[1..]),
                (Some("--version"), _) | (Some("auth"), Some("status")) => true,
                _ => false,
            }
        }
        // `sysctl` reads unless it is setting a value (`-w`, `name=value`).
        "sysctl" => !has("-w") && !args.iter().any(|a| a.contains('=')),
        // `cargo`/`rustc`/`rustup` only to say what they are.
        "cargo" | "rustc" | "rustup" => args.first().map(String::as_str) == Some("--version"),
        // ahma's own read-only commands, so a queued agent can still ask who is
        // holding the workspace (SPEC R2.7.9).
        "ahma" => {
            let mut it = args.iter().map(String::as_str);
            match (it.next(), it.next()) {
                (Some("queue"), None) | (Some("ps"), _) | (Some("--version"), _) => true,
                (Some("sandbox" | "permissions" | "network"), Some("list")) => true,
                (Some("hooks"), Some("status")) => true,
                (Some("doctor"), _) => !has("--fix"),
                _ => false,
            }
        }
        p => READ_ONLY_PROGRAMS.contains(&p),
    }
}

/// Shell keywords that open or close a loop or a conditional around the
/// commands of a list: `until gh pr checks 87; do sleep 60; done` is a reader
/// exactly when every command in it is.
const LIST_KEYWORDS: &[&str] = &["until", "while", "if", "elif", "then", "else", "do", "!"];

/// Whether `word` is a `NAME=value` environment assignment.
fn is_assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        !name.is_empty()
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            && !name.starts_with(|c: char| c.is_ascii_digit())
    })
}

/// `gh api <args>` (endpoint included) reads unless it names a method other
/// than GET or sends a body: fields (`-f`, `-F`, `--field`, `--raw-field`)
/// turn it into a POST, and `--input` sends one.
fn gh_api_reads(args: &[String]) -> bool {
    let mut it = args.iter().map(String::as_str);
    while let Some(a) = it.next() {
        let method = match a {
            "-X" | "--method" => Some(it.next().unwrap_or("")),
            _ => a.strip_prefix("--method=").or_else(|| a.strip_prefix("-X")),
        };
        if let Some(method) = method {
            if !method.eq_ignore_ascii_case("GET") {
                return false;
            }
            continue;
        }
        if ["-f", "-F", "--field", "--raw-field", "--input"]
            .iter()
            .any(|p| a.starts_with(p))
        {
            return false;
        }
    }
    true
}

/// Whether a reading program can still write a file through its own
/// arguments — `sort -o`, `uniq in out`, a `sed` `w` command, `env` running
/// another program. Harmless in the read-only lane, where the kernel refuses
/// the write; disqualifying for a service verdict (SPEC R2.7.4), which runs in
/// the ordinary sandbox without the lease.
fn writes_through_args(program: &str, args: &[String]) -> bool {
    let short_has = |letters: &[char]| {
        args.iter()
            .any(|a| a.starts_with('-') && !a.starts_with("--") && a.contains(letters))
    };
    let long_has = |names: &[&str]| args.iter().any(|a| names.iter().any(|n| a.starts_with(n)));
    let operands = || args.iter().filter(|a| !a.starts_with('-')).count();
    match program {
        // Run another program, or a script that may write.
        "env" => !args.is_empty(),
        "sed" | "gsed" | "awk" | "gawk" | "mawk" => true,
        // Interactive: `s` saves, `!` runs a shell.
        "less" | "more" => true,
        "xxd" => operands() > 1,
        "uniq" => operands() > 1,
        "sort" => short_has(&['o']) || long_has(&["--output"]),
        "tree" => short_has(&['o']),
        "file" => short_has(&['C']) || long_has(&["--compile"]),
        "base64" => short_has(&['o']) || long_has(&["--output"]),
        "curl" => {
            short_has(&['o', 'O', 'D', 'c', 'K'])
                || long_has(&[
                    "--output",
                    "--remote-name",
                    "--dump-header",
                    "--cookie-jar",
                    "--trace",
                    "--stderr",
                    "--libcurl",
                    "--etag-save",
                    "--hsts",
                    "--alt-svc",
                    "--config",
                ])
        }
        _ => false,
    }
}

/// Whether a reading program follows remote or log state until it ends: a
/// CI watch or a log follower, which may run for most of an hour.
fn is_watcher(program: &str, args: &[String]) -> bool {
    let sub = (
        args.first().map(String::as_str),
        args.get(1).map(String::as_str),
    );
    match program {
        "gh" => match sub {
            (Some("run"), Some("watch")) => true,
            (Some("pr"), Some("checks")) => {
                args.iter().any(|a| a == "--watch" || a == "--watch=true")
            }
            _ => false,
        },
        "tail" => args.iter().any(|a| {
            a.starts_with("--follow")
                || (a.starts_with('-') && !a.starts_with("--") && a.contains(['f', 'F']))
        }),
        _ => false,
    }
}

/// The files `tee` writes, or `None` for an option this module does not model.
/// `tee` with no file only copies its input to its output.
fn tee_files(args: &[String]) -> Option<Vec<String>> {
    let mut files = Vec::new();
    for a in args {
        match a.as_str() {
            "-a" | "--append" | "-i" | "--ignore-interrupts" | "-p" | "-ai" | "-ia" => {}
            a if a.starts_with('-') => return None,
            _ => files.push(a.clone()),
        }
    }
    Some(files)
}

/// What the programs of a command line do, when every one of them reads.
#[derive(Default)]
struct Reading {
    /// Some program can write a file through its own arguments
    /// ([`writes_through_args`]), or runs under an environment assignment.
    writes_through_args: bool,
    /// Some program watches until its subject ends ([`is_watcher`]).
    watches: bool,
    /// The line changes directory, so a relative path in it may not be
    /// relative to the directory it started in.
    changes_dir: bool,
    /// Files `tee` writes, as written.
    tee_files: Vec<String>,
}

/// Judge a command line's programs. `None` unless every one only reads (or is
/// `tee`, whose files are returned for the caller to place). A `$(…)` runs a
/// command of its own: it is judged the same way and must write no file.
fn read_line(command: &str) -> Option<(Line, Reading)> {
    let (command, inner) = lift_substitutions(command)?;
    let line = parse_line(&command)?;
    let mut reading = read_segments(&line.segs)?;
    for c in &inner {
        let (inner_line, inner_reading) = read_line(c)?;
        if !inner_line.writes.is_empty() || !inner_reading.tee_files.is_empty() {
            return None;
        }
        reading.writes_through_args |= inner_reading.writes_through_args;
        reading.watches |= inner_reading.watches;
    }
    Some((line, reading))
}

/// The lane for a `run_terminal_command` command line, judged without knowing
/// where it runs: a line that writes any file — even through a redirection
/// outside the workspace — is exclusive. [`classify_shell_command_at`] is the
/// verdict the adapter uses.
pub fn classify_shell_command(command: &str) -> Lane {
    match read_line(command) {
        Some((line, reading)) if line.writes.is_empty() && reading.tee_files.is_empty() => {
            Lane::ReadOnly
        }
        _ => Lane::Exclusive,
    }
}

/// Where a command line runs, for [`classify_shell_command_at`].
pub struct RunSite<'a> {
    /// The directory the line starts in; a relative redirection target is
    /// relative to it.
    pub cwd: &'a Path,
    /// Whether the kernel can hold a command to the read-only lane here
    /// (`Sandbox::can_enforce_read_only`).
    pub read_only_enforced: bool,
    /// Whether writing this path writes a workspace: this one, or any other
    /// the session can write ([`writes_inside`]).
    pub writes_workspace: &'a dyn Fn(&Path) -> bool,
}

/// The lane for a `run_terminal_command` command line run at `site`
/// (SPEC R2.7.4).
///
/// - A line of readers that writes no file is read-only where the kernel
///   enforces that lane. Where it does not, it is exclusive — unless it
///   watches (`gh run watch`, `gh pr checks --watch`, `tail -f`), which would
///   otherwise hold the workspace for its whole life: then it is service.
/// - A line of readers whose redirections (and `tee`) write only files
///   outside every workspace — `gh pr checks 87 --watch > /tmp/ci.log` — is
///   service. The read-only lane would refuse to open those files.
/// - Everything else is exclusive: a writer, a file inside a workspace, a
///   target the shell would expand (`$X`, a glob, `~`), a relative target
///   after a `cd`, or a program that writes through its arguments.
pub fn classify_shell_command_at(command: &str, site: &RunSite<'_>) -> Lane {
    let Some((line, reading)) = read_line(command) else {
        return Lane::Exclusive;
    };
    let mut targets = line.writes.iter().chain(&reading.tee_files).peekable();
    if targets.peek().is_none() {
        return if site.read_only_enforced {
            Lane::ReadOnly
        } else if reading.watches && !reading.writes_through_args {
            Lane::Service
        } else {
            Lane::Exclusive
        };
    }
    if reading.writes_through_args {
        return Lane::Exclusive;
    }
    let all_outside = targets.all(|word| {
        redirection_target(word, site.cwd, reading.changes_dir)
            .is_some_and(|path| !(site.writes_workspace)(&path))
    });
    if all_outside {
        Lane::Service
    } else {
        Lane::Exclusive
    }
}

/// The path a redirection target names, read the way the shell would without
/// expanding anything: `None` for a word the shell would expand (a variable,
/// a glob, a leading `~`, a brace) and for a relative path in a line that changes
/// directory first.
fn redirection_target(word: &str, cwd: &Path, changes_dir: bool) -> Option<PathBuf> {
    // A `~` expands only at the start of a word; inside a path it is literal
    // (Windows 8.3 short names such as `RUNNER~1`).
    if word.is_empty()
        || word.starts_with('~')
        || word.contains(['$', '*', '?', '[', ']', '{', '}', '`'])
    {
        return None;
    }
    let path = Path::new(word);
    if !path.has_root() && changes_dir {
        return None;
    }
    Some(cwd.join(path))
}

/// Whether writing `path` would write inside `workspace` or any of `scopes`
/// (SPEC R2.7.2, R2.7.4), resolved the way the kernel will: the deepest part
/// of the path that exists is canonicalized, so a symlink into a workspace
/// counts as the workspace, and the rest is appended. Anything that cannot be
/// resolved with certainty — a dangling symlink, a `..` beyond what exists —
/// counts as inside. Compared without regard to case, which can only make it
/// say "inside" more often.
pub fn writes_inside(path: &Path, workspace: &Path, scopes: &[PathBuf]) -> bool {
    let mut existing = path;
    let mut rest = Vec::new();
    let resolved = loop {
        match dunce::canonicalize(existing) {
            Ok(real) => break real,
            Err(_) => {
                // An entry that exists but does not resolve is a dangling
                // symlink: writing it creates its target, wherever that is.
                if std::fs::symlink_metadata(existing).is_ok() {
                    return true;
                }
                let (Some(parent), Some(name)) = (existing.parent(), existing.file_name()) else {
                    return true;
                };
                rest.push(name);
                existing = parent;
            }
        }
    };
    let full = rest.iter().rev().fold(resolved, |p, name| p.join(name));
    let fold = |p: &Path| PathBuf::from(p.to_string_lossy().to_lowercase());
    let full = fold(&full);
    std::iter::once(workspace)
        .chain(scopes.iter().map(PathBuf::as_path))
        .any(|root| {
            let root = dunce::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
            full.starts_with(fold(&root))
        })
}

/// Judge each segment of a parsed line: `None` as soon as one does not read.
fn read_segments(segs: &[Segment]) -> Option<Reading> {
    let mut reading = Reading::default();
    for seg in segs {
        // Loop and conditional keywords, and `NAME=value` assignments, only
        // frame the command that follows; the command decides. The read lane
        // is still the kernel's word: an assignment such as `LD_PRELOAD` that
        // makes a reader try to write fails, it cannot write.
        let words: Vec<&String> = seg
            .iter()
            .skip_while(|w| LIST_KEYWORDS.contains(&w.as_str()) || is_assignment(w))
            .collect();
        if matches!(words.as_slice(), [w] if matches!(w.as_str(), "done" | "fi")) {
            continue;
        }
        // `S=/path;` on its own sets a shell variable and nothing else.
        if words.is_empty() && seg.iter().all(|w| is_assignment(w)) {
            continue;
        }
        // `for f in a b` only names what the loop body reads; the body decides.
        if let [first, name, rest @ ..] = words.as_slice()
            && first.as_str() == "for"
            && is_assignment(&format!("{name}=x"))
            && rest.first().is_none_or(|w| w.as_str() == "in")
        {
            continue;
        }
        let (program, args) = words.split_first()?;
        let program = program.as_str();
        let args: Vec<String> = args.iter().map(|a| (*a).clone()).collect();
        let args = args.as_slice();
        // A path to a program: not worth guessing.
        if program.contains('=') || program.contains('/') {
            return None;
        }
        // `cd <dir>` changes nothing on disk; the segment after it decides.
        if program == "cd" && args.len() <= 1 {
            reading.changes_dir = true;
            continue;
        }
        // `tee` copies its input to its output and to its files, which the
        // caller places like a redirection's.
        if program == "tee" {
            reading.tee_files.extend(tee_files(args)?);
            continue;
        }
        if !program_is_read_only(program, args) {
            return None;
        }
        // An environment assignment before a program (`LD_PRELOAD=…`) can
        // make a reader do anything; only the kernel's read-only lane makes
        // that harmless, so it rules out a service verdict.
        let assigns = seg
            .iter()
            .take_while(|w| LIST_KEYWORDS.contains(&w.as_str()) || is_assignment(w))
            .any(|w| is_assignment(w));
        reading.writes_through_args |= assigns || writes_through_args(program, args);
        reading.watches |= is_watcher(program, args);
    }
    Some(reading)
}

/// Replace every `$(…)` outside single quotes with a plain word, returning
/// the rewritten line and the commands the substitutions run. `None` for an
/// unbalanced one. Single-quoted text is literal, so `'$(rm x)'` is not lifted.
fn lift_substitutions(command: &str) -> Option<(String, Vec<String>)> {
    let chars: Vec<char> = command.chars().collect();
    let mut out = String::with_capacity(command.len());
    let mut inner = Vec::new();
    let mut in_single = false;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '\'' {
            in_single = !in_single;
            out.push(c);
            i += 1;
            continue;
        }
        if !in_single && c == '$' && chars.get(i + 1) == Some(&'(') {
            let start = i + 2;
            let mut depth = 1;
            let mut j = start;
            let mut quoted = false;
            while j < chars.len() {
                match chars[j] {
                    '\'' => quoted = !quoted,
                    '(' if !quoted => depth += 1,
                    ')' if !quoted => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    _ => {}
                }
                j += 1;
            }
            if depth != 0 {
                return None;
            }
            inner.push(chars[start..j].iter().collect());
            out.push_str("SUBSTITUTION");
            i = j + 1;
            continue;
        }
        out.push(c);
        i += 1;
    }
    Some((out, inner))
}

/// What a command line does to the workspace's source files (SPEC R2.7.8), for
/// the edit guard: a build or test only reads them, a formatter or checkout
/// rewrites them, and anything this module cannot read with certainty is
/// unknown — which the guard treats as a writer, as it treated every command
/// before. `declared` are a project's own wrappers (`[tools] source_readers`):
/// a command line starting with one, as whole words, reads sources.
pub fn classify_source_effect(command: &str, declared: &[String]) -> SourceEffect {
    let trimmed = command.trim();
    if declared.iter().map(|d| d.trim()).any(|d| {
        !d.is_empty()
            && (trimmed == d
                || trimmed
                    .strip_prefix(d)
                    .is_some_and(|rest| rest.starts_with(char::is_whitespace)))
    }) {
        return SourceEffect::ReadsSources;
    }
    let Some(segs) = segments(trimmed) else {
        return SourceEffect::Unknown;
    };
    segs.iter()
        .map(|seg| segment_effect(seg, declared))
        .fold(SourceEffect::ReadsSources, worse)
}

/// The more conservative of two effects: a rewrite anywhere in a pipeline or
/// list makes the whole line a rewrite, and an unknown part makes it unknown.
fn worse(a: SourceEffect, b: SourceEffect) -> SourceEffect {
    use SourceEffect::*;
    match (a, b) {
        (RewritesSources, _) | (_, RewritesSources) => RewritesSources,
        (Unknown, _) | (_, Unknown) => Unknown,
        _ => ReadsSources,
    }
}

/// The first argument that is not an option (`+toolchain` and `-q` skipped).
fn subcommand(args: &[String]) -> Option<&str> {
    args.iter()
        .map(String::as_str)
        .find(|a| !a.starts_with('-') && !a.starts_with('+'))
}

fn segment_effect(seg: &[String], declared: &[String]) -> SourceEffect {
    use SourceEffect::*;
    // `VAR=value cmd`: the assignment only sets the command's environment.
    let words: Vec<String> = seg
        .iter()
        .skip_while(|w| w.contains('=') && !w.starts_with('-'))
        .cloned()
        .collect();
    let Some((program, args)) = words.split_first() else {
        return Unknown;
    };
    if program == "cd" && args.len() <= 1 {
        return ReadsSources;
    }
    // `bash -c '<line>'`: what the line does.
    if matches!(program.as_str(), "bash" | "sh" | "zsh") && args.len() == 2 && args[0] == "-c" {
        return classify_source_effect(&args[1], declared);
    }
    // A path to a program is a project script — except the build wrappers
    // every Gradle and Maven project ships.
    let program = match program.rsplit('/').next() {
        Some(base @ ("gradlew" | "mvnw")) => base,
        _ if program.contains('/') => return Unknown,
        _ => program.as_str(),
    };
    if program_is_read_only(program, args) {
        return ReadsSources;
    }
    let has = |flag: &str| {
        args.iter()
            .any(|a| a == flag || a.starts_with(&format!("{flag}=")))
    };
    let sub = subcommand(args);
    match program {
        "cargo" => match sub {
            Some("fmt") if has("--check") => ReadsSources,
            Some("clippy") if has("--fix") => RewritesSources,
            Some(
                "build" | "b" | "test" | "t" | "nextest" | "check" | "c" | "clippy" | "doc"
                | "bench" | "tree" | "metadata" | "audit" | "deny" | "llvm-cov",
            ) => ReadsSources,
            Some("fmt" | "fix" | "update" | "add" | "remove" | "rm" | "upgrade") => RewritesSources,
            _ => Unknown,
        },
        "npm" | "pnpm" | "yarn" | "bun" => match sub {
            Some("test" | "t") => ReadsSources,
            Some("run") if args.iter().any(|a| a == "test") => ReadsSources,
            Some("install" | "i" | "ci" | "add" | "remove" | "update" | "upgrade") => {
                RewritesSources
            }
            _ => Unknown,
        },
        "go" => match sub {
            Some("build" | "test" | "vet" | "list") => ReadsSources,
            Some("fmt" | "generate" | "get" | "mod") => RewritesSources,
            _ => Unknown,
        },
        "python" | "python3" => {
            if args.first().map(String::as_str) == Some("-m")
                && matches!(
                    args.get(1).map(String::as_str),
                    Some("pytest" | "mypy" | "unittest")
                )
            {
                ReadsSources
            } else {
                Unknown
            }
        }
        "pytest" | "mypy" | "tsc" | "jest" | "vitest" | "tox" | "xcodebuild" => ReadsSources,
        "swift" => match sub {
            Some("build" | "test") => ReadsSources,
            _ => Unknown,
        },
        "gradle" | "gradlew" | "mvn" | "mvnw" => {
            if args.iter().any(|a| {
                let a = a.to_ascii_lowercase();
                a.contains("spotlessapply") || a.contains("format") || a.contains("ktlintformat")
            }) {
                RewritesSources
            } else {
                ReadsSources
            }
        }
        "eslint" => {
            if has("--fix") {
                RewritesSources
            } else {
                ReadsSources
            }
        }
        "ruff" => match sub {
            Some("check") if !has("--fix") => ReadsSources,
            Some("check" | "format") => RewritesSources,
            _ => Unknown,
        },
        "black" | "isort" | "gofmt" | "rustfmt" => {
            if has("--check") {
                ReadsSources
            } else {
                RewritesSources
            }
        }
        "prettier" => {
            if has("--write") || has("-w") {
                RewritesSources
            } else if has("--check") || has("-c") {
                ReadsSources
            } else {
                Unknown
            }
        }
        "git" => match sub {
            Some("commit" | "add" | "push" | "fetch" | "tag") => ReadsSources,
            Some(
                "checkout" | "switch" | "stash" | "reset" | "rebase" | "merge" | "pull" | "restore"
                | "apply" | "am" | "cherry-pick" | "revert" | "clean" | "mv" | "rm",
            ) => RewritesSources,
            _ => Unknown,
        },
        "sed" | "gsed" | "perl" | "rm" | "mv" | "cp" | "touch" | "mkdir" | "ln" | "truncate"
        | "patch" => RewritesSources,
        _ => Unknown,
    }
}

fn git_is_read_only(args: &[String]) -> bool {
    // Global options (`-c core.fsmonitor=…`, `-C dir`, `--exec-path`) come before
    // the subcommand; any of them makes the call something other than a plain read.
    let Some(sub) = args.first() else {
        return false;
    };
    if sub.starts_with('-') {
        return false;
    }
    if !READ_ONLY_GIT.contains(&sub.as_str()) {
        return false;
    }
    // Options that make a reading subcommand write a file.
    !args
        .iter()
        .any(|a| a == "--output" || a.starts_with("--output="))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lane(cmd: &str) -> Lane {
        classify_shell_command(cmd)
    }

    #[test]
    fn plain_readers_are_read_only() {
        for cmd in [
            "git status",
            "git diff --stat",
            "git log --oneline -5",
            "git show HEAD:README.md",
            "rg foo src",
            "rg \"two words\" src",
            "grep -rn 'fn main' .",
            "ls -la",
            "cat Cargo.toml",
            "tail -n 50 build.log",
            // A follower never ends and never writes: holding the workspace
            // for its whole life would stall every later writer.
            "tail -f build.log",
            "tail --follow=name build.log",
            "find . -name '*.rs'",
            "wc -l src/lib.rs",
        ] {
            assert_eq!(lane(cmd), Lane::ReadOnly, "{cmd}");
        }
    }

    #[test]
    fn writers_and_unknowns_are_exclusive() {
        for cmd in [
            "cargo build",
            "cargo nextest run",
            "git commit -m x",
            "git checkout main",
            "git stash",
            "git branch",
            "rm -rf target",
            "sed -i s/a/b/ f",
            "npm test",
            "./script.sh",
            "FOO=1 cargo build",
            "",
        ] {
            assert_eq!(lane(cmd), Lane::Exclusive, "{cmd}");
        }
    }

    #[test]
    fn shell_operators_make_anything_exclusive() {
        for cmd in [
            "cat a > b",
            "ls | tee out",
            "git status; rm x",
            "echo $(rm x)",
            "echo `rm x`",
            "ls && make",
            "cat <<EOF",
            "echo hi \\",
        ] {
            assert_eq!(lane(cmd), Lane::Exclusive, "{cmd}");
        }
    }

    #[test]
    fn writing_options_of_reading_programs_are_exclusive() {
        for cmd in [
            "find . -delete",
            "find . -exec rm {} ;",
            "rg --pre ./x foo",
            "git -c core.fsmonitor=evil status",
            "git -C other status",
            "git diff --output=patch.diff",
        ] {
            assert_eq!(lane(cmd), Lane::Exclusive, "{cmd}");
        }
    }

    #[test]
    fn unbalanced_quotes_are_exclusive() {
        assert_eq!(lane("rg 'foo"), Lane::Exclusive);
    }
}

#[cfg(test)]
mod pipeline_tests {
    use super::*;

    fn lane(cmd: &str) -> Lane {
        classify_shell_command(cmd)
    }

    /// SPEC R2.7.4: a pipeline or list whose every program only reads is a
    /// read. The kernel still enforces it; this only stops `grep … | head`
    /// from queueing behind a ten-minute build.
    #[test]
    fn pipelines_and_lists_of_readers_are_read_only() {
        for cmd in [
            "grep -rn foo src | head -20",
            "git log --oneline -5 | cat",
            "ls -la && git status",
            "cat a; wc -l b",
            "rg foo src 2>&1 | head",
            "ps aux | grep ahma",
            "pgrep -fl sccache",
            "sed -n 1,20p src/lib.rs",
            "sed -n '/fn main/,/^}/p' src/main.rs",
            "awk '{print $1}' data.txt",
            "gh pr checks 82",
            "gh pr view 82 --json state",
            "gh run list --limit 5",
            "gh api repos/o/r/pulls/1",
            "curl -s https://example.com/health",
            "echo $HOME",
            "ls $TMPDIR",
            "cd src && ls",
            "cd src && grep -n foo lib.rs | head -5",
            "cat Cargo.toml > /dev/null",
            "git status 2>/dev/null",
            "printenv PATH",
            "ahma sandbox list",
            "ahma queue",
            "ahma ps sccache",
            "ahma doctor",
            "ahma --version",
            "cargo --version",
            "jq .name package.json",
            "lsof -i :8080",
        ] {
            assert_eq!(lane(cmd), Lane::ReadOnly, "{cmd}");
        }
    }

    #[test]
    fn pipelines_with_a_writer_or_an_unknown_stay_exclusive() {
        for cmd in [
            "ls | tee out",
            "cat a > b",
            "cat a >> b",
            "sed -i s/a/b/ f | cat",
            "sed --in-place s/a/b/ f",
            "awk '{print > \"f\"}' data.txt",
            "echo $(rm x)",
            "ls && make",
            "cd src && cargo build",
            "git status; rm x",
            "curl -o f https://x",
            "curl -O https://x/f",
            "curl --output f https://x",
            "wget https://x",
            "gh pr merge 1",
            "gh pr create",
            "gh api -X POST repos/o/r/issues",
            "gh api --method DELETE x",
            "ahma doctor --fix",
            "ahma sandbox grant /x",
            "cargo build | head",
            "ps aux | xargs kill",
            "ls | sh",
            "cat <<EOF",
            "echo hi \\",
            "grep foo a || touch b",
            "ls > out | cat",
        ] {
            assert_eq!(lane(cmd), Lane::Exclusive, "{cmd}");
        }
    }
}

#[cfg(test)]
mod source_effect_tests {
    use super::*;
    use crate::adapter::workspace_queue::SourceEffect;

    fn effect(cmd: &str) -> SourceEffect {
        classify_source_effect(cmd, &[])
    }

    /// SPEC R2.7.8: builds, tests and linters read sources; an edit made while
    /// one runs is reported by the drift report (R2.7.6), not refused.
    #[test]
    fn builds_tests_and_linters_read_sources() {
        for cmd in [
            "cargo nextest run",
            "cargo nextest run --no-fail-fast -E 'test(foo)'",
            "cargo test -p stat3",
            "cargo build --release",
            "cargo check --all-targets",
            "cargo clippy --all-targets",
            "cargo fmt --check",
            "RUST_LOG=debug cargo test",
            "cd rust && cargo nextest run 2>&1 | tail -60",
            "bash -c 'cd rust && cargo nextest run'",
            "npm test",
            "pnpm test",
            "yarn test",
            "go test ./...",
            "go vet ./...",
            "pytest -q",
            "python -m pytest tests",
            "./gradlew test",
            "./gradlew :app:assembleDebug",
            "mvn -q test",
            "swift test",
            "xcodebuild test -scheme App",
            "tsc --noEmit",
            "eslint src",
            "git status",
            "git commit -m wip",
            "git push",
        ] {
            assert_eq!(effect(cmd), SourceEffect::ReadsSources, "{cmd}");
        }
    }

    /// Commands that rewrite source files: an edit racing one is lost or
    /// clobbered, so it is still refused.
    #[test]
    fn formatters_codemods_and_checkouts_rewrite_sources() {
        for cmd in [
            "cargo fmt",
            "cargo fmt --all",
            "cargo clippy --fix --allow-dirty",
            "cargo fix",
            "cargo update",
            "cargo add serde",
            "git checkout main",
            "git switch feature",
            "git stash",
            "git rebase origin/main",
            "git pull",
            "git restore src/lib.rs",
            "sed -i s/a/b/ src/lib.rs",
            "prettier --write .",
            "black .",
            "ruff format .",
            "go fmt ./...",
            "npm install",
            "rm -rf src/gen",
            "mv a.rs b.rs",
            "cargo build && cargo fmt",
        ] {
            assert_eq!(effect(cmd), SourceEffect::RewritesSources, "{cmd}");
        }
    }

    /// Anything the classifier cannot read with certainty stays unknown, which
    /// the guard treats exactly as it treated every writer before.
    #[test]
    fn scripts_and_unparsed_lines_are_unknown() {
        for cmd in [
            "./build.sh",
            "scripts/heavy bash -c 'cargo test'",
            "make",
            "npm run gen",
            "cargo run --bin codegen",
            "cargo test > out.log",
            "",
        ] {
            assert_eq!(effect(cmd), SourceEffect::Unknown, "{cmd}");
        }
    }

    /// A project declares its own wrappers (`[tools] source_readers`): a
    /// command line that starts with a declared prefix reads sources.
    #[test]
    fn a_declared_wrapper_reads_sources() {
        let declared = vec!["scripts/heavy".to_string()];
        assert_eq!(
            classify_source_effect(
                "scripts/heavy bash -c 'rust/gen.sh && cd rust && cargo nextest run'",
                &declared
            ),
            SourceEffect::ReadsSources
        );
        assert_eq!(
            classify_source_effect("scripts/heavyweight", &declared),
            SourceEffect::Unknown,
            "a prefix matches whole words, not a longer name"
        );
    }
}

#[cfg(test)]
mod poll_loop_tests {
    use super::*;

    /// A poll — `sleep`, an environment assignment for a reader, a loop whose
    /// every command reads — is a reader. Classified as a writer, a CI poll
    /// held the workspace's write lease for forty minutes and every later
    /// command queued behind it. The kernel's read-only lane still enforces
    /// it: a misclassified poll fails, it cannot write.
    /// The diagnostics an agent runs to see why a long job is slow must not
    /// queue behind that job (bug report from an agent in another repo:
    /// "ahma queued my uptime diagnostic behind the very job I was trying to
    /// inspect").
    #[test]
    fn diagnostics_of_a_running_job_never_wait_for_it() {
        let queued = [
            "uptime; sysctl -n hw.ncpu; ls -la ~/.cache/neubit/; cat ~/.cache/neubit/heavy.lock.holder; date",
            "rg -n 'lintAnalyze' build 2>/dev/null",
            "for f in a.log b.log; do grep -c ERROR $f; done",
            "S=/tmp/x; grep -n foo $S",
            "which timeout gtimeout",
            "git diff --numstat",
            "vm_stat",
            "top -l 1 -n 10",
            "lsof -p 123",
            "pgrep -fl gradle",
            // From an agent in another repository (clownbot): globs, a shell
            // variable and paths outside the workspace are all still reads.
            "ls ~/.cargo/registry/src/*/ | grep dioxus; grep -n Event $T/src/event.rs",
            "grep -n with_menu ~/.cargo/registry/src/index.crates.io-1949/dioxus-desktop-0.7.10/src/*.rs",
            "strings target/debug/app | grep -c ahma",
        ]
        .into_iter()
        .filter(|c| classify_shell_command(c) != Lane::ReadOnly)
        .collect::<Vec<_>>();
        assert!(
            queued.is_empty(),
            "still queued behind writers: {queued:#?}"
        );
        // Still exclusive: these write.
        for command in [
            "sysctl -w kern.x=1",
            "top -l 1 > top.txt",
            "for f in *; do rm $f; done",
        ] {
            assert_eq!(
                classify_shell_command(command),
                Lane::Exclusive,
                "{command}"
            );
        }
    }

    /// A `$(…)` substitution is classified by what it runs: a reader inside a
    /// reader is a reader. A CI watch that printed `$(gh pr checks …)` held the
    /// workspace's write lease for half an hour and queued every later command.
    #[test]
    fn substitutions_are_judged_by_what_they_run() {
        for cmd in [
            "echo \"== $p: $(gh pr checks 87 --json name)\"",
            "for p in 1 2; do gh pr checks $p --watch; echo \"$(gh pr view $p)\"; done",
            "echo $(echo $(ls))",
            "N=$(git rev-parse HEAD); git log -1 $N",
        ] {
            assert_eq!(classify_shell_command(cmd), Lane::ReadOnly, "{cmd}");
        }
        for cmd in [
            "echo $(rm -rf x)",
            "X=$(cargo build); echo $X",
            "echo \"$(touch f)\"",
            "echo $(ls",
            "echo `ls`",
        ] {
            assert_eq!(classify_shell_command(cmd), Lane::Exclusive, "{cmd}");
        }
    }

    /// `!` negates the command after it and changes nothing on disk, so a
    /// poll written `until ! cmd` is a reader exactly when `cmd` is. Treated as
    /// an unknown operator, a CI watch held the workspace for half an hour.
    #[test]
    fn a_negated_command_is_judged_by_the_command() {
        assert_eq!(
            classify_shell_command(
                "until ! gh pr checks 87 --json state | grep -q PENDING; do sleep 60; done"
            ),
            Lane::ReadOnly
        );
        assert_eq!(classify_shell_command("! grep -q x f.txt"), Lane::ReadOnly);
        assert_eq!(classify_shell_command("! rm -f x"), Lane::Exclusive);
        assert_eq!(
            classify_shell_command("echo hi!"),
            Lane::Exclusive,
            "a bare ! inside a word is not modelled"
        );
    }

    #[test]
    fn polls_and_waits_are_read_only() {
        for cmd in [
            "sleep 60",
            "XDG_CACHE_HOME=/tmp/x gh pr checks 87",
            "until gh pr checks 87 | grep -q pass; do sleep 60; done",
            "while pgrep -f cargo; do sleep 5; done",
            "sleep 30; gh run list --limit 5",
        ] {
            assert_eq!(classify_shell_command(cmd), Lane::ReadOnly, "{cmd}");
        }
    }

    #[test]
    fn a_loop_or_assignment_around_a_writer_stays_exclusive() {
        for cmd in [
            "until cargo build; do sleep 5; done",
            "while true; do rm -f x; done",
            "FOO=1 cargo build",
            "LD_PRELOAD=/x.so ls; touch y",
        ] {
            assert_eq!(classify_shell_command(cmd), Lane::Exclusive, "{cmd}");
        }
    }
}

/// SPEC R2.7.4: CI watchers and redirections outside every workspace. A
/// `gh pr checks 113 --watch --interval 60 > /tmp/…/ci.log` was classified
/// exclusive for its redirection, held the workspace for a twenty-minute CI
/// run, and every later possibly-writing command queued behind it.
#[cfg(test)]
mod watch_and_redirect_tests {
    use super::*;

    /// The shell expands `~` only at the start of a word, so a tilde inside a
    /// path is literal: Windows' 8.3 short names (`C:\Users\RUNNER~1\...`,
    /// which is what %TEMP% often is) must not make an outside target ambiguous.
    #[test]
    fn a_tilde_inside_a_path_is_literal_and_only_a_leading_one_expands() {
        assert_eq!(
            at("gh pr checks 113 --watch > /scratch/RUNNER~1/ci.log", true),
            Lane::Service
        );
        assert_eq!(
            at("gh pr checks 113 --watch > ~/ci.log", true),
            Lane::Exclusive
        );
    }

    const WS: &str = "/ws/repo";

    /// Classify at a fake site: the workspace is `/ws/repo`, judged lexically.
    fn at(cmd: &str, read_only_enforced: bool) -> Lane {
        let inside = |p: &Path| p.starts_with(WS);
        classify_shell_command_at(
            cmd,
            &RunSite {
                cwd: Path::new(WS),
                read_only_enforced,
                writes_workspace: &inside,
            },
        )
    }

    #[test]
    fn gh_commands_that_only_read_remote_state_are_read_only() {
        for cmd in [
            "gh pr view 113",
            "gh pr view 113 --json state,mergeable",
            "gh pr checks 113",
            "gh pr list --state open",
            "gh run view 42 --log-failed",
            "gh run list --limit 5",
            "gh run watch 42",
            "gh api repos/o/r/pulls/1",
            "gh api -X GET repos/o/r/pulls",
            "gh api --method=get repos/o/r/pulls",
            "gh api repos/o/r/pulls --paginate --jq '.[].number'",
        ] {
            assert_eq!(classify_shell_command(cmd), Lane::ReadOnly, "{cmd}");
            assert_eq!(at(cmd, true), Lane::ReadOnly, "{cmd}");
        }
    }

    #[test]
    fn gh_commands_that_change_anything_stay_exclusive() {
        for cmd in [
            "gh pr merge 113 --squash",
            "gh pr create --fill",
            "gh pr checkout 113",
            "gh run rerun 42",
            "gh run download 42",
            "gh api -X POST repos/o/r/issues",
            "gh api -XPOST repos/o/r/issues",
            "gh api --method=PATCH repos/o/r/issues/1",
            "gh api --method DELETE x",
            "gh api -X",
            "gh api repos/o/r/issues -f title=x",
            "gh api repos/o/r/issues -Ftitle=x",
            "gh api repos/o/r/issues --field title=x",
            "gh api repos/o/r/issues --raw-field title=x",
            "gh api repos/o/r/issues --input body.json",
        ] {
            for enforced in [true, false] {
                assert_eq!(at(cmd, enforced), Lane::Exclusive, "{cmd}");
            }
            // Not even with the output sent outside the workspace.
            assert_eq!(at(&format!("{cmd} > /tmp/out.log"), true), Lane::Exclusive);
        }
    }

    /// Where the kernel has a read-only lane a watcher already takes no lease
    /// there; where it has none, a watcher is service rather than holding the
    /// workspace for the whole run.
    #[test]
    fn watchers_never_hold_the_workspace() {
        for cmd in [
            "gh run watch 42",
            "gh run watch 42 --exit-status",
            "gh pr checks 113 --watch",
            "gh pr checks 113 --watch --interval 60",
            "gh pr checks --watch --fail-fast 113",
            "gh pr checks 113 --watch 2>&1 | tail -20",
            "tail -f build.log",
            "tail -n 50 -F build.log | grep --line-buffered ERROR",
        ] {
            assert_eq!(at(cmd, true), Lane::ReadOnly, "{cmd}");
            assert_eq!(at(cmd, false), Lane::Service, "{cmd}");
        }
        // A finite reader is not a watcher: without the kernel lane it stays
        // exclusive, exactly as before.
        for cmd in [
            "gh pr checks 113",
            "gh run view 42 --log-failed",
            "tail -n 5 f",
        ] {
            assert_eq!(at(cmd, false), Lane::Exclusive, "{cmd}");
        }
        // A watcher beside something that could write through its arguments,
        // or under an environment assignment, gets no service verdict.
        for cmd in [
            "tail -f build.log | sed -n /ERROR/p",
            "gh run watch 42; sort -o out.txt in.txt",
            "LD_PRELOAD=/x.so gh run watch 42",
            "gh run watch 42 && cargo build",
        ] {
            assert_eq!(at(cmd, false), Lane::Exclusive, "{cmd}");
        }
    }

    /// The observed failure, and every redirection spelling of it: a file
    /// outside every workspace is not a workspace write. The read-only lane
    /// would refuse to open it, so the line is service — on every platform.
    #[test]
    fn output_sent_outside_every_workspace_takes_no_lease() {
        for cmd in [
            "gh pr checks 113 --watch --interval 60 > /private/tmp/scratch/ci.log",
            "gh pr checks 113 --watch --interval 60 >/private/tmp/scratch/ci.log",
            "gh pr checks 113 --watch >> /tmp/ci.log",
            "gh pr checks 113 --watch > /tmp/ci.log 2>&1",
            "gh pr checks 113 --watch 2> /tmp/ci.err",
            "gh pr checks 113 --watch 2>>/tmp/ci.err",
            "gh pr checks 113 --watch &> /tmp/ci.log",
            "gh pr checks 113 --watch &>>/tmp/ci.log",
            "gh pr checks 113 --watch >| /tmp/ci.log",
            "gh pr checks 113 --watch > '/tmp/with space/ci.log'",
            "gh pr checks 113 --watch | tee /tmp/ci.log",
            "gh pr checks 113 --watch 2>&1 | tee -a /tmp/ci.log",
            "git log --oneline -20 > /tmp/log.txt",
            "rg -n TODO src > /tmp/todo.txt",
            "echo \"$(gh pr view 113 --json state)\" > /tmp/state.json",
        ] {
            assert_eq!(at(cmd, true), Lane::Service, "{cmd}");
            assert_eq!(at(cmd, false), Lane::Service, "{cmd}");
            assert_eq!(
                classify_shell_command(cmd),
                Lane::Exclusive,
                "without a site, a file write is exclusive: {cmd}"
            );
        }
    }

    #[test]
    fn output_sent_into_the_workspace_stays_exclusive() {
        for cmd in [
            "gh pr checks 113 --watch --interval 60 > ci.log",
            "gh pr checks 113 --watch > ./target/ci.log",
            "gh pr checks 113 --watch > /ws/repo/ci.log",
            "gh pr checks 113 --watch 2> err.log",
            "gh pr checks 113 --watch &> ci.log",
            "gh pr checks 113 --watch | tee ci.log",
            "gh pr checks 113 --watch | tee /tmp/a.log ci.log",
            "gh pr checks 113 --watch > /tmp/ci.log 2> err.log",
            "git log > /tmp/a.log; git status > status.txt",
            "gh run watch 42 > sub/../ci.log",
        ] {
            for enforced in [true, false] {
                assert_eq!(at(cmd, enforced), Lane::Exclusive, "{cmd}");
            }
        }
    }

    /// A target the shell would expand, a relative one after a `cd`, a
    /// redirection this module does not model, or a writer anywhere in the
    /// line: exclusive, as before.
    #[test]
    fn ambiguous_or_writing_lines_stay_exclusive() {
        for cmd in [
            "gh pr checks 113 --watch > $OUT",
            "gh pr checks 113 --watch > \"$TMPDIR/ci.log\"",
            "gh pr checks 113 --watch > ${TMPDIR}/ci.log",
            "gh pr checks 113 --watch > /tmp/*.log",
            "gh pr checks 113 --watch > /tmp/ci?.log",
            "gh pr checks 113 --watch > ~/ci.log",
            "gh pr checks 113 --watch > >(cat)",
            "gh pr checks 113 --watch >&ci.log",
            "gh pr checks 113 --watch >",
            "gh pr checks 113 --watch | tee --unknown /tmp/x",
            "gh pr checks 113 --watch | tee - ",
            "cd /tmp && gh pr checks 113 --watch > ci.log",
            "gh pr checks 113 --watch > /tmp/ci.log; rm -f x",
            "cargo build > /tmp/build.log 2>&1",
            "sort -o out.txt in.txt > /tmp/x",
            "sed -n 1p f > /tmp/x",
            "env rm -rf x > /tmp/x",
            "XDG_CACHE_HOME=/tmp/c gh pr checks 113 > /tmp/x",
            "echo $(sed 'w f' x) > /tmp/x",
            "echo $(gh pr checks 1 > f) > /tmp/x",
            "curl -s -D headers.txt https://x > /tmp/x",
            "grep '>' /tmp/x",
            "cat < /tmp/in > /tmp/out",
            "gh pr checks 113 --watch 3>&1 1>&2 > /tmp/x &",
        ] {
            for enforced in [true, false] {
                assert_eq!(at(cmd, enforced), Lane::Exclusive, "{cmd}");
            }
        }
    }

    /// `&>/dev/null`, `< /dev/null` and stream merges open no file: the
    /// read-only lane can run them.
    #[test]
    fn redirections_that_open_no_file_keep_a_reader_read_only() {
        for cmd in [
            "gh pr checks 113 &>/dev/null",
            "gh pr checks 113 &> /dev/null",
            "gh pr checks 113 >&2",
            "gh pr checks 113 2>&-",
            "gh pr checks 113 < /dev/null",
            "gh pr checks 113 >> /dev/null 2>&1",
            "gh pr checks 113 | tee",
        ] {
            assert_eq!(classify_shell_command(cmd), Lane::ReadOnly, "{cmd}");
            assert_eq!(at(cmd, true), Lane::ReadOnly, "{cmd}");
        }
    }
}

/// [`writes_inside`] resolves a path the way the kernel will.
#[cfg(test)]
mod writes_inside_tests {
    use super::*;

    fn dirs() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let td = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(td.path()).unwrap();
        let ws = root.join("ws");
        let out = root.join("out");
        std::fs::create_dir_all(ws.join("sub")).unwrap();
        std::fs::create_dir_all(&out).unwrap();
        (td, ws, out)
    }

    #[test]
    fn a_new_file_outside_the_workspace_is_outside() {
        let (_td, ws, out) = dirs();
        assert!(!writes_inside(&out.join("ci.log"), &ws, &[]));
        assert!(!writes_inside(&out.join("new/dir/ci.log"), &ws, &[]));
    }

    #[test]
    fn a_file_in_the_workspace_or_any_scope_is_inside() {
        let (_td, ws, out) = dirs();
        assert!(writes_inside(&ws.join("ci.log"), &ws, &[]));
        assert!(writes_inside(&ws.join("sub/new/ci.log"), &ws, &[]));
        assert!(writes_inside(&out.join("../ws/ci.log"), &ws, &[]));
        assert!(
            writes_inside(&out.join("ci.log"), &ws, std::slice::from_ref(&out)),
            "another workspace the session can write is a workspace too"
        );
        let shouted = PathBuf::from(ws.to_string_lossy().to_uppercase()).join("ci.log");
        assert!(
            writes_inside(&shouted, &ws, &[]),
            "case is ignored, which only ever says inside more often"
        );
    }

    #[test]
    fn what_cannot_be_resolved_is_inside() {
        let (_td, ws, out) = dirs();
        assert!(writes_inside(
            &out.join("missing/../../ws/ci.log"),
            &ws,
            &[]
        ));
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_into_the_workspace_is_the_workspace() {
        let (_td, ws, out) = dirs();
        std::os::unix::fs::symlink(&ws, out.join("link")).unwrap();
        assert!(writes_inside(&out.join("link/ci.log"), &ws, &[]));
        std::os::unix::fs::symlink(ws.join("not-yet.log"), out.join("dangling")).unwrap();
        assert!(
            writes_inside(&out.join("dangling"), &ws, &[]),
            "writing a dangling symlink creates its target"
        );
    }
}
