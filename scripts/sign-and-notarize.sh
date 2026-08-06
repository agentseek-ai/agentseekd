#!/bin/bash
#
# sign-and-notarize.sh — macOS DMG signing + notarization (CI only)
#
# Usage:
#   ./scripts/sign-and-notarize.sh \
#     --dmg ~/Downloads/AgentSeek_Desktop.dmg \
#     --output ~/Desktop/signed.dmg \
#     [--tag v0.0.1-rc.1]
#
# Prerequisites:
#   1. Developer ID Application certificate imported into Keychain
#   2. Environment variables:
#      export APPLE_SIGNING_IDENTITY="Developer ID Application: Your Name (TEAMXXXXXX)"
#      export APPLE_ID="your@email.com"
#      export APPLE_APP_SPECIFIC_PASSWORD="xxxx-xxxx-xxxx-xxxx"
#      export APPLE_TEAM_ID="TEAMXXXXXX"
#
# Steps:
#   1. Extract .app from DMG
#   2. Sign .app (with --options runtime + --timestamp)
#   3. Rebuild DMG with custom background
#   4. Sign DMG
#   5. Notarize + Staple
#   6. Upload to GitHub Release (if --tag specified)

set -euo pipefail

APP_NAME="AgentSeek Desktop"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
BG_IMAGE=""
WORK_DIR="$(mktemp -d)"
UPLOAD_TAG=""

# ── Color output (to stderr) ──
red()    { printf "\033[31m%s\033[0m\n" "$1" >&2; }
green()  { printf "\033[32m%s\033[0m\n" "$1" >&2; }
yellow() { printf "\033[33m%s\033[0m\n" "$1" >&2; }
cyan()   { printf "\033[36m%s\033[0m\n" "$1" >&2; }

# ── Cleanup temp files ──
cleanup() {
    if [ -d "$WORK_DIR" ]; then
        rm -rf "$WORK_DIR"
    fi
}
trap cleanup EXIT

# ── Check environment ──
check_env() {
    for cmd in codesign hdiutil; do
        if ! command -v "$cmd" &>/dev/null; then
            red "✗ Command not found: $cmd"
            exit 1
        fi
    done
    if [ -z "${APPLE_SIGNING_IDENTITY:-}" ]; then
        red "✗ Missing environment variable: APPLE_SIGNING_IDENTITY"
        exit 1
    fi
    if [ -z "${APPLE_ID:-}" ] || [ -z "${APPLE_APP_SPECIFIC_PASSWORD:-}" ] || [ -z "${APPLE_TEAM_ID:-}" ]; then
        red "✗ Missing notarization credentials"
        echo "  Set env: APPLE_ID, APPLE_APP_SPECIFIC_PASSWORD, APPLE_TEAM_ID" >&2
        exit 1
    fi
    if [ -n "$BG_IMAGE" ] && [ ! -f "$BG_IMAGE" ]; then
        red "✗ Background image not found: $BG_IMAGE"
        exit 1
    fi
}

# ── Mount DMG and extract .app + background ──
extract_app() {
    local dmg="$1"
    cyan "📂 Mounting DMG and extracting .app..."

    local dmg_real
    dmg_real="$(cd "$(dirname "$dmg")" && pwd)/$(basename "$dmg")"
    cyan "  DMG path: $dmg_real"

    # Eject any previously mounted instance of this DMG (prevents hang).
    # NOTE: mount points can contain spaces ("AgentSeek Desktop"), so paths
    # are extracted from `hdiutil info` lines via regex, never via awk fields.
    local existing_mount=""
    existing_mount="$(hdiutil info 2>/dev/null \
        | awk -v dmg="$dmg_real" '
            /image-path/  { path=$0; sub(/^.*image-path:[[:space:]]*/, "", path) }
            /mount-point/ { mp=$0;  sub(/^.*mount-point:[[:space:]]*/, "", mp) }
            END { if (path == dmg) print mp }
        ')"
    if [ -n "$existing_mount" ] && [ -d "$existing_mount" ]; then
        yellow "   DMG already mounted at $existing_mount, ejecting first..."
        hdiutil detach "$existing_mount" -force 2>/dev/null || true
        sleep 1
    fi

    # Mount the DMG (use default mount point, more reliable than -mountpoint).
    # hdiutil attach can transiently fail on shared CI runners (output stops
    # at "Checksumming..." with no mount point), so retry a few times and
    # fall back to `hdiutil info` when the attach output is truncated.
    cyan "  Mounting..."
    local attach_output=""
    local mount_point=""
    local attempt
    for attempt in 1 2 3; do
        attach_output="$(hdiutil attach "$dmg_real" -nobrowse -readonly 2>&1)" || {
            red " Failed to mount DMG (attempt $attempt/3)"
            red "  Output: $attach_output"
            hdiutil detach "$dmg_real" -force 2>/dev/null || true
            sleep 3
            continue
        }
        # Extract mount point from attach output (last non-empty line, minus
        # the device and filesystem columns), falling back to hdiutil info
        # when the output is truncated. Keep spaces in the mount point.
        mount_point="$(echo "$attach_output" \
            | grep -v '^[[:space:]]*$' \
            | tail -1 \
            | sed -E 's/^([^[:space:]]+[[:space:]]+){2}//')"
        if [ -z "$mount_point" ] || [ ! -d "$mount_point" ]; then
            mount_point="$(hdiutil info 2>/dev/null \
                | awk -v dmg="$dmg_real" '
                    /image-path/  { path=$0; sub(/^.*image-path:[[:space:]]*/, "", path) }
                    /mount-point/ { mp=$0;  sub(/^.*mount-point:[[:space:]]*/, "", mp) }
                    END { if (path == dmg) print mp }
                ')"
        fi
        if [ -n "$mount_point" ] && [ -d "$mount_point" ]; then
            break
        fi
        red " Could not determine mount point (attempt $attempt/3)"
        red "  attach output: $attach_output"
        hdiutil detach "${mount_point:-$dmg_real}" -force 2>/dev/null || true
        sleep 3
        mount_point=""
    done
    if [ -z "$mount_point" ]; then
        red " Failed to mount DMG after 3 attempts"
        exit 1
    fi
    green "  ✓ Mounted at: $mount_point"

    # List mount contents
    ls -la "$mount_point" 2>&1 | sed 's/^/  /' >&2

    local app_path
    app_path="$(find "$mount_point" -maxdepth 2 -name '*.app' -type d | head -1)"
    if [ -z "$app_path" ]; then
        red " No .app found inside DMG"
        echo "  Mount contents:" >&2
        find "$mount_point" -maxdepth 2 2>&1 | sed 's/^/    /' >&2
        exit 1
    fi
    cyan "  Found: $(basename "$app_path")"

    cp -R "$app_path" "$WORK_DIR/"

    # Extract background image from DMG if available and not specified
    if [ -z "$BG_IMAGE" ]; then
        local dmg_bg
        dmg_bg="$(find "$mount_point" -maxdepth 3 -path '*/.background/*.png' | head -1)"
        if [ -n "$dmg_bg" ] && [ -f "$dmg_bg" ]; then
            cp "$dmg_bg" "$WORK_DIR/dmg-background.png"
            BG_IMAGE="$WORK_DIR/dmg-background.png"
            green "  ✓ Background image extracted from DMG"
        fi
    fi

    # Unmount the DMG
    hdiutil detach "$mount_point" 2>/dev/null || true

    EXTRACTED_APP="$WORK_DIR/$(basename "$app_path")"
}

# ── Sign .app ─
sign_app() {
    local app="$1"
    cyan "🔐 Signing .app ($APPLE_SIGNING_IDENTITY)..."

    # Remove old signature
    cyan "  Removing old signature..."
    find "$app" -name '_CodeSignature' -type d -exec rm -rf {} + 2>/dev/null || true
    xattr -cr "$app" 2>/dev/null || true

    # 1. Sign all Frameworks first (inside-out)
    if [ -d "$app/Contents/Frameworks" ]; then
        find "$app/Contents/Frameworks" -type d -name "*.framework" | while read -r fw; do
            codesign --deep --force --verbose --options runtime --timestamp --sign "$APPLE_SIGNING_IDENTITY" "$fw" 2>&1 | sed 's/^/    [FW] /' >&2 || true
        done
    fi

    # 2. Sign all Helper Apps
    find "$app" -type d -name "*.app" ! -path "$app" | while read -r helper; do
        codesign --deep --force --verbose --options runtime --timestamp --sign "$APPLE_SIGNING_IDENTITY" "$helper" 2>&1 | sed 's/^/    [HLP] /' >&2 || true
    done

    # 3. Sign main app (must be last)
    local sign_rc
    codesign --deep --force --verbose=4 --options runtime --timestamp --sign "$APPLE_SIGNING_IDENTITY" "$app" 2>&1 | sed 's/^/  /' >&2 || sign_rc=$?
    sign_rc=${sign_rc:-0}

    if [ "$sign_rc" -ne 0 ]; then
        red "✗ Main app signing failed (exit code: $sign_rc)"
        exit 1
    fi

    cyan "✓ Verifying signature..."
    if codesign --verify --deep --strict --verbose=4 "$app" 2>&1 | tail -5 >&2; then
        green "  ✓ .app signature verified"
    else
        red "✗ Signature verification failed, check logs above"
        exit 1
    fi
}

# ─ Rebuild DMG ──
rebuild_dmg() {
    local app="$1"
    local output="$2"
    local volname
    volname="$(basename "$app" .app)"
    local dmg_dir
    dmg_dir="$(dirname "$output")"
    local rw="$dmg_dir/rw.dmg"

    if [ -z "$BG_IMAGE" ] || [ ! -f "$BG_IMAGE" ]; then
        red "✗ No background image available, cannot rebuild DMG"
        exit 1
    fi

    cyan "📦 Rebuilding DMG (with custom background)..."
    mkdir -p "$dmg_dir"
    rm -f "$rw" "$output" 2>/dev/null || true
    hdiutil detach "/Volumes/$volname" -force 2>/dev/null || true

    hdiutil create -ov -volname "$volname" -fs HFS+ -size 200m "$rw" >/dev/null 2>&1
    hdiutil attach "$rw" -nobrowse >/dev/null 2>&1

    ditto "$app" "/Volumes/$volname/$(basename "$app")"
    ln -s /Applications "/Volumes/$volname/Applications"
    mkdir -p "/Volumes/$volname/.background"
    cp "$BG_IMAGE" "/Volumes/$volname/.background/dmg-background.png"

    /usr/bin/osascript <<APPLESCRIPT 2>&1 | sed 's/^/  /' >&2
tell application "Finder"
    tell disk "$volname"
        open
        set current view of container window to icon view
        set toolbar visible of container window to false
        set statusbar visible of container window to false
        set the bounds of container window to {0, 0, 660, 400}
        set viewOptions to the icon view options of container window
        set arrangement of viewOptions to not arranged
        set icon size of viewOptions to 128
        set background picture of viewOptions to file ".background:dmg-background.png"
        set position of item "$volname" of container window to {180, 190}
        set position of item "Applications" of container window to {480, 190}
        close
        open
    end tell
end tell
APPLESCRIPT

    chmod -Rf go-w "/Volumes/$volname" 2>/dev/null || true
    rm -rf "/Volumes/$volname/.fseventsd" 2>/dev/null || true
    hdiutil detach "/Volumes/$volname" >/dev/null 2>&1
    hdiutil convert "$rw" -format UDZO -imagekey zlib-level=9 -o "$output" >/dev/null 2>&1
    rm -f "$rw"
    green "  ✓ DMG created"
}

# ── Sign DMG ─
sign_dmg() {
    local dmg="$1"
    cyan "🔐 Signing DMG..."
    codesign --sign "$APPLE_SIGNING_IDENTITY" --force --timestamp "$dmg"
    green "  ✓ DMG signed"
}

# ── Notarize DMG ──
notarize_dmg() {
    local dmg="$1"
    cyan "📤 Submitting for notarization..."

    local notary_output
    notary_output="$(xcrun notarytool submit "$dmg" \
        --apple-id "$APPLE_ID" \
        --password "$APPLE_APP_SPECIFIC_PASSWORD" \
        --team-id "$APPLE_TEAM_ID" --wait 2>&1)"
    echo "$notary_output" | sed 's/^/  /' >&2

    # Check actual notarization status (notarytool may return 0 even when Invalid)
    if echo "$notary_output" | grep -q 'status: Accepted'; then
        green "  ✓ Notarization approved"
    else
        red " Notarization rejected (status not Accepted)"
        echo "  Check the log above for rejection reasons" >&2
        # Fetch detailed log if submission ID is available
        local submission_id
        submission_id="$(echo "$notary_output" | grep -o 'Submission ID: [a-f0-9-]*' | awk '{print $3}')"
        if [ -n "$submission_id" ]; then
            echo "  Fetching notarization log ($submission_id)..." >&2
            xcrun notarytool log "$submission_id" \
                --apple-id "$APPLE_ID" \
                --password "$APPLE_APP_SPECIFIC_PASSWORD" \
                --team-id "$APPLE_TEAM_ID" 2>&1 | sed 's/^/  /' >&2 || true
        fi
        exit 1
    fi
}

# ── Staple notarization ticket (with retry) ──
staple_dmg() {
    local dmg="$1"
    cyan "📌 Stapling notarization ticket..."

    # Apple CloudKit may take a few minutes to propagate the ticket after approval.
    # Retry up to 5 times with 30s delay.
    local max_retries=5
    local delay=30
    for i in $(seq 1 $max_retries); do
        if xcrun stapler staple "$dmg" 2>&1 | sed 's/^/  /' >&2; then
            green "  ✓ Ticket stapled (attempt $i)"
            return 0
        fi
        if [ "$i" -lt "$max_retries" ]; then
            yellow "  ⚠ Staple attempt $i failed, retrying in ${delay}s..."
            sleep "$delay"
        fi
    done
    red "✗ Stapling failed after $max_retries attempts"
    red "  The notarization was approved, but the ticket could not be stapled."
    red "  You can manually staple later: xcrun stapler staple \"$dmg\""
    exit 1
}

# ── Upload to GitHub Release ──
upload_release() {
    local dmg="$1"
    local tag="$2"
    cyan "🚀 Uploading to GitHub Release ($tag)..."
    if ! gh release upload "$tag" "$dmg" --clobber 2>&1 | sed 's/^/  /' >&2; then
        red "✗ Upload failed"
        exit 1
    fi
    green "  ✓ Uploaded to GitHub Release"
}

# ── Main ──
main() {
    local dmg_file=""
    local output_file=""

    while [ $# -gt 0 ]; do
        case "$1" in
            --dmg)
                dmg_file="$2"
                shift 2
                ;;
            --output)
                output_file="$2"
                shift 2
                ;;
            --background)
                BG_IMAGE="$2"
                shift 2
                ;;
            --identity)
                APPLE_SIGNING_IDENTITY="$2"
                shift 2
                ;;
            --tag)
                UPLOAD_TAG="$2"
                shift 2
                ;;
            -h|--help)
                cat <<EOF >&2

Usage:
  $0 --dmg <unsigned.dmg> --output <signed.dmg> [options]

Options:
  --dmg <path>         Input DMG file path
  --output <path>      Output path for signed DMG
  --background <path>  DMG background image (auto-extracted from input DMG if omitted)
  --identity <name>    Developer ID Application signing identity
                       (or set env: APPLE_SIGNING_IDENTITY)
  --tag <tag>          Upload to GitHub Release after signing + notarization

Environment:
  APPLE_SIGNING_IDENTITY       Developer ID Application signing identity
  APPLE_ID                     Apple ID email
  APPLE_APP_SPECIFIC_PASSWORD  App-specific password
  APPLE_TEAM_ID                Team ID

Examples:
  # Sign + notarize + staple
  $0 --dmg input.dmg --output signed.dmg

  # Sign + notarize + staple + upload to GitHub Release
  $0 --dmg input.dmg --output signed.dmg --tag v0.0.1-rc.1

EOF
                exit 0
                ;;
            *)
                red "Unknown argument: $1"
                exit 1
                ;;
        esac
    done

    if [ -z "$dmg_file" ]; then
        red "✗ --dmg parameter is required"
        exit 1
    fi
    if [ -z "$output_file" ]; then
        output_file="$(pwd)/$(basename "$dmg_file" .dmg)-signed.dmg"
    fi

    check_env

    echo "" >&2
    yellow "══════════════════════════════════════════════════" >&2
    yellow "  AgentSeek Desktop macOS Sign + Notarize" >&2
    yellow "══════════════════════════════════════════════════" >&2
    echo "" >&2
    green "  Input:  $dmg_file" >&2
    green "  Output: $output_file" >&2
    echo "" >&2

    # Step 1: Extract .app + background
    extract_app "$dmg_file"
    local app="$EXTRACTED_APP"

    # Step 2: Sign .app
    sign_app "$app"

    # Step 3: Rebuild DMG
    rebuild_dmg "$app" "$output_file"

    # Step 4: Sign DMG
    sign_dmg "$output_file"

    # Step 5: Notarize + Staple
    notarize_dmg "$output_file"
    staple_dmg "$output_file"

    # Step 6: Upload to GitHub Release (if --tag)
    if [ -n "$UPLOAD_TAG" ]; then
        upload_release "$output_file" "$UPLOAD_TAG"
    fi

    echo "" >&2
    green "══════════════════════════════════════════════════" >&2
    if [ -n "$UPLOAD_TAG" ]; then
        green "  ✅ Sign + Notarize + Staple + Upload complete!" >&2
    else
        green "  ✅ Sign + Notarize + Staple complete!" >&2
    fi
    green "══════════════════════════════════════════════════" >&2
    echo "" >&2
    echo "Signing identity: ${APPLE_SIGNING_IDENTITY}" >&2
    echo "Output file:      $output_file" >&2
    echo "" >&2
}

main "$@"
