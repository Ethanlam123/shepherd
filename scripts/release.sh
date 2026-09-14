#!/bin/bash
# Release build: universal (arm64 + x86_64) Shepherd.app + dmg, ad-hoc signed,
# minisign-signed updater artifacts. Requires TAURI_SIGNING_PRIVATE_KEY
# (see README: Update signing). Output lands in src-tauri/target/release/bundle.
set -euo pipefail
cd "$(dirname "$0")/.."

: "${TAURI_SIGNING_PRIVATE_KEY:?set TAURI_SIGNING_PRIVATE_KEY to the minisign private key content (README: Update signing)}"
export TAURI_SIGNING_PRIVATE_KEY_PASSWORD="${TAURI_SIGNING_PRIVATE_KEY_PASSWORD:-}"

# universal builds need x86_64 std, which only the rustup toolchain carries
# (a Homebrew rust ships host-std only); rustup's cargo proxies the toolchain.
# /usr/bin must also lead: the bundler shells out to `xattr`, and shims from
# Homebrew/miniconda on this class of machine reject the -r flag it passes.
export PATH="$HOME/.cargo/bin:/usr/bin:$PATH"

./scripts/sidecar.sh universal-apple-darwin
npx tauri build --target universal-apple-darwin
