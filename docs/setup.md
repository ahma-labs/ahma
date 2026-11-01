# Setup — `ahma setup`, `/setup` and `ahma setup --plan`

**Status:** Stable. SPEC: [R-SETUP.3, R-SETUP.4](../SPEC.md).

## Why

The sandbox is strictest exactly where a project needs something: a local model needs the
GPU, a project test script needs to be known as read-only, a Gradle build needs its
caches. Without help you find each of these when a command fails. Setup shows what ahma
is set to and what it has been told to allow, looks at the project, and proposes the
settings with the reason, so the decision is yours and made in advance rather than in the
middle of a failed build.

It proposes; it never decides. Every item is applied only after you answer `y` for that
one item, through the same audited writer as the dedicated commands.

## The guided view

```bash
ahma setup          # in a terminal: opens the view (also: /setup in ahma tui)
ahma setup --cli    # the question-by-question installer wizard instead
```

The view has four tabs (`Tab`, `←/→` or `1`-`4` to switch, `Esc` to close):

| Tab | Shows | Keys |
|---|---|---|
| **Install** | MCP entries, hooks, skills, TLS: what is known about each and the command that (re)installs it | |
| **Settings** | every setting with its value, where it comes from (`repo`, `you`, `default`) and whether it is a security setting; only the ones you changed unless you press `a`. A security key a repository's file tried to set is named as ignored | the explanation of the selected row is shown below the list; `e` changes it, `a` shows all |
| **Allow & deny** | everything granted or switched on: folders, network hosts, web patterns, tool approvals, bundles, profiles, readers, switches (GPU, keychain, …) | `u` takes one back |
| **Proposed changes** | the plan, each item with why, what it allows, the exact change and its undo | `y` applies this one, `n` dismisses it |

A bundle, profile, reader or switch is taken back as a proposal in the plan. A grant, host,
web pattern or tool approval is taken back with its own command (`ahma sandbox revoke`,
`ahma network revoke`, …), which the view shows, because those commands check and audit it.

### Say what you want

Press `t`, or type `/setup` followed by what you want:

```
/setup I run a local model and want it to use the GPU
/setup allow cargo to reach crates.io but nothing else on the network
/setup deny cargo publish
/setup set tools.timeout_secs 3600          # your own change, added to the plan
/setup set --project tools.source_readers ['scripts/check.sh']
```

The model you connected (`/connect`) is briefed with your settings, what is granted and
the plan. It can only answer and **propose**: its suggestions appear in *Proposed changes*
labelled `suggested by <model>`, validated like every other item, and nothing applies
until you press `y` on each. Ahma writes each item's title and "allows" text itself from
the change; only the model's own reason is shown, cleaned and labelled as the model's.
Text in your repository (AGENTS.md, file names) is data to it, not instructions.

**What "deny" can mean.** Ahma has no per-command deny list: a rule against one shell
command (`cargo publish`) cannot be enforced, because `sh -c`, scripts and aliases get
around it. The model is told so and will say it, and offer the nearest real lever instead:
remove a grant, bundle or profile, turn a switch off, or do not grant the network or
credentials that command needs.

Without a model connected the view still works fully; `/setup set` adds your own changes.

## Quickstart (no TUI)

```bash
ahma setup --plan             # what this folder suggests; changes nothing
ahma setup --plan --path ~/proj
ahma setup --plan --apply     # offers each item in turn; y applies it, anything else skips
```

Run it from a normal terminal, not from an agent's sandboxed shell: `~/.ahma` is
unreadable there, so an agent would see an empty settings file. Run it again after
applying: it prints `No changes proposed`.

A plan item looks like:

```
1. Let sandboxed commands use the GPU (Metal)  [changes the sandbox boundary]
   source:  detected: gpu
   why:     This project has a model file (models/tiny.gguf). Without the GPU, ...
   allows:  Commands can open the GPU. It is a capability, not a path, ...
   change:  sandbox.allow_gpu: false -> true (your settings)
   undo:    ahma settings unset sandbox.allow_gpu
```

## What is detected

| Detector | Looks for | Proposes |
|---|---|---|
| `gpu` | a `.gguf`/`.ggml` file, or a manifest mentioning `llama.cpp`, `ggml`, `whisper-rs` | `sandbox.allow_gpu = true` |
| `source-readers` | a path-like script in a "definition of done" section of `AGENTS.md`/`CLAUDE.md` that exists inside the project | one `tools.source_readers` entry per script |
| `profile` | `Cargo.toml`, `package.json`, `go.mod`, Gradle/Android, Xcode/Swift whose profile is off | the profile, added to `sandbox.profiles` |
| installation | an installed client's MCP entry that differs from the recommended one | refresh it |
| `ahma doctor` | every finding that has a fix | that fix |

Detectors read the project to a bounded depth, skip dependency and build folders, never
follow a symlink out of the project, and never guess `tools.long_running` (a lease-free
command is a decision only you can make).

## Guarantees

- **Nothing is applied without your `y`**, one item at a time. There is no "apply all".
- **Every item is validated twice**, when shown and when applied: a key that does not
  exist, a value of the wrong type, a security setting aimed at a repository file, a grant
  key that has its own command, a folder on the hard denylist and a host on the network
  denylist are all refused, with the reason. See [permissions.md](permissions.md).
- **Changes are recorded** in `~/.ahma/permissions-audit.jsonl`.
- **Two items for one list merge** (two scripts for `tools.source_readers`), so neither
  overwrites the other.

## See also

- [settings.md](settings.md) — `ahma settings explain <key>`, and what `settings set` will and will not write
- [doctor.md](doctor.md) — the fixes the plan includes
- [permissions.md](permissions.md) — grants and the hard denylist
- [tui.md](tui.md) — `/setup` and `/connect` among the other commands
- SPEC [R-SETUP.3, R-SETUP.4](../SPEC.md)
