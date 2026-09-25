# ahma_simplify Crate Specification

* **Status**: Approved
* **License**: MIT OR Apache-2.0
* **Depends on**: `ahma_common`
* **Used by**: `ahma_bin` only, as an optional dependency (feature `simplify`, on by default)

## 1. User Story / Problem Statement

*As a developer or agent, I want the most complex files in a project ranked, with concrete
fix instructions, so that refactoring effort goes where it matters.* User guide:
[docs/simplify.md](../docs/simplify.md).

## 2. Acceptance Criteria

- Scores every source file: Rust through `rust-code-analysis`; Kotlin, Swift and other
  languages through external analyzers with a Lizard fallback.
- Ranks hotspots and renders a Markdown or HTML report plus a structured AI fix prompt.
- `SimplifyArgs` lives in `ahma_common::simplify_args`, so the CLI parser in `ahma_mcp` can
  reserve the subcommand without depending on this crate.
- Built without the `simplify` feature, `ahma simplify` fails with a clear error naming the
  feature.
- `--auto [COUNT]` runs all analysis lenses (complexity, reuse, dead-code, altitude),
  prioritizes candidate fixes across all lenses by estimated impact, and outputs actionable
  implementation plans for the top N most valuable fixes (defaulting to 10).

### Feature matrix

| Feature | Status | Description |
|---------|--------|-------------|
| AST complexity scoring | PASS | Full AST metrics via `rust-code-analysis` for Rust, JavaScript, TypeScript, Java, Python and C/C++/Objective-C; composite score = 0.4 × MI + 0.3 × Cognitive Density + 0.2 × Peak Cognitive + 0.1 × Length. A Kotlin grammar is present but its metric traits are no-ops, so Kotlin is routed to the external registry instead |
| External analyzer registry | PASS | Kotlin via Detekt CLI → Gradle Detekt → Lizard; Swift via SwiftLint → Lizard; Java/Go/C#/ObjC/JS/TS via Lizard; `--no-external` disables the registry |
| Markdown report | PASS | `CODE_SIMPLICITY.md`, worst-to-best file ranking with per-file function hotspots; printed to stdout unless `--output-path`/`--html`/`--open` is set |
| HTML report | PASS | `CODE_SIMPLICITY.html` generated alongside the Markdown report with `--html` or `--heml` |
| AI fix prompt | PASS | `--ai-fix N` / MCP `ai_fix` appends a structured prompt naming the Nth-worst file's hotspot functions and instructing targeted-only changes |
| `--auto [COUNT]` prioritized planning | PASS | Runs all lenses (complexity, reuse, dead-code, altitude), ranks candidate findings into a unified impact score, and outputs prioritized actionable fix instructions for the top N most valuable items (default: 10) |
| `--verify` before/after comparison | PASS | Re-analyzes a file against the rust-code-analysis TOML baseline from the previous run and reports a verdict; not supported for Kotlin/Swift/ObjC (no persisted external baseline) |
| MCP `simplify` tool | PASS | Exposes `directory`, `auto`, `ai_fix`, `limit`, `verify`, `extensions`, `exclude`, `output_path`, `html`, `lens`, `diff` (requires `--tools simplify` at startup) |
| `--lens` analysis selection | PASS | Comma-separated `complexity`/`reuse`/`dead-code`/`altitude`/`all` (default `all`; `dead-code` also accepts `dead_code`/`deadcode`, case-insensitive); unknown values are a hard error listing the valid options. Selecting only non-complexity lenses skips the rust-code-analysis parse entirely |
| Reuse lens — duplicate code detection | PASS | Language-agnostic, text-based duplicate-block detector: strips comments per language, collapses whitespace, finds repeated blocks of 4+ lines. Deterministic output. Reports candidates for extraction, not defects. Known limitation: a comment delimiter inside a string literal is misread as a comment start. Adds a `## Duplicate Code (Reuse Lens)` report section capped by `--limit` |
| Dead-code lens — unreferenced exports | PASS | AST-based reachability lens (Rust, TypeScript, JavaScript, Python, Java only — no grammar for other languages, and Kotlin's call-expression nodes lack field names so it is excluded even though complexity/reuse cover it) that flags exported functions/methods with exactly one identifier occurrence in the scanned corpus (their own definition). Reports candidates, not verdicts: cannot see a downstream crate's use of a public API, macro-generated call sites, trait-object dispatch, or reflection by string name. Skips private functions, `main`, `test_`-prefixed functions, `tests/`-directory and `*_test(s).rs` files, functions preceded by a suppression marker (`#[allow(dead_code)]`, `#[cfg(test)]`, `@SuppressWarnings`, `# noqa`, `eslint-disable`, `pub use`), and, for Rust, common trait-required names (`new`, `default`, `from`, `try_from`, `fmt`, `drop`, `clone`, `eq`, `hash`, `next`, `poll`). Deterministic output, sorted by file then line. Adds a `## Possibly Unreferenced Exports (Dead Code Lens)` report section capped by `--limit` |
| Altitude lens — thin-wrapper delegation chains | PASS | AST-based lens (same five languages as dead-code: Rust, TypeScript, JavaScript, Python, Java) that flags a function as a thin wrapper when its body is exactly one statement making exactly one call, then walks forwarding chains and reports only those with 2+ hops (A→B→C); a single forwarding call is ordinary delegation and is not reported. Name resolution is conservative: the AST gives only a call's trailing identifier, so a callee name matching more than one definition in the corpus stops the walk there rather than guessing. Recursive/mutually-recursive wrappers terminate the walk instead of looping forever; only maximal chains are reported (the sub-chain of an already-reported chain is not listed separately). Reports candidates, not defects — a forwarding layer is frequently a deliberate facade, trait-impl delegation, or platform shim. Deterministic output. Adds a `## Delegation Chains (Altitude Lens)` report section listing each chain's hop count, description, and every caller→callee edge with file/line, capped by `--limit` |
| `--diff` change-scoped analysis | PASS | Restricts analysis to files git reports as changed (staged, unstaged, untracked-but-not-ignored) instead of the whole tree; fails loudly if the directory isn't a git repository or git isn't installed |

## 3. Non-Functional Requirements

- **Never a dependency of `ahma_mcp`**: the engine keeps one feature flavour and carries none
  of the analysis toolchain (AGENTS.md, one canonical build flavour).

## 4. Out of Scope

- Applying the fixes it suggests.
