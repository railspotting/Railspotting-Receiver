#!/bin/bash
# Build RailSpotting.app and copy it to /Applications, for testing on this Mac.
# Unsigned: fine on the Mac that built it. Releases come from release.yml (packaging/macos/README.md).
set -euo pipefail
cd "$(dirname "$0")/../../apps/desktop"
cargo tauri build --bundles app
app=../../target/release/bundle/macos/RailSpotting.app
if pgrep -xq railspotting-desktop; then
    echo "Quit RailSpotting first (menu bar icon, Quit RailSpotting)." >&2
    exit 1
fi
rm -rf /Applications/RailSpotting.app
ditto "$app" /Applications/RailSpotting.app
echo "Installed /Applications/RailSpotting.app"
