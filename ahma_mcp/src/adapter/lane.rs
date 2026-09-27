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

/// Characters that make a command line mean more than "run one program with
/// these words": pipes, lists, redirections, substitutions, escapes, comments.
const SHELL_OPERATORS: &[char] = &[
    ';', '&', '|', '<', '>', '$', '`', '(', ')', '{', '}', '\\', '\n', '\r', '#', '!',
];

/// Split a command line into words, honouring plain single and double quotes.
/// `None` for anything with a shell operator or an unbalanced quote — the
/// caller treats that as "cannot tell", i.e. exclusive.
fn words(command: &str) -> Option<Vec<String>> {
    if command.chars().any(|c| SHELL_OPERATORS.contains(&c)) {
        return None;
    }
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut quote: Option<char> = None;
    for c in command.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => cur.push(c),
            (None, '\'' | '"') => {
                quote = Some(c);
                in_word = true;
            }
            (None, c) if c.is_whitespace() => {
                if in_word {
                    out.push(std::mem::take(&mut cur));
                    in_word = false;
                }
            }
            (None, c) => {
                cur.push(c);
                in_word = true;
            }
        }
    }
    if quote.is_some() {
        return None;
    }
    if in_word {
        out.push(cur);
    }
    Some(out)
}

/// The lane for a `run_terminal_command` command line.
pub fn classify_shell_command(command: &str) -> Lane {
    let Some(words) = words(command) else {
        return Lane::Exclusive;
    };
    let Some((program, args)) = words.split_first() else {
        return Lane::Exclusive;
    };
    // A leading `VAR=value` assignment or a path to a program: not worth guessing.
    if program.contains('=') || program.contains('/') {
        return Lane::Exclusive;
    }
    let read_only = match program.as_str() {
        "git" => git_is_read_only(args),
        // `tail -f` never ends; it is read-only but belongs in no queue anyway,
        // and in the exclusive lane it would at least be visible as the holder.
        "tail" => !args.iter().any(|a| {
            a == "-f" || a == "-F" || a.starts_with("--follow") || a.starts_with("--retry")
        }),
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
        "rg" => !args.iter().any(|a| a.starts_with("--pre")),
        p => READ_ONLY_PROGRAMS.contains(&p),
    };
    if read_only {
        Lane::ReadOnly
    } else {
        Lane::Exclusive
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
            "tail -f log",
            "tail --follow=name log",
        ] {
            assert_eq!(lane(cmd), Lane::Exclusive, "{cmd}");
        }
    }

    #[test]
    fn unbalanced_quotes_are_exclusive() {
        assert_eq!(lane("rg 'foo"), Lane::Exclusive);
    }
}
