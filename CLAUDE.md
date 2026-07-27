<!--
  Claude Code reads CLAUDE.md and does NOT read AGENTS.md natively, so this file
  exists purely to pull AGENTS.md into context. AGENTS.md is the single source of
  truth for every harness (Claude Code, Codex, Cursor, ...).

  Do NOT copy project content into this file. It was previously a duplicate of
  AGENTS.md and the two silently diverged: AGENTS.md grew the test-pyramid rule,
  platform-aware timeouts, the no-Python rule and dual-transport coverage that
  Claude Code never saw, while this file kept a Windows CI fact that AGENTS.md had
  since got wrong. Anything that belongs to the project goes in AGENTS.md; only
  genuinely Claude-Code-specific guidance goes below the import.

  An import is used rather than `ln -s AGENTS.md CLAUDE.md` because a symlink
  cannot carry the Claude-only section below, and symlink creation on Windows
  needs Administrator or Developer Mode.

  These HTML comments are stripped before the file enters context, so they cost no
  tokens. Verify the import actually loaded with /context ("Memory files").
-->

@AGENTS.md

## Claude Code

- Subdirectories need no nested `CLAUDE.md`: Claude Code walks **up** the tree from the
  working directory, so this root file is found from anywhere in the workspace. The nested
  `AGENTS.md` symlinks exist for harnesses that don't walk up.
- `/ahmadev` and `/ahma` are available as slash commands via `.claude/skills/`, which symlinks
  into the harness-neutral `.agents/skills/`.
