#!/usr/bin/env bash
# Builds HQ for macOS as a universal app (Apple Silicon and Intel): HQ.app, a .dmg, and the
# .app.tar.gz plus .sig that installed copies update from. Run it on a Mac.
#
# The updater files are only made when TAURI_SIGNING_PRIVATE_KEY (the key itself, or a path to it)
# is set, with TAURI_SIGNING_PRIVATE_KEY_PASSWORD if it has a password. The app is signed ad hoc
# (no Apple Developer ID), so the first launch needs "Open Anyway" (see README.md).
set -euo pipefail
cd "$(dirname "$0")"

rustup target add aarch64-apple-darwin x86_64-apple-darwin

# Without the signing key there can be no updater files, and the build would fail asking for it.
extra=()
if [[ -z "${TAURI_SIGNING_PRIVATE_KEY:-}" ]]; then
  echo "TAURI_SIGNING_PRIVATE_KEY is not set: building without updater files."
  extra=(--config '{"bundle":{"createUpdaterArtifacts":false}}')
fi

# Prefer an installed Tauri CLI (`cargo install tauri-cli --version '^2'`), else fetch it with npx.
if cargo tauri --version >/dev/null 2>&1; then
  cargo tauri build --target universal-apple-darwin ${extra[@]+"${extra[@]}"} "$@"
else
  npx --yes @tauri-apps/cli@2.12.3 build --target universal-apple-darwin ${extra[@]+"${extra[@]}"} "$@"
fi

bundle="target/universal-apple-darwin/release/bundle"
echo
echo "Built:"
ls -1 "$bundle"/dmg/*.dmg "$bundle"/macos/*.tar.gz "$bundle"/macos/*.sig 2>/dev/null || true
