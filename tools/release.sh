#!/bin/bash
# Publish a release: tag main, and the Release workflow (.github/workflows/release.yml) builds the
# macOS app and puts it on GitHub Releases. Installed Mac apps offer the update within six hours.
#
#   tools/release.sh patch "What changed, in a sentence or two"
#
# The first argument is the step: patch (2.0.1 to 2.0.2) for fixes and small features, the usual
# one; minor only when the owner decides a release is that big; or an exact version (2.1.0). The
# notes go in the tag, the release and the app's update offer.
#
# Tags the commit at the tip of origin/main once its CI has passed, and pushes only the tag:
# main takes changes through pull requests. Needs gh signed in with push access. Once published,
# a release can't be taken back from Macs that installed it: publish only what you would run.
set -euo pipefail
cd "$(dirname "$0")/.."
step="${1:?usage: tools/release.sh patch|minor|<1.2.3> "<notes>"}"
notes="${2:?give release notes as the second argument}"
repo="railspotting/Railspotting-Receiver"

source_version=$(sed -n '/^\[workspace.package\]/,/^\[/ s/^version = "\(.*\)"/\1/p' Cargo.toml)
git fetch -q --tags origin main
latest=$(git tag -l 'v*' --sort=-v:refname | head -1)
current="${latest#v}"
[ -n "$current" ] || current="$source_version"
IFS=. read -r major minor patch <<< "$current"
case "$step" in
    patch) name="$major.$minor.$((patch + 1))" ;;
    minor) name="$major.$((minor + 1)).0" ;;
    *) name="$step" ;;
esac
[[ "$name" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || { echo "version must look like 1.2.3" >&2; exit 1; }
# the crates name each other at the source's major version
[ "${name%%.*}" = "${source_version%%.*}" ] || { echo "a new major version changes Cargo.toml first" >&2; exit 1; }
git rev-parse -q --verify "refs/tags/v$name" >/dev/null && { echo "v$name exists already" >&2; exit 1; }

commit=$(git rev-parse origin/main)
ci=$(gh run list --repo "$repo" --workflow CI --commit "$commit" --json conclusion --jq '.[0].conclusion // ""')
[ "$ci" = success ] || { echo "CI has not passed on main ($commit): ${ci:-no run yet}" >&2; exit 1; }

git tag -a "v$name" -m "$notes" "$commit"
git push -q origin "v$name"
echo "Tagged v$name ($commit). The Release workflow builds it: https://github.com/$repo/actions/workflows/release.yml"
