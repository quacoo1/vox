#!/bin/sh
# Installs the release build for the current user (no root needed):
#   ~/.local/opt/vox/vox + browser-extension/   the program
#   ~/.local/bin/vox                             symlink, so `vox --read-text "…"` works in a terminal
#   ~/.local/share/applications/vox.desktop      app launcher entry (Activities / app grid)
#   ~/.local/share/icons/hicolor/256x256/apps/vox.png
# Build first: cargo build -p vox-app --release
# Uninstall: scripts/install-linux.sh --uninstall
set -eu

repo=$(cd "$(dirname "$0")/.." && pwd)
opt="$HOME/.local/opt/vox"
bin="$HOME/.local/bin"
apps="${XDG_DATA_HOME:-$HOME/.local/share}/applications"
icons="${XDG_DATA_HOME:-$HOME/.local/share}/icons/hicolor/256x256/apps"

if [ "${1:-}" = "--uninstall" ]; then
    rm -rf "$opt" "$bin/vox" "$apps/vox.desktop" "$icons/vox.png"
    update-desktop-database "$apps" 2>/dev/null || true
    echo "Vox removed (settings and history stay in ~/.config/Vox)."
    exit 0
fi

src="$repo/target/release"
if [ ! -x "$src/vox" ]; then
    echo "No release build at $src/vox. Run: cargo build -p vox-app --release" >&2
    exit 1
fi

mkdir -p "$opt" "$bin" "$apps" "$icons"
# Copy to a temporary name first so a running copy is not overwritten in place.
cp "$src/vox" "$opt/vox.new" && mv -f "$opt/vox.new" "$opt/vox"
rm -rf "$opt/browser-extension"
cp -r "$repo/browser-extension" "$opt/browser-extension"
ln -sf "$opt/vox" "$bin/vox"
cp "$repo/crates/vox-app/assets/vox-256.png" "$icons/vox.png"

cat > "$apps/vox.desktop" <<EOF
[Desktop Entry]
Type=Application
Name=Vox
Comment=Read selected text aloud
Exec=$opt/vox
Icon=vox
Terminal=false
Categories=Utility;Accessibility;AudioVideo;
StartupWMClass=vox
Actions=ReadClipboard;

[Desktop Action ReadClipboard]
Name=Read clipboard
Exec=$opt/vox --read-clipboard
EOF

update-desktop-database "$apps" 2>/dev/null || true
gtk-update-icon-cache -q "${XDG_DATA_HOME:-$HOME/.local/share}/icons/hicolor" 2>/dev/null || true
echo "Vox installed: open it from Activities, or run 'vox'."
