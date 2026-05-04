#!/usr/bin/env bash
# Build vterm, vcat, and vmux in release mode and install them to
# ~/.local/bin (override with $PREFIX). Also drops a desktop entry for
# vterm into ~/.local/share/applications. Idempotent.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PREFIX="${PREFIX:-$HOME/.local}"
BINDIR="$PREFIX/bin"
APPDIR="$PREFIX/share/applications"

PACKAGES=(vterm vcat vmux)

cd "$REPO_ROOT"

echo "==> building ${PACKAGES[*]} in release mode"
cargo build --release "${PACKAGES[@]/#/--package=}"

# cargo respects $CARGO_TARGET_DIR — resolve it once.
target_dir="$(cargo metadata --no-deps --format-version=1 \
    | python3 -c 'import sys, json; print(json.load(sys.stdin)["target_directory"])' \
    2>/dev/null || echo "$REPO_ROOT/target")"

mkdir -p "$BINDIR"
for pkg in "${PACKAGES[@]}"; do
    src="$target_dir/release/$pkg"
    if [[ ! -x "$src" ]]; then
        echo "error: $pkg binary not found at $src" >&2
        exit 1
    fi
    install -m 0755 "$src" "$BINDIR/$pkg"
    echo "    $pkg -> $BINDIR/$pkg"
done

mkdir -p "$APPDIR"
desktop_file="$APPDIR/vterm.desktop"
cat > "$desktop_file" <<EOF
[Desktop Entry]
Type=Application
Name=vterm
GenericName=Terminal
Comment=vterm — VGE/PRT-aware terminal emulator
Exec=$BINDIR/vterm
TryExec=$BINDIR/vterm
Icon=utilities-terminal
Terminal=false
Categories=System;TerminalEmulator;
Keywords=shell;prompt;command;commandline;cmd;
StartupNotify=true
EOF
chmod 0644 "$desktop_file"
echo "    vterm.desktop -> $desktop_file"

# Refresh the desktop database so launchers pick the entry up immediately.
if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database "$APPDIR" >/dev/null 2>&1 || true
fi

case ":$PATH:" in
    *":$BINDIR:"*) ;;
    *) echo "note: $BINDIR is not on \$PATH; add it to your shell rc to use the binaries" ;;
esac
