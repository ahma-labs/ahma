# Setup plan — `ahma setup --plan`

**Status:** Stable (the plan and `--apply`). The guided `ahma setup` view in `ahma tui` is
planned. SPEC: [R-SETUP.3](../SPEC.md).

## Why

The sandbox is strictest exactly where a project needs something: a local model needs the
GPU, a project test script needs to be known as read-only, a Gradle build needs its
caches. Without help you find each of these when a command fails. The setup plan looks
first and proposes the settings, each with the reason, so the decision is yours and made
in advance rather than in the middle of a failed build.

It proposes; it never decides. Every item is applied only after you answer `y` for that
one item, through the same audited writer as the dedicated commands.

## Quickstart

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
- SPEC [R-SETUP.3](../SPEC.md)
