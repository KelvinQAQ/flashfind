#!/usr/bin/env bash
# Install a user-local, cross-platform Rust build toolchain.
# No sudo and no global shell-profile changes are required.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TOOLS_DIR="${FLASHFIND_TOOLS_DIR:-$ROOT/.tools}"
ZIG_VERSION="${ZIG_VERSION:-0.14.1}"
# ziglang.com.cn is the official Chinese Zig community mirror. Override this
# variable for an intranet mirror or a newer Zig version.
ZIG_MIRROR="${ZIG_MIRROR:-https://ziglang.com.cn/download}"
RUSTUP_DIST_SERVER="${RUSTUP_DIST_SERVER:-https://rsproxy.cn}"
RUSTUP_UPDATE_ROOT="${RUSTUP_UPDATE_ROOT:-https://rsproxy.cn/rustup}"
export RUSTUP_DIST_SERVER RUSTUP_UPDATE_ROOT

ZIG_DIR="$TOOLS_DIR/zig-$ZIG_VERSION"
ZIG_ARCHIVE="$TOOLS_DIR/zig-$ZIG_VERSION.tar.xz"
ZIG_BIN="$ZIG_DIR/zig"
TARGETS=(
  x86_64-unknown-linux-musl
  aarch64-unknown-linux-musl
  x86_64-pc-windows-gnu
  aarch64-pc-windows-gnullvm
)

require() {
  command -v "$1" >/dev/null || { echo "missing required command: $1" >&2; exit 1; }
}
require cargo
require rustup
require curl
require tar

mkdir -p "$TOOLS_DIR"
printf 'Rustup mirror: %s\nZig mirror: %s\n' "$RUSTUP_DIST_SERVER" "$ZIG_MIRROR"

for target in "${TARGETS[@]}"; do
  if ! rustup target list --installed | grep -qx "$target"; then
    echo "Installing Rust target: $target"
    rustup target add "$target"
  fi
done

if ! command -v cargo-zigbuild >/dev/null; then
  echo "Installing cargo-zigbuild through the configured Cargo registry"
  cargo install cargo-zigbuild --locked
fi

if [[ ! -x "$ZIG_BIN" ]]; then
  url="$ZIG_MIRROR/$ZIG_VERSION/zig-x86_64-linux-$ZIG_VERSION.tar.xz"
  echo "Downloading Zig $ZIG_VERSION: $url"
  # -C - makes interrupted domestic-network downloads resumable.
  curl --fail --location --retry 4 --retry-delay 2 --continue-at - \
    --output "$ZIG_ARCHIVE" "$url"
  tmp="$TOOLS_DIR/.zig-extract-$$"
  rm -rf "$tmp"
  mkdir -p "$tmp"
  tar -xJf "$ZIG_ARCHIVE" -C "$tmp"
  extracted="$tmp/zig-x86_64-linux-$ZIG_VERSION"
  [[ -x "$extracted/zig" ]] || { echo "unexpected Zig archive layout" >&2; exit 1; }
  rm -rf "$ZIG_DIR"
  mv "$extracted" "$ZIG_DIR"
  rm -rf "$tmp"
fi

"$ZIG_BIN" version
printf '\nCross toolchain ready. Build with:\n  scripts/build-release-local.sh\n'
