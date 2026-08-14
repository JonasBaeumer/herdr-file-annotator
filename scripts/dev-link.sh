#!/usr/bin/env bash
# Build the annotator and link this checkout into herdr as a local plugin.
# Re-run after code changes: the bin/ symlink points at the release build,
# so a rebuild is picked up without re-linking.
set -euo pipefail
cd "$(dirname "$0")/.."

cargo build --release
mkdir -p bin
ln -sf ../target/release/herdr-annotator bin/herdr-annotator

if herdr plugin list 2>/dev/null | grep -q "jonasbaeumer.file-annotator"; then
  echo "plugin already linked:"
else
  herdr plugin link "$PWD"
fi
herdr plugin list | grep -A2 "jonasbaeumer.file-annotator" || true

echo
echo "Register the MCP server with your agent, e.g.:"
echo "  claude mcp add annotator -- $PWD/bin/herdr-annotator mcp"
