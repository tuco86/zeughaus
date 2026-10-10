#!/bin/sh
# Builds the editor and wraps it in Zeughaus.app (macOS).
#
#   deploy/macos-app.sh [destination]     default: ~/Applications
#
# A macOS app has its icon and its name in the Info.plist of the bundle it
# runs in; a plain binary has neither, which is why `cargo run` shows the
# generic executable icon in the Dock. The bundle is therefore the icon:
# deploy/macos/Info.plist plus an icns that render.py draws at the ten sizes
# iconutil wants, around a release build of the editor.
#
# Only the editor. The runner is a headless process with no Dock presence; on
# macOS it is started by hand or by a launchd agent of the user's making, and
# the store (`spacetime start`) the same way.
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
dest=${1:-$HOME/Applications}
app=$dest/Zeughaus.app
version=$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/Cargo.toml" | head -1)

say() { printf '[deploy] %s\n' "$*"; }

say "building the editor"
cargo build --locked --release --manifest-path "$root/Cargo.toml" -p zeughaus

say "icns"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
mkdir "$work/zeughaus.iconset"
# iconutil names each size; @2x is the same pixel count as the next size up,
# drawn again rather than copied so neither file is a scaled one.
set -- 16:icon_16x16 32:icon_16x16@2x 32:icon_32x32 64:icon_32x32@2x \
    128:icon_128x128 256:icon_128x128@2x 256:icon_256x256 512:icon_256x256@2x \
    512:icon_512x512 1024:icon_512x512@2x
for entry; do
    python3 "$root/zeughaus/assets/icon/render.py" "${entry%%:*}" \
        "$work/zeughaus.iconset/${entry#*:}.png" >/dev/null
done
iconutil -c icns "$work/zeughaus.iconset" -o "$work/zeughaus.icns"

say "bundle into $app"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
sed "s|@VERSION@|$version|g" "$root/deploy/macos/Info.plist" >"$app/Contents/Info.plist"
cp "$work/zeughaus.icns" "$app/Contents/Resources/zeughaus.icns"
cp "${CARGO_TARGET_DIR:-$root/target}/release/zeughaus" "$app/Contents/MacOS/zeughaus"
# Ad-hoc signature: an unsigned binary that was copied around is killed on
# launch by Gatekeeper on Apple silicon.
codesign --force --sign - "$app" >/dev/null 2>&1 || say "codesign failed; the app may refuse to launch"
# LaunchServices caches the bundle by its modification date; without this the
# Dock keeps the icon of the bundle it saw before.
touch "$app"

say "done: open $app"
