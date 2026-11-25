//! Shell command lines read as words: the one reader behind every surface that
//! asks what a shell line runs.
//!
//! Three readers, one per contract, side by side so they cannot drift apart:
//!
//! | Reader | Contract | Used for |
//! |---|---|---|
//! | [`split_strict`] | **Fails closed**: refuses ([`Refusal`]) anything it does not model — substitutions, grouping, escapes, comments, history expansion, heredocs, a bare `&`. | The workspace-queue lane (SPEC R2.7.4), where "cannot tell" must mean exclusive, and the edit guard's source effect (R2.7.8). |
//! | [`segments`] | **Best effort**: never fails. Quotes, escapes, comments, heredocs (bodies skipped), `$(…)` and backticks (their commands become segments of their own). | Naming what a line runs: refusal matching (R-PERM.10.1), `sudo` detection and rewriting (R-ESCAPE.4), the folders a line enters (R-PERM.11), the soft hold (R2.7.11). |
//! | [`split_literal`] | Best effort, **no escapes**: a backslash is a character. | The folders a line names on Windows, where `\` separates paths (R-PERM.11). |
//!
//! Where [`split_strict`] accepts a line, [`segments`] reads the same words,
//! except that a backslash inside double quotes is kept as written by the
//! strict reader (`"\$x"` is `\$x` there, `$x` here). The tests pin that.
//!
//! Above the readers, one vocabulary for the words that frame a command
//! rather than being it: [`KEYWORDS`], [`is_assignment`], [`is_framing_only`];
//! and two readings of wrappers, deliberately different:
//! [`harmless_wrapper_len`] (strict: wrappers that write nothing themselves,
//! never `nohup`, used to decide a lane) and [`command_chain`] (lenient: every
//! word in [`RUNS_THE_NEXT`], used to name the program). Every harmless
//! wrapper is also in [`RUNS_THE_NEXT`]; the tests pin that too.

use std::ops::Range;

// ---------------------------------------------------------------------------
// Framing words
// ---------------------------------------------------------------------------

/// Shell keywords that open or close a loop or a conditional around the
/// commands of a list, and `!`, which negates the command after it:
/// `until gh pr checks 87; do sleep 60; done` runs `gh` and `sleep`.
pub const KEYWORDS: &[&str] = &["until", "while", "if", "elif", "then", "else", "do", "!"];

/// Whether `word` is a `NAME=value` environment assignment: a name of ASCII
/// letters, digits and `_`, not starting with a digit, then `=`. Anything
/// else with an `=` in it (`a.b=c`, `1X=y`, `--opt=v`) is a word the shell
/// runs or passes on, never an assignment — so it is never skipped as one.
pub fn is_assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        !name.is_empty()
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            && !name.starts_with(|c: char| c.is_ascii_digit())
    })
}

/// A word that only frames the command after it: a [`KEYWORDS`] entry or an
/// assignment. `if cd /repo; then …` and `X=1 cd /repo` move in all the same.
pub fn is_frame(word: &str) -> bool {
    KEYWORDS.contains(&word) || is_assignment(word)
}

/// [`is_frame`], or a brace that groups commands (`{ cargo build; }`).
/// [`split_strict`] refuses an unquoted brace, so only the lenient readers
/// meet one.
pub fn is_frame_or_group(word: &str) -> bool {
    is_frame(word) || word == "{" || word == "}"
}

/// `for NAME` or `for NAME in …`: a loop header that only names what the
/// body reads; the body decides.
pub fn is_for_header<S: AsRef<str>>(words: &[S]) -> bool {
    match words {
        [first, name, rest @ ..] => {
            first.as_ref() == "for"
                && is_assignment(&format!("{}=x", name.as_ref()))
                && rest.first().is_none_or(|w| w.as_ref() == "in")
        }
        _ => false,
    }
}

/// Whether a segment runs nothing: past its leading [`is_frame`] words it is
/// `done` or `fi` alone, a `for` header, or it was only assignments
/// (`S=/path;` sets a shell variable and nothing else).
pub fn is_framing_only<S: AsRef<str>>(segment: &[S]) -> bool {
    let at = segment.iter().take_while(|w| is_frame(w.as_ref())).count();
    let words = &segment[at..];
    matches!(words, [w] if matches!(w.as_ref(), "done" | "fi"))
        // Keywords and assignments only, with at least one assignment: a
        // loop body's `do out=$(…)` sets a variable and runs nothing. A bare
        // keyword (`then`) still frames whatever the next segment runs.
        || (words.is_empty() && segment.iter().any(|w| is_assignment(w.as_ref())))
        || is_for_header(words)
}

// ---------------------------------------------------------------------------
// Wrappers
// ---------------------------------------------------------------------------

fn is_duration(w: &str) -> bool {
    let digits = w.trim_end_matches(['s', 'm', 'h', 'd']);
    !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit() || c == '.')
}

/// How many leading words are wrappers that only change how the command after
/// them runs and write nothing themselves: `timeout`, `time`, `nice`,
/// `caffeinate` and `env NAME=value…`. Parsed strictly, so a wrapper option
/// this does not know leaves the wrapper in place (and the line as unknown as
/// it was). Never `nohup`: it writes `nohup.out` into the working directory.
/// A wrapper with nothing after it runs nothing to judge: 0.
pub fn harmless_wrapper_len<S: AsRef<str>>(words: &[S]) -> usize {
    let mut at = 0;
    while let Some(n) = one_harmless_wrapper_len(&words[at..]) {
        at += n;
    }
    if at >= words.len() { 0 } else { at }
}

fn one_harmless_wrapper_len<S: AsRef<str>>(words: &[S]) -> Option<usize> {
    let word = |i: usize| words.get(i).map(AsRef::as_ref);
    match word(0)? {
        "time" => Some(if word(1) == Some("-p") { 2 } else { 1 }),
        "nice" => match word(1) {
            Some("-n") if word(2).is_some_and(|n| n.parse::<i32>().is_ok()) => Some(3),
            Some(w) if w.starts_with('-') => w[1..].parse::<i32>().ok().map(|_| 2),
            _ => Some(1),
        },
        "caffeinate" => {
            let flags = words[1..]
                .iter()
                .take_while(|w| {
                    let w = w.as_ref();
                    w.len() > 1 && w.starts_with('-') && w[1..].chars().all(|c| "dimsu".contains(c))
                })
                .count();
            match word(1 + flags) {
                Some(w) if w.starts_with('-') => None,
                _ => Some(1 + flags),
            }
        }
        "env" => {
            let names = words[1..]
                .iter()
                .take_while(|w| is_assignment(w.as_ref()))
                .count();
            match word(1 + names) {
                Some(w) if w.starts_with('-') => None,
                _ => Some(1 + names),
            }
        }
        "timeout" => {
            let mut i = 1;
            loop {
                match word(i)? {
                    "-k" | "-s" | "--kill-after" | "--signal" => i += 2,
                    "--foreground" | "--preserve-status" => i += 1,
                    w if w.starts_with("--kill-after=") || w.starts_with("--signal=") => i += 1,
                    w if is_duration(w) => return Some(i + 1),
                    _ => return None,
                }
            }
        }
        _ => None,
    }
}

/// Words that run the program named after them, for naming that program
/// (never for deciding a lane: see [`harmless_wrapper_len`]). `sudo` is one:
/// [`command_chain`] lists it among the wrappers, so a caller asking "does
/// this run `sudo`" looks there as well as at the program.
pub const RUNS_THE_NEXT: &[&str] = &[
    "env",
    "time",
    "nice",
    "nohup",
    "exec",
    "command",
    "sudo",
    "timeout",
    "caffeinate",
];

/// Options of a [`RUNS_THE_NEXT`] wrapper that take the next word as their
/// value, so that value is not mistaken for the program (`sudo -u root make`
/// runs `make`).
fn takes_value(wrapper: &str, option: &str) -> bool {
    let opts: &[&str] = match wrapper {
        "sudo" => &[
            "-u",
            "-g",
            "-p",
            "-C",
            "-D",
            "-R",
            "-T",
            "-U",
            "-r",
            "-t",
            "--user",
            "--group",
            "--prompt",
            "--chdir",
            "--chroot",
            "--close-from",
            "--command-timeout",
            "--other-user",
            "--role",
            "--type",
        ],
        "env" => &["-u", "-C", "--unset", "--chdir"],
        "nice" => &["-n", "--adjustment"],
        "exec" => &["-a"],
        "caffeinate" => &["-t", "-w"],
        "timeout" => &["-k", "-s", "--kill-after", "--signal"],
        _ => &[],
    };
    opts.contains(&option)
}

/// How many words one [`RUNS_THE_NEXT`] wrapper at the head of `words` spans,
/// with its options and their values, and `NAME=value` arguments (`env`,
/// `sudo`); for `timeout`, its duration too. `None` when `words` does not
/// start with one.
fn naming_wrapper_len<S: AsRef<str>>(words: &[S]) -> Option<usize> {
    let word = |i: usize| words.get(i).map(AsRef::as_ref);
    let name = word(0).filter(|w| RUNS_THE_NEXT.contains(w))?;
    let mut i = 1;
    while let Some(w) = word(i) {
        if takes_value(name, w) {
            i += 2;
        } else if w.starts_with('-') || is_assignment(w) {
            i += 1;
        } else {
            break;
        }
    }
    if name == "timeout" && word(i).is_some_and(is_duration) {
        i += 1;
    }
    Some(i)
}

/// What one segment runs: the indices of the wrappers on the way, in order,
/// and of the program, if any. Leading [`is_frame_or_group`] words are
/// skipped, a segment that [`is_framing_only`] runs nothing, and a word that
/// starts with `-` is never the program.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommandChain {
    pub wrappers: Vec<usize>,
    pub program: Option<usize>,
}

/// The [`CommandChain`] of one segment's words. Lenient: it names the
/// program for a question or a match, and never decides a lane.
pub fn command_chain<S: AsRef<str>>(segment: &[S]) -> CommandChain {
    let mut chain = CommandChain::default();
    if is_framing_only(segment) {
        return chain;
    }
    let mut i = segment
        .iter()
        .take_while(|w| is_frame_or_group(w.as_ref()))
        .count();
    while let Some(w) = segment.get(i).map(AsRef::as_ref) {
        if w.starts_with('-') || is_assignment(w) {
            i += 1;
            continue;
        }
        match naming_wrapper_len(&segment[i..]) {
            Some(n) => {
                chain.wrappers.push(i);
                i += n;
            }
            None => {
                chain.program = Some(i);
                break;
            }
        }
    }
    chain
}

/// The index of the program one segment runs ([`command_chain`]).
pub fn program_index<S: AsRef<str>>(segment: &[S]) -> Option<usize> {
    command_chain(segment).program
}

// ---------------------------------------------------------------------------
// The lenient reader
// ---------------------------------------------------------------------------

/// One word of a segment, as [`segments`] reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Word {
    /// Quotes removed and escapes applied; a `$(…)` or backtick substitution
    /// is kept as written, so it still reads as something the shell expands.
    pub text: String,
    /// Where the word is in the line, in bytes, quotes included: a rewrite
    /// can change one word and keep every other byte.
    pub span: Range<usize>,
    /// Some part of it was quoted or escaped. `"sudo"` is `sudo` to the shell,
    /// but `'X=1'` is a program name, not an assignment: [`command_chain`]
    /// does not yet tell them apart (it sees text only), a caller can.
    pub quoted: bool,
}

/// The words of each command a shell line runs, split at unquoted `&`, `|`,
/// `;`, newlines and parentheses. Quotes are removed and keep their contents
/// one word (`"/Applications/Google Chrome.app/…"`); a backslash outside
/// quotes takes the next character literally, and a backslash-newline joins
/// lines. Heredoc bodies and comments are dropped, since they run nothing; a
/// `<<<` here-string is a word. A `$(…)` or backtick substitution, bare or
/// inside double quotes, runs a command of its own: its segments follow the
/// segment it appears in. Redirection words stay whole (`2>&1`, `&>log`,
/// `>|f`). Never fails: an unbalanced quote or substitution runs to the end.
pub fn segments(command: &str) -> Vec<Vec<Word>> {
    let mut s = Splitter::new(command);
    s.run(Close::End);
    s.segments.retain(|seg| !seg.is_empty());
    s.segments
}

/// [`segments`], each word's text only.
pub fn segment_texts(command: &str) -> Vec<Vec<String>> {
    segments(command)
        .into_iter()
        .map(|seg| seg.into_iter().map(|w| w.text).collect())
        .collect()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Close {
    End,
    Paren,
    Backtick,
}

/// One nesting level's state: its open segment and word.
struct Level {
    seg: usize,
    word: Option<Word>,
    /// Open `(` inside a `$(…)`, so a subshell's `)` does not close it.
    depth: usize,
}

struct Splitter {
    chars: Vec<char>,
    /// Byte offset of each char, and the line's length after the last.
    offsets: Vec<usize>,
    i: usize,
    segments: Vec<Vec<Word>>,
    /// Heredocs announced on the current line: (closing word, strip tabs).
    heredocs: Vec<(String, bool)>,
}

impl Splitter {
    fn new(command: &str) -> Self {
        Self {
            chars: command.chars().collect(),
            offsets: command
                .char_indices()
                .map(|(b, _)| b)
                .chain(std::iter::once(command.len()))
                .collect(),
            i: 0,
            segments: Vec::new(),
            heredocs: Vec::new(),
        }
    }

    fn peek(&self, offset: usize) -> Option<char> {
        self.chars.get(self.i + offset).copied()
    }

    fn at(&self, s: &str) -> bool {
        s.chars().enumerate().all(|(k, c)| self.peek(k) == Some(c))
    }

    fn offset(&self, i: usize) -> usize {
        self.offsets[i.min(self.chars.len())]
    }

    fn new_segment(&mut self) -> usize {
        self.segments.push(Vec::new());
        self.segments.len() - 1
    }

    fn word<'l>(&self, lvl: &'l mut Level) -> &'l mut Word {
        let start = self.offset(self.i);
        lvl.word.get_or_insert_with(|| Word {
            text: String::new(),
            span: start..start,
            quoted: false,
        })
    }

    fn push(&self, lvl: &mut Level, c: char) {
        self.word(lvl).text.push(c);
    }

    fn end_word(&mut self, lvl: &mut Level) {
        if let Some(mut w) = lvl.word.take() {
            w.span.end = self.offset(self.i);
            self.segments[lvl.seg].push(w);
        }
    }

    /// `&`, `|`, `;`, parentheses and newlines: the next word starts a new command.
    fn end_command(&mut self, lvl: &mut Level) {
        self.end_word(lvl);
        lvl.seg = self.new_segment();
    }

    /// Read until `close` (left on its character) or the end of the line.
    fn run(&mut self, close: Close) {
        let mut lvl = Level {
            seg: self.new_segment(),
            word: None,
            depth: 0,
        };
        let ends_with =
            |lvl: &Level, c: &[char]| lvl.word.as_ref().is_some_and(|w| w.text.ends_with(c));
        while let Some(c) = self.peek(0) {
            match c {
                ')' if close == Close::Paren && lvl.depth == 0 => break,
                '`' if close == Close::Backtick => break,
                '\'' => self.single_quoted(&mut lvl),
                '"' => self.double_quoted(&mut lvl),
                // A line continuation adds nothing, not even an empty word.
                '\\' if self.peek(1) == Some('\n') => self.i += 1,
                '\\' if self.peek(1).is_some() => {
                    self.i += 1;
                    let w = self.word(&mut lvl);
                    w.quoted = true;
                    w.text.push(self.chars[self.i]);
                }
                '#' if lvl.word.is_none() => self.comment(),
                '<' if self.at("<<<") => {
                    for _ in 0..3 {
                        self.push(&mut lvl, '<');
                        self.i += 1;
                    }
                    continue;
                }
                '<' if self.at("<<") => {
                    self.heredoc_operator(&mut lvl);
                    continue; // already past the operator and its delimiter
                }
                '$' if self.peek(1) == Some('(') => self.substitution(&mut lvl, Close::Paren),
                '`' => self.substitution(&mut lvl, Close::Backtick),
                // `2>&1`, `>&2`, `<&3`: part of a redirection word.
                '&' if ends_with(&lvl, &['>', '<']) => self.push(&mut lvl, c),
                // `>|file`: write even under `noclobber`, not a pipe.
                '|' if ends_with(&lvl, &['>']) => self.push(&mut lvl, c),
                // `&>file`, `&>>file`: a redirection word of its own.
                '&' if self.peek(1) == Some('>') => {
                    self.end_word(&mut lvl);
                    self.push(&mut lvl, c);
                }
                '\n' => {
                    self.end_command(&mut lvl);
                    self.skip_heredoc_bodies();
                }
                '(' => {
                    if close == Close::Paren {
                        lvl.depth += 1;
                    }
                    self.end_command(&mut lvl);
                }
                ')' => {
                    lvl.depth = lvl.depth.saturating_sub(1);
                    self.end_command(&mut lvl);
                }
                '&' | '|' | ';' => self.end_command(&mut lvl),
                c if c.is_whitespace() => self.end_word(&mut lvl),
                c => self.push(&mut lvl, c),
            }
            self.i += 1;
        }
        self.end_word(&mut lvl);
    }

    /// At `$(` or a backtick: the command inside becomes segments of its own,
    /// and the word keeps the substitution as written. Leaves `i` on the
    /// closing character, or past the end.
    fn substitution(&mut self, lvl: &mut Level, close: Close) {
        let start = self.i;
        self.word(lvl);
        self.i += if close == Close::Paren { 2 } else { 1 };
        self.run(close);
        let end = (self.i + 1).min(self.chars.len());
        let raw: String = self.chars[start..end].iter().collect();
        self.word(lvl).text.push_str(&raw);
    }

    /// At an opening quote: copy up to the closing one, which the caller steps past.
    fn single_quoted(&mut self, lvl: &mut Level) {
        self.word(lvl).quoted = true;
        self.i += 1;
        while let Some(c) = self.peek(0).filter(|c| *c != '\'') {
            self.push(lvl, c);
            self.i += 1;
        }
    }

    /// Like [`Self::single_quoted`], but a backslash escapes only `$`, a
    /// backtick, `"`, `\` and a newline (POSIX), and a substitution runs.
    fn double_quoted(&mut self, lvl: &mut Level) {
        self.word(lvl).quoted = true;
        self.i += 1;
        while let Some(c) = self.peek(0).filter(|c| *c != '"') {
            match c {
                '\\' if self.peek(1) == Some('\n') => self.i += 1,
                '\\' if matches!(self.peek(1), Some('$' | '`' | '"' | '\\')) => {
                    self.i += 1;
                    self.push(lvl, self.chars[self.i]);
                }
                '$' if self.peek(1) == Some('(') => self.substitution(lvl, Close::Paren),
                '`' => self.substitution(lvl, Close::Backtick),
                c => self.push(lvl, c),
            }
            self.i += 1;
        }
    }

    /// At a `#` that starts a word: stop on the last character before the newline.
    fn comment(&mut self) {
        while self.peek(1).is_some_and(|c| c != '\n') {
            self.i += 1;
        }
    }

    /// At `<<` (not `<<<`): read the closing word and remember it. Leaves `i` on the
    /// character after the delimiter.
    fn heredoc_operator(&mut self, lvl: &mut Level) {
        self.end_word(lvl);
        self.i += 2;
        let strip_tabs = self.peek(0) == Some('-');
        if strip_tabs {
            self.i += 1;
        }
        while self.peek(0).is_some_and(|c| c == ' ' || c == '\t') {
            self.i += 1;
        }
        let mut delimiter = String::new();
        while let Some(d) = self.peek(0) {
            if d.is_whitespace() || matches!(d, ';' | '&' | '|' | '(' | ')' | '<' | '>') {
                break;
            }
            if !matches!(d, '\'' | '"' | '\\') {
                delimiter.push(d);
            }
            self.i += 1;
        }
        if !delimiter.is_empty() {
            self.heredocs.push((delimiter, strip_tabs));
        }
    }

    /// At a newline: skip the bodies of the heredocs the line just ended announced.
    fn skip_heredoc_bodies(&mut self) {
        for (delimiter, strip_tabs) in std::mem::take(&mut self.heredocs) {
            self.skip_heredoc_body(&delimiter, strip_tabs);
        }
    }

    /// Leaves `i` on the newline that ends the closing line, or past the end.
    fn skip_heredoc_body(&mut self, delimiter: &str, strip_tabs: bool) {
        loop {
            let start = self.i + 1;
            if start >= self.chars.len() {
                self.i = self.chars.len();
                return;
            }
            let end = self.chars[start..]
                .iter()
                .position(|c| *c == '\n')
                .map_or(self.chars.len(), |p| start + p);
            let line: String = self.chars[start..end].iter().collect();
            self.i = end;
            let line = if strip_tabs {
                line.trim_start_matches('\t')
            } else {
                line.as_str()
            };
            if line == delimiter {
                return;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The strict reader
// ---------------------------------------------------------------------------

/// Characters that make a command line mean more than "run some programs with
/// these words, piped or listed": substitutions, grouping, escapes, comments,
/// history expansion, heredocs. Pipes, lists and redirections are read
/// explicitly by [`split_strict`].
const SHELL_OPERATORS: &[char] = &['`', '(', ')', '{', '}', '\\', '\n', '\r', '#', '!'];

/// One word of a segment, as [`split_strict`] reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StrictWord {
    /// Quotes removed; nothing else is unescaped (the reader refuses an
    /// unquoted backslash, and keeps a quoted one as written).
    pub text: String,
    /// A `<` or `>` in it was *quoted*: an argument (`grep '>' f`), never a
    /// redirection.
    pub quoted_angle: bool,
}

/// Why [`split_strict`] would not read a line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// A `<<` anywhere, quoted or not.
    Heredoc,
    /// A construct it does not model, outside quotes (or a backtick inside
    /// double quotes): one of `` ` ( ) { } \ # ! `` or a newline, or a braced
    /// expansion other than `${NAME}` (reported as `{`).
    Construct(char),
    /// A bare `&`, or an operator with no command before or after it.
    Operator,
    /// A quote that is never closed.
    UnbalancedQuote,
}

#[derive(Default)]
struct LineTokenizer {
    segs: Vec<Vec<StrictWord>>,
    cur: String,
    quoted_angle: bool,
    in_word: bool,
    quote: Option<char>,
}

impl LineTokenizer {
    fn new() -> Self {
        Self {
            segs: vec![Vec::new()],
            ..Self::default()
        }
    }

    fn flush(&mut self) {
        if self.in_word {
            self.segs.last_mut().expect("one segment").push(StrictWord {
                text: std::mem::take(&mut self.cur),
                quoted_angle: std::mem::take(&mut self.quoted_angle),
            });
            self.in_word = false;
        }
    }

    fn push_char(&mut self, c: char) {
        self.cur.push(c);
        self.in_word = true;
    }

    fn consume_operator(&mut self, c: char, next: Option<char>) -> Result<usize, Refusal> {
        let op_len = match (c, next) {
            ('|', Some('|')) | ('&', Some('&')) => 2,
            ('|', _) | (';', _) => 1,
            // A bare `&`: background or a redirection we do not model.
            ('&', _) => return Err(Refusal::Operator),
            _ => 1,
        };
        self.flush();
        if self.segs.last().is_some_and(Vec::is_empty) {
            return Err(Refusal::Operator); // `| cmd`, `cmd ||  | cmd`
        }
        self.segs.push(Vec::new());
        Ok(op_len)
    }

    fn step_unquoted(&mut self, chars: &[char], i: usize) -> Result<usize, Refusal> {
        let c = chars[i];
        match c {
            // `!` standing alone negates the command after it: a word of
            // its own, which a classifier skips like a loop keyword.
            '!' if !self.in_word && chars.get(i + 1).is_none_or(|n| n.is_whitespace()) => {
                self.push_char('!');
            }
            // `${NAME}` is the same plain variable as `$NAME`. Anything else
            // between the braces (`${a:-b}`, `${#a}`) is an expansion this
            // reader does not model.
            '$' if chars.get(i + 1) == Some(&'{') => {
                let len = braced_name_len(&chars[i..]).ok_or(Refusal::Construct('{'))?;
                for &ch in &chars[i..i + len] {
                    self.push_char(ch);
                }
                return Ok(len);
            }
            // An operator this reader does not model, outside quotes only:
            // a `}` inside a sed script is just a character.
            c if SHELL_OPERATORS.contains(&c) => return Err(Refusal::Construct(c)),
            '\'' | '"' => {
                self.quote = Some(c);
                self.in_word = true;
            }
            // `2>&1`, `>&2`: part of a redirection word, not an operator.
            '&' if self.cur.ends_with('>') || self.cur.ends_with('<') => self.push_char(c),
            // `>|file`: write even under `noclobber`, not a pipe.
            '|' if self.cur.ends_with('>') => self.push_char(c),
            // `&>file`, `&>>file`: a redirection word of its own.
            '&' if chars.get(i + 1) == Some(&'>') => {
                self.flush();
                self.push_char(c);
            }
            '|' | ';' | '&' => return self.consume_operator(c, chars.get(i + 1).copied()),
            c if c.is_whitespace() => self.flush(),
            _ => self.push_char(c),
        }
        Ok(1)
    }
}

/// Split a line into pipeline/list segments at `|`, `||`, `&&` and `;`,
/// honouring plain single and double quotes, or say why not ([`Refusal`]).
/// Fails closed: a caller that cannot read a line treats it as "cannot
/// tell". Each word carries whether a `<` or `>` in it was quoted.
/// Redirection words stay whole (`2>&1`, `&>log`, `>|f`); what they open is
/// the caller's to judge. A `$(…)` must be lifted out before (it is refused
/// as `(`).
pub fn split_strict(command: &str) -> Result<Vec<Vec<StrictWord>>, Refusal> {
    if command.contains("<<") {
        return Err(Refusal::Heredoc);
    }
    let mut tok = LineTokenizer::new();
    let chars: Vec<char> = command.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match tok.quote {
            Some(q) if c == q => {
                tok.quote = None;
                i += 1;
            }
            // Inside double quotes a backtick is still a command
            // substitution; inside single quotes it is a character.
            Some('"') if c == '`' => return Err(Refusal::Construct('`')),
            Some(_) => {
                tok.quoted_angle |= c == '<' || c == '>';
                tok.cur.push(c);
                i += 1;
            }
            None => i += tok.step_unquoted(&chars, i)?,
        }
    }
    if tok.quote.is_some() {
        return Err(Refusal::UnbalancedQuote);
    }
    tok.flush();
    if tok.segs.last().is_some_and(Vec::is_empty) {
        return Err(Refusal::Operator); // trailing operator
    }
    Ok(tok.segs)
}

/// The length of a `${NAME}` at the start of `chars` (`$`, `{`, a name of
/// letters, digits and `_`, `}`), or `None` when the braces hold anything else.
pub fn braced_name_len(chars: &[char]) -> Option<usize> {
    let close = chars.iter().skip(2).position(|&c| c == '}')? + 2;
    let name = &chars[2..close];
    (!name.is_empty() && name.iter().all(|c| c.is_ascii_alphanumeric() || *c == '_'))
        .then_some(close + 1)
}

// ---------------------------------------------------------------------------
// The literal reader
// ---------------------------------------------------------------------------

/// The words of each command a line runs, split at unquoted `&`, `|`, `;` and
/// newlines, with quotes removed and a backslash kept as written: a Windows
/// path (`cd C:\Users\me\repo`) is one word, where [`segments`]' POSIX escapes
/// would read `C:Usersmerepo`. No heredocs, comments or substitutions.
pub fn split_literal(command: &str) -> Vec<Vec<String>> {
    let mut segments: Vec<Vec<String>> = vec![Vec::new()];
    let mut word = String::new();
    let mut in_word = false;
    let mut quote: Option<char> = None;
    fn flush(segments: &mut [Vec<String>], word: &mut String, in_word: &mut bool) {
        if *in_word {
            if let Some(last) = segments.last_mut() {
                last.push(std::mem::take(word));
            }
            *in_word = false;
        }
    }
    for c in command.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => word.push(c),
            (None, '"' | '\'') => {
                quote = Some(c);
                in_word = true;
            }
            (None, '&' | '|' | ';' | '\n') => {
                flush(&mut segments, &mut word, &mut in_word);
                segments.push(Vec::new());
            }
            (None, c) if c.is_whitespace() => flush(&mut segments, &mut word, &mut in_word),
            (None, c) => {
                word.push(c);
                in_word = true;
            }
        }
    }
    flush(&mut segments, &mut word, &mut in_word);
    segments.retain(|s| !s.is_empty());
    segments
}

impl AsRef<str> for Word {
    fn as_ref(&self) -> &str {
        &self.text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strict(line: &str) -> Result<Vec<Vec<String>>, Refusal> {
        split_strict(line).map(|segs| {
            segs.into_iter()
                .map(|s| s.into_iter().map(|w| w.text).collect())
                .collect()
        })
    }

    fn v(segs: &[&[&str]]) -> Vec<Vec<String>> {
        segs.iter()
            .map(|s| s.iter().map(|w| w.to_string()).collect())
            .collect()
    }

    /// Lines both readers accept, and must read alike: where the strict
    /// reader says yes, the lenient one never reads different words. Every
    /// classifier and every question therefore starts from the same words.
    const AGREE: &[(&str, &[&[&str]])] = &[
        ("git status", &[&["git", "status"]]),
        ("  ls   -la  ", &[&["ls", "-la"]]),
        (
            "a && b || c | d ; e",
            &[&["a"], &["b"], &["c"], &["d"], &["e"]],
        ),
        (
            "echo 'a; rm -rf x' && make",
            &[&["echo", "a; rm -rf x"], &["make"]],
        ),
        ("echo \"a b\" c", &[&["echo", "a b", "c"]]),
        ("echo a'b'c", &[&["echo", "abc"]]),
        ("echo ''", &[&["echo", ""]]),
        ("cmd 2>&1 | tail -5", &[&["cmd", "2>&1"], &["tail", "-5"]]),
        ("cmd >&2", &[&["cmd", ">&2"]]),
        ("cmd &>log", &[&["cmd", "&>log"]]),
        ("cmd >|f", &[&["cmd", ">|f"]]),
        ("echo ${HOME}/x", &[&["echo", "${HOME}/x"]]),
        ("! git diff --quiet", &[&["!", "git", "diff", "--quiet"]]),
        ("FOO=1 sudo x", &[&["FOO=1", "sudo", "x"]]),
        ("echo \"$(date)\"", &[&["echo", "$(date)"]]),
        ("echo '`date`'", &[&["echo", "`date`"]]),
        (
            "if true; then cargo build; fi",
            &[&["if", "true"], &["then", "cargo", "build"], &["fi"]],
        ),
        (
            "for f in a b; do cat $f; done",
            &[
                &["for", "f", "in", "a", "b"],
                &["do", "cat", "$f"],
                &["done"],
            ],
        ),
        ("grep '>' f", &[&["grep", ">", "f"]]),
        ("cat < in.txt", &[&["cat", "<", "in.txt"]]),
        ("echo é ü", &[&["echo", "é", "ü"]]),
        ("echo 'a\nb'", &[&["echo", "a\nb"]]),
        ("grep \"\\.rs\" f", &[&["grep", "\\.rs", "f"]]),
    ];

    #[test]
    fn where_the_strict_reader_reads_a_line_the_lenient_one_reads_it_alike() {
        for (line, want) in AGREE {
            assert_eq!(strict(line), Ok(v(want)), "strict: {line:?}");
            // `echo "$(date)"`: the lenient reader also runs the substitution.
            let lenient = segment_texts(line);
            assert_eq!(lenient[..want.len()], v(want)[..], "lenient: {line:?}");
            assert!(
                lenient.len() == want.len() || line.contains("$("),
                "{line:?}"
            );
        }
    }

    /// The one place they differ on a line both read: inside double quotes
    /// the strict reader keeps every backslash as written; the lenient one
    /// unescapes `$`, a backtick, `"` and `\` as POSIX does. Only argument
    /// text differs, never where a segment starts or ends.
    #[test]
    fn a_backslash_in_double_quotes_is_the_one_known_difference() {
        for (line, strict_words, lenient_words) in [
            ("echo \"a\\\\b\"", ["echo", "a\\\\b"], ["echo", "a\\b"]),
            ("echo \"\\$x\"", ["echo", "\\$x"], ["echo", "$x"]),
        ] {
            assert_eq!(strict(line), Ok(v(&[&strict_words])), "{line:?}");
            assert_eq!(segment_texts(line), v(&[&lenient_words]), "{line:?}");
        }
    }

    /// The strict reader fails closed, and says why: `ahma` names the
    /// construct to the agent (SPEC R2.7.4).
    #[test]
    fn the_strict_reader_refuses_what_it_does_not_model() {
        use Refusal::*;
        for (line, why) in [
            ("echo ${a:-b}", Construct('{')),
            ("echo hi!", Construct('!')),
            ("a;;b", Operator),
            ("| cmd", Operator),
            ("cmd |", Operator),
            ("sleep 1 &", Operator),
            ("cmd & other", Operator),
            ("echo $(date)", Construct('(')),
            ("echo `date`", Construct('`')),
            ("echo \"`date`\"", Construct('`')),
            ("(cd x && make)", Construct('(')),
            ("{ cargo build; }", Construct('{')),
            ("echo a\\ b", Construct('\\')),
            ("echo \"a\\\"b\"", UnbalancedQuote),
            ("ls # comment", Construct('#')),
            ("ls a#b", Construct('#')),
            ("cat <<EOF\nsudo x\nEOF", Heredoc),
            ("echo '<<'", Heredoc),
            ("make \\\n  -j4", Construct('\\')),
            ("echo \"unterminated", UnbalancedQuote),
            ("a\r\nb", Construct('\r')),
            ("x=$(cargo build)", Construct('(')),
        ] {
            assert_eq!(strict(line), Err(why), "{line:?}");
        }
    }

    /// The lenient reader reads everything, and what runs inside a
    /// substitution is a command of its own (SPEC R-ESCAPE.4, R2.7.11).
    #[test]
    fn the_lenient_reader_reads_what_the_strict_one_refuses() {
        for (line, want) in [
            ("echo ${a:-b}", v(&[&["echo", "${a:-b}"]])),
            ("a;;b", v(&[&["a"], &["b"]])),
            ("sleep 1 &", v(&[&["sleep", "1"]])),
            ("echo $(date) x", v(&[&["echo", "$(date)", "x"], &["date"]])),
            ("echo `date` x", v(&[&["echo", "`date`", "x"], &["date"]])),
            ("echo \"`date`\"", v(&[&["echo", "`date`"], &["date"]])),
            (
                "echo \"$(a \")\" b)\"",
                v(&[&["echo", "$(a \")\" b)"], &["a", ")", "b"]]),
            ),
            (
                "echo $( (a; b) ) c",
                v(&[&["echo", "$( (a; b) )", "c"], &["a"], &["b"]]),
            ),
            ("(cd x && make)", v(&[&["cd", "x"], &["make"]])),
            ("{ cargo build; }", v(&[&["{", "cargo", "build"], &["}"]])),
            ("echo a\\ b", v(&[&["echo", "a b"]])),
            ("echo \"a\\\"b\"", v(&[&["echo", "a\"b"]])),
            ("ls # comment", v(&[&["ls"]])),
            ("ls a#b", v(&[&["ls", "a#b"]])),
            ("cat <<EOF\nsudo x\nEOF\nls", v(&[&["cat"], &["ls"]])),
            (
                "cat <<-END\n\tnot-a-program\n\tEND\nnpm test",
                v(&[&["cat"], &["npm", "test"]]),
            ),
            (
                "cat <<< hi\nsudo rm x",
                v(&[&["cat", "<<<", "hi"], &["sudo", "rm", "x"]]),
            ),
            ("make \\\n  -j4", v(&[&["make", "-j4"]])),
            ("FOO=1 \\\n cargo build", v(&[&["FOO=1", "cargo", "build"]])),
            ("echo 'unterminated", v(&[&["echo", "unterminated"]])),
            (
                "x=$(cargo build)",
                v(&[&["x=$(cargo build)"], &["cargo", "build"]]),
            ),
        ] {
            assert_eq!(segment_texts(line), want, "{line:?}");
        }
    }

    /// A word's span is where it is in the line, quotes included, so a
    /// rewrite (`sudo` → `sudo -A`) keeps every other byte (SPEC R-ESCAPE.4).
    #[test]
    fn a_word_knows_where_it_is() {
        let line = "é \"su\"do x; echo \"$(sudo y)\"";
        let segs = segments(line);
        let sudo = &segs[0][1];
        assert_eq!(sudo.text, "sudo");
        assert!(sudo.quoted);
        assert_eq!(&line[sudo.span.clone()], "\"su\"do");
        let inner = &segs[2][0];
        assert_eq!(inner.text, "sudo");
        assert_eq!(&line[inner.span.clone()], "sudo");
        assert!(!inner.quoted);
    }

    #[test]
    fn an_assignment_has_a_shell_name() {
        for yes in ["X=1", "_x=", "RUSTFLAGS=-D warnings", "a1=b=c"] {
            assert!(is_assignment(yes), "{yes}");
        }
        for no in ["=x", "1X=y", "a.b=c", "--opt=v", "x", "a-b=c"] {
            assert!(!is_assignment(no), "{no}");
        }
    }

    /// The program of a segment, past keywords, assignments, groups and
    /// every wrapper in [`RUNS_THE_NEXT`] with its options (SPEC R-PERM.10.1).
    #[test]
    fn the_program_of_a_segment() {
        let program = |line: &str| -> Vec<String> {
            segment_texts(line)
                .into_iter()
                .filter_map(|s| program_index(&s).map(|i| s[i].clone()))
                .collect()
        };
        for (line, want) in [
            ("FOO=1 env BAR=2 ./x ask; echo done", &["./x", "echo"][..]),
            ("if true; then cargo build; fi", &["true", "cargo"]),
            ("for f in a b; do make; done", &["make"]),
            ("while true; do sleep 1; done", &["true", "sleep"]),
            ("! git diff", &["git"]),
            ("{ cargo build; }", &["cargo"]),
            ("nice -n 10 make", &["make"]),
            ("sudo -u root make", &["make"]),
            ("timeout -k 5 600 nice -5 cargo test", &["cargo"]),
            ("caffeinate -i -t 60 make", &["make"]),
            ("env -i FOO=1 make", &["make"]),
            ("time -p cargo build", &["cargo"]),
            ("nohup exec command make", &["make"]),
            ("a.b=c ls", &["a.b=c"]),
            ("S=/x; ls", &["ls"]),
            ("env", &[]),
            ("cmd 2>&1 | tail -5", &["cmd", "tail"]),
        ] {
            assert_eq!(program(line), want, "{line:?}");
        }
        let chain = command_chain(&["FOO=1", "nohup", "sudo", "-E", "make", "x"]);
        assert_eq!(chain.wrappers, [1, 2]);
        assert_eq!(chain.program, Some(4));
    }

    /// Two readings of wrappers, one rule between them: every wrapper that
    /// decides a lane (SPEC R2.7.4) also names the program, and where the
    /// strict reading strips a wrapper, the lenient one names the same
    /// program. `nohup` names the program but never decides a lane: it
    /// writes `nohup.out`.
    #[test]
    fn harmless_wrappers_are_a_strict_reading_of_runs_the_next() {
        for line in [
            "timeout 600 cargo nextest run",
            "timeout -k 10 -s TERM 30m cargo test",
            "time cargo test",
            "time -p cargo test",
            "nice -n 5 cargo test",
            "nice -5 cargo test",
            "caffeinate -i cargo nextest run",
            "env RUST_LOG=debug cargo test",
            "timeout 600 nice cargo test",
        ] {
            let words: Vec<&str> = line.split(' ').collect();
            let n = harmless_wrapper_len(&words);
            assert!(n > 0, "{line}");
            assert_eq!(program_index(&words), Some(n), "{line}");
            assert!(RUNS_THE_NEXT.contains(&words[0]), "{line}");
        }
        for line in [
            "nohup cargo test",
            "timeout --weird 5 cargo test",
            "env -i cargo test",
            "timeout 5",
            "caffeinate -t 5 cargo test",
        ] {
            let words: Vec<&str> = line.split(' ').collect();
            assert_eq!(harmless_wrapper_len(&words), 0, "{line} is not stripped");
        }
        for name in ["time", "nice", "caffeinate", "env", "timeout", "nohup"] {
            assert!(RUNS_THE_NEXT.contains(&name), "{name}");
        }
    }

    #[test]
    fn framing_only_segments_run_nothing() {
        for seg in [
            &["done"][..],
            &["fi"],
            &["S=/x"],
            &["for", "f", "in", "a"],
            &["for", "f"],
            &["then", "fi"],
            &["do", "out=x"],
            &["then", "A=1", "B=2"],
        ] {
            assert!(is_framing_only(seg), "{seg:?}");
        }
        for seg in [&["then"][..], &["for", "1x", "in"], &["ls"], &["X=1", "ls"]] {
            assert!(!is_framing_only(seg), "{seg:?}");
        }
    }

    /// SPEC R-PERM.11, PR #259's Windows leg: a backslash is a character.
    #[test]
    fn a_windows_path_stays_one_word() {
        assert_eq!(
            split_literal(r#"cd C:\Users\me\neubit4 && git -C "C:\a b\x" status"#),
            v(&[
                &["cd", r"C:\Users\me\neubit4"],
                &["git", "-C", r"C:\a b\x", "status"]
            ])
        );
    }
}
