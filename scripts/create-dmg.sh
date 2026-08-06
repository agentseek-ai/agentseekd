#!/bin/bash
set -euo pipefail

APP="${1:?Usage: create-dmg.sh <path-to.app> <output.dmg> <background.png>}"
OUTPUT="${2:?}"
BG="${3:?}"
VOLNAME="$(basename "$APP" .app)"
DMG_DIR="$(dirname "$OUTPUT")"
RW="$DMG_DIR/rw.dmg"

mkdir -p "$DMG_DIR"
rm -f "$RW" "$OUTPUT" 2>/dev/null || true
hdiutil detach "/Volumes/$VOLNAME" -force 2>/dev/null || true

hdiutil create -ov -volname "$VOLNAME" -fs HFS+ -size 200m "$RW" >/dev/null 2>&1

# Attach can transiently fail on shared CI runners; retry a few times.
for _ in 1 2 3; do
    if hdiutil attach "$RW" -nobrowse >/dev/null 2>&1; then
        break
    fi
    hdiutil detach "/Volumes/$VOLNAME" -force 2>/dev/null || true
    sleep 3
done
hdiutil info 2>/dev/null | grep -q "/Volumes/$VOLNAME" || {
    echo "Failed to attach $RW after 3 attempts" >&2
    exit 1
}

# Use ditto to preserve extended attributes / resource forks
ditto "$APP" "/Volumes/$VOLNAME/$(basename "$APP")"
ln -s /Applications "/Volumes/$VOLNAME/Applications"
mkdir -p "/Volumes/$VOLNAME/.background"
cp "$BG" "/Volumes/$VOLNAME/.background/dmg-background.png"

# Only ad-hoc sign when no real signing identity is available.
# When SIGNING_IDENTITY is set (CI with Apple Developer cert),
# Tauri already signed the .app during build — don't overwrite.
if [ -z "${SIGNING_IDENTITY:-}" ]; then
    codesign --force --deep --sign - "/Volumes/$VOLNAME/$(basename "$APP")" 2>/dev/null || true
fi

/usr/bin/osascript <<APPLESCRIPT
tell application "Finder"
    tell disk "$VOLNAME"
        open
        set current view of container window to icon view
        set toolbar visible of container window to false
        set statusbar visible of container window to false
        set the bounds of container window to {0, 0, 660, 400}
        set viewOptions to the icon view options of container window
        set arrangement of viewOptions to not arranged
        set icon size of viewOptions to 128
        set background picture of viewOptions to file ".background:dmg-background.png"
        set position of item "$VOLNAME" of container window to {180, 190}
        set position of item "Applications" of container window to {480, 190}
        close
        open
    end tell
end tell
APPLESCRIPT

# Fix permissions for DMG creation
chmod -Rf go-w "/Volumes/$VOLNAME" 2>/dev/null || true
rm -rf "/Volumes/$VOLNAME/.fseventsd" 2>/dev/null || true
hdiutil detach "/Volumes/$VOLNAME" >/dev/null 2>&1
hdiutil convert "$RW" -format UDZO -imagekey zlib-level=9 -o "$OUTPUT" >/dev/null 2>&1
rm -f "$RW"
echo "✓ Created: $OUTPUT"
