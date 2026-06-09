# Recursive Task Tree: LLM-Orchestrated Depth-First Execution

> **Status**: Implemented
> This document describes the architecture for recursive task decomposition and execution in ahma,
> where large jobs are broken into a tree of LLM-planned subtasks interspersed with sandboxed
> shell tool calls, all within a single security scope.

## 1. Problem Statement

Current ahma execution is flat: a user or LLM issues a single command, ahma runs it in a sandboxed shell, and returns stdout/stderr. The existing `decompose` tool type adds one level of fan-out (split a question into sub-questions, dispatch to an LLM, reduce), but it cannot recurse, cannot interleave tool calls with reasoning, and cannot dynamically re-plan based on intermediate results.

Real-world tasks — "refactor this module to use async I/O", "set up CI for this repo", "investigate and fix the performance regression in the billing service" — require dozens of coordinated steps: reading files, running commands, interpreting output, deciding the next action, sometimes backtracking. Today, that orchestration loop lives entirely inside the calling LLM (Cursor, Claude Code, etc.), which means:

1. **Context bloat.** The calling LLM accumulates every tool result in its context window, hitting limits or degrading quality on long tasks.
2. **No structured recovery.** If step 14 of 20 fails, the LLM must reason about the entire history to decide what to retry.
3. **No parallelism below the top level.** Independent subtasks (e.g., "fix linting" and "write tests") execute sequentially because the LLM is single-threaded in its reasoning.
4. **No delegation.** The same LLM (often a large, expensive cloud model) handles both high-level planning and trivial "run `cargo fmt`" steps. There is no way to hand branches to a cheaper local model or a different machine.

The Recursive Task Tree architecture solves these problems by moving the orchestration loop *into* ahma, where it can be structured, sandboxed, parallelised, and eventually distributed.

---

## 2. Prior Art and Lessons Learned

### 2.1 What Works

| Approach | Key Insight | Example Systems |
|----------|-------------|-----------------|
| **Tree of Thoughts (ToT)** | Modelling reasoning as a search tree with backtracking dramatically improves complex problem-solving over linear chain-of-thought | Yao et al. (2023), LangChain ToT |
| **Hierarchical task decomposition** | Breaking a goal into sub-goals that can be solved independently reduces per-step complexity and enables parallelism | ADaPT, HuggingGPT, AutoGen |
| **Harness-based tool execution** | The LLM emits structured tool calls; a harness executes them and feeds observations back. This separates reasoning from action | SWE-agent (ACI), OpenHands, Claude Code |
| **Sandboxed execution** | Kernel-level isolation (containers, Landlock, Seatbelt) makes it safe to let an LLM run arbitrary commands | E2B, OpenHands (Docker), ahma |
| **Context engineering** | Treating the context window as a scarce resource — summarising, offloading, pruning — is more important than raw window size | Anthropic (2025), LangGraph memory |
| **Actor model for agents** | Each agent is an isolated actor with its own state, communicating via messages. This prevents shared-state bugs and maps naturally to distribution | Ray, Erlang/OTP supervision trees |

### 2.2 What Does Not Work Well

| Anti-pattern | Problem | Mitigation in our design |
|-------------|---------|--------------------------|
| **Monolithic context accumulation** | Dumping every tool output into one growing context degrades LLM quality and eventually overflows | Each tree node has its own context; parent sees only summaries |
| **Fixed decomposition heuristics** | Pre-determined task splits miss the structure of the actual problem | LLM-driven dynamic decomposition with a "is this atomic?" test |
| **Polling-based coordination** | Waiting loops between subtasks waste resources and introduce timing bugs | Channel-based completion notification (ahma R18: No-Wait State Transitions) |
| **Unrestricted agent scope** | Broad filesystem access leads to accidental damage or prompt-injection exploits | Task vault per tree root; subtasks inherit (never widen) the sandbox scope |
| **No verification between steps** | Errors propagate silently through the tree until the final result is wrong | Mandatory verification predicate after each tool call; backtracking on failure |
| **Pipeline-style data flow to LLMs** | Streaming partial input to an LLM does not help — it needs the complete context before generating | Batch delivery of summarised results; no speculative pipelining of LLM input |

### 2.3 Landscape of Existing Systems

**Claude Code / Cursor / Codex CLI** — These systems implement the orchestration loop *inside* the LLM client. The LLM itself decides what tool to call next, accumulating the entire conversation in its context. This works for medium tasks but struggles with very long sequences (context overflow) and cannot parallelise or delegate to cheaper models.

**OpenHands** — Uses a containerised event-stream architecture. Each agent action is an event; the runtime executes it in a Docker sandbox and returns an observation event. This is close to our model but uses containers (heavy, no kernel-level sandboxing) and does not support tree-structured decomposition.

**LangGraph** — Provides graph-based orchestration with explicit state machines. Supports branching and looping but is Python-only, does not include sandboxing, and has no distribution story.

**AutoGen** — Multi-agent conversation framework. Agents can delegate to other agents, but coordination is via unstructured chat messages rather than a formal task tree. Context management is left to the user.

**ahma `decompose` (current)** — Single-level fan-out: split question → N sub-questions → LLM each → reduce. No recursion, no tool calls within sub-tasks, no backtracking. This is the direct ancestor of the Recursive Task Tree.

---

## 3. Architecture

### 3.1 The Task Tree Model

A task tree is a rooted tree where:

- The **root node** represents the original user goal (from MCP `tools/call`, TUI input, or CLI).
- **Interior nodes** are **planning nodes**: an LLM decomposes a goal into ordered sub-goals.
- **Leaf nodes** are **execution nodes**: either a sandboxed shell command or an atomic LLM call (e.g., "summarise this output").
- Edges are ordered; children of a planning node execute in the order determined by the planner, unless explicitly marked as parallelisable.

```
                    ┌─────────────────────┐
                    │  ROOT: "Fix perf    │
                    │  regression in      │
                    │  billing service"   │
                    └──────────┬──────────┘
                               │ LLM decomposes
              ┌────────────────┼────────────────┐
              ▼                ▼                ▼
     ┌────────────┐   ┌──────────────┐   ┌──────────────┐
     │ Investigate │   │ Implement    │   │ Verify       │
     │ (planning)  │   │ fix          │   │ (planning)   │
     └──────┬─────┘   │ (planning)   │   └──────┬───────┘
            │         └──────┬───────┘          │
      ┌─────┼─────┐    ┌────┼────┐        ┌────┼────┐
      ▼     ▼     ▼    ▼    ▼    ▼        ▼         ▼
    [git   [run  [LLM  [edit [run  [LLM   [run      [run
     log]  bench] anal] file] build] rev]  bench]    test]
```

### 3.2 Node Lifecycle

Every node in the tree follows this state machine:

```
  Created ──► Planning ──► Ready ──► Running ──► Completed
                │                      │              │
                │                      ▼              │
                │                   Failed ◄──────────┘
                │                      │
                │                      ▼
                └──────────────── Backtracking
```

| State | Description |
|-------|-------------|
| `Created` | Node exists in the tree but has not been evaluated yet |
| `Planning` | LLM is decomposing this node into children (planning nodes only) |
| `Ready` | Children (if any) have been determined; waiting for execution slot |
| `Running` | Shell command is executing or LLM is generating |
| `Completed` | Execution succeeded; result available to parent |
| `Failed` | Execution failed; error available for backtracking decision |
| `Backtracking` | Parent LLM is re-evaluating this branch after a failure |

### 3.3 Context as a Branch Path

The key insight: **the context for any node is the path from root to that node**, not the entire tree. When a leaf node executes, the LLM context contains:

1. **Root goal** — the original user request.
2. **Branch summaries** — for each ancestor, a one-paragraph summary of what it decided and why.
3. **Sibling results** — summaries of previously completed siblings (for sequential execution) or nothing (for parallel).
4. **Current node instruction** — the specific task for this node.

This ensures that context size grows linearly with tree *depth*, not tree *breadth* or total work done. In practice, most task trees are 3–5 levels deep, keeping context well within even small model windows.

```
Context for leaf node "run bench":

┌─ Root goal: "Fix perf regression in billing service"
├─ Branch[1] summary: "Investigation phase: profiling identified
│                      N+1 query in invoice_batch_processor.rs"
├─ Branch[2] summary: "Fix phase: replaced batch loop with single
│                      JOIN query in process_invoices()"
├─ Sibling[0] result: "Edit applied successfully to line 142-158"
├─ Sibling[1] result: "cargo build succeeded (23.4s, 0 warnings)"
└─ Current task: "Run cargo bench --bench billing_bench and report
                  whether p99 latency improved"
```

### 3.4 Output Summarisation

Tool outputs (stdout/stderr) are often large — a full `cargo build` emits hundreds of lines. Putting raw output into the parent's context would defeat the purpose of the tree structure.

The summarisation pipeline:

```
Raw stdout/stderr (potentially megabytes)
        │
        ▼
  ┌─────────────┐
  │ Size check   │ ── under threshold (e.g., 500 chars) ──► use verbatim
  └──────┬──────┘
         │ over threshold
         ▼
  ┌─────────────┐
  │ LLM summary │  "Summarise this tool output in ≤3 sentences,
  │ (local/small│   preserving error messages, numbers, and file paths"
  │  model)     │
  └──────┬──────┘
         │
         ▼
  Summary string (stored in node result, propagated to parent context)
```

> **Design decision**: Summarisation uses the same LLM provider configured for the task (typically a small local model like `gemma3:4b`). This keeps it fast and private. The raw output is always persisted in the task vault's `audit.jsonl` for human review.

### 3.5 Execution Strategy: Depth-First with Controlled Parallelism

The default traversal is **depth-first, left-to-right**. This is the natural execution order for tasks with sequential dependencies (you must read the code before you can fix it).

However, the planner LLM can annotate groups of children as **parallel** when they are independent:

```json
{
  "children": [
    { "task": "Run cargo clippy", "group": "quality" },
    { "task": "Run cargo test",   "group": "quality" },
    { "task": "Run cargo bench",  "group": "quality" }
  ],
  "parallel_groups": ["quality"]
}
```

Children within a parallel group execute concurrently (bounded by `max_concurrent`, respecting the machine's resources). Children not in a parallel group execute sequentially after any preceding parallel group completes.

This is a middle ground between pure sequential (safe but slow) and pure parallel (fast but risks conflicting filesystem operations). The planner LLM, which understands the task semantics, is the right entity to make the sequencing decision.

### 3.6 Backtracking and Recovery

When a leaf node fails (non-zero exit code, timeout, LLM reports error):

1. The **parent planning node** receives the failure summary.
2. The parent LLM is invoked with the branch context plus the failure information.
3. The LLM decides one of:
   - **Retry** — re-execute the same node (up to `max_retries`, default 2).
   - **Re-plan** — discard remaining children, generate a new plan from this point.
   - **Escalate** — propagate the failure to *this* node's parent for a higher-level re-plan.
   - **Abort** — mark the entire subtree as failed with an explanation.

This gives the system structured error recovery without the chaos of an LLM trying to reason about 50 prior steps. Each planning node only reasons about its own children.

### 3.7 Security Model

**Invariant: subtasks can never widen the sandbox scope.**

```
Root sandbox scope: /Users/dev/project
        │
        ├── Subtask A: scope = /Users/dev/project (inherited)
        │       │
        │       └── Subtask A.1: scope = /Users/dev/project (inherited)
        │
        └── Subtask B: scope = /Users/dev/project/src (narrowed — allowed)
```

- The root node's sandbox scope is set at task creation (from `--sandbox-scope`, vault `workdir/`, or cwd).
- Planning nodes may specify a **narrower** scope for children (e.g., restricting a subtask to `src/`).
- No node may specify a scope wider than its parent's.
- All shell commands execute via ahma's existing shell pool with Landlock/Seatbelt enforcement.
- LLM calls are outbound HTTP and are not subject to filesystem sandboxing (same as current `decompose` and `livelog`).

### 3.8 The Orchestrator

The orchestrator is the central runtime that manages the task tree. It is a Rust async task (not a separate process) running in the ahma server's Tokio runtime.

```rust
pub struct TaskTreeOrchestrator {
    /// The task tree (nodes with parent/child relationships).
    tree: TaskTree,
    /// LLM client for planning and summarisation.
    llm: Arc<LlmClient>,
    /// Shell pool for executing commands.
    shell_pool: Arc<ShellPool>,
    /// Sandbox for path validation.
    sandbox: Arc<Sandbox>,
    /// Configuration (max depth, max retries, concurrency).
    config: TaskTreeConfig,
    /// Audit log writer.
    audit: AuditWriter,
}

impl TaskTreeOrchestrator {
    /// Entry point: execute the root goal and return the final result.
    pub async fn execute(&mut self, goal: &str) -> Result<TaskTreeResult> {
        let root = self.tree.create_root(goal);
        self.execute_node(root).await
    }

    /// Recursive depth-first execution of a single node.
    async fn execute_node(&mut self, node_id: NodeId) -> Result<NodeResult> {
        // 1. Ask LLM: "Is this task atomic, or should it be decomposed?"
        // 2. If atomic: execute as shell command or LLM call
        // 3. If composite: plan children, execute each, collect results
        // 4. Summarise result for parent context
        // 5. Handle failures via backtracking
    }
}
```

### 3.9 Integration Points

The task tree integrates with existing ahma subsystems:

| Subsystem | Integration |
|-----------|-------------|
| **Shell pool** | Leaf command nodes execute via `ShellPool::execute()`, getting the same 5–20ms startup latency |
| **Sandbox** | All paths validated by `path_security` before execution; Landlock/Seatbelt enforced per-command (macOS) or per-process (Linux) |
| **Operation monitor** | The root task is an operation with an `operation_id`; progress notifications propagate as subtasks complete |
| **Task vault** | If a vault is active, all intermediate files go to `workdir/`, outputs to `outputs/`, and every node execution is logged to `audit.jsonl` |
| **MCP interface** | A new `task_tree` tool (or extension to `run_terminal_command`) accepts a goal string and returns an `operation_id`; the tree executes asynchronously |
| **TUI** | The TUI can visualise the tree in real-time, showing node states and allowing the user to inspect, pause, or cancel branches |
| **Cluster scheduler** | In future, planning nodes can dispatch subtree branches to remote peers (see §5) |

---

## 4. Incremental Implementation Plan

The architecture is designed for incremental delivery. Each phase is independently useful.

### Phase 1: Single-Level Planning with Tool Calls

**Goal**: Extend `decompose` to interleave LLM reasoning with shell tool calls.

**What changes**:
- A new `TaskTreeOrchestrator` in a new `ahma_task_tree` crate.
- Accepts a goal string, asks the LLM to produce a flat plan (ordered list of steps).
- Each step is either a shell command (executed via shell pool) or an LLM call.
- Results flow sequentially; the LLM sees accumulated summaries.
- No recursion yet — the plan is a flat list, like a smarter sequence tool.

**Why this first**: It exercises the core orchestration loop (plan → execute → observe → next step) without the complexity of recursion or parallelism. It immediately provides value over the existing `decompose` (which cannot call tools) and `sequence` (which cannot reason).

**Deliverables**:
- [ ] `ahma_task_tree` crate with `TaskTreeOrchestrator`
- [ ] `TaskNode` enum: `ShellCommand | LlmCall | Planning`
- [ ] Single-level planning prompt and response parser
- [ ] Output summarisation for tool results exceeding threshold
- [ ] Integration with `OperationMonitor` for async result delivery
- [ ] MCP tool registration (`task_tree` tool type in MTDF)
- [ ] Unit tests with mock LLM and mock shell
- [ ] Integration test with real Ollama (gated on `AHMA_TEST_LLM=1`)

### Phase 2: Recursive Decomposition

**Goal**: Allow the planner to create sub-planners, enabling tree depth > 1.

**What changes**:
- `execute_node` becomes genuinely recursive.
- Add `max_depth` configuration (default 4) to prevent infinite recursion.
- Each planning node produces children; children may themselves be planning nodes.
- Branch context (§3.3) is constructed by walking the path from root to current node.
- Add the "is this atomic?" LLM prompt that decides whether to decompose further.

**Deliverables**:
- [ ] Recursive `execute_node` implementation
- [ ] Branch context builder (root goal + ancestor summaries + sibling results)
- [ ] `max_depth` enforcement
- [ ] Atomicity check prompt and parsing
- [ ] Tests for 3-level decomposition with mock LLM

### Phase 3: Backtracking and Recovery

**Goal**: When a subtask fails, enable structured recovery instead of aborting.

**What changes**:
- On leaf failure, parent planner is re-invoked with failure context.
- Implement retry/re-plan/escalate/abort decision.
- Add `max_retries` configuration.
- Persist failed attempts in audit log for debugging.

**Deliverables**:
- [ ] Failure handler in `execute_node`
- [ ] Recovery decision prompt and parser
- [ ] Re-plan support (discard remaining children, generate new plan)
- [ ] Escalation propagation up the tree
- [ ] Tests for failure-and-recovery scenarios

### Phase 4: Controlled Parallelism

**Goal**: Allow independent subtasks to run concurrently.

**What changes**:
- Planner output includes `parallel_groups` annotation.
- Children within a parallel group are dispatched concurrently via `join_all`.
- Bounded by `max_concurrent` to prevent resource exhaustion.
- Results from parallel groups are collected before proceeding to the next group.

**Deliverables**:
- [ ] Parallel group parsing from planner output
- [ ] Concurrent execution with bounded parallelism
- [ ] Result aggregation for parallel groups
- [ ] Tests verifying independence (parallel tasks do not interfere)

### Phase 5: TUI Visualisation

**Goal**: Show the task tree in the terminal UI with real-time updates.

**What changes**:
- Tree view widget in `ahma_tui` showing nodes and their states.
- Live updates via the existing MCP notification channel.
- User can expand/collapse branches, inspect node details, cancel subtrees.

**Deliverables**:
- [ ] Tree widget in `ahma_tui`
- [ ] Real-time node state updates
- [ ] Interactive controls (cancel, inspect, retry)

### Phase 6: Cluster Distribution (Future)

See §5 for the distributed execution design. This phase is deferred but the architecture from Phases 1–5 is designed to support it.

---

## 5. Future: Distributed Task Trees

> **Not in scope for initial implementation.** This section documents architectural decisions that keep the single-machine design compatible with future distribution.

### 5.1 The Vision

A cluster of machines (LAN or Tailscale mesh), each running `ahma serve http`, can collaborate on a task tree. The coordinator (the machine where the task was initiated) delegates entire subtree branches to peers. Each peer:

- Has its own kernel sandbox (scope set to a vault `workdir/` synced to the peer).
- Has its own local LLM (e.g., Ollama with `gemma3:4b`).
- Executes the subtree branch independently.
- Returns the result (summary + artifacts) to the coordinator.

### 5.2 Why This is a Good Fit

The task tree model maps naturally to distribution because:

1. **Subtrees are self-contained.** A subtree branch has a defined input (the branch context) and a defined output (the node result summary). There is no shared mutable state between branches.
2. **Security scopes are inherited.** A remote peer can be given a scope no wider than the subtree's scope. The coordinator never sends paths outside the sandbox.
3. **Message passing replaces shared memory.** The only data flow between nodes is the context (input) and result (output) — both are serialisable strings. This is the same whether the nodes are in the same process or on different machines.

### 5.3 Design Decisions for Future Compatibility

| Decision | Rationale |
|----------|-----------|
| **Node results are serialisable summaries, not raw stdout** | Raw stdout may be gigabytes; summaries are kilobytes. This is essential for network transfer |
| **Branch context is constructed from the tree path** | A remote peer needs only the root-to-node path context, not the entire tree state |
| **No shared mutable state between sibling nodes** | Siblings in a parallel group must not share files. This constraint, enforced for correctness on one machine, also enables distribution |
| **Vault-based artifact exchange** | Artifacts (files produced by a subtask) are written to the vault, not passed inline. On a cluster, the vault is synced (e.g., via rsync or a shared NFS mount) |
| **HMAC-signed task manifests** | The existing cluster scheduler already signs task manifests. Subtree dispatch reuses this |

### 5.4 Transport Considerations

| Transport | Local (pipes) | Remote (network) |
|-----------|---------------|-------------------|
| **Node context → executor** | In-memory struct passed to async task | Serialised JSON POST to peer's `/mcp` endpoint |
| **Tool stdout → summariser** | Rust `tokio::io::AsyncRead` stream | Same (tool runs on the remote peer, summarisation is local to the peer) |
| **Node result → parent** | In-memory `NodeResult` struct | Serialised JSON in HTTP response |
| **Streaming intermediate results** | `tokio::sync::watch` channel | SSE event stream over HTTP |

**On pipelining**: Current LLMs require the complete context before generating a response — there is no benefit to streaming partial input to the LLM. However, *tool output* benefits from streaming: the summariser can begin tokenising the output while the tool is still running, and the orchestrator can detect early failures (e.g., a compilation error on line 1) without waiting for the full output. The architecture supports streaming tool output via `AsyncRead`, with the summariser consuming it in chunks.

### 5.5 Job Bidding (Sketch)

In a future version, subtree dispatch could use a bidding protocol:

1. Coordinator broadcasts a `TaskBid` request (subtree summary, required model, estimated complexity).
2. Peers respond with bids (available capacity, estimated completion time, loaded models).
3. Coordinator selects the best bid and dispatches the subtree.
4. If no peer bids within a timeout, the coordinator executes locally.

This replaces the current "pick least-loaded peer" heuristic with a market-based approach that naturally handles heterogeneous hardware (a machine with a fast GPU bids lower latency).

---

## 6. Key Design Constraints

### 6.1 LLM Interaction Model

Current LLMs are **batch processors**, not stream processors. They require the complete input context before generating output. This has profound implications:

- **No speculative execution.** You cannot start a subtask before its predecessor's result is summarised and added to the context.
- **No incremental context updates.** You cannot "append" to an ongoing LLM generation; you must make a new call.
- **Summarisation is a bottleneck.** Every tool output must be summarised before the next planning step can begin. The summarisation LLM call adds latency.

**Mitigation**: Use the smallest effective model for summarisation (a 3B model can summarise stdout effectively). Run summarisation in parallel with non-dependent work. Cache common summaries (e.g., "cargo build succeeded with 0 warnings" is a fixed string, no LLM call needed).

### 6.2 Context Window Budget

For a tree of depth *d*, the branch context contains *d* ancestor summaries plus sibling results. Budget:

| Component | Typical size | Notes |
|-----------|-------------|-------|
| Root goal | 50–200 tokens | User's original request |
| Ancestor summary (each) | 100–300 tokens | One paragraph per ancestor |
| Sibling result (each) | 50–200 tokens | Summarised output |
| Current task instruction | 100–500 tokens | Generated by parent planner |
| System prompt | 200–500 tokens | Fixed orchestrator instructions |

For a depth-4 tree with 3 completed siblings: ~500 + 4×200 + 3×100 + 300 + 400 = **2,300 tokens**. This fits comfortably in even a 4K context window, leaving ample room for the LLM's response.

### 6.3 Failure Modes

| Failure | Detection | Recovery |
|---------|-----------|----------|
| Tool returns non-zero exit | Exit code check | Backtrack to parent planner |
| Tool output exceeds size limit | Byte count | Truncate + summarise tail |
| LLM produces unparseable plan | Schema validation | Retry with clarified prompt (max 2) |
| LLM enters infinite decomposition | `max_depth` limit | Force execution as atomic at max depth |
| LLM hallucinates non-existent tool | Tool name validation | Error + re-plan without that tool |
| Timeout | `tokio::time::timeout` | Kill process, report timeout to parent |
| Sandbox violation | Kernel rejection (EACCES) | Report to parent; cannot retry (path is genuinely outside scope) |

---

## 7. Relation to Existing Ahma Components

```
                    ┌─────────────────────────────────────────┐
                    │              ahma_task_tree              │  ◄── NEW
                    │  (TaskTreeOrchestrator, TaskNode, etc.)  │
                    └────┬──────────┬───────────┬─────────────┘
                         │          │           │
            ┌────────────▼──┐  ┌───▼─────┐  ┌──▼──────────────┐
            │ ahma_decompose│  │ahma_mcp │  │  ahma_cluster   │
            │ (Reducer only)│  │(ShellPool│  │  (distribution) │
            └───────────────┘  │ Sandbox) │  └─────────────────┘
                               └───┬──────┘
                                   │
                         ┌─────────▼────────────┐
                         │  ahma_llm_monitor     │
                         │  (LlmClient for       │
                         │   planning + summary)  │
                         └───────────────────────┘
```

- **`ahma_task_tree`** (new crate) — the orchestrator, tree data structures, and planning prompts.
- **`ahma_decompose`** — the `Reducer` is reused for aggregating parallel group results. The `DecomposeOrchestrator` becomes a simplified frontend that creates a single-level task tree.
- **`ahma_mcp`** — provides `ShellPool` for command execution and `Sandbox` for path validation. The `MCP service` registers the `task_tree` tool.
- **`ahma_llm_monitor`** — provides `LlmClient` for all LLM calls (planning, summarisation, atomicity checks).
- **`ahma_cluster`** — in future, the orchestrator delegates subtree branches to remote peers via the cluster scheduler.
- **`ahma_vault`** — task trees run inside a vault; each node's execution is logged to `audit.jsonl`.

---

## 8. Open Questions

1. **Planning prompt design.** The quality of the task tree depends heavily on the planning prompt. How structured should the expected output be — free-form text, JSON, or a constrained schema? JSON is more parseable but harder for small models. Recommendation: start with numbered-list format (like current `decompose`), add JSON schema once the orchestration loop is proven.

2. **Model selection per node type.** Should planning nodes use a larger model than leaf execution nodes? This optimises cost but adds configuration complexity. Recommendation: single model initially, add per-node-type model override in Phase 3 or later.

3. **Depth limit tuning.** The default `max_depth` of 4 is a guess. Too shallow forces atomic execution of complex tasks; too deep wastes tokens on unnecessary decomposition. Recommendation: start at 4, instrument actual depth usage, adjust based on data.

4. **Parallel safety.** How do we verify that parallel siblings truly do not conflict? File-level locking? Directory-level isolation? Recommendation: start with advisory annotation by the planner LLM, add file-watch detection in a later phase.

5. **Human-in-the-loop checkpoints.** Should the user be asked for approval at certain tree depths? This is valuable for safety but breaks autonomy. Recommendation: configurable `checkpoint_depth` — if set, the orchestrator pauses and notifies the user before executing nodes deeper than this level.

---

## 9. Success Metrics

| Metric | Target | Measurement |
|--------|--------|-------------|
| **Tasks completable** | Complex tasks (20+ steps) that currently require manual LLM orchestration should complete autonomously | End-to-end test suite with representative tasks |
| **Context efficiency** | Branch context size < 3,000 tokens at depth 4 | Token count instrumentation |
| **Failure recovery rate** | ≥ 70% of recoverable failures handled without user intervention | Backtracking success rate in audit logs |
| **Throughput improvement** | ≥ 2x speedup on parallelisable tasks vs. sequential execution | Wall-clock time comparison |
| **LLM cost per task** | ≤ 50% of equivalent cost for monolithic execution (due to smaller per-call contexts) | Token usage tracking |

---

## See Also

- [docs/decompose.md](decompose.md) — current single-level decomposition (predecessor)
- [docs/cluster-scheduler.md](cluster-scheduler.md) — distributed peer scheduling
- [docs/task-vault.md](task-vault.md) — per-task isolated working directories
- [docs/worker-synthesis.md](worker-synthesis.md) — sandboxed code execution
- [SPEC.md §5.6](../SPEC.md) — decompose tool type specification
- [SPEC.md §2.3](../SPEC.md) — async-first architecture

# Critical Analysis and Action Plan for the above proposal

 Conversation with Gemini

I want you to expand on the design below. Give your ciritcal review improvements to this approach. Accepting your improvements to the design, Do additional research on exactly what and how to do each step in this architecture, and apply that to https://github.com/paulirotta/ahma/ in particular you can find the details in README.md, /docs, and several SPEC.md documents in that repo.


One addition: we should consider scoping down (never up) the sandbox (both drive and MCP access of each LLM and command line tool in the chain, and extend this to include dynamic scoping of network access- this needs careful thought and planning to be both safe and usable in a practical world without driving users mad with restrictions such that they just YOLO or choose a different tool if this is not sufficiently easy to use. Asking questions of the user should be seen as a form of low quality and strictly avoided. We ask a few questions up front when the process starts, then it either completes with adaptation along the way or it fails, only if the user opts in to further questions do we bother them once the process has started.

The result should be suitable for both coding work and business work like Claude Cowork. We do not see any difference- it is all knowledge work. 

