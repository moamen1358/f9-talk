#!/usr/bin/env bash
# Build an AppImage for f9-talk using linuxdeploy to bundle shared libraries.
# Usage: ./packaging/appimage/build-appimage.sh [path-to-binary]
#
# If no binary path is given, it builds one with `cargo build --release`.
# The resulting .AppImage is placed in the repo root.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
BINARY="${1:-$REPO_ROOT/target/release/f9-talk}"

if [ ! -f "$BINARY" ]; then
    echo "Binary not found at $BINARY — building..."
    cargo build --release --manifest-path "$REPO_ROOT/Cargo.toml"
    BINARY="$REPO_ROOT/target/release/f9-talk"
fi

# Read version from workspace root Cargo.toml (app crate inherits via version.workspace = true)
VERSION=$(grep '^version = ' "$REPO_ROOT/Cargo.toml" | head -1 | sed 's/.*"\(.*\)".*/\1/')
export VERSION

# Download linuxdeploy if not present (bundles shared libs into AppImage)
LINUXDEPLOY="/tmp/linuxdeploy-x86_64.AppImage"
if [ ! -f "$LINUXDEPLOY" ]; then
    echo "Downloading linuxdeploy..."
    curl -sSL "https://github.com/linuxdeploy/linuxdeploy/releases/download/continuous/linuxdeploy-x86_64.AppImage" \
        -o "$LINUXDEPLOY"
    chmod +x "$LINUXDEPLOY"
fi

# Prepare AppDir
APPDIR="$REPO_ROOT/AppDir"
rm -rf "$APPDIR"
mkdir -p "$APPDIR/usr/bin"

# Copy binary
cp "$BINARY" "$APPDIR/usr/bin/f9-talk"
chmod 755 "$APPDIR/usr/bin/f9-talk"

# Bundle the runtime typing tools so the AppImage is self-contained. The
# typer shells out to wl-copy / wl-paste / wtype (crates/input/src/typer/
# linux.rs) and looks for them at $APPDIR/usr/bin first. Passing each via
# --executable makes linuxdeploy bundle their shared libraries too.
EXTRA_EXECUTABLES=()
for tool in wl-copy wl-paste wtype; do
    src="$(command -v "$tool" 2>/dev/null || true)"
    if [ -n "$src" ]; then
        cp "$src" "$APPDIR/usr/bin/$tool"
        chmod 755 "$APPDIR/usr/bin/$tool"
        EXTRA_EXECUTABLES+=(--executable "$APPDIR/usr/bin/$tool")
        echo "bundled $tool ($src)"
    else
        echo "WARNING: $tool not found on PATH — not bundling it"
    fi
done

OUTPUT="$REPO_ROOT/f9-talk-${VERSION}-x86_64.AppImage"

# Use linuxdeploy to create the AppImage — it automatically:
# - Copies and bundles required shared libraries (GTK, X11, ALSA, etc.)
# - Sets up the AppDir structure with desktop file and icon
# - Generates the final .AppImage with appimagetool
ARCH=x86_64 OUTPUT="$OUTPUT" "$LINUXDEPLOY" \
    --appdir "$APPDIR" \
    --executable "$APPDIR/usr/bin/f9-talk" \
    "${EXTRA_EXECUTABLES[@]}" \
    --desktop-file "$REPO_ROOT/packaging/debian/applications/f9-talk.desktop" \
    --icon-file "$REPO_ROOT/assets/f9-talk.svg" \
    --output appimage

# linuxdeploy may use a different output name; find and rename if needed
if [ ! -f "$OUTPUT" ]; then
    FOUND=$(find "$REPO_ROOT" -maxdepth 1 -name '*.AppImage' -newer "$BINARY" | head -1)
    if [ -n "$FOUND" ]; then
        mv "$FOUND" "$OUTPUT"
    fi
fi

# Clean up
rm -rf "$APPDIR"

echo ""
echo "AppImage created: f9-talk-${VERSION}-x86_64.AppImage"
echo "  Run it: chmod +x f9-talk-${VERSION}-x86_64.AppImage && ./f9-talk-${VERSION}-x86_64.AppImage"
