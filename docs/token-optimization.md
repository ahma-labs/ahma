# Token Optimization & Small-Model Harness for Ahma

Last updated: 2026-06-12
Status: Design document / Research report — core features implemented

## Implementation Status

| Capability | Status | Where |
|------------|--------|-------|
| `--minimize-tokens` / `--no-minimize-tokens` flags | implemented | `ahma_bin` → `ahma tui`; flag > deprecated env > settings |
| `--small-model-harness` / `--no-small-model-harness` flags | implemented | same precedence chain |
| `--context-length <tokens>` | implemented | drives per-tool-result and conversation character budgets (~4 chars/token) |
| Per-tool-result truncation (head+tail with elision marker) | implemented | `ahma_core/src/agent.rs` (`truncate_middle`, `McpChatConfig::tool_result_char_cap`) |
| Conversation trimming (system prompt + latest messages preserved) | implemented | `ahma_core/src/agent.rs` (`trim_conversation`, `McpChatConfig::conversation_char_budget`) |
| Streaming line minimisation (server side) | implemented | `OutputOptimizer::process_streaming_line` in the adapter streaming path |
| Full-output spill file (escape hatch from truncation) | implemented | `adapter::spill`; `output_file` advertised in results |
| Remaining proposals in this document | design only | see sections below |

## Executive Summary

This document is a critical analysis and architectural guide for adding two independent
capabilities to ahma:

1. **`--minimize-tokens`** — Output compression and context-window efficiency for all models
2. **`--small-model-harness`** — Scaffold adaptations that help small (<35B) local LLMs
   perform closer to frontier-model levels

These features target different problems and should be independently toggleable.
They compose well — a user running a local 9B model benefits from both simultaneously.

The analysis synthesizes techniques from rtk-ai/rtk, Itay Inbar's little-coder
("Honey, I Shrunk the Coding Agent"), Julius Brussee's "caveman" prompting,
the Gloaguen et al. study on context files, and recent context engineering research
from Anthropic, LangChain, and academic sources.

> [!IMPORTANT]
> RTK is a **binary-only CLI proxy**, not a library crate. Its `Cargo.toml` produces
> an executable; there is no `lib.rs` to depend on. We must reimplement the valuable
> techniques natively in ahma's Rust streaming pipeline, not attempt to link RTK.
> This is actually preferable — ahma already has a streaming output infrastructure
> (`BoundedLineCollector`, `process_streaming_line`, `LogMonitor`) that is the ideal
> integration point.

---

## Table of Contents

1. [Source Analysis & Critique](#1-source-analysis--critique)
2. [Prioritized Feature List](#2-prioritized-feature-list)
3. [Token Minimizer (`--minimize-tokens`)](#3-token-minimizer---minimize-tokens)
4. [Small-Model Harness (`--small-model-harness`)](#4-small-model-harness---small-model-harness)
5. [Novel Ideas & Original Proposals](#5-novel-ideas--original-proposals)
6. [What to Avoid (Anti-Patterns)](#6-what-to-avoid-anti-patterns)
7. [Architecture & Integration Points](#7-architecture--integration-points)
8. [Rust Libraries & Dependencies](#8-rust-libraries--dependencies)
9. [Research Bibliography](#9-research-bibliography)

---

## 1. Source Analysis & Critique

### 1.1 rtk-ai/rtk

**What it is:** A standalone Rust CLI binary that acts as a transparent proxy between
the agent's shell and the LLM. It intercepts stdout/stderr from common dev commands
(git, cargo, npm, etc.) and applies four strategies: smart filtering, grouping,
truncation, and deduplication. Claims 60–90% token reduction.

**Critical assessment:**

| Aspect | Verdict |
|--------|---------|
| Library reuse | ❌ Binary-only. No `lib.rs`. Cannot `cargo add rtk`. |
| Core idea quality | ✅ Excellent. The four strategies are sound and proven. |
| Implementation style | ⚠️ Uses per-command parsers with tool-specific rules. This is the regex-dictionary anti-pattern the user wants to avoid, though RTK hides it behind a cleaner abstraction. |
| Applicability to ahma | ✅ High — but we must reimplement. Ahma's `process_streaming_line()` is the natural hook. |

**What to extract:**
- The four-strategy taxonomy (filter, group, truncate, deduplicate) is the right mental model
- Exit-code-aware truncation (success → minimal output, failure → preserve errors) is high value
- Line deduplication with count annotation (`[×143]`) is trivially implementable and high-impact

**What to reject:**
- RTK's per-command parser registry approach. They maintain ~100 command-specific rules. This is exactly the maintenance burden the user wants to avoid. We should favor **generic, structural** techniques over command-specific parsing.

### 1.2 Itay Inbar's little-coder ("Honey, I Shrunk the Coding Agent")

**What it is:** A Python-based agent scaffold (built on the `pi` substrate) that
demonstrated a 9B model jumping from ~19% to >45% on the Aider Polyglot benchmark
through scaffold engineering alone. A 35B model reached ~78%, competitive with frontier.

**Critical assessment:**

| Aspect | Verdict |
|--------|---------|
| Library reuse | ❌ Python; wrong language. |
| Core idea quality | ✅✅ Exceptional. This is the most rigorous work on scaffold-model fit. |
| Key insight | "Small models fail at orchestration, not coding." The harness must compensate for tool-use weakness, not coding weakness. |
| Applicability | ✅ High — the techniques translate directly to ahma's MCP tool pipeline. |

**Key techniques worth adopting:**
1. **Write guards** — Reject `WriteFile` on existing files; force edit tools instead
2. **Granular skill injections** — Per-turn context snippets instead of one giant system prompt
3. **Bounded reasoning budget** — Limit the number of tool-call rounds
4. **Deterministic healing** — Detect and fix common format errors automatically
5. **Explicit workspace discovery** — Proactively list project structure for the model

**Key insight to internalize:**
> The biggest finding is that _scaffold-model fit_ is a critical, often overlooked
> variable. The same model can go from 19% to 45% with zero retraining — just by
> adapting the harness.

### 1.3 Julius Brussee's "Caveman" Approach

**What it is:** A system-prompt technique that forces LLMs to drop conversational
filler, articles, pleasantries, and hedging. Reports 60–75% output token savings.
Also includes `caveman-compress` for shrinking context files by ~40%.

**Critical assessment:**

| Aspect | Verdict |
|--------|---------|
| Output savings | ✅ Real and substantial, especially for output tokens (which cost more). |
| Quality impact | ⚠️ Mixed. Frontier models handle it fine. Small models may degrade — they need structured guidance, not just "be brief." Ultra/Wenyan modes hurt readability. |
| Applicability | ✅ Moderate — the principle is correct, but ahma should implement it as a tunable system-prompt suffix, not stylistic mangling. |

**What to adopt:**
- The **principle** of appending a conciseness instruction to the system prompt
- Context file compression for `.ahma/*.md` files

**What to reject:**
- The specific "caveman speak" style (dropping articles, telegraphic grammar). This is
  a matter of individual taste, as the user correctly notes.
- The idea that this is sufficient alone. Token savings from LLM output brevity are
  dwarfed by savings from not sending 50K lines of `cargo build` output in the first place.

### 1.4 Academic & Industry Research

**Gloaguen et al. (ETH Zurich, 2026):**
> LLM-generated context files (like `AGENTS.md`) **reduced** task success rates by ~3%
> while increasing costs by >20%. Human-written files improved success by ~4% but
> with the same cost penalty.

**Implications for ahma:** This validates minimalism. Context files should be short,
curated, and enforced by tooling (linters, CI) where possible, not by prompt instructions
that the model obediently follows into aimless exploration.

**"Lost in the Middle" (Liu et al., 2023; refined through 2026):**
> Models exhibit a U-shaped recall curve — they attend well to the beginning and end
> of context, but lose information buried in the middle.

**Implications for ahma:** Place critical instructions and errors at context edges.
When truncating output, keep the first few lines (headers) and last few lines
(summary/errors) while dropping the middle.

**Context Engineering as a discipline (Anthropic, LangChain, 2025–2026):**
> The context window is not a bucket to fill — it's RAM. Effective agents curate
> what goes in, summarize old turns, and retrieve on-demand.

**Implications for ahma:** The "dynamic context pressure" idea from the user's proposal
is validated by this research. The best version tracks approximate token usage and
adjusts compression aggressiveness accordingly.

---

## 2. Prioritized Feature List

Features are ordered by **value-to-complexity ratio** (highest first).
Each is tagged with which switch it belongs to.

### Tier 1 — Implement First (High Value, Moderate Complexity)

| # | Feature | Switch | Est. Token Savings | Complexity |
|---|---------|--------|-------------------|------------|
| 1 | [Streaming line deduplication](#31-streaming-line-deduplication) | `--minimize-tokens` | 30–80% on noisy commands | Low |
| 2 | [Exit-code-aware truncation](#32-exit-code-aware-truncation) | `--minimize-tokens` | 50–95% on success paths | Low |
| 3 | [Anti-thrashing loop detector](#42-anti-thrashing-loop-detector) | `--small-model-harness` | Indirect (prevents waste) | Low |
| 4 | [Write-guard enforcement](#41-write-guard-enforcement) | `--small-model-harness` | Indirect (prevents corruption) | Low |
| 5 | [Head+tail truncation (drop middle)](#33-headtail-truncation-drop-middle) | `--minimize-tokens` | 40–70% on long output | Low |

### Tier 2 — Implement Second (High Value, Higher Complexity)

| # | Feature | Switch | Est. Token Savings | Complexity |
|---|---------|--------|-------------------|------------|
| 6 | [Conciseness system-prompt suffix](#34-conciseness-system-prompt-suffix) | `--minimize-tokens` | 30–60% on LLM output tokens | Low |
| 7 | [Granular skill injection](#43-granular-skill-injection) | `--small-model-harness` | Indirect (improves accuracy) | Medium |
| 8 | [Dynamic context pressure governor](#51-dynamic-context-pressure-governor) | Both | Adaptive | Medium |
| 9 | [ANSI/progress-bar stripping](#35-ansi-and-progress-bar-stripping) | `--minimize-tokens` | 5–40% on build output | Low |

### Tier 3 — Implement If Resources Allow (Moderate Value)

| # | Feature | Switch | Est. Token Savings | Complexity |
|---|---------|--------|-------------------|------------|
| 10 | [Pre-flight workspace discovery](#44-pre-flight-workspace-discovery) | `--small-model-harness` | Indirect (reduces exploration) | Medium |
| 11 | [Deterministic format healing](#45-deterministic-format-healing) | `--small-model-harness` | Indirect (prevents retries) | Medium |
| 12 | [Context file compression](#36-context-file-compression) | `--minimize-tokens` | 30–45% on context docs | Medium |

### Tier 4 — Defer or Skip (Low Value-to-Complexity)

| # | Feature | Switch | Reason to Defer |
|---|---------|--------|----------------|
| 13 | Token counting via BPE | Both | Adds large dependency (`tiktoken`); approximate byte/line counting is 90% as useful. See [§5.1](#51-dynamic-context-pressure-governor). |
| 14 | AST/code stripping | `--minimize-tokens` | Requires language-specific parsers; causes hallucination of missing bodies. See [§6.1](#61-ast-code-stripping). |
| 15 | Per-command parser registry (RTK-style) | `--minimize-tokens` | Maintenance nightmare; generic techniques achieve 80% of the benefit. See [§6.2](#62-per-command-parser-registry). |

---

## 3. Token Minimizer (`--minimize-tokens`)

When enabled, this flag activates output compression in ahma's streaming pipeline.
All techniques are applied at the `process_streaming_line()` level or in
`complete_operation_with_output()` / `finalize_streaming_operation()` — the exact
points where ahma currently collects command output.

### 3.1 Streaming Line Deduplication

**Mechanism:** Maintain a small rolling hash set (last N distinct lines, N≈16).
When a new line matches the previous line (or recent lines), suppress it and
increment a counter. When a different line arrives, flush the counter as an
annotation: `[previous line ×143]`.

**Why it's #1:** This single technique eliminates the catastrophic token bleeds from:
- Progress bars and spinners (`npm install`, `cargo build` download progress)
- Infinite loops printing the same error
- Build systems re-stating the same warning across hundreds of files
- Test runners printing dots or repeated "PASS" lines

**Implementation:**

```rust
/// Streaming deduplicator for consecutive identical lines.
/// Sits inside the `process_streaming_line` pipeline.
pub struct LineDeduplicator {
    prev_line: Option<String>,
    repeat_count: u64,
}

impl LineDeduplicator {
    pub fn new() -> Self {
        Self { prev_line: None, repeat_count: 0 }
    }

    /// Returns None if the line is a duplicate (suppressed).
    /// Returns Some(lines) when a new line breaks the streak,
    /// flushing any accumulated count annotation first.
    pub fn process(&mut self, line: &str) -> Vec<String> {
        let trimmed = line.trim_end();
        if Some(trimmed) == self.prev_line.as_deref() {
            self.repeat_count += 1;
            return vec![];
        }

        let mut output = Vec::with_capacity(2);
        if self.repeat_count > 0 {
            output.push(format!(
                "[... repeated {} more times]",
                self.repeat_count
            ));
        }
        self.repeat_count = 0;
        self.prev_line = Some(trimmed.to_string());
        output.push(line.to_string());
        output
    }

    /// Flush any pending repeat annotation at end-of-stream.
    pub fn flush(&mut self) -> Option<String> {
        if self.repeat_count > 0 {
            let annotation = format!(
                "[... repeated {} more times]",
                self.repeat_count
            );
            self.repeat_count = 0;
            Some(annotation)
        } else {
            None
        }
    }
}
```

**Complexity:** ~50 lines of Rust. Zero dependencies. Zero regex.

### 3.2 Exit-Code-Aware Truncation

**Mechanism:** After a command completes, examine the exit code before returning
output to the LLM:

- **Exit code 0 (success):** Return a compact summary:
  `✅ Command succeeded (exit 0). [stdout: 847 lines, stderr: 12 lines]`
  Optionally append the last 5 lines of stdout for commands where the summary
  matters (e.g., test count).

- **Exit code ≠ 0 (failure):** Keep the **last N lines** (default: 50) of combined
  output, which almost always contain the actual error. Drop the preceding pass/build
  progress lines.

**Why it's #2:** LLMs only need to know _why_ something failed. On success, they
need almost nothing. This technique alone can eliminate 95% of `cargo build` output
on a clean build and 80% of `cargo nextest run` output when tests pass.

**Implementation location:** Inside `complete_operation_with_output()` and
`finalize_streaming_operation()` in `ahma_mcp/src/adapter/mod.rs`. These functions
already have access to `exit_code`, `stdout`, and `stderr`.

```rust
fn compress_by_exit_code(
    exit_code: i32,
    stdout: &str,
    stderr: &str,
    tail_lines: usize,  // configurable, default 50
) -> CompressedOutput {
    if exit_code == 0 {
        let summary = format!(
            "✅ Command succeeded (exit 0). \
             [stdout: {} lines, stderr: {} lines]",
            stdout.lines().count(),
            stderr.lines().count(),
        );
        // Include last 5 lines for test-runner summaries
        let tail: String = stdout
            .lines()
            .rev()
            .take(5)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("\n");
        CompressedOutput {
            text: if tail.is_empty() { summary } else {
                format!("{}\n{}", summary, tail)
            },
            tokens_saved_estimate: stdout.len() + stderr.len(),
        }
    } else {
        // On failure, keep the tail which contains the actual error
        let combined = combine_stdout_stderr(
            stdout.to_string(),
            stderr.to_string(),
        );
        let error_tail: String = combined
            .lines()
            .rev()
            .take(tail_lines)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("\n");
        CompressedOutput {
            text: format!(
                "❌ Command failed (exit {}). Last {} lines:\n{}",
                exit_code, tail_lines, error_tail
            ),
            tokens_saved_estimate: combined.len().saturating_sub(error_tail.len()),
        }
    }
}
```

> [!WARNING]
> **Design decision needed:** Should success-path truncation be opt-in per tool
> definition (via MTDF schema), or always-on when `--minimize-tokens` is set?
> Some commands produce output that _is_ the result (e.g., `grep`, `cat`, `ls`).
> **Recommendation:** Default to truncation, but allow MTDF tool definitions to
> set `"preserve_full_output": true` for read-type tools.

### 3.3 Head+Tail Truncation (Drop Middle)

**Mechanism:** For outputs exceeding a configurable threshold (default: 200 lines),
keep the first 20 lines (typically headers, metadata) and the last 50 lines
(errors, summaries). Replace the middle with:

```
[... 1,847 lines omitted ...]
```

**Why this works:** Directly supported by the "lost in the middle" research — LLMs
attend poorly to middle content anyway. The first lines often contain file paths,
command echoes, or section headers; the last lines contain results and errors.

**Interaction with deduplication:** Apply deduplication _first_, then head+tail
truncation. This ensures the truncation threshold applies to the _deduplicated_
output, not the raw (possibly massively duplicated) output.

### 3.4 Conciseness System-Prompt Suffix

**Mechanism:** When `--minimize-tokens` is active and ahma's TUI agent loop sends
messages to the LLM, append a brief instruction to the system prompt:

```
Respond concisely. No preamble, no conversational filler.
Output only the tool call, code, or bare answer.
```

**Why it works:** Empirically saves 30–60% on LLM _output_ tokens. Output tokens
are typically 3–4× more expensive than input tokens on cloud APIs, so this has
outsized cost impact.

**Why not full "caveman":** Small models may misinterpret aggressive brevity
instructions and produce truncated or malformed tool calls. The instruction above
is firm but not destructively terse.

**Implementation:** In `ahma_tui/src/llm_bridge.rs`, within the system prompt
construction for the agent loop.

### 3.5 ANSI and Progress-Bar Stripping

**Mechanism:** Strip ANSI escape sequences (colors, cursor movement, bold, etc.)
and carriage-return-based progress bars (`\r`-terminated lines) from output
before it reaches the LLM.

```rust
/// Strip ANSI escape sequences. No regex needed — parse ESC[ sequences directly.
pub fn strip_ansi(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // Skip ESC[...m and similar CSI sequences
            if chars.peek() == Some(&'[') {
                chars.next(); // consume '['
                while let Some(&next) = chars.peek() {
                    chars.next();
                    if next.is_ascii_alphabetic() { break; }
                }
            }
        } else {
            output.push(c);
        }
    }
    output
}
```

**Why no regex:** ANSI escapes follow a deterministic grammar (`ESC[` + params + letter).
A state-machine parser is faster, more correct, and avoids regex compilation overhead
on every line.

**Carriage-return stripping:** Lines containing `\r` without `\n` are progress
indicators. Only keep the final state (last `\r`-delimited segment). This alone
eliminates thousands of tokens from download progress bars.

### 3.6 Context File Compression

**Mechanism:** When serving context files (`.ahma/*.md`, `AGENTS.md`, `SPEC.md`)
to the LLM, apply structural compression:

- Remove markdown formatting that doesn't carry semantic content (horizontal rules,
  decorative headers, HTML comments)
- Collapse consecutive blank lines to single blank lines
- Remove boilerplate sections (license headers, contributor guides) that don't help coding

**Critical note from Gloaguen et al.:** Auto-generated context files hurt more than
they help. This compression should focus on making _human-curated_ files more efficient,
not on auto-generating context that the model will obediently but aimlessly explore.

**Recommendation:** Lower priority. The ROI is ~40% on context files, but context files
are usually a small fraction of total token usage compared to command output.

---

## 4. Small-Model Harness (`--small-model-harness`)

When enabled, this flag activates runtime invariants and guardrails designed to
compensate for the specific weaknesses of small local LLMs. These models typically
fail at **tool orchestration** (choosing the right tool, formatting arguments correctly,
recovering from errors) rather than **coding ability**.

### 4.1 Write-Guard Enforcement — implemented, then removed

**Status: removed.** This was implemented as `harness_guard::write_guard`, but —
contrary to this section's `--small-model-harness` framing — it was wired to the
same `harness_guard.enabled` flag as the universally-on self-correction guards
(name/argument healing, failure-loop detection), not gated on
`small_model_harness` at all. So it blocked `write_file` overwrites for every
client, including frontier models with no small-LLM overwrite problem, and it
contradicted `write_file`'s own advertised "(create or overwrite)" contract. It
was removed once `write_file`/`replace_in_file` were also gated away from
clients with native file tools (Claude Code, Cursor, VS Code) — the guard was
protecting a tool surface those clients no longer even see.

The original mechanism, for reference: intercept MCP `tools/call` requests at
the adapter layer, and if the model called `write_file` targeting a file that
**already exists**, reject the call with a structured error:
```json
{
  "error": "FILE_EXISTS: Use replace_in_file for existing files. write_file is for new files only.",
  "hint": "Call replace_in_file with old_string/new_string to edit the specific section."
}
```

**Still relevant, not removed:** forcing edit tools toward **exact string
replacement** (`old_string` → `new_string`) rather than line-number-based edits
remains a reasonable small-model accommodation — small LLMs cannot count lines
accurately, but they excel at pattern matching for `str.replace()`. That is a
property of `replace_in_file`'s own interface, not of the removed write-guard,
and needs no gating.

**Why it seemed critical:** the goal — preventing a small model from silently
overwriting an entire file when it meant to change one function (Inbar's
research found this one of the highest-impact interventions) — is real for the
population `--small-model-harness` targets. A future re-implementation should
gate on that flag specifically, rather than on the shared self-correction
switch, so it doesn't reach clients it was never meant for.

### 4.2 Anti-Thrashing Loop Detector

**Mechanism:** Hash each tool call's name + arguments. Maintain a small circular
buffer (last 5 calls). If the same exact call fails 3 times consecutively, intercept
the next attempt and return an injected guidance:

```json
{
  "error": "LOOP_DETECTED: This exact call has failed 3 times. The approach is not working.",
  "hint": "Re-read the error messages above. Try a fundamentally different approach or read relevant documentation first."
}
```

**Why it works:** Small models get stuck in tight loops — running the same failing
command, getting the same error, trying the same command again. The loop detector
breaks this cycle before it wastes the entire context window.

**Implementation:** A `HashMap<u64, u32>` (hash → fail count) in the adapter layer,
reset on any successful tool call. ~30 lines of Rust.

```rust
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::collections::hash_map::DefaultHasher;

pub struct LoopDetector {
    fail_counts: HashMap<u64, u32>,
    max_retries: u32,
}

impl LoopDetector {
    pub fn new(max_retries: u32) -> Self {
        Self {
            fail_counts: HashMap::new(),
            max_retries,
        }
    }

    pub fn record_failure(&mut self, tool_name: &str, args: &str) -> bool {
        let hash = self.hash_call(tool_name, args);
        let count = self.fail_counts.entry(hash).or_insert(0);
        *count += 1;
        *count >= self.max_retries
    }

    pub fn record_success(&mut self) {
        self.fail_counts.clear();
    }

    fn hash_call(&self, tool_name: &str, args: &str) -> u64 {
        let mut hasher = DefaultHasher::new();
        tool_name.hash(&mut hasher);
        args.hash(&mut hasher);
        hasher.finish()
    }
}
```

### 4.3 Granular Skill Injection

**Mechanism:** Instead of one massive system prompt, inject small, contextual
"skill snippets" at specific points in the agent loop:

- **On first tool error:** Inject formatting guidance for that specific tool
- **On file read:** Inject a reminder about the edit workflow
- **On test failure:** Inject debugging strategy guidance
- **After N turns without progress:** Inject a "step back and plan" prompt

**Why it works (Inbar's key insight):** Small models have limited "attention budget."
A 4K-token system prompt wastes most of that budget on instructions that aren't
relevant to the current turn. Granular injection puts the right instruction at the
right time, keeping instructions in the "Smart Zone" (early in context, high attention).

**Implementation:** A `SkillInjector` that observes the tool-call stream and appends
brief (50–100 token) guidance snippets to the next user message when trigger conditions
are met. This integrates into the TUI's `spawn_agent_task` loop.

### 4.4 Pre-Flight Workspace Discovery

**Mechanism:** When a session starts in `--small-model-harness` mode, automatically
execute a lightweight workspace scan and include the result in the initial context:

1. List top-level files and directories (1 level deep)
2. Read the first 50 lines of README.md, AGENTS.md, or package.json if they exist
3. Format as a compact tree view

This prevents the model from wasting 3–5 turns calling `list_dir` and `read_file`
to understand the project structure — turns that small models often waste because
they "guess" rather than "explore."

**Cost-benefit:** ~200–500 tokens upfront saves 1,000–3,000 tokens of exploratory
tool calls. Net positive for context windows ≥ 8K.

**Implementation:** In the TUI session initialization, before the first LLM call.

### 4.5 Deterministic Format Healing

**Mechanism:** When the model produces a malformed tool call (common with small models),
attempt to fix it deterministically before returning an error:

- Missing required fields → inject sensible defaults with a warning
- JSON with trailing commas → strip and re-parse
- Tool name typos → fuzzy-match against known tool list (edit distance ≤ 2)
- String where array expected → wrap in `["..."]`

**Why this helps:** Small models frequently produce _almost_ correct tool calls.
Returning a parse error forces a retry that consumes another full round-trip.
Healing the call and proceeding (with a note) saves 500–1,000 tokens per occurrence.

**Risk:** Over-aggressive healing could silently execute unintended actions.
**Mitigation:** Only heal structural/syntactic issues, never semantic ones. Log all
healed calls at `warn!` level. If the healed call fails, report both the original
and healed versions.

---

## 5. Novel Ideas & Original Proposals

### 5.1 Dynamic Context Pressure Governor

**Concept:** Instead of static compression settings, make ahma context-aware by
tracking approximate token usage across the session.

**Design:**

```
Context Pressure = (estimated_tokens_used / context_window_size) × 100%

Pressure < 40%:  RELAXED   — Full output, no truncation
Pressure 40–70%: MODERATE  — Enable deduplication + head/tail truncation
Pressure 70–85%: ELEVATED  — Enable exit-code truncation, shorter tails
Pressure > 85%:  CRITICAL  — Extreme truncation, success-path → 1 line summary
```

**Token estimation without BPE:** A full BPE tokenizer (`tiktoken`) adds a
significant dependency (~15MB of vocabulary data) for marginal accuracy improvement.
A simpler heuristic — **bytes ÷ 4** for English text, **bytes ÷ 3** for code — is
within 10% of BPE counts and costs zero dependencies.

```rust
/// Fast approximate token count. Within ~10% of BPE for mixed code/English.
pub fn estimate_tokens(text: &str) -> usize {
    // Heuristic: 1 token ≈ 4 bytes for English, 3.2 for code.
    // Split the difference at 3.5.
    (text.len() as f64 / 3.5).ceil() as usize
}
```

**Why this is powerful:** It acts as an auto-scaling governor that gives the LLM
maximum context when the window is fresh, and progressively tightens as the session
runs long. This prevents the catastrophic failure mode where a long session fills
the context window and the model starts losing older instructions.

> [!TIP]
> The context window size should be configurable. Detect it from the model name
> when possible (e.g., `llama-3.1-8b` → 128K, `qwen-2.5-7b` → 32K), or let the
> user set it via `--context-window-size <tokens>`.

### 5.2 Semantic Exit-Code Enrichment

**Concept:** Beyond just checking exit code 0 vs. non-zero, examine the _type_ of
command to apply smarter compression:

| Command Pattern | On Success | On Failure |
|----------------|-----------|-----------|
| `cargo test` / `pytest` / `npm test` | "✅ N tests passed" (parse summary line) | Keep only failing test output |
| `cargo build` / `make` / `gcc` | "✅ Build succeeded" | Keep only error lines (filter warnings unless they're the only output) |
| `cargo clippy` / `eslint` | Keep warnings (they're the point) | Keep all |
| `git status` / `git diff` | Keep all (output _is_ the result) | Keep all |
| `ls` / `find` / `cat` / `grep` | Keep all (output _is_ the result) | Keep all |

**How to classify without regex:** Use the program name (first argument / basename).
Maintain a small categorization map:

```rust
enum OutputSemantics {
    /// Output is informational noise; truncate on success
    BuildLike,
    /// Output IS the result; always preserve
    ReadLike,
    /// Output contains actionable items; preserve even on success
    LintLike,
}

fn classify_command(program: &str) -> OutputSemantics {
    match program {
        "cargo" | "make" | "gcc" | "g++" | "rustc" | "javac" | "go" | "npm" | "yarn" | "pnpm" =>
            OutputSemantics::BuildLike,
        "cat" | "head" | "tail" | "grep" | "rg" | "find" | "ls" | "tree" | "git" | "echo" | "curl" =>
            OutputSemantics::ReadLike,
        "clippy" | "eslint" | "pylint" | "mypy" | "shellcheck" =>
            OutputSemantics::LintLike,
        _ => OutputSemantics::BuildLike, // default to aggressive compression
    }
}
```

**Why this is better than the proposed plan's simple exit-code check:** A `git status`
with exit code 0 should NOT be truncated to "✅ succeeded" — its output is the entire
point. A `cargo build` with exit code 0 absolutely should be. The command
classification handles this without any regex.

> [!NOTE]
> This is a **small, finite map** (~30 entries), not a per-command parser registry.
> It classifies the _semantics_ of the output, not how to parse it. Adding a new
> entry is one line. This is the right level of tool-awareness.

### 5.3 Conversation Compaction for Long Sessions

**Concept:** After N turns (configurable, default 20), automatically summarize the
older portion of the conversation history into a compact "session state" block:

```
[Session Summary: User is debugging a test failure in ahma_mcp/src/adapter/mod.rs.
The test `test_streaming_timeout` fails with "deadline exceeded". We tried increasing
the timeout (didn't help) and adding debug logging (revealed the issue is in the
cancellation handler). Current approach: fixing the select! bias order.]
```

**Implementation options:**
1. **LLM-powered summarization** — Use the same LLM to summarize older turns.
   This costs tokens but produces the best summaries.
2. **Extractive summarization** — Keep only tool-call/result pairs and error messages
   from older turns, dropping prose. Zero-cost but less nuanced.
3. **Hybrid** — Use extractive for turns 1–N, then LLM-summarize that extraction.

**Recommendation:** Start with option 2 (extractive). It's deterministic, free, and
handles 80% of the use case. LLM summarization can be added later as an opt-in.

This addresses the Phase 4 checklist item in `harness-epoch.md`:
`[ ] Add optional context-window compaction/summarization.`

### 5.4 Output Fingerprinting for Change Detection

**Concept:** When the same command is run multiple times in a session (common in
edit-test cycles), compute a hash of the output. If the output is identical to the
previous run, return only:

```
Output unchanged from previous run (hash: a3f2c1).
```

If the output changed, show a diff-like summary:

```
Output changed from previous run:
- 3 errors (was 5)
- New error: "lifetime mismatch in line 142"
+ Fixed: "unused variable" warnings resolved
```

**Why this is novel:** No existing tool does this. It directly targets the
edit-test-debug loop where the LLM runs `cargo build` 10 times in a session and
sees 10 copies of nearly identical output. Fingerprinting collapses subsequent
identical runs to ~10 tokens each.

**Implementation:** A `HashMap<String, u64>` mapping command strings to their last
output hash. On match, suppress full output. ~40 lines of Rust.

---

## 6. What to Avoid (Anti-Patterns)

### 6.1 AST / Code Stripping

**What it is:** Removing function bodies from source files so the LLM only sees
signatures.

**Why to avoid:**
- Requires language-specific parsers (tree-sitter or similar), bloating the binary
- Models hallucinate the "missing" bodies, producing overwrite edits that destroy
  the actual implementation
- Breaks the model's ability to reason about control flow and data dependencies
- The Gloaguen study suggests that _less_ context is often better than _distorted_ context

**Verdict:** ❌ Skip entirely. If a file is too large, show a subset; don't mutilate it.

### 6.2 Per-Command Parser Registry (RTK-Style)

**What it is:** Maintaining a large dictionary of command-specific output parsers
(e.g., "if command is `npm install`, parse progress bars; if `cargo test`, parse
the summary line").

**Why to avoid:**
- Maintenance burden scales linearly with tool ecosystem changes
- Breaks on non-standard output (CI environments, custom formatters)
- The generic techniques (deduplication, head+tail truncation, exit-code awareness)
  achieve ~80% of the benefit with ~5% of the maintenance cost

**Verdict:** ❌ Use generic structural techniques + the small command classification
map from §5.2, not per-command parsers.

### 6.3 Line-Number-Based Editing for Small Models

**What it is:** Edit tools that accept line numbers (e.g., "replace lines 42–48").

**Why to avoid for small models:**
- Small LLMs consistently miscalculate line numbers by ±1–3 lines
- The error compounds when multiple edits shift line numbers
- `old_string` → `new_string` exact replacement is strictly more reliable

**Verdict:** When `--small-model-harness` is active, prefer `replace_in_file`
(which uses exact string matching) over any line-number-based tool. The write-guard
from §4.1 steers models toward this automatically.

### 6.4 Aggressive "Caveman" Styling

**What it is:** Forcing the LLM to drop articles, use telegraphic grammar, etc.

**Why to avoid:**
- Hurts small model performance (they need structured guidance, not brevity pressure)
- Reduces readability for human operators monitoring the session
- The conciseness suffix from §3.4 achieves most of the token savings without
  the quality degradation

**Verdict:** ⚠️ Use the mild conciseness instruction, not the aggressive caveman style.

### 6.5 Token Counting with Full BPE Tokenizer

**What it is:** Using `tiktoken-rs` or similar for exact token counting.

**Why to defer:**
- The `tiktoken` crate adds ~15MB of vocabulary data to the binary
- Different models use different tokenizers (cl100k_base vs o200k_base vs llama-tokenizer)
- The `bytes ÷ 3.5` heuristic is within 10% accuracy for the pressure governor's
  needs (it doesn't need to be exact — it needs to be directionally correct)
- Can be added later behind a feature flag if exact counting proves necessary

**Verdict:** ⚠️ Defer. Use byte-based estimation initially.

---

## 7. Architecture & Integration Points

### 7.1 Output Pipeline

The token minimizer integrates as a new stage in the existing output pipeline:

```
                    ┌─────────────────────────┐
  process output    │  1. Redact sensitive     │  (existing: redact_sensitive_line)
  ─────────────────>│  2. Strip ANSI           │  (NEW: strip_ansi)
                    │  3. Deduplicate lines    │  (NEW: LineDeduplicator)
                    │  4. Collect in bounded   │  (existing: BoundedLineCollector)
                    └────────────┬────────────┘
                                 │
                    ┌────────────▼────────────┐
  finalize output   │  5. Head+tail truncation │  (NEW: head_tail_truncate)
  ─────────────────>│  6. Exit-code compress   │  (NEW: compress_by_exit_code)
                    │  7. Fingerprint check    │  (NEW: output_fingerprint)
                    └────────────┬────────────┘
                                 │
                    ┌────────────▼────────────┐
  return to LLM     │  8. Context pressure     │  (NEW: adjust by pressure level)
                    │     adjustment           │
                    └─────────────────────────┘
```

### 7.2 Small-Model Guardrails

The harness integrates at the MCP tool-call boundary:

```
                    ┌─────────────────────────┐
  incoming tool     │  1. Loop detection       │  (NEW: LoopDetector)
  call from LLM ───>│  2. Format healing       │  (NEW: fix malformed JSON args)
                    └────────────┬────────────┘
                                 │
                    ┌────────────▼────────────┐
  execute tool      │  3. Normal MCP dispatch  │  (existing: tool handler)
                    └────────────┬────────────┘
                                 │
                    ┌────────────▼────────────┐
  tool result       │  4. Record success/fail  │  (NEW: update LoopDetector)
  ─────────────────>│  5. Skill injection      │  (NEW: inject contextual guidance)
                    └─────────────────────────┘
```

### 7.3 Configuration

Both switches should be exposed as:
- CLI flags: `--minimize-tokens` and `--small-model-harness`
- Settings keys: `tools.minimize_tokens` / `tools.small_model_harness` in `~/.ahma/settings.toml`. **Not** environment variables — `AHMA_*` configuration vars are retired and ignored (SPEC R-CFG1.2)
- MTDF tool-definition overrides (per-tool `"preserve_full_output": true`)
- Settings file: `ahma_mcp/src/config.rs` additions

Both are **off by default**. They are independent and composable.

### 7.4 New Crate or Module?

**Recommendation:** Create a new module `ahma_mcp/src/output_optimizer/` containing:

```
output_optimizer/
├── mod.rs              # OutputOptimizer orchestrator
├── deduplicator.rs     # LineDeduplicator
├── ansi_strip.rs       # ANSI escape stripping
├── truncator.rs        # Head+tail truncation, exit-code compression
├── fingerprint.rs      # Output fingerprinting for change detection
├── command_classify.rs # Semantic command classification
└── pressure.rs         # Context pressure governor
```

And a new module `ahma_mcp/src/harness_guard/` containing:

```
harness_guard/
├── mod.rs              # HarnessGuard orchestrator
├── write_guard.rs      # Write-guard enforcement
├── loop_detector.rs    # Anti-thrashing loop detection
├── format_healer.rs    # JSON/tool-call format healing
└── skill_injector.rs   # Granular skill injection
```

This keeps the features modular, independently testable, and easy to enable/disable.

---

## 8. Rust Libraries & Dependencies

| Library | Purpose | Size Impact | Recommendation |
|---------|---------|-------------|----------------|
| None needed for Tier 1 | Dedup, truncation, loop detection, write guard | 0 | ✅ Pure Rust, zero dependencies |
| `serde_json` | Already a dependency for format healing | 0 (existing) | ✅ Use |
| `tiktoken` | BPE token counting | ~15MB vocab | ⚠️ Defer; use byte heuristic |
| `strsim` | Fuzzy matching for tool name healing | ~10KB | ✅ If format healing is implemented |
| `strip-ansi-escapes` | ANSI stripping | ~5KB | ⚠️ Or implement manually (~30 lines) |

**Recommendation:** Implement Tier 1 and Tier 2 features with **zero new dependencies**.
All techniques are simple enough for inline Rust implementations. Add `strsim` only
if fuzzy tool-name matching proves necessary.

---

## 9. Research Bibliography

### Primary Sources

1. **rtk-ai/rtk** — CLI proxy for LLM token reduction
   https://github.com/rtk-ai/rtk

2. **Inbar, I.** — "Honey, I Shrunk the Coding Agent" (little-coder)
   https://github.com/itayinbarr/little-coder
   - Demonstrated 19% → 45% benchmark improvement via scaffold engineering alone (9B model)
   - 35B model reached ~78% with scaffold adaptations, competitive with frontier models

3. **Brussee, J.** — Caveman Coding (system prompt token reduction)
   https://github.com/JuliusBrussee/caveman
   - 60–75% output token reduction via conciseness prompting
   - `caveman-compress` utility for ~40% context file compression

4. **Gloaguen et al. (ETH Zurich, 2026)** — Study on AGENTS.md effectiveness
   - LLM-generated context files reduced task success by ~3%, increased costs by >20%
   - Human-written files improved success by ~4% but with same cost penalty
   - Conclusion: minimal, targeted context outperforms bulk context injection

### Secondary Sources

5. **Liu et al. (2023)** — "Lost in the Middle: How Language Models Use Long Contexts"
   https://arxiv.org/abs/2307.03172
   - U-shaped attention curve: models attend best to beginning and end of context

6. **Anthropic (2025–2026)** — Context Engineering best practices
   - "Context window is RAM, not a bucket"
   - Selective injection, sub-agent isolation, context pruning

7. **POLO (Project-Level Optimizer)** — IJCAI
   - Structural analysis (call graphs) outperforms raw code dumping for coding agents

8. **LangChain (2025–2026)** — Context Engineering framework documentation
   - Tiered relevance scoring, caching stable prefixes, pruning redundant history

---

## Appendix A: Quick-Start Implementation Checklist

For an agent implementing this plan:

- [ ] Create `ahma_mcp/src/output_optimizer/mod.rs` with feature flag gate
- [ ] Implement `LineDeduplicator` (§3.1) — ~50 lines
- [ ] Implement `strip_ansi()` (§3.5) — ~30 lines
- [ ] Implement `compress_by_exit_code()` (§3.2) — ~40 lines
- [ ] Implement `head_tail_truncate()` (§3.3) — ~20 lines
- [ ] Integrate into `process_streaming_line()` and `finalize_streaming_operation()`
- [ ] Add `--minimize-tokens` CLI flag and config
- [ ] Create `ahma_mcp/src/harness_guard/mod.rs` with feature flag gate
- [ ] Implement `LoopDetector` (§4.2) — ~30 lines
- [x] Implement write guard in `write_file` handler (§4.1) — then removed, see §4.1
- [ ] Add `--small-model-harness` CLI flag and config
- [ ] Add unit tests for all new components
- [ ] Integration test: verify `--minimize-tokens` reduces output for `cargo build`

## Appendix B: Estimated Total Implementation Size

| Component | Lines of Rust | Dependencies |
|-----------|---------------|--------------|
| Output optimizer module | ~300 | None |
| Harness guard module | ~250 | None |
| Config/CLI integration | ~100 | None |
| Tests | ~400 | None |
| **Total** | **~1,050** | **0 new** |

This is a modest, low-risk addition to the codebase that delivers substantial value
for both cloud API cost reduction and local small-model performance improvement.
