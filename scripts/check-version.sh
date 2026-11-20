#!/usr/bin/env bash
# Fails when the Cargo workspace version differs from a release tag.
#
# Usage: check-version.sh [vX.Y.Z]
# Without an argument the tag is the v* tag on HEAD; a commit without such
# a tag passes, because only a release has to match. Run from the repository
# root. `just release` stamps the version, and the release workflow runs this
# against the tag it builds, so a tag whose manifests say something else is
# caught before any binary is published.
set -euo pipefail

manifest_version() {
  sed -n '/^\[workspace\.package\]/,/^\[/s/^version = "\(.*\)"$/\1/p' Cargo.toml
}

main() {
  local tag="${1:-}"
  if [ -z "$tag" ]; then
    tag="$(git describe --exact-match --tags --match 'v*' HEAD 2>/dev/null || true)"
    [ -n "$tag" ] || exit 0
  fi
  local version
  version="$(manifest_version)"
  [ -n "$version" ] || { echo "❌ No [workspace.package] version in Cargo.toml." >&2; exit 1; }
  if [ "v${version}" != "$tag" ]; then
    echo "❌ Cargo.toml says ${version} but the tag is ${tag}." >&2
    exit 1
  fi
}

main "$@"
