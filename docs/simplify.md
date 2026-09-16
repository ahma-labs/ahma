# Code Complexity Analysis (`ahma simplify`)

**Status: Stable.** `simplify` is a default cargo feature of `ahma_bin` — it ships in every
standard build (`cargo install ahma_bin`, the release binaries, `cargo build --release`).
SPEC.md's Quick Status table lists it `tests-pass`. Building with `--no-default-features`
drops the feature; the `simplify` subcommand still parses but fails at run time with a message
naming the missing feature.

Ahma includes a built-in code analyzer (`ahma simplify`) that runs one or more independent
analysis **lenses** over your project and returns a structured AI prompt to fix what it finds,
with minimal, targeted changes. Three lenses exist today: **complexity** (the original analysis —
scores every source file, identifies the worst hotspot functions, and ranks files worst-first),
**reuse** (duplicate-code-block detection, see below), and **dead-code** (unreferenced exported
symbols, see below). `--lens` selects which lenses run; the default, `all`, runs every lens.

Supports: **Rust, Python, JavaScript, TypeScript, Kotlin, Swift, Objective-C, C, C++, Java, C#,
Go, CSS, HTML**. The reuse lens covers all of these, including the languages with no AST
support, because it works on text rather than a parsed structure. The dead-code lens covers a
narrower set — Rust, TypeScript, JavaScript, Python, and Java — because it needs a parsed AST;
see [Dead Code Lens](#dead-code-lens-unreferenced-exports) below.

---

## Why

Complexity is invisible until it isn't: a file accretes nested conditionals and long functions
one small change at a time, and nothing flags the point where it becomes expensive to modify
safely. By the time a maintainer (human or AI) notices, the fix is a large, risky rewrite instead
of a small one.

`simplify` makes that cost visible before it compounds. A *file-level* score tells you which
files in the project are worth worrying about at all; *function-level* hotspots inside those
files tell you exactly which functions to touch, so refactoring effort goes where it pays off
instead of being guessed at or applied uniformly across a file that is mostly fine. The scoring
is deliberately calibrated for AI-assisted maintenance: an agent making a change has to hold the
relevant code in its context window, and large, deeply nested functions are where
misunderstanding and regression risk concentrate.

---

## Quick Start

```bash
# Analyze the current directory and get fix instructions for the worst file
ahma simplify . --ai-fix 1

# Rust files only
ahma simplify . --extensions rust --ai-fix 1

# Duplicate-code detection only — skips the AST parse, so it's fast
ahma simplify . --lens reuse

# Only the files you just changed
ahma simplify . --diff

# Verify improvement after editing
ahma simplify . --verify src/my_module.rs
```

Or via the `simplify` MCP tool (requires `--tools simplify` or `--tools rust,simplify` at
server startup):

```
simplify(directory=".", ai_fix=1)
```

---

## Installation

`ahma simplify` is built into the `ahma` binary — no separate install needed. The analyzer
lives in its own crate, `ahma_simplify`, which the `ahma` binary (`ahma_bin`) links through
its `simplify` cargo feature. That feature is **on by default**; a binary built with
`--no-default-features` still lists the subcommand but fails with a message naming the
feature when it is run.

**Quick install (Linux/macOS):**
```bash
cargo install --git https://github.com/ahma-labs/ahma ahma_bin --bin ahma --root ~/.local --locked --force
```

Or after `ahma` is installed: `ahma update`

**Windows (PowerShell 5.1+):**
```powershell
irm https://raw.githubusercontent.com/ahma-labs/ahma/main/scripts/install.ps1 | iex
```

**From source:**
```bash
cargo build --release -p ahma_bin
```

The `simplify` feature is enabled by default. To build without it (smaller binary, no
`rust-code-analysis` toolchain compiled in):
```bash
cargo build --release -p ahma_bin --no-default-features
```

To use the analyzer as a library in your own Rust code, depend on the `ahma_simplify` crate
(MIT OR Apache-2.0) directly: `ahma_simplify::run(SimplifyArgs)` is the same entry point the
CLI calls.

---

## Usage

### Basic analysis

```bash
# Analyze all supported files, get fix prompt for the worst file
ahma simplify <directory> --ai-fix 1

# Get fix prompt for the 2nd worst file
ahma simplify <directory> --ai-fix 2

# Show top 20 issues in the report (default: 50)
ahma simplify <directory> --limit 20
```

### Language filtering

```bash
# Single language (name or raw extension)
ahma simplify . --extensions rust
ahma simplify . --extensions rs

# Multiple languages
ahma simplify . --extensions rust,python

# Kotlin only
ahma simplify . --extensions kotlin
```

| Language name | Extensions scanned |
|---------------|--------------------|
| `rust` | `.rs` |
| `kotlin` | `.kt`, `.kts` |
| `swift` | `.swift` |
| `objc` | `.m`, `.mm` |
| `python` | `.py` |
| `javascript` | `.js`, `.jsx` |
| `typescript` | `.ts`, `.tsx` |
| `java` | `.java` |
| `c++` / `cpp` | `.cpp`, `.cc`, `.hpp`, `.hh` |
| `c#` / `csharp` | `.cs` |
| `go` | `.go` |
| `html` | `.html`, `.htm` |
| `css` | `.css` |

Rust is analyzed with full AST metrics (`rust-code-analysis`). Kotlin, Swift, and the
Lizard-supported languages go through the external analyzer registry described below.

### Verification

After editing a file, re-analyze it to confirm improvement:

```bash
ahma simplify <directory> --verify src/my_module.rs
```

Output shows before/after metrics with a verdict:

| Verdict | Meaning |
|---------|---------|
| Significant improvement (≥10%) | Success — move to next issue |
| Modest improvement (1–9%) | Acceptable |
| No change | Hotspot functions may not have been modified |
| Regression | Revert and try a different approach |

`--verify` compares against the rust-code-analysis TOML baseline from the previous full
analysis run in the same output directory. It does not currently support Kotlin, Swift, or
Objective-C files, because their baselines come from the external analyzers (Detekt,
SwiftLint, Lizard) and are not persisted for later comparison; run a full `ahma simplify`
before and after your change for those languages instead.

### Diff mode

```bash
# Analyze only files git reports as changed, instead of the whole tree
ahma simplify . --diff
```

`--diff` restricts analysis to files that are staged, unstaged, or untracked-but-not-ignored
according to git. It fails with a clear error if the directory is not a git repository or git
is not installed, rather than silently falling back to a full scan.

### Report output

```bash
# Write report to a directory (CODE_SIMPLICITY.md + CODE_SIMPLICITY.html)
ahma simplify <directory> --output-path ./reports

# Generate HTML report
ahma simplify <directory> --html

# Exclude generated code
ahma simplify <directory> --exclude '**/generated/**,**/vendor/**'
```

Report output is **Markdown and HTML only** — there is no JSON output format. With no
`--output-path`, `--html`, or `--open`, the Markdown report is printed to stdout.

---

## CLI Flags

Authoritative source: `ahma_common/src/simplify_args.rs` (`SimplifyArgs`).

| Flag | Type | Default | Purpose |
|------|------|---------|---------|
| `directory` (positional) | path | — | Project root to analyze |
| `--output` / `-o` | path | `analysis_results` | Working directory for intermediate per-file metrics; cleared and recreated on each run |
| `--limit` / `-l` | integer | `50` | Number of issues shown in the report |
| `--open` | flag | off | Open the generated report automatically after writing it |
| `--html` | flag | off | Also render `CODE_SIMPLICITY.html` beside the Markdown report; implies writing to disk rather than stdout |
| `--heml` | flag | off | Shorthand for `--html` and `--open` combined |
| `--extensions` / `-e` | comma-separated list | all supported extensions | File extensions or language names to analyze (e.g. `rs,py`, `rust,kotlin`); language names are case-insensitive |
| `--exclude` / `-x` | comma-separated list | none | Additional glob patterns to exclude, e.g. `**/generated/**,**/vendor/**` |
| `--no-external` | flag | off | Disable external language-specific analyzers (Detekt, SwiftLint, Lizard); use only rust-code-analysis metrics |
| `--output-path` | path | none (prints to stdout) | Directory to write `CODE_SIMPLICITY.md` / `CODE_SIMPLICITY.html` into, instead of printing to stdout |
| `--ai-fix` | integer | none | Generate a structured AI fix prompt for the Nth most complex file (1-indexed) |
| `--verify` | path | none | Re-analyze a specific file and compare against the baseline from the previous run |
| `--lens` | comma-separated list | `all` | Which analysis lenses to run: `complexity`, `reuse`, `dead-code` (also accepted: `dead_code`, `deadcode`, case-insensitive), or `all`. Unknown values are a hard error listing the valid options. Selecting only non-complexity lenses skips the rust-code-analysis parse entirely (the dominant cost), so e.g. `--lens reuse` is substantially faster than a full run |
| `--diff` | flag | off | Restrict analysis to files git reports as changed (staged, unstaged, and untracked-but-not-ignored) instead of walking the whole tree; fails if the directory isn't a git repository or git isn't installed, rather than silently falling back to a full scan |

## MCP Tool Reference

Tool name: `simplify` (requires `--tools simplify` at ahma startup).

| Argument | Type | Default | Purpose |
|----------|------|---------|---------|
| `directory` | path (required) | — | Project root to analyze |
| `ai_fix` | integer | — | Issue number for fix prompt (1 = worst file) |
| `limit` | integer | 50 | Issues to include in report |
| `verify` | path | — | Re-analyze a file vs. baseline |
| `extensions` | array | all | Restrict to file types (e.g. `["rs","py"]`) |
| `exclude` | array | — | Additional glob patterns to exclude |
| `output_path` | path | — | Write report to directory instead of stdout |
| `html` | boolean | false | Also generate HTML report |
| `lens` | array | all | Which analysis lenses to run (e.g. `["reuse"]`, `["dead-code"]`); see `--lens` above |
| `diff` | boolean | false | Restrict analysis to files git reports as changed instead of the whole tree; see `--diff` above |

### MCP tool invocation examples

```
# Via /ahma simplify skill subcommand
/ahma simplify
/ahma simplify rust
/ahma simplify kotlin 2

# Direct MCP tool call
simplify(directory=".", ai_fix=1)
simplify(directory=".", extensions=["rs"], ai_fix=1)
simplify(directory=".", verify="src/my_module.rs")
simplify(directory=".", lens=["reuse"])
simplify(directory=".", diff=true)
```

---

## Score Interpretation

Each file receives a composite score (0–100%):

```
Score = 0.4 × MI + 0.3 × Cognitive Density + 0.2 × Peak Cognitive + 0.1 × Length Score
```

| Component | Weight | What it measures |
|-----------|--------|-----------------|
| **MI** | 40% | Function-weighted Maintainability Index; rewards decomposed, well-structured code |
| **Cognitive Density** | 30% | Cognitive complexity normalised by SLOC; rewards focused, readable functions |
| **Peak Cognitive** | 20% | Cognitive complexity of the single worst function |
| **Length Score** | 10% | 100% at ≤300 SLOC, scaling down linearly above that |

Cyclomatic complexity is reported for context only — it is already embedded inside the MI
component and is not double-counted in the score.

For files covered only by an external analyzer (Kotlin, Swift, or a Lizard-only language,
where rust-code-analysis produces no metrics), there is no MI component, so the weight is
redistributed: when cognitive complexity is available, the score is
`0.5 × Cognitive Density + 0.3 × Peak Cognitive + 0.2 × Length`; when the analyzer reports
only cyclomatic complexity (e.g. Lizard without cognitive data), it falls back to
`0.6 × Cyclomatic Score + 0.4 × Length`.

| Score Range | Status | Guidance |
|-------------|--------|----------|
| 85–100% | Excellent | No action needed |
| 70–84% | Good | Acceptable; fix only the worst outliers |
| 55–69% | Fair | Plan a simplification sprint |
| 40–54% | Poor | Prioritize before adding features |
| 0–39% | Critical | Address now; maintenance cost is high |

A project score below 70% is a signal to run `--ai-fix` on the top 3–5 files.

---

## External Analyzer Registry

Rust files are analyzed directly with `rust-code-analysis` (full AST metrics). Other languages
go through a registry of external analyzers, tried in order until one succeeds for a given
file, unless `--no-external` is set:

1. **Detekt CLI** — standalone `detekt-cli`, tried first for Kotlin so a Gradle project isn't
   required.
2. **Detekt (Gradle)** — falls back to a project's own Gradle-managed Detekt if the CLI isn't
   available.
3. **Lizard** — universal fallback for Kotlin, Swift, Java, Go, C#, Objective-C, JavaScript,
   and TypeScript when the more specific analyzers above aren't available or don't apply.
4. **SwiftLint** — dedicated Swift analyzer for richer cognitive and cyclomatic metrics than
   Lizard alone provides.

`--no-external` disables all of the above and reports only rust-code-analysis metrics, which
is faster and doesn't require any of these tools to be installed, at the cost of no coverage
for Kotlin/Swift/Objective-C files and coarser metrics for the Lizard-supported languages.

---

## Reuse Lens: Duplicate Code Detection

The `reuse` lens finds duplicated blocks of code so you can judge whether extracting a shared
helper is worthwhile. Run it on its own with `--lens reuse`, or leave `--lens` at its default
(`all`) to run it alongside the complexity lens.

```bash
# Duplicate-code detection only — skips the rust-code-analysis parse, so it's fast
ahma simplify . --lens reuse
```

Key properties:

- **Language-agnostic.** Detection is purely textual: it strips comments using each language's
  comment syntax, collapses whitespace, then finds repeated blocks. This means it works on
  every language `simplify` supports, including Swift, Go, and C#, which have no AST-based
  complexity metrics.
- **Minimum block size is 4 lines.** Shorter matches are not reported.
- **Candidates for extraction, not defects.** Identical-looking code can be coincidental —
  the reader decides whether a shared helper is actually clearer. The report lists duplicate
  blocks to be evaluated, not a list of confirmed problems.
- **Deterministic.** The same input always produces byte-identical output.
- **Known limitation — string literals.** Comment stripping is textual, not a real
  per-language lexer, so a comment delimiter that happens to appear inside a string literal
  (e.g. `let url = "https://example.com";`) is misread as the start of a comment, which can
  cause a missed or spurious match. A real per-language lexer would fix this but is out of
  proportion for what this lens is for.

For example, running the reuse lens on this repository surfaces 19 identical lines shared
between `ahma_simplify/src/analysis/detekt.rs` and `ahma_simplify/src/analysis/checkstyle.rs`
— both parse Checkstyle XML issues.

Findings appear in the report under `## Duplicate Code (Reuse Lens)`, one entry per duplicate
group, each listing the block's line count, occurrence count, every file/line location, and a
sample of the duplicated code. The section is capped by `--limit`, same as the complexity
findings.

---

## Dead Code Lens: Unreferenced Exports

The `dead-code` lens finds exported functions and methods that have no reference anywhere in
the scanned files. Run it on its own with `--lens dead-code` (also accepted: `dead_code`,
`deadcode`, case-insensitive), or leave `--lens` at its default (`all`) to run it alongside the
other two lenses.

```bash
# Dead-code detection only
ahma simplify . --lens dead-code
```

**How it works.** The lens parses real tree-sitter ASTs to locate exported functions, then
counts identifier occurrences across every scanned file. A symbol with exactly one occurrence —
its own definition — is a candidate.

**Language support.** Rust, TypeScript, JavaScript, Python, and Java only. Other languages are
skipped because no AST grammar is available for them in this lens. This is narrower than the
reuse lens, which covers every language `simplify` supports because it works on text rather
than a parsed structure. Kotlin is deliberately excluded even though the complexity and reuse
lenses both handle it: Kotlin's tree-sitter grammar declares no field names on call
expressions, so the callee cannot be extracted the way it can for the other five languages.
Kotlin coverage still comes from the complexity lens (via Detekt) and the reuse lens.

**It reports candidates, never verdicts.** Acting on a finding without checking it first is how
live code gets deleted. Four classes of false positive are structurally invisible to this lens:

- A public API consumed only by a downstream crate — a library's entire public surface
  legitimately looks unreferenced from inside the library itself.
- Call sites generated by a macro.
- Dispatch through a trait object.
- Reflection or dynamic dispatch by string name.

**Mitigations already applied**, so a finding has already survived some filtering before it
reaches the report:

- Private functions are skipped entirely — rustc/clippy already catch those.
- `main` is skipped.
- `test_`-prefixed functions, anything under a `tests/` directory, and anything in a
  `*_test.rs`/`*_tests.rs` file are skipped.
- A function is skipped if any of the 3 lines above it contains `#[allow(dead_code)]`,
  `#[cfg(test)]`, `@SuppressWarnings`, `# noqa`, `eslint-disable`, or `pub use`.
- For Rust, commonly trait-required names (`new`, `default`, `from`, `try_from`, `fmt`, `drop`,
  `clone`, `eq`, `hash`, `next`, `poll`) are skipped, since a trait method reached only through
  a trait object has no textual call site and these are overwhelmingly required implementations
  rather than genuinely unused code.

Output is deterministic, sorted by file then line.

For example, running the dead-code lens against the whole ahma workspace surfaced 30
candidates; run against just the `ahma_simplify` crate alone, it surfaced exactly one —
`AnalysisConfidence::is_reliable` — which really is referenced nowhere.

Findings appear in the report under `## Possibly Unreferenced Exports (Dead Code Lens)`, a
table of Symbol / Kind / Visibility / Location, capped by `--limit`, same as the other lenses.

---

## Report Generation and the AI Fix Prompt

`ahma simplify` produces a Markdown report (`CODE_SIMPLICITY.md`) ranking files worst-to-best
by score, with per-file function hotspots (complexity lens), and, depending on which lenses
ran, a `## Duplicate Code (Reuse Lens)` section and/or a
`## Possibly Unreferenced Exports (Dead Code Lens)` section (see above). With `--html` it
additionally renders `CODE_SIMPLICITY.html`. There is no other output format.

`--ai-fix N` (CLI) / `ai_fix` (MCP) appends a structured fix prompt for the Nth-worst file
(1-indexed) after the report: it names the file, its score, and its specific hotspot
functions, and instructs the agent to make targeted changes to just those functions rather
than rewriting the file. This is the same mechanism whether the report is printed to stdout
or written to `--output-path`.

---

## AI Workflow

When using the `simplify` MCP tool or `/ahma simplify` chat command, follow this sequence:

1. **Run analysis** — `simplify(directory=".", ai_fix=1)`
2. **Read the structured fix prompt** in the output — it lists exact hotspot functions and constraints
3. **Apply targeted changes** to only the listed hotspot functions
4. **Verify improvement** — `simplify(directory=".", verify="<edited-file>")`
5. **Iterate** — move to `ai_fix=2`, `ai_fix=3`, etc.

### Anti-patterns to avoid

- Do not refactor the whole file; follow the hotspot list exactly
- Do not add comments to improve scores — structural change is required
- Do not inline complex logic to reduce function count
- Do not skip the verify step — metric confirmation is required

---

## CI Integration

The project itself tracks code simplicity on every push. The CI report is published at:
[ahma-labs.github.io/ahma/CODE_SIMPLICITY.html](https://ahma-labs.github.io/ahma/CODE_SIMPLICITY.html)

To add simplify to your own CI pipeline:

```yaml
- name: Code Simplicity Report
  run: ahma simplify . --limit 20 --html --output-path ./simplicity-report
```

---

## See Also

- [agent-skills.md](agent-skills.md) — AI agent skill configuration
- [../README.md](../README.md) — Main Ahma documentation
- [../SPEC.md](../SPEC.md) §9.5 — Full specification for `ahma simplify`
