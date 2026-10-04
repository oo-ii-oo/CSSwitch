#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TARGET="x86_64-apple-darwin"

if [[ "$(uname -s)" != Darwin ]]; then
  echo "Intel Mac 安装包必须在 macOS 上构建；也可使用手动 GitHub Actions 流程。" >&2
  exit 1
fi

# Finder-launched Terminal sessions may not have loaded Cargo's shell setup.
export PATH="${HOME}/.cargo/bin:$PATH"
for tool in npm node cargo rustup xcrun python3 ditto hdiutil lipo codesign; do
  if ! command -v "$tool" >/dev/null 2>&1; then
    echo "缺少构建工具：$tool。请按 docs/operations/development.md 的 Intel 步骤安装。" >&2
    exit 1
  fi
done
xcrun --find clang >/dev/null

# Keep the Intel output separate from native/acceptance builds. build.rs stages
# the Gateway for the same TARGET from its own nested Cargo output directory.
export CARGO_TARGET_DIR="$ROOT/desktop/src-tauri/target/intel"
export MACOSX_DEPLOYMENT_TARGET=13.0
export APPLE_SIGNING_IDENTITY="-"
rustup target add "$TARGET"
cd "$ROOT/desktop"
npm ci
npm run tauri -- build --target "$TARGET" --config src-tauri/tauri.intel.conf.json --bundles app --ci -- --locked

APP="$CARGO_TARGET_DIR/$TARGET/release/bundle/macos/CSSwitch.app"
python3 "$ROOT/scripts/verify-macos-intel.py" --app "$APP"

VERSION="$(node -p "JSON.parse(require('fs').readFileSync('src-tauri/tauri.conf.json', 'utf8')).version")"
OUTPUT="$ROOT/dist/intel"
mkdir -p "$OUTPUT"
WORK_DIR="$(mktemp -d "${TMPDIR:-/tmp}/csswitch-intel.XXXXXX")"
trap 'rm -rf "$WORK_DIR"' EXIT
mkdir "$WORK_DIR/payload"
ditto "$APP" "$WORK_DIR/payload/CSSwitch.app"
ln -s /Applications "$WORK_DIR/payload/Applications"

# A fresh staging directory prevents old Test/Acceptance bundles from entering
# the image. Validate the mounted bytes before replacing any prior output.
NAME="CSSwitch_${VERSION}_x64.dmg"
hdiutil create -volname CSSwitch -srcfolder "$WORK_DIR/payload" -format UDZO "$WORK_DIR/$NAME"
hdiutil verify "$WORK_DIR/$NAME"
python3 "$ROOT/scripts/verify-macos-intel.py" --dmg "$WORK_DIR/$NAME" --report "$WORK_DIR/build-verification.json"
mv "$WORK_DIR/$NAME" "$OUTPUT/$NAME"
mv "$WORK_DIR/build-verification.json" "$OUTPUT/build-verification.json"
(cd "$OUTPUT" && shasum -a 256 "$NAME" > "$NAME.sha256")
echo "构建完成：$OUTPUT/$NAME"
echo "主程序与 Gateway 均已核对为 Intel x86_64；Science 和真实 Provider 需另行真机验证。"
