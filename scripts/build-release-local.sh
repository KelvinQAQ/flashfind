#!/usr/bin/env bash
# Build reproducible vX.Y.Z Linux/Windows release archives locally.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
VERSION="${VERSION:-v$(cargo metadata --no-deps --format-version 1 | python3 -c 'import json,sys; print(json.load(sys.stdin)["packages"][0]["version"])')}"
OUT_DIR="${OUT_DIR:-$ROOT/dist/$VERSION}"
TOOLS_DIR="${FLASHFIND_TOOLS_DIR:-$ROOT/.tools}"
ZIG_VERSION="${ZIG_VERSION:-0.14.1}"
ZIG_BIN="$TOOLS_DIR/zig-$ZIG_VERSION/zig"

[[ -x "$ZIG_BIN" ]] || {
  echo "Missing local Zig. Run scripts/bootstrap-cross.sh first." >&2
  exit 1
}
command -v cargo-zigbuild >/dev/null || {
  echo "Missing cargo-zigbuild. Run scripts/bootstrap-cross.sh first." >&2
  exit 1
}

# cargo-zigbuild honors this executable, so this does not require modifying
# ~/.cargo/config or installing a system-wide linker.
export PATH="$(dirname "$(command -v cargo-zigbuild)"):$PATH"
export CARGO_ZIGBUILD_ZIG_COMMAND="$ZIG_BIN"
export SOURCE_DATE_EPOCH="${SOURCE_DATE_EPOCH:-$(git log -1 --format=%ct)}"

build_target() {
  local target="$1" platform="$2" arch="$3" extension="$4"
  local asset="flashfind-$VERSION-$platform-$arch"
  local stage="$OUT_DIR/$asset"

  echo "==> $target"
  cargo zigbuild --locked --release --target "$target"
  rm -rf "$stage"
  mkdir -p "$stage"
  install -m 0755 "target/$target/release/flashfind$extension" "$stage/flashfind$extension"
  cp README.md LICENSE "$stage/"

  if [[ "$platform" == linux ]]; then
    tar --sort=name --mtime="@$SOURCE_DATE_EPOCH" --owner=0 --group=0 --numeric-owner \
      -czf "$OUT_DIR/$asset.tar.gz" -C "$OUT_DIR" "$asset"
    (cd "$OUT_DIR" && sha256sum "$asset.tar.gz") > "$OUT_DIR/$asset.tar.gz.sha256"
  else
    # Python's standard library avoids requiring a system `zip` package. Fixed
    # timestamps plus sorted entries make the archive reproducible.
    SOURCE_DATE_EPOCH="$SOURCE_DATE_EPOCH" python3 - "$OUT_DIR" "$asset" <<'PY'
import datetime
import os
import stat
import sys
import zipfile

output = os.path.abspath(sys.argv[1])
asset = sys.argv[2]
epoch = max(int(os.environ["SOURCE_DATE_EPOCH"]), 315532800)  # ZIP starts in 1980
stamp = datetime.datetime.fromtimestamp(epoch, datetime.timezone.utc).timetuple()[:6]
archive = os.path.join(output, f"{asset}.zip")
root = os.path.join(output, asset)
with zipfile.ZipFile(archive, "w", compression=zipfile.ZIP_DEFLATED, compresslevel=9) as zf:
    for directory, directories, files in os.walk(root):
        directories.sort()
        for filename in sorted(files):
            source = os.path.join(directory, filename)
            relative = os.path.relpath(source, output).replace(os.sep, "/")
            info = zipfile.ZipInfo(relative, stamp)
            info.compress_type = zipfile.ZIP_DEFLATED
            info.external_attr = (stat.S_IMODE(os.stat(source).st_mode) | stat.S_IFREG) << 16
            with open(source, "rb") as handle:
                zf.writestr(info, handle.read(), compress_type=zipfile.ZIP_DEFLATED, compresslevel=9)
PY
    (cd "$OUT_DIR" && sha256sum "$asset.zip") > "$OUT_DIR/$asset.zip.sha256"
  fi
}

rm -rf "$OUT_DIR"
mkdir -p "$OUT_DIR"
build_target x86_64-unknown-linux-musl linux x86_64 ""
build_target aarch64-unknown-linux-musl linux aarch64 ""
build_target x86_64-pc-windows-gnu windows x86_64 ".exe"
build_target aarch64-pc-windows-gnullvm windows aarch64 ".exe"

printf '\nRelease assets:\n'
(cd "$OUT_DIR" && sha256sum -c ./*.sha256 && find . -maxdepth 1 -type f -printf '%f\n' | sort)
