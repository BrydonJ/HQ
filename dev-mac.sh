#!/usr/bin/env bash
# Builds and starts the dev copy of HQ (the Mac counterpart of dev.cmd). It runs next to the
# installed HQ with its own sessions (profile "dev") and reads ui/ from disk, so Cmd+R shows UI
# edits without a rebuild. Inspect it from Safari's Develop menu.
set -euo pipefail
cd "$(dirname "$0")"
EXE="$PWD/target/debug/hq"
# A running dev copy would be replaced under it, so stop that exact file (never the installed HQ).
pkill -fx "$(printf '%s' "$EXE" | sed 's/[][\\.^$*+?(){}|]/\\&/g')" 2>/dev/null || true
cargo build "$@"
"$EXE" &
