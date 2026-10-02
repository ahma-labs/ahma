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

use super::workspace_queue::Lane;

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
/// history expansion, heredocs. Pipes, lists and the few harmless
/// redirections are parsed explicitly below.
const SHELL_OPERATORS: &[char] = &['`', '(', ')', '{', '}', '\\', '\n', '\r', '#', '!'];

/// Redirections that cannot write a file: merging or discarding streams.
const HARMLESS_REDIRECTIONS: &[&str] = &[
    "2>&1",
    "1>&2",
    ">/dev/null",
    "2>/dev/null",
    "&>/dev/null",
    "</dev/null",
    "> /dev/null",
    "2> /dev/null",
];

/// A simple command: the words of one pipeline segment.
type Segment = Vec<String>;

/// Split a command line into pipeline/list segments of words, honouring plain
/// single and double quotes. `None` for anything with a shell operator this
/// module does not model, an unbalanced quote, a writing redirection, or a
/// substitution — the caller treats that as "cannot tell", i.e. exclusive.
fn segments(command: &str) -> Option<Vec<Segment>> {
    if command.contains("<<") {
        return None;
    }
    // Tokenize: words, quoted strings, and the operators `|`, `||`, `&&`, `;`.
    // Any other `&` (background, `>&2` outside the harmless list) is unknown.
    let mut segs: Vec<Segment> = vec![Vec::new()];
    let mut cur = String::new();
    let mut in_word = false;
    let mut quote: Option<char> = None;
    let chars: Vec<char> = command.chars().collect();
    let mut i = 0;
    let flush = |cur: &mut String, in_word: &mut bool, segs: &mut Vec<Segment>| {
        if *in_word {
            segs.last_mut().unwrap().push(std::mem::take(cur));
            *in_word = false;
        }
    };
    while i < chars.len() {
        let c = chars[i];
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => cur.push(c),
            None => match c {
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
                '|' | ';' | '&' => {
                    let next = chars.get(i + 1).copied();
                    let op_len = match (c, next) {
                        ('|', Some('|')) | ('&', Some('&')) => 2,
                        ('|', _) | (';', _) => 1,
                        // A bare `&`: background or a redirection we do not model.
                        ('&', _) => return None,
                        _ => 1,
                    };
                    flush(&mut cur, &mut in_word, &mut segs);
                    if segs.last().is_some_and(Vec::is_empty) {
                        return None; // `| cmd`, `cmd ||  | cmd`: not a command line we read
                    }
                    segs.push(Vec::new());
                    i += op_len;
                    continue;
                }
                c if c.is_whitespace() => flush(&mut cur, &mut in_word, &mut segs),
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
    flush(&mut cur, &mut in_word, &mut segs);
    if segs.last().is_some_and(Vec::is_empty) {
        return None; // trailing operator
    }
    // Redirections: drop the harmless ones, refuse anything else with `<`/`>`.
    for seg in &mut segs {
        let mut cleaned = Vec::new();
        let mut skip_next = false;
        for (idx, w) in seg.iter().enumerate() {
            if skip_next {
                skip_next = false;
                continue;
            }
            if HARMLESS_REDIRECTIONS.contains(&w.as_str()) {
                continue;
            }
            // `> /dev/null` and `2> /dev/null` as two words.
            if (w == ">" || w == "2>") && seg.get(idx + 1).map(String::as_str) == Some("/dev/null")
            {
                skip_next = true;
                continue;
            }
            if w.contains('<') || w.contains('>') {
                return None;
            }
            // `$(…)` is a substitution; a plain `$VAR` reference only reads the
            // environment and is left to the shell.
            if w.contains("$(") {
                return None;
            }
            cleaned.push(w.clone());
        }
        if cleaned.is_empty() {
            return None;
        }
        *seg = cleaned;
    }
    Some(segs)
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
        "gh" => {
            let mut it = args.iter().map(String::as_str);
            match (it.next(), it.next()) {
                (Some("pr"), Some("view" | "checks" | "list" | "status" | "diff")) => true,
                (Some("run"), Some("view" | "list")) => true,
                (Some("issue"), Some("view" | "list")) => true,
                (Some("repo"), Some("view")) => true,
                (Some("api"), Some(_)) => {
                    !has("-X")
                        && !has("--method")
                        && !has("-f")
                        && !has("-F")
                        && !has("--input")
                        && !starts("--method=")
                }
                (Some("--version"), _) | (Some("auth"), Some("status")) => true,
                _ => false,
            }
        }
        // `cargo`/`rustc`/`rustup` only to say what they are.
        "cargo" | "rustc" | "rustup" => args.first().map(String::as_str) == Some("--version"),
        // ahma's own read-only commands, so a queued agent can still ask who is
        // holding the workspace (SPEC R2.7.9).
        "ahma" => {
            let mut it = args.iter().map(String::as_str);
            match (it.next(), it.next()) {
                (Some("queue"), None) | (Some("--version"), _) => true,
                (Some("sandbox" | "permissions" | "network"), Some("list")) => true,
                (Some("hooks"), Some("status")) => true,
                (Some("doctor"), _) => !has("--fix"),
                _ => false,
            }
        }
        p => READ_ONLY_PROGRAMS.contains(&p),
    }
}

/// The lane for a `run_terminal_command` command line.
pub fn classify_shell_command(command: &str) -> Lane {
    let Some(segs) = segments(command) else {
        return Lane::Exclusive;
    };
    for seg in &segs {
        let Some((program, args)) = seg.split_first() else {
            return Lane::Exclusive;
        };
        // A leading `VAR=value` assignment or a path to a program: not worth guessing.
        if program.contains('=') || program.contains('/') {
            return Lane::Exclusive;
        }
        // `cd <dir>` changes nothing on disk; the segment after it decides.
        if program == "cd" && args.len() <= 1 {
            continue;
        }
        if !program_is_read_only(program, args) {
            return Lane::Exclusive;
        }
    }
    Lane::ReadOnly
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
            "FOO=1 ls",
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
