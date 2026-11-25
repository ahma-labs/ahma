#!/usr/bin/env bash
# Delete local branches whose pull request has merged, and prune stale worktrees
# (AGENTS.md §7, "When a branch lands, delete it and go back to main").
#
# `git branch --merged` cannot see a squash merge, so it never finds a landed
# branch here. This asks GitHub instead: a branch whose upstream is gone AND
# whose pull request (by head branch) is MERGED is deleted with -D. Anything
# else is listed for a human, never deleted:
#   - upstream gone, no merged PR: closed unmerged, or landed some other way;
#   - no upstream at all: work that was never pushed.
#
# Usage: scripts/prune-merged-branches.sh [--dry-run]
set -euo pipefail

DRY_RUN=0
case "${1:-}" in
  --dry-run) DRY_RUN=1 ;;
  "") ;;
  *) echo "Usage: $0 [--dry-run]" >&2; exit 2 ;;
esac

command -v gh >/dev/null || { echo "gh is required (https://cli.github.com)" >&2; exit 1; }

git fetch --prune --quiet origin
current=$(git branch --show-current)
review=()

while IFS='|' read -r branch upstream track; do
  [ "$branch" = "main" ] && continue
  if [ -z "$upstream" ]; then
    review+=("$branch: no upstream (never pushed?)")
    continue
  fi
  [ "$track" = "[gone]" ] || continue
  pr=$(gh pr list --head "$branch" --state merged --json number --jq '.[0].number // empty')
  if [ -z "$pr" ]; then
    review+=("$branch: upstream gone, no merged PR (closed unmerged?)")
    continue
  fi
  if [ "$DRY_RUN" = 1 ]; then
    echo "would delete $branch (PR #$pr merged)"
    continue
  fi
  if [ "$branch" = "$current" ]; then
    git switch --quiet main
    git pull --ff-only --quiet
    current=main
  fi
  git branch -D "$branch" >/dev/null
  echo "deleted $branch (PR #$pr merged)"
done < <(git for-each-ref --format='%(refname:short)|%(upstream)|%(upstream:track)' refs/heads)

if [ "$DRY_RUN" = 1 ]; then
  git worktree prune --dry-run --verbose
else
  git worktree prune --verbose
fi
extra=$(git worktree list --porcelain | grep -c '^worktree ' || true)
if [ "$extra" -gt 1 ]; then
  review+=("$((extra - 1)) worktree(s) besides the main checkout: land or rescue their work, then git worktree remove <path>")
fi

if [ "${#review[@]}" -gt 0 ]; then
  echo
  echo "Needs a human (not deleted):"
  printf '  %s\n' "${review[@]}"
fi
