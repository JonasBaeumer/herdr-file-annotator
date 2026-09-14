#!/bin/sh
# Re-vendor the bundled TypeScript/TSX grammars from the pinned bat commit.
# See assets/syntaxes/README.md for provenance and why these files exist.
# Usage: scripts/update-syntaxes.sh
# To move to a newer upstream: edit PIN, re-run, update the commit hash in
# assets/syntaxes/README.md, and run `cargo test`.
set -eu

PIN="7323a7514f7601737640e7172be115127d6db08c"
BASE="https://raw.githubusercontent.com/sharkdp/bat/${PIN}/assets/syntaxes/02_Extra"
DEST="$(dirname "$0")/../assets/syntaxes"

curl -sSL "${BASE}/TypeScript.sublime-syntax" -o "${DEST}/TypeScript.sublime-syntax"
curl -sSL "${BASE}/TypsecriptReact.sublime-syntax" -o "${DEST}/TSX.sublime-syntax"
# NOTE: upstream filename carries the "Typsecript" typo; ours is TSX.sublime-syntax.

ls -la "${DEST}"
