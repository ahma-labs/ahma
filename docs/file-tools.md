# Built-in File Tools

**Status:** stable.

ahma ships its own file tools — read, write, edit, patch, list, find and search
— for clients that have none of their own (ahma's own agent in `ahma tui`, the
`agent` sub-agent tool, and MCP clients without native file tools). Clients
that already have native equivalents (Claude Code, Cursor, VS Code) are not
offered them, so a model never has two ways to write the same file.

## Why

An edit tool that is easy to get wrong gets wrong edits. The old
`replace_in_file` replaced **every** occurrence of its match, never checked the
model had seen the file, wrote in place, and answered "Replaced 3
occurrence(s)" — a one-line fix could silently rewrite a file, over a version
the user had changed a second earlier. The rules below are the ones the best
current harnesses converged on, applied to every write path.

## The rules

- **Read before you change.** `write_file` over an existing file,
  `replace_in_file`, `multi_edit`, and an `apply_patch` update or delete are
  refused unless the session has read that file and it has not changed since
  (by the model, a tool, or the user). Creating a new file needs no read. Each
  successful write re-records the file, so consecutive edits do not need a
  re-read.
- **One place per edit.** `old_str` must match exactly once; add surrounding
  lines to make it unique, or pass `replace_all: true` to change every match.
- **All or nothing.** `multi_edit` applies its edits in order and writes only if
  all succeed; `apply_patch` computes every file operation before it writes any.
- **Atomic writes.** A file is replaced by writing a temp file beside it and
  renaming, keeping its permissions — a crash leaves the old file or the new one.
- **A miss says what is there.** "Not found" says whether the text matches when
  whitespace is ignored, or where its first line is, so the next try can be right.
- **The file's line endings are kept.** A CRLF file is matched with `\n` text and
  written back as CRLF.
- **Output is bounded.** `read_file` returns up to 2000 numbered lines and says
  how to continue; `grep_search` caps at 200 matches; `fetch_webpage` at 50,000
  characters. Each says when it cut something and how to narrow.
- **The same guard as every write.** All of it goes through ahma's write guard
  (refusing git hooks, `.ahma/`, venv interpreters; disclosing editor/harness
  auto-run config) and the execution audit log — see
  [security-sandbox.md](security-sandbox.md).

## Quickstart

```text
read_file     {"path": "src/lib.rs"}
                → "     1\tuse std::io;\n     2\t…"   (numbers are not part of the file)
replace_in_file {"path": "src/lib.rs", "old_str": "fn old()", "new_str": "fn new()"}
                → "Edited src/lib.rs (1 replacement):\n    12\tfn new() {…"
multi_edit    {"path": "src/lib.rs", "edits": [{"old_str": "a", "new_str": "b"},
                                               {"old_str": "c", "new_str": "d"}]}
apply_patch   {"patch": "*** Begin Patch\n*** Update File: src/lib.rs\n@@ fn main\n-    old();\n+    new();\n*** Add File: src/new.rs\n+pub fn n() {}\n*** End Patch"}
grep_search   {"query": "TODO", "context": 2, "include_pattern": "**/*.rs"}
file_search   {"pattern": "**/*.toml"}          → newest first
```

Models trained on other harnesses can use those names: `Read`, `Write`, `Edit`,
`str_replace`, `MultiEdit`, `Grep`, `Glob`, `LS`, `WebFetch`, `TodoWrite`,
`Bash`, `shell`, `patch` — and their argument names (`file_path`,
`old_string`, a Grep `pattern`…) are mapped to ahma's.

## Reference

| Tool | Key arguments | Behaviour |
|---|---|---|
| `read_file` | `path`, `start_line`/`offset`, `end_line`/`limit` | Numbered lines, 2000 by default, lines cut at 2000 chars, binary refused |
| `write_file` | `path`, `content` | Create, or overwrite a file read and unchanged since |
| `replace_in_file` | `path`, `old_str`, `new_str`, `replace_all` | Unique match unless `replace_all`; returns the edited lines |
| `multi_edit` | `path`, `edits[]` | Ordered edits, all or nothing |
| `apply_patch` | `patch`, `base_dir` | `*** Begin Patch` format: Add / Delete / Update (with `@@` anchors, `*** Move to:`); whitespace-tolerant context matching; all or nothing |
| `list_dir` | `path` | One level, sorted |
| `file_search` | `pattern`, `base_dir` | Glob on the relative path; `.gitignore` respected, hidden skipped; newest first; ≤ 1000 |
| `grep_search` | `query`, `is_regex`, `case_sensitive`, `include_pattern`, `context`, `output_mode` (`content`/`files`/`count`), `max_results` | `.gitignore` respected; binary and >10 MB files skipped |
| `fetch_webpage` | `url`, `query` | Readable text, ≤ 50,000 chars; `query` keeps matching lines |

## See also

- [SPEC.md](../SPEC.md) R26 (the file-tool contract), R1.5 (built-in tool names)
- [security-sandbox.md](security-sandbox.md) — the scope and write guard these run under
- [tui.md](tui.md) — ahma's own agent, the main user of these tools
