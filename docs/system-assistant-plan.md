# Supervised Local System Assistant — Implementation Plan

* **Author**: Paul Houghton
* **Status**: Draft
* **Date**: 2026-09-23
* **Spec**: extends [SPEC.md](../SPEC.md) R-PERM, R-DOCTOR, R5 (not yet a requirement)

A local model plus ahma's own awareness of the machine lets a user ask for
changes to their computer's settings — "turn off the startup item that keeps
reopening", "why is my disk full", "make the display sleep later" — and have
them explained, confirmed and carried out, safely. This plan says how, and
what it must never become.

## 1. Technical Approach

**The model proposes; ahma disposes.** Every safety property comes from ahma's
hard rules — the kernel sandbox, a denylist, typed actions, confirmation gates,
an audit log — and none from the model's judgement. A model's opinion that an
action is safe is shown to the user as advice and never relaxes a gate. This is
the same split `/doctor` already has (R-DOCTOR.3), generalised.

1. **Typed actions, not shell.** System changes go through narrow tools, each
   declaring what it touches, e.g. `defaults.read/write <domain> <key>`,
   `launchd.list/disable <label>`, `pmset.get/set <setting>`,
   `networksetup.get/set …`, `disk.usage <path>`. Each action has
   * a **read-current** step, always run first, whose output is shown;
   * a **preview**: the literal change (`com.apple.dock autohide: 0 → 1`), not
     the model's summary of it;
   * an **undo** recorded before applying (a snapshot of the old value), kept in
     a change log the user can replay backwards (`/undo`, `ahma changes`);
   * a **risk class** fixed in code (below), never set by the model.

   Raw shell outside the workspace stays inside the sandbox, or asks every time
   through the existing `!` / grant path. The model is not handed a root shell
   dressed up as a feature.

2. **Risk classes, owned by ahma.**

   | Class | Examples | Gate |
   |---|---|---|
   | Read | list startup items, disk usage, read a preference | none |
   | Reversible, local | a UI preference, disable a login item | confirm, with preview; undo recorded |
   | Consequential | power, network, sharing settings | confirm + a typed phrase; never "always" |
   | Forbidden | credential/keychain export, turning off SIP, Gatekeeper, FileVault or the firewall, mass deletion, new outbound channels | refused by ahma, whatever the model or user says in chat; a human can still do it by hand |

3. **Egress off while acting on the system.** Network egress is default-deny
   while a system-assistant session runs (R-WEB policy `deny`); anything read
   from the system is treated as untrusted input to the model. A local model
   makes exfiltration cheaper to attempt, not impossible: prompt injection
   through a file name, a log line or a web page is the realistic attack.

4. **Secrets never enter the context.** Readers for known secret locations
   (keychains, `~/.ssh`, browser profiles, cloud credentials) return
   "present / absent", never contents. The existing credential-read denylist
   (`sandbox.deny_credential_reads`) is the enforcement.

5. **Budgets.** Every session has a turn cap (`tools.max_turns`) and an action
   cap; exceeding either stops and asks. No background autonomy: an action is
   always on behalf of a message the user just sent.

## 2. File Changes

* **Create**: `ahma_system/` (typed actions per platform, each with
  read/preview/apply/undo and a risk class); `docs/system-assistant.md`
  once stable.
* **Modify**: `ahma_core/src/agent.rs` (route system actions through a
  risk-class gate, like `needs_approval`); `ahma_tui` (preview modal showing
  the literal change and the undo; `/undo`; change log view);
  `ahma_common/src/permissions.rs` (a `system` grant kind — reversible class
  only, per action, never per class).

## 3. Data Structures and Logic

* **`SystemAction`**: `id`, `risk: RiskClass`, `read_current() -> Value`,
  `preview(args) -> Change`, `apply(change) -> UndoRecord`,
  `undo(UndoRecord)`.
* **`Change`**: `target` (what), `before`, `after` — rendered verbatim in the
  confirmation; the model's explanation is shown beside it, labelled as such.
* **`ChangeLog`**: append-only JSONL in `~/.ahma/changes.jsonl` (outside every
  sandbox, like the permission ledger), newest first in the UI.

## 4. Dependencies

None new for the first platform (macOS): `defaults`, `launchctl`, `pmset` and
`networksetup` are system binaries invoked with fixed argument shapes.

## 5. Test Plan

* **Unit**: each action's preview is exact; undo restores `read_current()`;
  the risk class of every action is asserted in a table test so a change to it
  is a visible diff; forbidden actions refuse regardless of arguments.
* **Integration (in-process)**: a scripted model that tries to (a) skip the
  preview, (b) escalate a class, (c) exfiltrate after reading a planted
  "ignore previous instructions" file — each must fail closed.
* **Manual**: a sacrificial macOS user account; every action applied and undone.

## Anti-patterns to avoid

* **Approval fatigue.** "Always allow" on everything trains click-through — the
  exact failure in the 2026-09-23 TUI session. Trust is scoped (a folder, one
  action), and consequential actions never offer "always".
* **Confirming a summary.** The user confirms the literal change, never the
  model's paraphrase of it.
* **A model that edits its own permissions.** The ledger stays outside every
  sandbox (R5.4.8); no tool writes it.
* **Classifier-as-boundary.** An "is this safe?" model call can rank or explain;
  it cannot be the thing that allows.
* **Silent retries and silent fallbacks.** Anything ahma does on its own
  initiative is announced (R24.10.8, R24.12.5).

## Prior art worth borrowing

* Claude Code permission modes and allow/deny rules; Codex CLI approval modes
  with the network off by default — scoped, visible, revocable permissions.
* macOS TCC — one place to see and revoke what each app may touch.
* Terraform plan/apply — show the exact diff, then apply exactly that.
* Time Machine / Nix generations — every change reversible by construction.
* Apple Shortcuts "ask before running" — confirmation at the point of action.
