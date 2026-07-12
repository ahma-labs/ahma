# Execution Plan: Unified Permissions Model, Question Ladder, and TUI Operation Identity

**Status**: ✅ **Complete** — all phases landed on `main` (PRs #455–#461). Retained as
the design record: it explains *why* each piece is shaped the way it is. For the
user-facing guide see [`permissions.md`](permissions.md); for the requirements see
SPEC R-PERM.
**Audience**: An implementing AI agent (or human) with full workspace access
**Origin**: Architecture/UX synthesis session, 2026-07-12 (main @ `c6fb17e9`)
**Scope**: This plan converges existing mechanisms — it introduces almost no new invariants. Every phase applies principles already in SPEC.md: *nothing silent, no self-widening, ask only at the moment of genuine need, fail closed to a shown default.*

---

## 0. Context: the problem being solved

Three intertwined problems, one root cause (fragmentation of already-built pieces):

1. **Hooks are disabled by default** because when a sandboxed command is denied, there is no realistic UX for the user to grant an exception. The pieces (kernel denial detection, grant coordinator, elicitation, TUI modal, persistent grants) all exist but are not wired into one loop.
2. **App-specific carve-outs are hard-coded** (`.cargo`, `.rustup`, `.nvm`, etc. in the sandbox backends). These "crutches" work for the Rust-developer case but cannot scale to the thousands of apps we cannot anticipate — and they are invisible to the user.
3. **TUI monitor/chat labels are meaningless** (`op_41_echo_hello`, raw tool names) because the hub wire format doesn't carry the information needed to render a human-meaningful operation identity. Late-attach replay works but replays bad labels.

The unifying design decisions (each is a phase below):

- **One ledger**: all persistent permissions live in `~/.ahma/` (already kernel-unreadable/unwritable from inside the sandbox per SPEC R5.4.8) with one record shape and one CLI.
- **One question ladder**: harness (MCP elicitation) → attached ahma TUI (modal) → fail closed with a copy-pasteable CLI remediation. Harness preferred when functioning; demote on timeout/error, not on decline.
- **Crutches become data**: hard-coded toolchain carve-outs become shipped, visible, disableable *profiles* that flow through the same code path as user grants.
- **One operation identity**: server-computed `title` + `cwd` + `exit_code` + `origin` on the hub wire; one identity line rendered identically in chat, monitor, grant prompts, and logs.
- **Hooks readiness = ladder-complete**, per client, not classification-perfect globally.

### Hard constraints (user-stated, verbatim intent — do not violate)

- Persistent permissions go in **the single config dir which the ahma sandbox NEVER includes automatically** (`~/.ahma/`), modifiable **one-by-one** only when **the user previews and approves the exact change**.
- The user must be asked **clearly, with context about what they are granting**.
- **No app-specific exceptions hard-coded** into ahma binaries after Phase 3.
- The user must not be hassled: ask at most once per `(subject, access)` per session; defaults reasonable.

### SPEC invariants that must survive every phase

- **R5.1** lock-once: the live MCP-server sandbox scope cannot widen mid-session. Grants for the server path take effect at next start (the success text in `sandbox_grant_tool.rs:556` already says this — keep it true). Hooks are different: they re-derive the sandbox per command, so grants apply on the *next command* (this is a feature, see Phase 4).
- **R5.3.x** downgrade-only prompting; in any confirm UI, **Enter alone must never widen** scope; Esc/Enter default to deny.
- **R5.4** scope always visible with provenance.
- **R5.4.4–R5.4.8** persistent grants: two-gate `sandbox_grant`, hard denylist (fs root, `$HOME` exact, ancestors of live scopes, credential dirs, OS dirs), `~/.ahma` never in scope.
- **R7.2/R7.5** defer-to-host in nested sandboxes; disclosure honesty ("host detected ≠ host sandbox proven enabled").
- **R24.5** hub wire format may evolve by *adding fields only*; unknown fields ignored by old readers.

### Ground rules for the implementing agent

- Follow `AGENTS.md`/`CLAUDE.md` (async I/O rules, `emit_stdout_notification` for stdout JSON-RPC, cross-platform test checklist, tempdir isolation).
- Each phase lands as its own PR, `[crate] description` title, conventional commits.
- Definition of done per phase: `cargo fmt --all && cargo clippy --all-targets && cargo nextest run` green, plus `cargo nextest run --workspace --run-ignored all` on your platform.
- Update `SPEC.md` status tables in the same PR as the code they describe.
- Line numbers below are anchors as of `c6fb17e9`; re-locate by symbol name if drifted.

---

## Phase 0 — SPEC deltas (codify before coding)

**Goal**: Write the target model into SPEC.md so later phases implement against a spec, not a chat transcript.

**Files**: `SPEC.md` only.

### Steps

1. Add a new requirement family **R-PERM** (place after R5, cross-reference R5.4.x and R-WEB.5/6/7):
   - **R-PERM.1 (one ledger)**: All persistent permissions — filesystem scope grants, web-domain grants, per-workspace tool approvals, hook fall-open consent — are stored under `~/.ahma/` and nowhere else. `~/.config/ahma/` is retired (Phase 1 migrates it). The ledger directory is never included in any sandbox scope automatically (restates R5.4.8; R-PERM inherits it).
   - **R-PERM.2 (one record shape)**: every grant is representable as
     `{kind: fs-scope | web-domain | tool | hook-unsandboxed, subject, access, tier: once|session|always, granted_by, granted_at, surface, note}`.
     `once` is never stored. `session` lives only in memory. Only `always` is written to disk, and only after a preview-and-approve exchange showing the exact file and exact line to be written (R5.4.6 pattern, generalized).
   - **R-PERM.3 (question ladder)**: when a permission question must be asked, surfaces are tried in order: (a) the initiating MCP client via `elicitation/create` *iff* it advertised the elicitation capability and has not been demoted this session; (b) an attached ahma TUI via the grant modal (R-WEB.6 semantics: Enter/Esc deny, persist option shows file+line); (c) no surface → **fail closed** with a structured `sandbox_denial` payload and a copy-pasteable `ahma sandbox grant <path> --ro|--rw` remediation. Demotion rule: an elicitation **timeout or transport error** demotes the harness for the rest of the session ("one strike"); a **decline is an answer**, not a demotion. When a question is answered on one surface, other live surfaces are notified (existing `decision_id` fan-out, first-answer-wins, most-restrictive-wins per R5.3.3/R5.3.4). The user is always told *where* the question went if a fallback occurred.
   - **R-PERM.4 (ask-once)**: at most one question per `(subject, access)` per session, across all surfaces and all concurrent operations (existing `GrantCoordinator` dedup, elevated to a requirement).
   - **R-PERM.5 (profiles)**: toolchain carve-outs ship as declarative profile files (data), folded into scope through the same code path as user grants, visible with provenance `builtin-profile(<name>)`, individually disableable via `[sandbox] profiles`. No toolchain paths in Rust source. Platform limitations that cannot be expressed as profiles (macOS blanket read-allow) must be disclosed in every scope display.
   - **R-PERM.6 (hooks gating)**: hooks may be enabled per-client only when that client passes the readiness checklist in Phase 4 (denial→grant loop round-trips; fail-closed message legible in that client; defer-to-host default in nested sandboxes).
2. Amend **R24** (task tree / observability): `OpStarted` gains `title`, `cwd`, `command`, `origin`; `OpFinished` gains `exit_code: Option<i64>`. Define the **operation identity line** format (Phase 5) as the required rendering in chat history, monitor rows, grant prompts, and per-op log naming.
3. Update the R5.5.3 hooks section to reference R-PERM.6 instead of the blanket "not ready" language.

### Acceptance

- SPEC.md builds a coherent story: someone reading only R-PERM + amended R24 could reconstruct Phases 1–5.
- No existing requirement contradicted; cross-references added both directions.

---

## Phase 1 — One ledger: unify persistence under `~/.ahma/`

**Goal**: collapse the two config trees into one, add the unified record shape and a single `ahma permissions` CLI, without changing any grant *semantics*.

**Current state (verified anchors)**:
- Tree A: `ahma_common/src/config.rs` — `ahma_home_dir()` (:533), `settings_path()` (:549) → `~/.ahma/settings.toml`; `[sandbox] persistent_scopes` (:870); `PersistentScope` (:770). Writer: `ahma_common/src/scope_grant.rs::persist_grant` (:288) — atomic, refuses to clobber corrupt settings (tests at :475–:560).
- Tree B: `ahma_core/src/approvals.rs` — `config_dir()` (:32) honors `AHMA_CONFIG_DIR` else `dirs::config_dir()/ahma` → `~/.config/ahma/approvals.json`; per-workspace tool approvals (`remember_tool_approval` :159, `is_tool_approved` :139).
- Also in scope: web-domain grants (`ahma_common/src/web_policy.rs`, `web_approval.rs`) and hook consent markers (`ahma_mcp/src/hooks/consent.rs`, `ahma_common/src/hook_consent.rs`) — audit where each persists and route through the same dir.

### Steps

1. **Introduce the unified record type** in `ahma_common` (new module `permissions.rs` or extend `scope_grant.rs`):
   ```rust
   pub enum GrantKind { FsScope, WebDomain, Tool, HookUnsandboxed }
   pub enum GrantTier { Once, Session, Always }
   pub struct GrantRecord {
       pub kind: GrantKind,
       pub subject: String,        // path, domain, tool name, or client id
       pub access: Option<String>, // "ro"|"rw" for fs; None otherwise
       pub tier: GrantTier,
       pub granted_by: String,     // "user", "builtin-profile(rust)", client name
       pub granted_at: String,     // RFC3339
       pub surface: String,        // "harness:cursor" | "tui" | "cli" | "setup"
       pub note: Option<String>,
   }
   ```
   Keep `PersistentScope` as the on-disk TOML shape for `kind=fs-scope` (backward compatible); add sibling TOML tables for the other kinds in `settings.toml` (e.g. `[[permissions.tool]]`, `[[permissions.web]]`). Do **not** invent a second file unless `settings.toml` sections become unwieldy — one file, one preview, one diff.
2. **Migrate `approvals.json`**: move read/write logic into the unified store. On first run, if `~/.config/ahma/approvals.json` exists, migrate its entries into `settings.toml` (`kind=tool`, `granted_by="migrated"`), log `info!` once, and leave the old file with a `.migrated` suffix (don't delete user data). Keep `AHMA_CONFIG_DIR` honored for tests but pointing at the unified dir; deprecate it with a `warn!` (matches the retired-env-var pattern in `shell/cli/mod.rs:2313`).
3. **Session-tier store**: an in-memory `SessionGrants` map (per server instance) consulted before disk. `GrantCoordinator` (`scope_grant.rs:139`) already dedups per session — extend it to record the answered tier so "session" answers suppress re-asks without touching disk.
4. **One CLI**: add `ahma permissions list|grant|revoke` (in `ahma_mcp/src/shell/cli/`), where:
   - `list` shows every record with kind, subject, access, tier, provenance (`granted_by`/`surface`), and — after Phase 3 — profile-sourced entries.
   - `grant`/`revoke` reuse the existing two-phase preview machinery (`sandbox_grant_tool.rs::preview_text` :497): print the exact file path and exact line(s) to be added/removed, require explicit confirmation.
   - Keep `ahma sandbox grant` as a kind-scoped alias (it's already the remediation string emitted in denials — don't break it).
5. **Audit trail**: append one JSONL line per persist/revoke to `~/.ahma/permissions-audit.jsonl` (subject, kind, tier, surface, timestamp). Reuse the R-WEB audit-log pattern if one exists; otherwise create minimal append-only writer (async I/O, `tokio::fs`).
6. **Denylist parity**: `classify_grant_risk` (`sandbox_grant_tool.rs:307`) must gate *every* fs-scope write path into the ledger, including the new CLI — refactor so CLI and MCP tool call the same gate. Non-fs kinds get kind-appropriate gates (e.g., refuse `web-domain: *`).

### Tests

- Migration test: fabricated `approvals.json` in a tempdir `AHMA_CONFIG_DIR` → entries appear in unified store, old file renamed, second run is a no-op.
- `persist_grant` regression tests still pass unchanged (`scope_grant.rs:475+`).
- CLI round-trip: `grant` preview does not write; confirmed grant writes exactly the previewed line; `revoke` removes it; `list` shows provenance.
- Denylist: CLI path refuses `$HOME`, fs root, `~/.ahma` itself.
- Cross-platform: no hard-coded `/tmp`, use `test_utils::path_helpers`.

### Acceptance

- `~/.config/ahma/` is written by nothing; `grep -rn "config_dir\|approvals.json"` shows only migration code.
- One file (`settings.toml`) holds all `always` grants of all kinds; one CLI manages them; every write previewed.
- Docs: fix `docs/security-sandbox.md:188` (says `~/.config/ahma/settings.toml`; code uses `~/.ahma/settings.toml`) and stale env-var references in `ahma_mcp/src/lib.rs:82` and `shell/cli/mod.rs:478`.

---

## Phase 2 — The question ladder

**Goal**: one asking pipeline used by every denial (fs, web, tool, hook), with harness-first routing, demotion, TUI fallback, and legible fail-closed.

**Current state (verified anchors)**:
- Denial detection: `ahma_mcp/src/mcp_service/handlers/common.rs:79` (pre-exec) and `:112` (runtime) emit structured `sandbox_denial` JSON with `current_scopes` and `remediation`. `adapter/mod.rs:504-529` maps stderr scans → `SandboxError::RuntimeDenial`.
- Notifier plumbing: `ahma_mcp/src/sandbox/grant_channel.rs` — `ScopeGrantNotifier` trait (:128), `HubGrantNotifier` (:182 → TUI modal), `LoggingGrantNotifier` (:144, no-UI fallback), `grant_dir_for` (:37, offers the denied file's parent dir).
- Harness asking: `sandbox_grant_tool.rs` routes external clients to `elicit_with_timeout::<ScopeGrantForm>` (:191, 120s) and refuses self-persist for the in-process agent (:165–:166).
- Coordination: `GrantCoordinator` (`scope_grant.rs:139`) ask-once dedup; R5.3.3/R5.3.4 `decision_id` fan-out with first-answer-wins/most-restrictive-wins already specified and partially built for web grants.

### Steps

1. **Create a single `PermissionBroker`** (suggest `ahma_mcp/src/permissions/broker.rs`) that owns the ladder. Inputs: a `GrantQuestion { kind, subject, access, context }` where `context` carries the operation identity line (Phase 5), the denied path/domain, and *why* (e.g. `path_outside_sandbox`). Output: `GrantAnswer { decision: Allow|Deny, tier }`. All existing notifiers become rungs inside the broker rather than parallel ad-hoc paths.
2. **Rung 1 — harness**: at session init, record whether the client's `initialize` advertised the `elicitation` capability. Maintain per-session `elicitation_state: Untried | Proven | Demoted`. On a question: if capable and not demoted, send `elicitation/create` with a schema that makes the grant explicit — subject, access, tier choices (`once`/`session`/`always`), and for `always` the exact settings line that would be written. Timeout (keep 120s) or transport error → mark `Demoted`, log `warn!`, fall to rung 2 **and include in the eventual answer surface a note that the harness didn't respond**. A decline → answer is Deny; harness stays `Proven`.
3. **Rung 2 — TUI modal**: route through `HubGrantNotifier` to any attached TUI. Reuse the R-WEB.6 modal contract: Enter/Esc = deny; the persist option displays file + exact line; modal renders over both chat and monitor modes. The `decision_id` fan-out means rung 1 and rung 2 may both be live for the same question — that's fine and already specified: first answer wins, most restrictive wins on ties, and the losing surface gets a "answered elsewhere: <decision>" dismissal.
4. **Rung 3 — fail closed**: no surface answered → operation fails with the structured `sandbox_denial` payload (already emitted) **and** a remediation line the user can paste: `ahma sandbox grant /path/to/dir --ro` (via `grant_dir_for` for the parent-dir suggestion). This is the universal fallback that works in *every* harness, including ones that render only tool-result text.
5. **Ask-once across surfaces**: broker consults `GrantCoordinator` before rung 1; a `session`-tier answer (either direction) suppresses further questions for that `(subject, access)` this session. A `once` allow applies to the single pending operation only.
6. **Answer application paths** (keep the split honest):
   - MCP server path: an `always` grant persists (preview-approved) and the success text says "takes effect at next server start" (R5.1; existing text at `sandbox_grant_tool.rs:556`). A `session` grant on the server path surfaces as "recorded — applies on restart" *unless* the pending operation can be safely retried under hooks-style per-command sandbox derivation; do not attempt live-widening of the locked scope.
   - Hooks path: grants apply to the next command automatically (Phase 4).
7. **Web grants converge**: route the existing R-WEB three-tier approval through the same broker so web and fs questions are one UX. Do not regress R-WEB.9.1 dedup or the R-WEB.6 modal contract.

### Tests

- Broker unit tests with a scripted fake elicitation transport: capable+answers → rung 1 only; capable+timeout → demoted, rung 2 asked, second question skips straight to rung 2; decline → no demotion.
- Integration (HTTP bridge, following the mandatory handshake in AGENTS.md): denial before any grant → 409/-32001 unaffected; post-lock runtime denial → elicitation request observed over SSE; declining → operation fails closed with remediation text asserted (no print-only tests).
- Fan-out test: TUI-sim + harness-sim both receive the question; answer on one dismisses the other; most-restrictive-wins on simultaneous conflicting answers.
- Ask-once: two concurrent ops denied on the same path produce exactly one question.

### Acceptance

- Exactly one code path asks permission questions; `grep` shows no notifier invoked outside the broker.
- A user in Cursor (elicitation-capable) gets an in-IDE question; a user in a non-capable client gets a clear failure with a paste-able command; a TUI user gets a modal. In all three, Enter/Esc/timeout deny.

---

## Phase 3 — Crutches become profiles (data, not code)

**Goal**: remove every hard-coded toolchain path from the sandbox backends; ship equivalent *profiles* as data folded through the same grant pipeline; disclose what can't be expressed.

**Current state (verified anchors)**:
- macOS `ahma_mcp/src/sandbox/seatbelt.rs`: `get_macos_user_tool_rules` (:191-207) hard-codes `[".cargo", ".rustup"]`; `get_macos_temp_rules` (:244); `get_macos_system_rules` (:181-189) emits a blanket `(allow file-read*)` — an APFS firmlink/cryptex workaround, i.e. **all reads are open on macOS**.
- Linux `ahma_mcp/src/sandbox/landlock.rs`: `add_landlock_home_tool_rules` (:233) hard-codes `[".cargo", ".rustup", ".nvm", ".npm", ".go", ".cache"]`; system rules (:208); temp (:302).
- Package-cache carve-out `ahma_mcp/src/sandbox/pkg_cache.rs` (:46-62): cargo registry/git writable, `bin/`+`config.toml`+`credentials.toml` never (:16-20); npm/pip/go stubs (:84).
- Credential-read denylist `ahma_mcp/src/sandbox/credential_reads.rs:48-56` (macOS-only today).

### Steps

1. **Profile format**: define `ProfileDef` loaded from TOML shipped via `include_str!` (compiled-in data files under e.g. `ahma_mcp/profiles/*.toml`) — *data files in the repo*, not Rust arrays:
   ```toml
   # profiles/rust.toml
   name = "rust"
   description = "Rust toolchain: cargo registry/git caches, rustup toolchains"
   [[scopes]]
   path = "~/.cargo/registry"
   access = "rw"
   [[scopes]]
   path = "~/.rustup"
   access = "ro"
   [[never]]                # carve-outs within the profile that stay denied
   path = "~/.cargo/credentials.toml"
   ```
   Ship `rust.toml`, `node.toml`, `python.toml`, `go.toml` initially (contents derived from the current hard-coded lists + `pkg_cache.rs` semantics — preserve the `bin/`/`credentials.toml` exclusions as `never` entries).
2. **Fold-in path**: profiles resolve to `GrantRecord { granted_by: "builtin-profile(rust)" }` entries merged into the effective scope at the same point `resolve_persistent_scopes` (`shell/cli/mod.rs:569`) merges user grants. Backends (`seatbelt.rs`, `landlock.rs`) then consume *only* the resolved scope list — delete `get_macos_user_tool_rules`, `add_landlock_home_tool_rules`, and migrate `pkg_cache.rs` logic into the `rust` profile (keep `pre_create_package_cache_paths` behavior as a profile attribute, e.g. `precreate = true`).
3. **Configurability**: `[sandbox] profiles = ["rust", "node"]` in `settings.toml`; default = all builtin profiles enabled (**opt-out first** — preserves current behavior while making it visible and disableable). `ahma permissions list` shows profile entries with provenance; `ahma permissions profiles list|enable|disable` manages them.
4. **Suggest-on-denial** (stretch, may split to follow-up PR): when a denial path matches a *disabled or unshipped* profile pattern (e.g. `~/.gradle/caches`), the broker's question offers "enable the `<name>` profile" as an alternative to a one-off grant.
5. **Disclosure of the undeniable**: the macOS blanket read-allow cannot be a profile. Add a persistent disclosure line to every scope display (TUI scope panel, `--list-tools`/startup banner, `status` tool output): `macOS: writes kernel-scoped; reads unrestricted (platform limitation)`. This applies R7.5's honesty principle to ahma's own backend. Do not bury it in docs only.
6. **System dirs stay code**: `/usr`, `/bin`, `/etc` read-only rules and device-path denials are platform invariants, not app exceptions — they remain in the backends. The test for "is this a crutch?" is *app-specific*, not *platform-specific*.

### Tests

- Golden-scope test per platform: resolved scope with default profiles == pre-refactor behavior (snapshot the SBPL / Landlock ruleset before refactoring and diff).
- Profile disable test: `profiles = []` → `.cargo` writes denied (red-team style, kernel-enforced where the platform supports it).
- `never` entries: `~/.cargo/credentials.toml` unreadable/unwritable even with rust profile on (extend the existing `pkg_cache` exclusion tests).
- Disclosure string asserted present in `status` output on macOS.

### Acceptance

- `grep -rn '\.cargo\|\.rustup\|\.nvm\|\.npm' ahma_mcp/src/sandbox/*.rs` returns only profile-loading code and platform-invariant comments — no path lists in `.rs` files.
- `ahma permissions list` shows profile-sourced scopes with `builtin-profile(...)` provenance.
- Behavior for a default Rust user is byte-identical to today (golden test).

---

## Phase 4 — Hooks readiness, per client

**Goal**: replace the global "hooks not ready" default with a per-client readiness gate: hooks turn on for a client once the ladder demonstrably works there.

**Current state (verified anchors)**:
- `ahma_mcp/src/setup.rs::default_setup_actions` (:271-277) filters out `SetupAction::Hooks`; rationale comment at :263-270.
- `ahma_mcp/src/hooks/mod.rs`: `HooksDecision` (:37-67), `compute_exec_decision` (:1386), wrapper `build_wrapped_shell_command` (:1575), `run-shell` handler (:1125) which **fails OPEN on sandbox-init failure** (:1146-1163), `defer_to_host_decision` (:1087), activation precedence `describe_activation` (:661).
- Post-exec denial observer: `ahma_mcp/src/hooks/post_exec.rs` (the `aws-lc-sys` build-cache denial is the motivating bug — a bare `Operation not permitted` buried in a build log).
- Host detection: `ahma_mcp/src/sandbox/host_detect.rs` (`Cursor`, `ClaudeCode`, `VsCode`, `Docker`).

### Steps

1. **Wire hooks denials into the broker**: `run-shell` executions route pre-exec (`notify_pre_exec`, `grant_channel.rs:231`) and post-exec (`post_exec.rs` observer, `notify_stderr_denial` :257) denials into the Phase-2 broker. The hooks path has no MCP session, so rung 1 is skipped unless an MCP session for the same workspace is live (the hub knows — query attached instances); rung 2 (TUI) and rung 3 (CLI remediation printed to the terminal the hook ran in) are the mainline.
2. **Next-command application** (the hooks advantage): hooks re-derive the sandbox per command, so an `always` or `session` grant is picked up by the very next command with **no restart**. Make `run-shell` re-read the session grant store + settings on each invocation (verify this is cheap; cache with mtime check if not). Assert this in a test: deny → grant via CLI → immediate rerun succeeds.
3. **Legible fail-closed in the terminal**: when a hooked command is denied and unanswered, the wrapper must print (stderr, after the command's own output) a short block: what was denied, why, and the paste-able `ahma sandbox grant` line. This is the fix for the `aws-lc-sys` failure mode — the denial must never be *only* a cryptic line inside a build log. Regression test extends `post_exec.rs:85-100`.
4. **Fail-open consent stays gated**: the `run-shell` fail-OPEN on sandbox-init failure (:1146-1163) must remain behind the explicit `HookConsentStore` consent (boot-nonce-bound marker, `hooks/consent.rs:122-141`) with its loud banner. Do not widen fail-open.
5. **Per-client readiness checklist**, encoded as `ahma setup --check-hooks <client>` (or part of `ahma setup` interactive flow):
   - (a) a scripted denial in that client round-trips the full loop (deny → question on the right rung → grant → next command succeeds);
   - (b) the fail-closed message is verified legible in that client's UI (manual check first time, then pinned by an integration test where the client is scriptable);
   - (c) nested-sandbox handling correct: in detected host sandboxes, `DeferToHost` remains the default (R7.2) — which removes most "gets in the way" surface — with `AHMA_PREFER_OWN_SANDBOX=1` as the override.
6. **Flip the default per-client**: `default_setup_actions` offers `SetupAction::Hooks` for clients on the passed list (start with the ones exercised by CI/integration: Claude Code, Cursor). Keep the global `--hooks`/`AHMA_HOOKS` precedence (`describe_activation` :661) unchanged. Update the rationale comment at `setup.rs:263-270` to point at R-PERM.6.

### Tests

- End-to-end hooks loop test (macOS + Linux, `#[cfg_attr(windows, ignore)]` until AppContainer lands, mirroring `red_team_command_write_escape_blocked`): wrapped command writes outside scope → kernel-denied → broker notified → simulated TUI grant (`session` tier) → rerun same command succeeds → no third question asked.
- Terminal remediation block asserted in captured stderr.
- Defer-to-host: with `CURSOR_*` env markers set, decision is `DeferToHost` and no ahma denial UX fires.
- Consent gating: fail-open without valid consent marker → hard failure, not silent open.

### Acceptance

- `ahma setup` on a supported client installs hooks by default; the "not ready" filter is gone, replaced by the checklist gate.
- A denied `cargo build` touching a novel cache dir produces, within one command cycle: a clear question (or clear terminal remediation), and after approval the next build passes. No hard-coded exception involved.

---

## Phase 5 — TUI operation identity (wire fields + rendering)

**Goal**: every operation, in every surface, is identified by one human-meaningful line. Fix the data at the source; rendering follows.

**Current state (verified anchors)**:
- Wire: `ahma_common/src/daemon_hub.rs::DaemonEvent::OpStarted` (:122) carries only `id`, `tool_name`, free-text `description`; `OpFinished` (:137) has string `status`, no exit code. `op_history` replay (:604-731, `MAX_OPS_PER_INSTANCE = 500` :86, `replay_events` :709).
- TUI reverse-engineering: `ahma_tui/src/state.rs::Operation::display_name` (:792) chains `try_parse_run_terminal_command` → JSON parse → "Execute …" sentence parse → **deriving words from the op id** (`parse_command_from_op_id` :723, e.g. `op_41_echo_hello` → "echo hello") → raw `tool_name` fallback. This is the root cause; no formatter can rescue data not on the wire.
- Rendering: `ahma_tui/src/ui.rs` fixed prefixes `"you"` (:896), `"ahma"` (:1035), `" tool "` + raw tool name (:1051-1055); monitor panes `draw_ai_activity` (:1740), `draw_ops_dag` (:1830), `draw_detail` (:2355).
- Late-attach/backdating already works: `ahma_tui/src/daemon_source.rs::backdate` (:203), `TERMINAL_OP_RETENTION` (:282). It replays *bad labels*; fixing the wire fixes replay for free.

### Steps

1. **Wire additions** (R24.5 permits add-only evolution; all fields `#[serde(default)]`-tolerant for old readers):
   - `OpStarted`: add `title: String` (server-computed human summary — the server *knows* the command: for `run_terminal_command` it is the command string, first line, trimmed to ~80 chars; for other tools, `<tool_name> <salient arg>`), `cwd: Option<String>`, `command: Option<String>` (full, for the detail pane), `origin: Option<String>` (which attached session initiated: `"cursor" | "claude-code" | "tui" | "cli" | "hook"` — derive from the MCP client info captured at `initialize`, or the invocation path).
   - `OpFinished`: add `exit_code: Option<i64>`.
   - Emit sites: wherever the adapter publishes `OpStarted`/`OpFinished` to the hub (locate via `grep -n "OpStarted" ahma_mcp/src -r`); compute `title` in **one** server-side function (suggest `ahma_common::op_identity::title_for(tool_name, args)`) so CLI-mode, hub events, and log naming share it.
2. **Define the identity line** (one formatter in `ahma_tui`, used everywhere):
   ```
   ⚙ cargo nextest run -p ahma_core · ahma/ · running 12s      (ongoing)
   ✓ cargo nextest run -p ahma_core · ahma/ · exit 0 · 41s     (completed)
   ✗ touch /etc/foo · ahma/ · denied: outside scope             (sandbox denial)
   ```
   Components: status glyph · `title` · basename of `cwd` · state (elapsed | `exit N` + duration | denial reason). Origin badge (`[cursor]`, `[tui]`) appended when more than one origin is present in the visible set.
3. **Rewrite `Operation::display_name`**: prefer wire `title`; keep the existing parse chain *only* as fallback for events from pre-upgrade servers; delete `parse_command_from_op_id` heuristics once a deprecation window passes (leave a TODO with the version). Update `test_operation_display_name` (:2650) accordingly.
4. **Apply in all four surfaces**:
   - Chat history: `push_tool_call_chat_lines` (`ui.rs:1051`) renders the identity line instead of raw tool name; completion updates the same entry with exit status.
   - Monitor rows: `draw_ai_activity` / `draw_ops_dag` rows use the identity line; `draw_detail` shows full `command`, `cwd`, `origin`, output tail.
   - Grant prompts (Phase 2 broker context): the question header *is* the identity line of the denied operation.
   - Log naming (`ahma_mcp/src/utils/logging.rs:115` per-op logs): include a slugged `title` in the filename alongside the op id.
5. **Denied-row escape hatch**: a denial-state row in monitor mode is selectable; pressing Enter on it re-raises the grant question through the broker (with ask-once suppression respected — re-raise is an explicit user action, so it bypasses the session-deny memo but re-asks with the same preview). This is the "escape when the user agrees" affordance hooks never had.
6. **Origin interleaving**: with `origin` on the wire, the timeline naturally interleaves IDE-initiated and TUI-initiated work. Add origin badges; no other architecture change — **do not** build a second agent loop in the TUI. The sidecar model is already shipped (per-workspace instance + hub + thin TUI + `ClientMsg::SubmitPrompt` `daemon_hub.rs:192`). The only genuinely missing piece is a named `/btw <prompt>` alias in TUI chat that tags the submitted prompt's ops with `origin=tui` — small, optional, last.

### Tests

- Wire compat: old-reader tolerance (deserialize new events with unknown fields ignored — assert via serde round-trip against a struct missing the new fields) and new-reader-old-event (missing `title` → fallback chain).
- `title_for` unit tests: `run_terminal_command` with long/multiline commands, other tools, pathological args.
- Replay test: populate hub `op_history` with new-format events, attach TUI late, assert rendered rows carry command text and exit codes (extend existing backdate tests in `daemon_source.rs`).
- Identity line formatter: ongoing/completed/failed/denied variants, origin badge only when mixed.
- Denied-row re-raise: simulated denial op → Enter → broker receives a question with the same subject.

### Acceptance

- Enter `ahma tui` *after* an IDE session has run 20 ops: monitor mode shows 20 rows reading like `✓ cargo nextest run -p ahma_core · ahma/ · exit 0 · 41s`, not `op_17_cargo_nextest`.
- Chat mode tool lines show the same identity text.
- A denial is visible, selectable, and answerable from monitor mode.

---

## Phase 6 — Cleanup, docs, and rollout

1. **Docs**: update `docs/security-sandbox.md` (ledger location fix from Phase 1; profiles section; question ladder), `docs/tui.md` (identity line, denied-row flow, origin badges), `docs/settings.md` (`[sandbox] profiles`, `[[permissions.*]]` tables), `docs/environment-variables.md` (deprecate `AHMA_CONFIG_DIR`). Add `docs/permissions.md` as the user-facing explanation of the ladder + tiers + ledger (short; link from README).
2. **SPEC status tables**: mark R-PERM items `tests-pass` only with the tests above green; keep Windows rows honest (AppContainer still pending — hooks end-to-end test stays ignored on Windows per the existing pattern).
3. **Rollout order & risk**: Phases are sequenced by dependency: 0 → 1 → 2 → {3, 5 in parallel} → 4 → 6. Phase 4 (hooks default-on per client) ships **last** and only after 2 is proven in integration tests — it is the user-facing risk. Each phase behind its own PR; Phase 3 must include the golden-scope snapshot diff in the PR description.
4. **Out of scope** (do not do): live-widening of a locked MCP-server scope (violates R5.1); a TUI-embedded second agent loop; Windows AppContainer work (tracked separately under R6.3); auto-enabling any profile not previously covered by the removed hard-coded lists.

---

## Appendix: quick reference of load-bearing anchors

| Concern | File | Anchor |
|---|---|---|
| Persistent fs grants (write) | `ahma_common/src/scope_grant.rs` | `persist_grant` :288, `GrantCoordinator` :139 |
| Settings path / home dir | `ahma_common/src/config.rs` | `ahma_home_dir` :533, `settings_path` :549, `PersistentScope` :770 |
| Legacy tool approvals (to migrate) | `ahma_core/src/approvals.rs` | `config_dir` :32, `remember_tool_approval` :159 |
| Grant tool (two-gate, elicitation) | `ahma_mcp/src/mcp_service/handlers/sandbox_grant_tool.rs` | `handle_sandbox_grant` :109, `classify_grant_risk` :307, `preview_text` :497 |
| Denial detection | `ahma_mcp/src/mcp_service/handlers/common.rs` | :79 pre-exec, :112 runtime |
| Notifier rungs | `ahma_mcp/src/sandbox/grant_channel.rs` | `ScopeGrantNotifier` :128, `HubGrantNotifier` :182, `grant_dir_for` :37 |
| macOS crutches | `ahma_mcp/src/sandbox/seatbelt.rs` | :181-207, :244 |
| Linux crutches | `ahma_mcp/src/sandbox/landlock.rs` | :208, :233, :302 |
| Package cache carve-out | `ahma_mcp/src/sandbox/pkg_cache.rs` | :16-20, :46-62 |
| Hooks decisions / wrapper | `ahma_mcp/src/hooks/mod.rs` | :37, :1125, :1146 (fail-open), :1386, :1575 |
| Hooks setup filter | `ahma_mcp/src/setup.rs` | :263-277 |
| Hub wire format | `ahma_common/src/daemon_hub.rs` | `OpStarted` :122, `OpFinished` :137, history :604-731 |
| TUI label heuristics (to replace) | `ahma_tui/src/state.rs` | `display_name` :792, id-parse :723 |
| TUI rendering | `ahma_tui/src/ui.rs` | :896, :1035, :1051, :1740, :1830, :2355 |
| Late-attach replay | `ahma_tui/src/daemon_source.rs` | `backdate` :203, retention :282 |
