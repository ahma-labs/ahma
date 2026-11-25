# Agent Skills

Agent skills are optional. They are not required to use ahma for normal build, test, git, or shell workflows.

## `ahma`

The `ahma` skill teaches agents how to use `ahma` effectively: sandboxed shell execution, tool bundles, progressive disclosure, async/await patterns, code complexity analysis (`ahma simplify`), and more.

`ahma setup --skills` installs it to `~/.agents/skills/ahma/` (Cursor, VS Code and other agents that read that folder) and `~/.claude/skills/ahma/` (Claude Code). `ahma update` refreshes it; `ahma uninstall --skills` removes it.

## Install manually (from a checkout)

**Linux / macOS**

```bash
mkdir -p ~/.agents/skills/ahma ~/.claude/skills/ahma
cp skills/ahma/SKILL.md ~/.agents/skills/ahma/SKILL.md
cp skills/ahma/SKILL.md ~/.claude/skills/ahma/SKILL.md
```

**Windows**

```powershell
New-Item -ItemType Directory -Force "$HOME\.agents\skills\ahma"
Copy-Item skills\ahma\SKILL.md "$HOME\.agents\skills\ahma\SKILL.md"
```

To enable the skill per project:

```bash
mkdir -p .agents/skills/ahma
cp skills/ahma/SKILL.md .agents/skills/ahma/SKILL.md
```

Once installed, agents load the skill automatically when you ask about ahma, sandboxed execution, code complexity, or simplification. In Cursor you can also attach it explicitly with `@ahma`. Invoke sub-workflows with `/ahma simplify`, `/ahma update`, `/ahma help`, etc.

Install or upgrade the binary with `ahma update` (see [installation.md](installation.md)).

For code complexity analysis details, see [simplify.md](simplify.md).

## Running skills in `ahma tui`

The TUI chat runs any [Agent Skills](https://agentskills.io/specification) skill found in
`<workspace>/.agents/skills/`, `<workspace>/.claude/skills/`, `~/.agents/skills/` or
`~/.claude/skills/` (first match wins). `/<name> [args]` or `/skill <name> [args]` invokes
one; `/skills` lists them, with invalid ones and the reason. Built-in commands shadow skills;
`user-invocable: false` skills are listed but cannot be invoked (SPEC R-SK8).
