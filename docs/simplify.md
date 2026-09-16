# Code Complexity Analysis (`ahma simplify`)

**Status: Stable.** `simplify` is a default cargo feature of `ahma_bin` — it ships in every
standard build (`cargo install ahma_bin`, the release binaries, `cargo build --release`).
SPEC.md's Quick Status table lists it `tests-pass`. Building with `--no-default-features`
drops the feature; the `simplify` subcommand still parses but fails at run time with a message
naming the missing feature.

Ahma includes a built-in code complexity analyzer (`ahma simplify`) that scores every source
file in your project, identifies the worst hotspot functions, and returns a structured AI prompt
to fix them with minimal, targeted changes.

Supports: **Rust, Python, JavaScript, TypeScript, Kotlin, Swift, Objective-C, C, C++, Java, C#,
Go, CSS, HTML**.

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

## Report Generation and the AI Fix Prompt

`ahma simplify` produces a Markdown report (`CODE_SIMPLICITY.md`) ranking files worst-to-best
by score, with per-file function hotspots. With `--html` it additionally renders
`CODE_SIMPLICITY.html`. There is no other output format.

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
