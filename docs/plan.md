---
name: Ahma vs Cowork Strategy
overview: A critical comparison of Ahma vs Claude Cowork on security and use-case axes, plus a prioritized roadmap that turns Ahma's existing kernel-sandbox + async-first + OpenAI-compatible-LLM primitives into a non-developer, local-LLM-first agent platform that closes Cowork's known weaknesses by design.
todos:
  - id: m1-task-vault
    content: "M1 — Task Vault primitive: ahma vault create CLI, --task-vault flag, per-vault audit.jsonl, stage-don't-delete trash; extend docs/security-sandbox.md"
    status: in_progress
  - id: m2-decompose
    content: "M2 — Local-LLM decomposition tool: tool_type: decompose in MTDF, reuse ahma_llm_monitor::client, sub-task dispatch, deterministic Rust reducers, .ahma/decompose.json"
    status: pending
  - id: m3-tui-egress
    content: "M3 — TUI control plane (ratatui) + egress sandbox: ahma tui subcommand, ahma egress proxy, per-vault egress.allowlist, HTTP_PROXY plumbing into shell pool"
    status: pending
  - id: t2-1-artifact-channel
    content: T2.1 — Interactive HTML+WASM artifact output channel with per-task localhost API and embedded local-LLM chat (post-M3)
    status: pending
  - id: t2-2-worker-synthesis
    content: T2.2 — Ephemeral Rust/Python worker code synthesis inside sub-vault (post-M3)
    status: pending
  - id: t2-3-bundle-index
    content: T2.3 — Signed bundle index + ahma bundle audit using existing skill-scanner patterns
    status: pending
  - id: t2-4-audit-otel
    content: T2.4 — Audit JSONL + optional OTel/SIEM forwarder (uses opentelemetry deps already in Cargo.toml)
    status: pending
  - id: t3-1-cluster
    content: T3.1 — Best-available-now local cluster scheduler (mDNS/Tailscale discovery, per-worker sandbox, signed task manifests over QUIC)
    status: pending
  - id: t3-2-library
    content: T3.3 — ahma-core Rust crate for embedding
    status: pending
  - id: t3-3-renewal
    content: T3.3 — Renewal contract for long unattended tasks (checkpoint, halt, re-approve)
    status: pending
isProject: false
---

## 1. Critical comparison: Ahma vs Cowork

### Trust-boundary architecture (the pivotal axis)

| Concern | Cowork (per the report) | Ahma (today) |
|---|---|---|
| **Code execution isolation** | macOS VZVirtualMachine — strong but heavyweight (one VM per sandbox) | Kernel sandbox (Landlock / Seatbelt / AppContainer+JobObjects) — lightweight per process, no VM |
| **File-system blast radius** | Folder grants survive whole session; users grant `Documents`/`~` for convenience | Scope set once at `initialize`, **kernel-enforced**, **cannot widen** without restart (`SPEC.md` R5.1, R5.4, R10.4) |
| **Computer-use** | Direct screen/click control bypasses Cowork's other permission gates | Not a feature. Ahma is intentionally CLI-shaped — no clicks, no banking app interaction |
| **Web fetch / MCP egress** | Carve-out: bypasses org egress policy (per report §2) | Not addressed. `livelog` LLM endpoint is an outbound call from the ahma process; no per-task domain allowlist |
| **Plugin / MCP supply chain** | Untrusted-by-default; report cites malware in plugins | MTDF JSON schema validation at startup (R4); skill-scanner exists; no signed bundle index yet |
| **Audit visibility** | Excluded from Audit Logs / Compliance API; OpenTelemetry only | OpenTelemetry deps present; no per-task audit JSONL contract documented |
| **Long unattended runs** | Dangerous: scheduled tasks + injection runway | Async ops are first-class but no checkpoint/renewal contract for long sessions |
| **Prompt injection containment** | Filters/classifiers; ~1% bypass rate | Same risk surface, but **the kernel sandbox bounds the consequences** even on bypass — agent can't write outside scope, period |

### Resource usage under concurrent secure actions

This is where the architectures diverge most:

- **Cowork**: A VM-per-isolation-unit model is the strong containment lever. True parallel "secure actions" with separate trust boundaries push you toward multiple VMs. RAM cost per VM (Linux rootfs + kernel) is on the order of hundreds of MB; spin-up is seconds. So *scaling concurrent isolated actions* on a single laptop is a hard wall — most users will run one VM and serialize, weakening isolation in practice.
- **Ahma**: Kernel sandboxing is per-process; applying a Landlock ruleset or starting under `sandbox-exec` adds microseconds, not seconds, and ~zero RAM. The shell pool (`SPEC.md` R3) gives 5–20ms command startup. **Concurrent "secure actions" are the cheap path, not the expensive one.** This is exactly why Ahma can offer async-first parallel ops without compromising the boundary.

### Use-case fit today

- **Cowork**: Aimed at consumers and knowledge workers — runs apps, browses, schedules. High capability surface, large attack surface.
- **Ahma**: Aimed at developers in IDEs (Cursor / VS Code / Claude Code) — runs CLI tools (`cargo`, `git`, `python`, `gh`) under a hard FS boundary, plus livelog LLM analysis. Low attack surface, narrow audience.

### Where Cowork wins (and Ahma should learn)

- **One-app, non-developer onboarding**: Download an installer, point at a folder, go. Ahma needs an MCP client today.
- **Task ergonomics**: Scheduled tasks, mobile triggers, computer use — strong UX hooks even if security-fragile.
- **Skills / plugin marketplace**: Capability extension by non-developers.

### Where Ahma wins (and should double down)

- **Sandbox is the boundary, not a hint**: Cowork's "create a per-task working folder, never grant `Documents`" guidance is *user discipline*; in Ahma, the kernel enforces it.
- **Async-parallel secure actions are cheap.**
- **Local-LLM-ready today**: `livelog` already speaks OpenAI-compatible, used with Ollama out of the box.
- **MCP-native, IDE-portable.**

---

## 2. Cross-cutting principles for the roadmap

These principles should anchor every later milestone:

1. **Kernel sandbox first, classifiers never.** Defenses must not depend on a prompt-injection classifier as the trust boundary.
2. **Local-LLM by default.** Cloud LLMs require explicit per-task opt-in, not a global setting.
3. **Capability is request-scoped, not session-scoped.** Every elevation (write outside vault, network domain, deletion) is a one-shot grant tied to one `operation_id`.
4. **No long unattended runs without a renewal contract.** A task running > N minutes unattended must produce a checkpoint and *stop*, requiring re-approval. This closes Cowork's "scheduled task drift" risk by design.
5. **Stage, don't delete.** Two-phase delete: first move to `~/.ahma/trash/<task>/`, only purge after explicit per-batch user confirmation.
6. **Read-only on a copy by default.** Most exploration is on copied inputs in the task vault.

---

## 3. Prioritized roadmap

### Tier 1 — high ROI, builds directly on existing primitives

**T1.1 Task Vault: per-question dedicated working folders (foundation for everything else)**

The single most useful idea in the user's brainstorm. Promote "create a per-task folder" from user discipline to kernel-enforced architecture.

- Each user question creates `~/.ahma/tasks/<utc-date>-<slug>-<uuid>/` with `inputs/`, `workdir/`, `outputs/`, `trash/`, `audit.jsonl`.
- Inputs are *copied in* (not symlinked) by an orchestrator step.
- A per-task `ahma` subprocess is spawned with `--sandbox-scope` set to that folder (HTTP-bridge `--session-isolation` already gives us almost exactly this — see `ahma_http_bridge/`).
- Reuses existing `SPEC.md` R10 (Session Isolation) machinery.
- Closes Cowork's "user grants whole `Documents` for convenience" problem: there *is no* whole-Documents option.

**T1.2 Local-LLM orchestrator: split big problems → many small local-LLM tasks**

Make Ahma the conductor, not the soloist.

- New tool type `decompose` (sibling to `livelog`, `command`, `sequence` in [`SPEC.md`](SPEC.md) §5).
- Input: a business question + budget (max sub-tasks, max wall time, target model size).
- Output: a DAG of small sub-questions, each runnable on `gemma:4b` / `llama3.2:3b` via the existing `LlmProviderConfig` (`ahma_llm_monitor/src/client.rs`).
- Sub-tasks dispatch to a worker pool; results aggregate via a deterministic Rust reducer in the orchestrator.
- Each sub-task runs in its own task vault subdirectory under the parent vault.

**T1.3 TUI control plane (before GUI)**

`ratatui`-based dashboard, ships in the same binary, works over SSH, no Electron.

- Panels: active tasks, per-task chat thread, live log tail, resource usage, approval prompts.
- One shortcut to "open this task vault in Finder/Explorer".
- Approval gates inline: every elevation request appears here with a one-line summary and accept/deny.

**T1.4 Egress sandbox (close the Cowork web-fetch carve-out)**

- Add an outbound proxy (`ahma egress`) that the per-task subprocess must use (`HTTP_PROXY` env in the sandbox).
- Per-task domain allowlist defined in the task vault manifest.
- Default allowlist: empty. Cloud LLM domain only added when the user explicitly opts that task into a cloud model.
- This closes the report §2 "Network egress carve-outs" gap — and it works *because* the sandbox process can't simply set its own env or escape via DNS rebinding (kernel FS sandbox prevents resolver tampering and proxy bypass via local config writes).

### Tier 2 — high value, more substantial

**T2.1 Interactive HTML+WASM result artifacts**

- Tools can emit `outputs/result.html` containing data + a tiny WASM Rust module compiled from a per-result template.
- HTML opens in the user's normal browser; talks back to ahma over a localhost-only HTTP endpoint scoped to that one task vault and one short-lived bearer token.
- Embedded LLM chat in the artifact (using local Ollama by default) lets the user keep iterating *without* re-engaging the orchestrator agent.
- The artifact is part of the task vault, so it survives, is auditable, and can be re-opened.

**T2.2 Ephemeral worker code synthesis (Rust or Python)**

- Agent generates a small, single-purpose program for one task (e.g. "rename these 100 files according to this rule").
- Ahma compiles (rustc) or runs (python venv) it inside a sub-vault, captures output to `outputs/`, deletes the source after execution unless `--keep` was set.
- Stronger than "agent runs commands directly" because the synthesized program is *deterministic code without an LLM in the loop* — it can't be re-injected mid-run.

**T2.3 First-party signed bundle index + supply-chain auditor**

- Curated list of MTDF tool bundles + signing keys.
- `ahma bundle audit <path>` runs the existing `skills/skill-scanner` patterns over a candidate bundle.
- Closes the "plugin marketplace contains malware" risk class by inverting the default: third-party bundles require explicit `--allow-unsigned` per task.

**T2.4 Audit channel + SIEM forwarding**

- Per-task `audit.jsonl` with: task id, decomposition tree, every tool call (args + redaction policy), every elevation grant, every artifact written outside the vault.
- Optional OTel forwarder using the dependencies already in `Cargo.toml` (`opentelemetry`, `opentelemetry-otlp`).

### Tier 3 — longer horizon

**T3.1 Best-available-now local cluster scheduler**

- mDNS/Tailscale discovery of `ahma worker` peers (machines you or your company own).
- Each worker advertises: model inventory (`ollama list`), free RAM, current load.
- Scheduler routes each sub-task from T1.2 to the best-available worker for that model size.
- Each remote worker runs in its own kernel sandbox with task-vault contents shipped over via local QUIC (Ahma already has HTTP/3 client preference per `SPEC.md` R8.7).
- Important: the worker accepts only signed task manifests from a known peer — the task vault folder is the unit of work that travels.

**T3.2 Library packaging: `ahma-core` crate**

- `ahma-core` (Rust): exposes Sandbox, TaskVault, Orchestrator as a library so other Rust apps can embed them.
- Mobile: out of scope for the next year. Mobile sandboxing primitives differ enough (App Sandbox on iOS, SELinux on Android) that they need separate design.

**T3.3 Renewal contract for long unattended tasks**

Concrete mechanism for principle 4 above: a task that runs > `T_renew` minutes unattended produces a checkpoint to its vault, signals the user (TUI / push), and *halts* until re-approved. Schedules become approvable plans, not authorizations.

### Tier 4 — explicitly defer

- Computer-use (clicks, screen control) — too big a capability surface for the secure-by-default story; only consider after T1 and T2 land, and even then only inside a task vault that has no real apps to click on.
- Mobile native execution — defer to T3.3 outcome.
- Banking / payments / account-state-changing tasks — out of scope until the trust model has matured for at least a year of T2 production use.

---

## 4. Concrete first three milestones (if the user wants to start now)

These map onto the existing codebase with minimal restructuring:

1. **M1 — Task Vault primitive** (~2–3 weeks)
    - New `ahma vault create <slug>` CLI subcommand → creates the vault tree, returns its path.
    - `--task-vault <path>` flag on `ahma --mode http` — equivalent to `--sandbox-scope <vault>` but also wires per-vault `audit.jsonl` and stage-don't-delete trash directory.
    - Audit JSONL writer in `ahma_mcp/src/`.
    - File: extend [`docs/security-sandbox.md`](docs/security-sandbox.md) with the Task Vault section.

2. **M2 — Local-LLM decomposition tool** (~3–4 weeks)
    - New `tool_type: "decompose"` in MTDF schema (extend [`SPEC.md`](SPEC.md) §5).
    - Reuse `ahma_llm_monitor::client` for the OpenAI-compatible call.
    - Sub-task dispatch via existing async operation lifecycle.
    - Reducer is plain Rust per-task-type; ship reducers for "summarize", "extract-fields", "classify".
    - Bundled config in `.ahma/decompose.json`.

3. **M3 — TUI + egress sandbox** (~4–6 weeks, can run in parallel)
    - `ahma tui` subcommand using `ratatui`.
    - `ahma egress` proxy + per-vault `egress.allowlist` file.
    - HTTP_PROXY plumbing into the sandboxed shell pool.

After M3 lands, the artifact channel (T2.1) is the next high-leverage step because it converts every Ahma run into something a non-developer can interact with — which is the gating capability for the consumer use cases the user described.

---

## 5. Why this beats Cowork on its own terms

- **Same containment per task, much higher concurrency** — kernel sandbox per process scales to dozens of secure actions on one laptop where Cowork's VM model serializes them.
- **No prompt-injection-driven blast radius** — the kernel boundary is unaffected by a 1% classifier-bypass.
- **Local-LLM-first** — privacy is a default, not a deployment choice.
- **Audit by construction** — task vault + audit.jsonl is the unit of accountability, unlike Cowork's "excluded from Audit Logs" gap.
- **No computer-use gap** — Ahma never adds a capability that bypasses its own permission gate; new capabilities (T2.1 artifact channel, T2.2 worker synthesis) are enforced *through* the vault, not around it.