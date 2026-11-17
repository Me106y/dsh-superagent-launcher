#!/bin/sh
# Install the built launcher for the current user and add it to the desktop application menu.
#
#   ./packaging/install.sh
#
# Re-run after `cargo build --release` to pick up a new binary.
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
binary="$root/target/release/dsh-superagent-launcher"
[ -x "$binary" ] || {
    echo "build first: cargo build --release" >&2
    exit 1
}

bin_dir="$HOME/.local/lib/dsh-superagent-launcher"
apps_dir="$HOME/.local/share/applications"
icon_dir="$HOME/.local/share/icons/hicolor/scalable/apps"
wrapper="$HOME/.local/bin/dsh-superagent-launcher"

install -Dm755 "$binary" "$bin_dir/dsh-superagent-launcher"
install -Dm644 "$root/packaging/dsh-superagent-launcher.svg" "$icon_dir/dsh-superagent-launcher.svg"

# A desktop launch does not inherit the shell PATH; add the Node installations the launcher and the
# DSH processes it starts need, then exec the binary.
mkdir -p "$HOME/.local/bin"
cat > "$wrapper" <<'EOF'
#!/bin/sh
for dir in "$HOME"/local/node-*/bin "$HOME"/.nvm/versions/node/*/bin; do
    [ -d "$dir" ] && PATH="$dir:$PATH"
done
PATH="$HOME/.local/bin:$PATH"
export PATH
exec "$HOME/.local/lib/dsh-superagent-launcher/dsh-superagent-launcher" "$@"
EOF
chmod 755 "$wrapper"

mkdir -p "$apps_dir"
cat > "$apps_dir/dsh-superagent-launcher.desktop" <<EOF
[Desktop Entry]
Type=Application
Version=1.0
Name=DSH Superagent
Name[zh_CN]=DSH Superagent 启动器
Comment=Start, open and stop DeepSeek Harness profiles that mount dsh-superagent
Comment[zh_CN]=启动、打开、停止加载了 dsh-superagent 的 DSH profile
Exec=$wrapper
Icon=dsh-superagent-launcher
Terminal=false
Categories=Development;
StartupNotify=true
StartupWMClass=dsh-superagent-launcher
EOF

command -v update-desktop-database >/dev/null 2>&1 && update-desktop-database "$apps_dir" || true
echo "installed: DSH Superagent (application menu, and $wrapper)"
