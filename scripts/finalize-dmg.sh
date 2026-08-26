#!/bin/bash
# Post-processes a .dmg built by `tauri build`:
#   1. Ad-hoc signs the whole .app bundle (`tauri build` only leaves Rust's
#      automatic per-binary linker signature - the bundle itself has no
#      sealed resources, which is exactly what makes Gatekeeper report a
#      freshly-downloaded copy as "damaged" on another machine) and swaps
#      the signed copy into the dmg. This does NOT get it fully past
#      Gatekeeper on another Mac - that needs a paid Developer ID and
#      notarization - but turns "damaged, move to Trash" (no bypass offered)
#      into the standard "unidentified developer" prompt (right-click > Open,
#      or System Settings > Privacy & Security > Open Anyway).
#   2. Styles the Finder window shown on open (icon view, background image,
#      hidden toolbar, app/Applications icon positions) via Finder AppleScript.
#      Tauri's own bundle_dmg.sh is supposed to do this but doesn't reliably
#      stick in this environment, so we redo it ourselves and take it over.
#   3. Sets the volume's icon (shown once mounted) and the .dmg file's own
#      Finder icon (shown before mounting) to the app's icon.
#
# Usage: scripts/finalize-dmg.sh path/to/App.dmg [path/to/icon.icns]
set -euo pipefail

DMG="$1"
ICNS="${2:-$(dirname "$0")/../src-tauri/icons/icon.icns}"
WINDOW_WIDTH=660
WINDOW_HEIGHT=400
APP_ICON_X=180
APP_ICON_Y=170
APPS_ICON_X=480
APPS_ICON_Y=170
ICON_SIZE=128

if [ ! -f "$DMG" ]; then
  echo "error: dmg not found: $DMG" >&2
  exit 1
fi
if [ ! -f "$ICNS" ]; then
  echo "error: icon not found: $ICNS" >&2
  exit 1
fi

WORK="$(mktemp -d)"
cleanup() {
  if [ -n "${MOUNT_POINT:-}" ] && mount | grep -q "$MOUNT_POINT"; then
    hdiutil detach "$MOUNT_POINT" -force -quiet || true
  fi
  rm -rf "$WORK"
}
trap cleanup EXIT

# A prior run (or manual poking around) can leave this dmg's volume mounted,
# which shifts the Finder disk name this run gets (e.g. "Foo 1") and breaks
# the `tell disk "$VOLUME_NAME"` addressing below - eject any stale mounts
# of this exact dmg first so each run starts from a clean slate.
ABS_DMG="$(cd "$(dirname "$DMG")" && pwd)/$(basename "$DMG")"
while read -r stale_disk; do
  [ -n "$stale_disk" ] && hdiutil detach "$stale_disk" -force -quiet 2>/dev/null || true
done < <(hdiutil info -plist | python3 -c "
import plistlib, sys
data = plistlib.loads(sys.stdin.buffer.read())
for img in data.get('images', []):
    if img.get('image-path') == '$ABS_DMG':
        for e in img.get('system-entities', []):
            if 'dev-entry' in e and 'mount-point' in e:
                print(e['dev-entry'])
")

RW_DMG="$WORK/rw.dmg"
hdiutil convert "$DMG" -format UDRW -o "$RW_DMG" -quiet

# Attach browseable (no -nobrowse) so Finder can address it by disk name.
ATTACH_PLIST="$WORK/attach.plist"
hdiutil attach "$RW_DMG" -plist > "$ATTACH_PLIST"
MOUNT_POINT="$(python3 -c "
import plistlib
with open('$ATTACH_PLIST', 'rb') as f:
    data = plistlib.load(f)
for e in data['system-entities']:
    if 'mount-point' in e:
        print(e['mount-point'])
")"
VOLUME_NAME="$(basename "$MOUNT_POINT")"
echo "mounted as: $VOLUME_NAME"

shopt -s nullglob
apps=("$MOUNT_POINT"/*.app)
shopt -u nullglob
if [ ${#apps[@]} -ne 1 ]; then
  echo "error: expected exactly one .app in $MOUNT_POINT, found ${#apps[@]}" >&2
  exit 1
fi
APP_NAME="$(basename "${apps[0]}")"

# Sign the app bundle that sits alongside the dmg in Tauri's own output
# layout (.../bundle/dmg/Foo.dmg + .../bundle/macos/Foo.app), then swap the
# signed copy into the mounted dmg in place of Tauri's unsigned one.
SOURCE_APP="$(dirname "$DMG")/../macos/$APP_NAME"
if [ -d "$SOURCE_APP" ]; then
  codesign --force --deep --sign - "$SOURCE_APP"
  rm -rf "${MOUNT_POINT:?}/$APP_NAME"
  cp -R "$SOURCE_APP" "$MOUNT_POINT/$APP_NAME"
else
  echo "warning: couldn't find $SOURCE_APP next to the dmg - shipping it unsigned" >&2
fi

cp "$ICNS" "$MOUNT_POINT/.VolumeIcon.icns"
SetFile -a C "$MOUNT_POINT"

# Give Finder a moment to notice the freshly-mounted volume before scripting it.
sleep 1

osascript <<APPLESCRIPT
tell application "Finder"
    tell disk "$VOLUME_NAME"
        open
        delay 1
        set current view of container window to icon view
        set toolbar visible of container window to false
        set statusbar visible of container window to false
        set the bounds of container window to {400, 100, ${WINDOW_WIDTH} + 400, ${WINDOW_HEIGHT} + 100}
        set theViewOptions to the icon view options of container window
        set arrangement of theViewOptions to not arranged
        set icon size of theViewOptions to ${ICON_SIZE}
        set background picture of theViewOptions to file ".background:background.png"
        set position of item "$APP_NAME" of container window to {${APP_ICON_X}, ${APP_ICON_Y}}
        set position of item "Applications" of container window to {${APPS_ICON_X}, ${APPS_ICON_Y}}
        update without registering applications
        delay 1
        close
    end tell
end tell
APPLESCRIPT

sync
hdiutil detach "$MOUNT_POINT" -quiet
MOUNT_POINT=""
hdiutil convert "$RW_DMG" -format UDZO -o "$DMG" -ov -quiet

# Set the icon Finder shows for the .dmg file itself (before mounting), via
# the classic resource-fork custom-icon mechanism. `sips -i` mutates its
# target in place to add the resource fork it needs - run it on a scratch
# copy, never on $ICNS itself, since $ICNS is also the source Tauri bundles
# into the .app, and a resource-forked icon.icns breaks codesign (Gatekeeper
# then reports the app as "damaged").
ICON_RSRC="$WORK/icon.rsrc"
ICNS_SCRATCH="$WORK/icon-for-rsrc.icns"
cp "$ICNS" "$ICNS_SCRATCH"
sips -i "$ICNS_SCRATCH" >/dev/null
DeRez -only icns "$ICNS_SCRATCH" > "$ICON_RSRC"
Rez -append "$ICON_RSRC" -o "$DMG"
SetFile -a C "$DMG"

echo "Finalized: $DMG"
