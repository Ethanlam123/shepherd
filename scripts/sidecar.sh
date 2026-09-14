#!/bin/bash
# Places shepherd-hook where tauri's externalBin expects it:
# src-tauri/binaries/shepherd-hook-<triple>. Builds the host triple by
# default; builds universal-apple-darwin (lipo of arm64 + x86_64) when passed
# explicitly or set via TAURI_ENV_TARGET_TRIPLE from tauri's build hooks.
set -euo pipefail
cd "$(dirname "$0")/.."

triple="${1:-${TAURI_ENV_TARGET_TRIPLE:-$(rustc -vV | sed -n 's/^host: //p')}}"
mkdir -p src-tauri/binaries

if [ "$triple" = "universal-apple-darwin" ]; then
  rustup target add aarch64-apple-darwin x86_64-apple-darwin
  cargo build -p shepherd-hook --release --target aarch64-apple-darwin
  cargo build -p shepherd-hook --release --target x86_64-apple-darwin
  # tauri builds the app per-arch before lipoing, so each arch needs its sidecar
  cp target/aarch64-apple-darwin/release/shepherd-hook src-tauri/binaries/shepherd-hook-aarch64-apple-darwin
  cp target/x86_64-apple-darwin/release/shepherd-hook src-tauri/binaries/shepherd-hook-x86_64-apple-darwin
  lipo -create \
    target/aarch64-apple-darwin/release/shepherd-hook \
    target/x86_64-apple-darwin/release/shepherd-hook \
    -output "src-tauri/binaries/shepherd-hook-$triple"
else
  cargo build -p shepherd-hook --release --target "$triple"
  cp "target/$triple/release/shepherd-hook" "src-tauri/binaries/shepherd-hook-$triple"
  # dev (tauri dev) runs from target/debug; keep the plain name there too
  cp "target/$triple/release/shepherd-hook" "target/debug/shepherd-hook" 2>/dev/null || true
fi
