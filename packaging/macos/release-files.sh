#!/bin/bash
# The files of a macOS release, from a `cargo tauri build --bundles app,dmg` (release.yml):
#
#   packaging/macos/release-files.sh <version> <notes file> <out dir>
#
# - RailSpotting-<version>-arm64.dmg: for new installs;
# - RailSpotting-<version>-arm64.app.tar.gz: the update bundle, signed with the updater key;
# - latest.json: what installed apps read (tauri.conf.json, plugins.updater.endpoints).
set -euo pipefail
cd "$(dirname "$0")/../.."
version="${1:?version}"
notes="${2:?notes file}"
out="${3:?out dir}"
repo="railspotting/Railspotting-Receiver"
bundle=target/release/bundle
mkdir -p "$out"
cp "$bundle/dmg/RailSpotting_${version}_aarch64.dmg" "$out/RailSpotting-$version-arm64.dmg"
cp "$bundle/macos/RailSpotting.app.tar.gz" "$out/RailSpotting-$version-arm64.app.tar.gz"
sig="$bundle/macos/RailSpotting.app.tar.gz.sig"
[ -s "$sig" ] || { echo "no updater signature: is TAURI_SIGNING_PRIVATE_KEY set?" >&2; exit 1; }
python3 - "$out/latest.json" "$version" "$repo" "$sig" "$notes" <<'PY'
import datetime, json, sys
path, version, repo, sig, notes = sys.argv[1:]
url = f"https://github.com/{repo}/releases/download/v{version}/RailSpotting-{version}-arm64.app.tar.gz"
with open(path, "w") as f:
    json.dump({
        "version": version,
        "notes": open(notes).read().strip(),
        "pub_date": datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
        "platforms": {"darwin-aarch64": {"signature": open(sig).read().strip(), "url": url}},
    }, f, indent=2)
    f.write("\n")
PY
ls -l "$out"
