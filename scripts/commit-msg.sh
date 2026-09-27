#!/usr/bin/env bash
# commit-msg hook: strip AI attribution trailers from the commit message.
# Squash bodies are built from commit messages, so a trailer left here lands
# on main. Portable `sed -i.bak` works with both GNU and BSD sed.
#
# Install:
#   cp scripts/commit-msg.sh .git/hooks/commit-msg && chmod +x .git/hooks/commit-msg
sed -i.bak \
  -e '/^Co-Authored-By: Claude/d' \
  -e '/^🤖 Generated with \[Claude Code\]/d' \
  -e '/^Claude-Session:/d' \
  "$1"
rm -f "$1.bak"
