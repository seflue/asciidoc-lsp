#!/usr/bin/env bash
# Tests for release.sh and check-version.sh. Every case runs in a throwaway
# git repo with a one-crate workspace, never in the real repository.
set -uo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
failures=0

fail() { echo "FAIL: $*" >&2; failures=$((failures + 1)); }

assert_eq() { [ "$1" = "$2" ] || fail "$3: expected '$2', got '$1'"; }

# A fresh repo at 0.9.0 with a tag, a changelog and a lockfile.
scratch_repo() {
  local dir
  dir="$(mktemp -d)"
  git -C "$dir" init -q -b main
  git -C "$dir" config user.email t@example.invalid
  git -C "$dir" config user.name t
  mkdir -p "$dir/crates/demo/src"
  printf '[workspace]\nmembers = ["crates/*"]\nresolver = "2"\n\n[workspace.package]\nversion = "0.9.0"\nlicense = "MIT"\n' >"$dir/Cargo.toml"
  printf '[package]\nname = "demo"\nversion.workspace = true\nedition = "2021"\n' >"$dir/crates/demo/Cargo.toml"
  : >"$dir/crates/demo/src/lib.rs"
  printf '# Changelog\n\n<!-- next-header -->\n\n## [Unreleased]\n\n## [0.9.0] - 2026-01-01\n\n<!-- next-url -->\n[Unreleased]: https://example.invalid/compare/v0.9.0...HEAD\n[0.9.0]: https://example.invalid/compare/v0.8.0...v0.9.0\n' >"$dir/CHANGELOG.md"
  cargo generate-lockfile --offline --manifest-path "$dir/Cargo.toml" >/dev/null 2>&1
  git -C "$dir" add -A
  git -C "$dir" commit -q -m init
  git -C "$dir" tag -a v0.9.0 -m v0.9.0
  echo "$dir"
}

# Runs the sourced release.sh functions inside the scratch repo.
in_repo() {
  local dir="$1"; shift
  (cd "$dir" && source "$here/release.sh" && "$@")
}

test_release_stamps_manifest_and_lock() {
  local dir; dir="$(scratch_repo)"
  in_repo "$dir" update_changelog 0.10.0 v0.10.0 2026-02-02 https://example.invalid >/dev/null
  in_repo "$dir" stamp_manifest_version 0.10.0 v0.10.0 2>/dev/null
  in_repo "$dir" commit_and_tag 0.10.0 v0.10.0 >/dev/null
  assert_eq "$(grep -c '^version = "0.10.0"$' "$dir/Cargo.toml")" 1 "manifest version"
  assert_eq "$(grep -A1 '^name = "demo"' "$dir/Cargo.lock" | tail -1)" 'version = "0.10.0"' "lock version"
  assert_eq "$(git -C "$dir" show --name-only --format= HEAD | LC_ALL=C sort | tr '\n' ' ')" "CHANGELOG.md Cargo.lock Cargo.toml " "release commit files"
  assert_eq "$(git -C "$dir" status --porcelain)" "" "clean tree after release commit"
  (cd "$dir" && "$here/check-version.sh") || fail "tagged release commit must pass check-version"
  rm -rf "$dir"
}

test_prepare_release_commits_and_tags_a_new_version() {
  local dir; dir="$(scratch_repo)"
  local before; before="$(git -C "$dir" rev-parse HEAD)"
  in_repo "$dir" prepare_release 0.10.0 v0.10.0 2026-02-02 https://example.invalid >/dev/null 2>&1
  assert_eq "$(git -C "$dir" rev-parse HEAD~1)" "$before" "one release commit"
  assert_eq "$(git -C "$dir" rev-parse 'v0.10.0^{commit}')" "$(git -C "$dir" rev-parse HEAD)" "tag on release commit"
  assert_eq "$(grep -c '^## \[0.10.0\] - 2026-02-02$' "$dir/CHANGELOG.md")" 1 "changelog section"
  rm -rf "$dir"
}

# An aborted release whose tag was deleted, followed by a fix commit: the
# rerun tags HEAD and leaves the changelog and history alone.
test_prepare_release_only_tags_an_already_prepared_version() {
  local dir; dir="$(scratch_repo)"
  in_repo "$dir" prepare_release 0.10.0 v0.10.0 2026-02-02 https://example.invalid >/dev/null 2>&1
  git -C "$dir" tag -d v0.10.0 >/dev/null
  printf '// fix\n' >"$dir/crates/demo/src/lib.rs"
  git -C "$dir" commit -q -am fix
  local head changelog
  head="$(git -C "$dir" rev-parse HEAD)"
  changelog="$(cat "$dir/CHANGELOG.md")"
  in_repo "$dir" prepare_release 0.10.0 v0.10.0 2026-02-03 https://example.invalid >/dev/null 2>&1
  assert_eq "$(git -C "$dir" rev-parse HEAD)" "$head" "no new commit"
  assert_eq "$(cat "$dir/CHANGELOG.md")" "$changelog" "changelog untouched"
  assert_eq "$(git -C "$dir" rev-parse 'v0.10.0^{commit}')" "$head" "tag on HEAD"
  rm -rf "$dir"
}

test_prepare_release_rejects_unreleased_entries_on_a_prepared_version() {
  local dir; dir="$(scratch_repo)"
  in_repo "$dir" prepare_release 0.10.0 v0.10.0 2026-02-02 https://example.invalid >/dev/null 2>&1
  git -C "$dir" tag -d v0.10.0 >/dev/null
  sed -i 's/^## \[Unreleased\]$/## [Unreleased]\n\n- Fixed a test/' "$dir/CHANGELOG.md"
  git -C "$dir" commit -q -am fix
  if in_repo "$dir" prepare_release 0.10.0 v0.10.0 2026-02-03 https://example.invalid >/dev/null 2>&1; then
    fail "entries under [Unreleased] must stop a rerun of a prepared version"
  fi
  assert_eq "$(git -C "$dir" tag -l v0.10.0)" "" "no tag"
  rm -rf "$dir"
}

test_prepare_release_rejects_prepared_changelog_with_stale_manifest() {
  local dir; dir="$(scratch_repo)"
  in_repo "$dir" update_changelog 0.10.0 v0.10.0 2026-02-02 https://example.invalid >/dev/null
  git -C "$dir" commit -q -am "changelog only"
  if in_repo "$dir" prepare_release 0.10.0 v0.10.0 2026-02-03 https://example.invalid >/dev/null 2>&1; then
    fail "a changelog section without a matching manifest must fail"
  fi
  assert_eq "$(git -C "$dir" tag -l v0.10.0)" "" "no tag"
  rm -rf "$dir"
}

test_stamp_only_touches_workspace_package_version() {
  local dir; dir="$(scratch_repo)"
  printf '\n[workspace.dependencies]\nfoo = { version = "1" }\n' >>"$dir/Cargo.toml"
  in_repo "$dir" stamp_manifest_version 0.9.1 v0.9.1 2>/dev/null
  assert_eq "$(grep -c '^version = "0.9.1"$' "$dir/Cargo.toml")" 1 "stamped once"
  assert_eq "$(grep -c 'foo = { version = "1" }' "$dir/Cargo.toml")" 1 "dependency version untouched"
  rm -rf "$dir"
}

test_stamp_rejects_second_version_line() {
  local dir; dir="$(scratch_repo)"
  printf 'version = "9.9.9"\n' >>"$dir/Cargo.toml"
  if in_repo "$dir" stamp_manifest_version 0.9.1 v0.9.1 2>/dev/null; then
    fail "stamp must fail on an ambiguous manifest"
  fi
  assert_eq "$(grep -c '^version = "0.9.0"$' "$dir/Cargo.toml")" 1 "manifest unchanged"
  rm -rf "$dir"
}

test_stamp_restores_changelog_and_manifest_on_failure() {
  local dir; dir="$(scratch_repo)"
  in_repo "$dir" update_changelog 0.9.1 v0.9.1 2026-02-02 https://example.invalid >/dev/null
  printf 'broken' >"$dir/crates/demo/Cargo.toml"
  if in_repo "$dir" stamp_manifest_version 0.9.1 v0.9.1 2>/dev/null; then
    fail "stamp must fail when the lockfile cannot be refreshed"
  fi
  assert_eq "$(git -C "$dir" status --porcelain -- CHANGELOG.md Cargo.toml Cargo.lock)" "" "release files restored"
  rm -rf "$dir"
}

test_check_version_flags_mismatch() {
  local dir; dir="$(scratch_repo)"
  if (cd "$dir" && "$here/check-version.sh" v0.9.1) 2>/dev/null; then
    fail "a tag differing from the manifest must fail"
  fi
  (cd "$dir" && "$here/check-version.sh" v0.9.0) || fail "a matching tag must pass"
  sed -i 's/^version = "0.9.0"/version = "0.9.5"/' "$dir/Cargo.toml"
  git -C "$dir" commit -q -am drift
  git -C "$dir" tag -a v0.9.1 -m v0.9.1
  if (cd "$dir" && "$here/check-version.sh") 2>/dev/null; then
    fail "a tagged HEAD whose manifest differs must fail"
  fi
  rm -rf "$dir"
}

test_check_version_passes_on_untagged_commit() {
  local dir; dir="$(scratch_repo)"
  sed -i 's/^version = "0.9.0"/version = "0.9.5"/' "$dir/Cargo.toml"
  git -C "$dir" commit -q -am later
  (cd "$dir" && "$here/check-version.sh") || fail "an untagged commit must pass"
  rm -rf "$dir"
}

for t in $(declare -F | awk '{print $3}' | grep '^test_'); do
  "$t"
done

if [ "$failures" -ne 0 ]; then
  echo "$failures failure(s)" >&2
  exit 1
fi
echo "release tests passed"
