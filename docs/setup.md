# Setup — `ahma setup`, `/setup` and `ahma setup --plan`

**Status:** Stable. SPEC: [R-SETUP.3, R-SETUP.4](../SPEC.md).

## Why

The sandbox is strictest exactly where a project needs something: a local model needs the
GPU, a project test script needs to be known as read-only, a Gradle build needs its
caches. Without help you find each of these when a command fails. Setup shows what ahma
is set to and what it has been told to allow, looks at the project, and proposes the
settings with the reason, so the decision is yours and made in advance rather than in the
middle of a failed build.

It proposes; it never decides. Every item is applied only after you answered `y` for that
one item, through the same audited writer as the dedicated commands.

## The guided view

```bash
ahma setup          # in a terminal: opens the view (also: /setup in ahma tui)
ahma setup --cli    # the question-by-question installer wizard instead
```

The view takes the whole screen and opens on the first thing ahma proposes for this folder,
asked as a question you can answer without knowing ahma's settings:

```
┌ Setup · question 1 of 2 ─────────────────────────────────────────────────────────┐
 ahma keeps the commands your AI agents run inside this folder. It checked this
 project and found 2 things that may get in their way. Answer each with y or n;
 nothing changes until you have answered all of them.

 Question 1 of 2   ○  ○

 Let programs your AI agent runs use the GPU (graphics chip)?  [security]

 Why we ask  This project has a model file (models/tiny.gguf). Without the GPU, a
             local AI model fails to start or runs slowly on the CPU.
 Benefit     Local AI models run at full speed instead of failing or falling back
             to the much slower CPU.
 Risk        Sandboxed programs can talk to the graphics driver. No extra files,
             folders or network access are opened.

 This changes what the sandbox allows. Make this change?   [y] yes   [n] no
```

`Space` shows the exact change, the command that undoes it and who found it:

```
 Change      sandbox.allow_gpu: false -> true (your settings)
 Undo        ahma settings unset sandbox.allow_gpu
 Found by    found in this project
```

What would stop something working (the GPU, a toolchain's folders) is asked first; a
convenience (letting edits continue while a script runs) is marked *optional* and asked
last. `y` or `n` answers it and the next one appears. **Nothing is written until every change has
an answer**; then the yeses are applied and one result screen lists what was applied (with
the command that undoes it), what failed and why, what you declined, and anything ahma
refused to offer. A key leaves, and `ahma setup` exits with it. When there is nothing to
propose it says so.

| Key | On the proposed change | On the result screen |
|---|---|---|
| `y` / `n` | yes / no (a high-risk change needs a second `y` after you have read its warning) | ignored, so the `y` that finished the review cannot close the result unread |
| `←` `→` | previous / next, to change an answer before anything is applied | |
| `Space` | show or hide the exact change, its undo and who found it | |
| `d` | see everything ahma allows: the reference below | the same |
| `r` | | ask again about what you declined earlier in this folder |
| `t` | ask a question in words (below) | the same |
| `Esc` | leave; nothing is written, your answers are kept for this session | leave |

**A no is remembered.** A change ahma itself proposed (a detector, a doctor fix, the
installation check) that you decline is not offered again in this folder; the result screen
and `ahma setup --plan` say how many are being left out, and `r` asks again. Declines are
kept in `~/.ahma/setup-declined.json`, by what the change does rather than by its title.

### See everything ahma allows

`d` opens the reference. `1`-`3`, `Tab` or `←/→` switch between its tabs, `p` returns to the
proposed changes, `Esc` closes.

| Tab | Shows | Keys |
|---|---|---|
| **Install** | MCP entries, hooks, skills, TLS: what is known about each and the command that (re)installs it | |
| **Settings** | every setting with its value, where it comes from (`repo`, `you`, `default`) and whether it is a security setting; only the ones you changed unless you press `a`. A security key a repository's file tried to set is named as ignored | the explanation of the selected row is shown below the list; `e` changes it, `a` shows all |
| **Allow & deny** | everything granted or switched on: folders, network hosts, web patterns, tool approvals, bundles, profiles, readers, switches (GPU, keychain, …) | `u` takes one back |

A bundle, profile, reader or switch is taken back as a proposal, which appears as the change
to answer. A grant, host, web pattern or tool approval is taken back with its own command
(`ahma sandbox revoke`, `ahma network revoke`, …), which the view shows, because those
commands check and audit it.

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
the plan. It can only answer and **propose**: its suggestions appear as proposed changes to answer,
labelled `suggested by <model>`, validated like every other item, and nothing applies
until you have answered each. Ahma writes each item's question, benefit and risk itself from
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
ahma setup --plan --apply     # offers each item in turn; y applies it, n declines it for good in this folder, Enter skips
```

Run it from a normal terminal, not from an agent's sandboxed shell: `~/.ahma` is
unreadable there, so an agent would see an empty settings file. Run it again after
applying: it prints `No changes proposed`.

A plan item looks like:

```
1. Let programs your AI agent runs use the GPU (graphics chip)?  [security]
   why:     This project has a model file (models/tiny.gguf). Without the GPU, ...
   benefit: Local AI models run at full speed instead of failing or falling back to ...
   risk:    Sandboxed programs can talk to the graphics driver. No extra files, ...
   source:  found in this project
   change:  sandbox.allow_gpu: false -> true (your settings)
   undo:    ahma settings unset sandbox.allow_gpu
```

## What is detected

| Detector | Looks for | Proposes |
|---|---|---|
| `gpu` | a `.gguf`/`.ggml` file, or a manifest mentioning `llama.cpp`, `ggml`, `whisper-rs` | `sandbox.allow_gpu = true` |
| `source-readers` | a script (a `#!` file, or one with a script extension) named in a "definition of done" section of `AGENTS.md`/`CLAUDE.md` that exists inside the project. Never one whose name says it writes (`generate.sh`, `fmt.sh`, `install-deps.sh`, …) or with a line ahma's edit guard calls a rewrite (`cargo fmt`, `go generate`, `sed -i`, `npm install`, …) | one `tools.source_readers` entry per script, in this project's `.ahma/settings.toml` (your own file if the project has no `.ahma` directory) |
| `profile` | `Cargo.toml`, `package.json`, `go.mod`, Gradle/Android, Xcode/Swift whose profile is off | the profile, added to `sandbox.profiles` |
| installation | an installed client's MCP entry that differs from the recommended one | refresh it |
| `ahma doctor` | every finding that has a fix | that fix |

Detectors read the project to a bounded depth, skip dependency and build folders, never
follow a symlink out of the project, and never guess `tools.long_running` (a lease-free
command is a decision only you can make).

## Guarantees

- **Nothing is applied without your `y`** on that item. There is no "apply all", and the guided view writes nothing before your last answer.
- **Every item is validated twice**, when shown and when applied: a key that does not
  exist, a value of the wrong type, a security setting aimed at a repository file, a grant
  key that has its own command, a folder on the hard denylist and a host on the network
  denylist are all refused, with the reason. See [permissions.md](permissions.md).
- **Changes are recorded** in `~/.ahma/permissions-audit.jsonl`.
- **A no from ahma's own proposals is remembered** per folder in `~/.ahma/setup-declined.json` and never hides what you asked for yourself.
- **Two items for one list merge** (two scripts for `tools.source_readers`), so neither
  overwrites the other.

## See also

- [settings.md](settings.md) — `ahma settings explain <key>`, and what `settings set` will and will not write
- [doctor.md](doctor.md) — the fixes the plan includes
- [permissions.md](permissions.md) — grants and the hard denylist
- [tui.md](tui.md) — `/setup` and `/connect` among the other commands
- SPEC [R-SETUP.3, R-SETUP.4](../SPEC.md)
