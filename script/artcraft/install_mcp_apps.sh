#!/usr/bin/env bash
# macOS only: builds artcraft-native and installs it as two MCP server apps,
#   /Applications/PhotoCraft.app/Contents/MacOS/photocraft  (image tools)
#   /Applications/FilmCraft.app/Contents/MacOS/filmcraft    (video tools)
# The binary picks its tool set from its own name when started with `--mcp`.
# Run from the repository root. Pass a different install folder as the first argument.

set -euo pipefail

install_dir="${1:-/Applications}"
root_dir=$(pwd)
binary="${root_dir}/target/release/artcraft-native"

echo "Building artcraft-native (release)..."
cargo build -p artcraft_native --release

install_app() {
  local app_name="$1"
  local exe_name="$2"
  local bundle_id="$3"
  local app_path="${install_dir}/${app_name}.app"

  echo "Installing ${app_path}..."
  mkdir -p "${app_path}/Contents/MacOS"
  cp "${binary}" "${app_path}/Contents/MacOS/${exe_name}"
  cat > "${app_path}/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>${app_name}</string>
  <key>CFBundleIdentifier</key><string>${bundle_id}</string>
  <key>CFBundleExecutable</key><string>${exe_name}</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>0.1.0</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST
  # Ad-hoc sign so Gatekeeper lets a locally built binary run.
  codesign --force --sign - "${app_path}" >/dev/null 2>&1 || echo "  (codesign skipped)"
}

install_app "PhotoCraft" "photocraft" "ai.artcraft.photocraft"
install_app "FilmCraft" "filmcraft" "ai.artcraft.filmcraft"

echo ""
echo "Done. Sign in once in the ArtCraft app (or run ${binary}) so the MCP servers have a session."
