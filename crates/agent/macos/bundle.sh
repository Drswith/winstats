#!/bin/sh
# winstats-agent.app from the agent's macOS builds: one universal executable, Info.plist with the
# crate version, the icon, an ad-hoc signature. Run from the repository root on a Mac:
#   crates/agent/macos/bundle.sh OUT_DIR BINARY...   (one or more builds, e.g. x86_64 and arm64)
set -eu
out="$1"
shift
app="$out/winstats-agent.app"
version=$(sed -n 's/^version = "\(.*\)"/\1/p' crates/agent/Cargo.toml | head -n 1)
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
lipo -create -output "$app/Contents/MacOS/winstats-agent" "$@"
sed "s/@VERSION@/$version/g" crates/agent/macos/Info.plist > "$app/Contents/Info.plist"
iconset=$(mktemp -d)/winstats.iconset
python3 assets/make_icon.py --iconset "$iconset" > /dev/null
iconutil -c icns "$iconset" -o "$app/Contents/Resources/winstats.icns"
rm -rf "$(dirname "$iconset")"
codesign --force --sign - --identifier org.mgvs.winstats-agent "$app"
echo "$app ($version)"
