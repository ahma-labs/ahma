# Code Complexity Analysis (`ahma simplify`)

**Status: tests-pass.** Built into every standard `ahma` binary (cargo feature `simplify`, on
by default). A `--no-default-features` build fails `ahma simplify` with an error naming the
feature. Library use: depend on `ahma_simplify` (MIT OR Apache-2.0); `ahma_simplify::run(SimplifyArgs)`
is the CLI entry point.

`ahma simplify` runs one or more analysis **lenses** over a project and returns a report plus,
on request, a structured prompt telling an agent exactly which functions to change:

- **complexity**: scores every source file, names its worst functions, ranks files worst-first;
- **reuse**: duplicated blocks of code;
- **dead-code**: exported symbols nothing references;
- **altitude**: chains of thin forwarding wrappers.

`--lens` selects lenses; the default, `all`, runs every one.

## Why

A file accretes nested conditionals and long functions one small change at a time, and nothing
flags the point where it becomes expensive to change safely. `simplify` makes that cost visible
early, and points at the functions to fix rather than the whole file.

## Quick Start

```bash
ahma simplify . --auto 6                  # all lenses, plans for the 6 highest-impact fixes
ahma simplify . --ai-fix 1                # fix prompt for the worst file
ahma simplify . --extensions rust --ai-fix 1
ahma simplify . --lens reuse              # duplicates only; skips the AST parse, so it is fast
ahma simplify . --diff                    # only files git reports as changed
ahma simplify . --verify src/my_module.rs # before/after metrics for one file
```

The same through MCP, with the `simplify` bundle (`--tools simplify` at startup). Argument
names use hyphens, as in the CLI:

```json
{"directory": ".", "auto": 6}
{"directory": ".", "ai-fix": 1}
{"directory": ".", "extensions": ["rs"], "ai-fix": 1}
{"directory": ".", "verify": "src/my_module.rs"}
{"directory": ".", "lens": ["reuse"]}
```

Agents: follow `/ahma simplify` in the ahma skill ([agent-skills.md](agent-skills.md)).

## Languages

| Language name | Extensions |
|---|---|
| `rust` | `.rs` |
| `kotlin` | `.kt`, `.kts` |
| `swift` | `.swift` |
| `objc` | `.m`, `.mm` |
| `python` | `.py` |
| `javascript` | `.js`, `.jsx` |
| `typescript` | `.ts`, `.tsx` |
| `java` | `.java` |
| `c++` / `cpp` | `.cpp`, `.cc`, `.cxx`, `.hpp`, `.hxx`, `.hh` |
| C (by extension only: `c`, `h`) | `.c`, `.h` |
| `c#` / `csharp` | `.cs` |
| `go` | `.go` |
| `html` | `.html`, `.htm` |
| `css` | `.css` |

`--extensions` takes language names (case-insensitive) or raw extensions: `rust,python` or `rs,py`.

rust-code-analysis gives full AST metrics for Rust, JavaScript, TypeScript, Java, Python and
C/C++/Objective-C. Kotlin, Swift, Go and C# rely on the external analyzers below.

### External analyzers

Unless `--no-external` is set, every installed analyzer that supports a language in the tree
runs: detekt-cli and Gradle Detekt (Kotlin), Lizard (Kotlin, Swift, Java, Go, C#, Objective-C,
JS, TS), and SwiftLint (Swift). Where two cover the same file, the worse value of each metric
is used. `--no-external` is faster and needs none of them installed, at the cost of no
Kotlin/Swift coverage and coarser metrics elsewhere.

## Lenses

| Lens | Finds | Languages | Report section |
|---|---|---|---|
| `complexity` | Files ranked by score, with their hotspot functions | all above | per-file ranking |
| `reuse` | Repeated blocks of 4+ lines after stripping comments and whitespace | all above (textual) | `## Duplicate Code (Reuse Lens)` |
| `dead-code` (also `dead_code`, `deadcode`) | Exported functions with no reference in the scanned files | Rust, TypeScript, JavaScript, Python, Java | `## Possibly Unreferenced Exports (Dead Code Lens)` |
| `altitude` | Chains of 2+ forwarding hops (A → B → C), where each body is one statement making one call | Rust, TypeScript, JavaScript, Python, Java | `## Delegation Chains (Altitude Lens)` |

Every lens reports **candidates, not defects**, deterministically, capped by `--limit`.

- **reuse**: identical-looking code can be coincidental; decide whether a shared helper is actually clearer. Comment stripping is textual, so a comment delimiter inside a string literal can cause a missed or spurious match.
- **dead-code**: blind to a public API used only downstream, macro-generated call sites, trait-object dispatch and reflection. It already skips private functions, `main`, tests (`test_*`, `tests/`, `*_test(s).rs`), anything marked within 3 lines above by `#[allow(dead_code)]`, `#[cfg(test)]`, `@SuppressWarnings`, `# noqa`, `eslint-disable` or `pub use`, and common Rust trait methods (`new`, `default`, `from`, `fmt`, `drop`, `clone`, `eq`, `hash`, `next`, `poll`, …).
- **altitude**: a facade, a trait impl delegating to a free function, or a platform shim is often deliberate. Name resolution is conservative: a callee name matching two definitions ends the chain. Only maximal chains are reported, and recursion terminates.
- Kotlin is not covered by dead-code or altitude: its grammar names no callee field.

## Score

Each file gets a composite score (0–100%):

```
Score = 0.4 × MI + 0.3 × Cognitive Density + 0.2 × Peak Cognitive + 0.1 × Length Score
```

| Component | Weight | What it measures |
|---|---|---|
| **MI** | 40% | Function-weighted Maintainability Index |
| **Cognitive Density** | 30% | Cognitive complexity per SLOC |
| **Peak Cognitive** | 20% | Cognitive complexity of the worst function |
| **Length Score** | 10% | 100% at ≤300 SLOC, falling linearly above that |

Cyclomatic complexity is reported for context only; MI already includes it. A file covered
only by an external analyzer has no MI, so the score is
`0.5 × Cognitive Density + 0.3 × Peak Cognitive + 0.2 × Length`, or
`0.6 × Cyclomatic Score + 0.4 × Length` when only cyclomatic data exists.

| Score | Status | Guidance |
|---|---|---|
| 85–100% | Excellent | No action needed |
| 70–84% | Good | Fix only the worst outliers |
| 55–69% | Fair | Plan a simplification pass |
| 40–54% | Poor | Prioritize before adding features |
| 0–39% | Critical | Address now |

## Verify, diff, and the fix prompt

- **`--ai-fix N`** appends a fix prompt for the Nth-worst file: its score, its hotspot functions, and an instruction to change just those.
- **`--verify <file>`** re-analyzes one file against the baseline of the previous full run in the same output directory, with a verdict: significant improvement (≥10%), modest (1–9%), no change, or regression (revert). Kotlin, Swift and Objective-C baselines come from external analyzers and are not kept; run a full analysis before and after instead.
- **`--diff`** analyzes only staged, unstaged and untracked-but-not-ignored files, and fails if the directory is not a git repository rather than falling back to a full scan.

## Report output

Markdown and HTML only. With no `--output-path`, `--html` or `--open`, the Markdown report is
printed to stdout.

```bash
ahma simplify . --output-path ./reports   # CODE_SIMPLICITY.md (+ .html with --html)
ahma simplify . --html --open
```

### What is skipped

- Files ignored by `.gitignore` and `.ignore`, at every level.
- Build and cache directories: `target`, `.gradle`, `node_modules`, `.next`, `venv`, `__pycache__`, `Pods`, `DerivedData`, `.build`, `cmake-build-*`, ….
- Generated code: by header in the first 2 KB (`@generated`, `DO NOT EDIT`, `Code generated by …`, protobuf, OpenAPI, bindgen, UniFFI), and by name (`*FFI.h`, `*.pb.go`, `*_pb2.py`, `*_generated.rs`, …).
- Migrations, minified assets, fixtures and snapshots.

Add your own with `--exclude 'stat3.kt,pkg/generated/,*.custom'`.

## CLI flags

Authoritative source: `ahma_common/src/simplify_args.rs` (`SimplifyArgs`).

| Flag | Default | Purpose |
|---|---|---|
| `directory` (positional) | `.` | Project root |
| `--auto [N]` | `10` when given | All lenses, fixes ranked by impact, plans for the top N |
| `--ai-fix N` | — | Fix prompt for the Nth most complex file (1-indexed) |
| `--lens` | `all` | `complexity`, `reuse`, `dead-code`, `altitude`, `all`; unknown values are an error |
| `--extensions` / `-e` | all | Languages or extensions to analyze |
| `--exclude` / `-x` | — | Extra paths or globs to skip |
| `--diff` | off | Only files git reports as changed |
| `--verify <file>` | — | Compare one file against the previous run |
| `--limit` / `-l` | `50` | Issues shown in the report |
| `--output-path <dir>` | stdout | Write `CODE_SIMPLICITY.md` / `.html` there |
| `--html` | off | Also write `CODE_SIMPLICITY.html` |
| `--open` | off | Open the report after writing it (`--heml` = `--html --open`) |
| `--output` / `-o` | `analysis_results` | Working directory for per-file metrics; recreated each run |
| `--no-external` | off | Skip Detekt, SwiftLint and Lizard |

The MCP tool takes the same names without the dashes in front (`ai-fix`, `output-path`, …),
plus `working_directory`. An unknown argument name is ignored with a log warning, so check
the spelling.

## CI

This project publishes its own report on every push:
[ahma-labs.github.io/ahma/CODE_SIMPLICITY.html](https://ahma-labs.github.io/ahma/CODE_SIMPLICITY.html).

```yaml
- name: Code Simplicity Report
  run: ahma simplify . --limit 20 --html --output-path ./simplicity-report
```

## See Also

- [agent-skills.md](agent-skills.md): the `/ahma simplify` agent workflow
- [ahma_simplify/SPEC.md](../ahma_simplify/SPEC.md): the specification
