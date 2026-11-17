#!/bin/sh
# Replace the executable inside the installed app bundle with a fresh release build.
#
# The bundle in ~/Applications is what Finder and Spotlight launch, and it holds its own copy of the
# binary — a `cargo build` alone never reaches it. Re-signing is not optional: replacing the main
# executable invalidates the ad-hoc signature and macOS refuses to start the result.
#
# Usage: scripts/install-app.sh [path/to/DSH Superagent.app]
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
app=${1:-"$HOME/Applications/DSH Superagent.app"}
binary="$app/Contents/MacOS/launcher"

if [ ! -d "$app" ]; then
    echo "no app bundle at $app" >&2
    exit 1
fi

(cd "$root" && cargo build --release)
cp "$root/target/release/dsh-superagent-launcher" "$binary"
chmod 755 "$binary"
codesign --force --sign - "$app"
codesign --verify --strict "$app"
echo "updated $app"
