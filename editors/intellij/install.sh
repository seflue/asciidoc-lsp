#!/usr/bin/env bash
# Install the asciidoc-lsp JetBrains plugin and LSP4IJ into installed JetBrains IDEs.
#
# Usage: install.sh [--dev <plugin.zip>] [--dry-run] [--check] [ide...]
#   ide        IDE name (rustrover, pycharm, ...) or path to its launcher or install
#              directory. Without one, the script finds the installed IDEs and asks.
#   --dev      install a locally built plugin ZIP instead of the latest release
#   --dry-run  print what would run, change nothing
#   --check    only fail if a selected IDE is running; asks nothing, so with several IDEs
#              and no ide argument there is nothing to check yet
#
# ASCIIDOC_LSP_IDE_ROOTS (colon-separated) replaces the directories searched for IDEs.
# The IDEs must be closed while the script runs.
set -euo pipefail

readonly LSP4IJ_ID="com.redhat.devtools.lsp4ij"
readonly PLUGIN_ID="io.github.seflue.asciidoclsp"
# The plugin's directory in an IDE's plugins directory: the root of the plugin ZIP.
readonly PLUGIN_DIR_NAME="asciidoc-lsp"
readonly RELEASE_XML="https://github.com/seflue/asciidoc-lsp/releases/latest/download/updatePlugins.xml"
readonly CONSENT="--give-consent-to-use-third-party-plugins"

dry_run=false
check_only=false
dev_zip=""
wanted=()

die() {
    printf 'install.sh: %s\n' "$*" >&2
    exit 1
}

# Every side effect goes through here, so --dry-run follows the same path as a real run.
run() {
    if $dry_run; then
        printf '+ %s\n' "$*"
    else
        "$@"
    fi
}

while [ $# -gt 0 ]; do
    case "$1" in
        --dry-run) dry_run=true ;;
        --check) check_only=true ;;
        --dev)
            [ $# -ge 2 ] || die "--dev needs the path to the plugin ZIP"
            dev_zip="$2"
            shift
            ;;
        -*) die "unknown option: $1" ;;
        *) wanted+=("$1") ;;
    esac
    shift
done

default_roots() {
    local roots=(
        "$HOME/.local/share/JetBrains/Toolbox/apps"
        "$HOME/.local/share/JetBrains"
        "$HOME/Applications"
        /opt
        /snap
        /Applications
    )
    printf '%s\n' "${roots[@]}"
}

# Launchers on PATH that are symlinks into an install (Snap, package managers, hand-made links).
path_ides() {
    local dir entry target
    local IFS=:
    for dir in $PATH; do
        for entry in "$dir"/*; do
            [ -L "$entry" ] || continue
            target="$(readlink -f "$entry")" || continue
            [ -f "$(dirname "$target")/../product-info.json" ] &&
                describe_ide "$(dirname "$target")/../product-info.json"
        done
    done
}

search_roots() {
    if [ -n "${ASCIIDOC_LSP_IDE_ROOTS:-}" ]; then
        tr ':' '\n' <<<"$ASCIIDOC_LSP_IDE_ROOTS"
    else
        default_roots
    fi
}

json_field() {
    sed -n "s/.*\"$1\"[[:space:]]*:[[:space:]]*\"\([^\"]*\)\".*/\1/p" "$2" | head -n 1
}

# Prints "<install dir>\t<name>\t<version>\t<launcher>\t<data directory name>" for the IDE
# described by a product-info.json.
describe_ide() {
    local info="$1" dir name version launcher_rel launcher data_dir
    dir="$(cd "$(dirname "$info")" && pwd -P)"
    name="$(json_field name "$info")"
    version="$(json_field version "$info")"
    launcher_rel="$(json_field launcherPath "$info")"
    data_dir="$(json_field dataDirectoryName "$info")"
    [ -n "$name" ] && [ -n "$launcher_rel" ] || return 0
    launcher="$dir/$launcher_rel"
    [ -x "$launcher" ] || return 0
    launcher="$(cd "$(dirname "$launcher")" && pwd -P)/$(basename "$launcher")"
    # macOS keeps product-info.json in Contents/Resources; the install is the .app.
    case "$dir" in
        */Contents/Resources) dir="${dir%/Contents/Resources}" ;;
    esac
    printf '%s\t%s\t%s\t%s\t%s\n' "$dir" "$name" "$version" "$launcher" "$data_dir"
}

discover_all() {
    local root info
    while IFS= read -r root; do
        [ -d "$root" ] || continue
        # An install's own lib/, plugins/ and jbr/ hold no further IDEs.
        find -L "$root" -maxdepth 5 \( -name lib -o -name plugins -o -name jbr \) -prune \
            -o -name product-info.json -print 2>/dev/null |
            while IFS= read -r info; do describe_ide "$info"; done
    done < <(search_roots)
    [ -n "${ASCIIDOC_LSP_IDE_ROOTS:-}" ] || path_ides
}

discover() {
    discover_all | sort -u -t $'\t' -k1,1
}

# An install directory, its launcher, or a path inside the install that holds product-info.json.
ide_from_path() {
    local path="$1" dir candidate
    if [ -f "$path" ]; then
        dir="$(dirname "$path")"
    else
        dir="$path"
    fi
    [ -d "$dir" ] || return 1
    dir="$(cd "$dir" && pwd -P)"
    for candidate in "$dir" "$dir/.." "$dir/Contents/Resources" "$dir/../Resources"; do
        if [ -f "$candidate/product-info.json" ]; then
            describe_ide "$candidate/product-info.json"
            return 0
        fi
    done
    return 1
}

select_by_arguments() {
    local arg match
    for arg in "${wanted[@]}"; do
        if [[ "$arg" == */* ]]; then
            match="$(ide_from_path "$arg")" || die "no JetBrains IDE at $arg"
        else
            match="$(awk -F'\t' -v want="$arg" '
                { n = tolower($2); gsub(/ /, "", n); l = $4; sub(/.*\//, "", l); sub(/(64)?(\.sh|\.exe)?$/, "", l) }
                n == tolower(want) || l == tolower(want)' <<<"$found")"
            [ -n "$match" ] || die "no installed JetBrains IDE matches '$arg'"
        fi
        printf '%s\n' "$match"
    done
}

prompt_selection() {
    local i=0 line answer
    printf 'Found these JetBrains IDEs:\n' >&2
    while IFS=$'\t' read -r dir name version _; do
        i=$((i + 1))
        printf '  %d) %s %s  %s\n' "$i" "$name" "$version" "$dir" >&2
    done <<<"$found"
    printf 'Select the IDEs to install into (numbers, separated by spaces or commas): ' >&2
    read -r answer || answer=""
    answer="${answer//,/ }"
    [ -n "${answer// /}" ] || die "nothing selected"
    for i in $answer; do
        [[ "$i" =~ ^[0-9]+$ ]] || die "not a number: $i"
        line="$(sed -n "${i}p" <<<"$found")"
        [ -n "$line" ] || die "no IDE with number $i"
        printf '%s\n' "$line"
    done | sort -u
}

prompt_path() {
    local path
    printf 'No JetBrains IDE found. Path to the IDE launcher or install directory (empty to cancel): ' >&2
    read -r path || path=""
    [ -n "$path" ] || die "no IDE given"
    ide_from_path "$path" || die "no JetBrains IDE at $path"
}

# An IDE process command line starts inside its install directory.
is_running() {
    local pattern
    # shellcheck disable=SC2001 # a character class of regex metacharacters, not a plain substitution
    pattern="$(sed 's/[][\\.*^$+?(){}|]/\\&/g' <<<"$1")"
    pgrep -f -- "^$pattern/" >/dev/null 2>&1
}

# The default plugins directory of the IDE whose data directory is named $1.
plugins_dir() {
    if [ "$(uname -s)" = Darwin ]; then
        printf '%s\n' "$HOME/Library/Application Support/JetBrains/$1"
    else
        printf '%s\n' "${XDG_DATA_HOME:-$HOME/.local/share}/JetBrains/$1"
    fi
}

# installPlugins skips a plugin that is installed in any version, so a dev install
# removes the installed one first. Release installs leave it to the IDE's updates.
remove_for_dev() {
    local installed
    [ -n "$dev_zip" ] && [ -n "$1" ] || return 0
    installed="$(plugins_dir "$1")/$PLUGIN_DIR_NAME"
    [ -d "$installed" ] || return 0
    run rm -rf "$installed"
}

install_into() {
    run "$1" installPlugins "$LSP4IJ_ID" "$PLUGIN_ID" "$repository" "$CONSENT"
}

# installPlugins takes plugin ids from a repository, never a ZIP path: a dev install
# gets a local repository that points at the ZIP, like the release's updatePlugins.xml.
write_dev_repository() {
    local zip version dir
    zip="$(cd "$(dirname "$dev_zip")" && pwd -P)/$(basename "$dev_zip")"
    version="$(basename "$zip" .zip)"
    version="${version#asciidoc-lsp-}"
    dir="$(mktemp -d)"
    cat >"$dir/updatePlugins.xml" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<plugins>
  <plugin id="$PLUGIN_ID" version="$version" url="file://$zip"/>
</plugins>
EOF
    printf 'file://%s/updatePlugins.xml\n' "$dir"
}

repository="$RELEASE_XML"
if [ -n "$dev_zip" ]; then
    [ -f "$dev_zip" ] || die "plugin ZIP not found: $dev_zip"
    repository="$(write_dev_repository)"
fi

found="$(discover)"

if $check_only; then
    if [ ${#wanted[@]} -gt 0 ]; then
        selected="$(select_by_arguments)"
    elif [ -n "$found" ] && [ "$(wc -l <<<"$found")" -eq 1 ]; then
        selected="$found"
    else
        exit 0
    fi
    running=0
    while IFS=$'\t' read -r dir name version _; do
        if is_running "$dir"; then
            printf '%s %s: running, close it and run the script again\n' "$name" "$version"
            running=$((running + 1))
        fi
    done <<<"$selected"
    exit $((running > 0))
fi

if [ ${#wanted[@]} -gt 0 ]; then
    selected="$(select_by_arguments)"
elif [ -z "$found" ]; then
    selected="$(prompt_path)"
elif [ "$(wc -l <<<"$found")" -eq 1 ]; then
    selected="$found"
else
    selected="$(prompt_selection)"
fi

printf 'LSP4IJ and asciidoc-lsp are third-party plugins: this script confirms the IDE'"'"'s third-party consent for you (%s).\n' "$CONSENT"

failed=0
while IFS=$'\t' read -r dir name version launcher data_dir; do
    label="$name $version"
    printf '%s: %s\n' "$label" "$dir"
    if is_running "$dir"; then
        printf '%s: running, close it and run the script again\n' "$label"
        failed=$((failed + 1))
    elif remove_for_dev "$data_dir" && install_into "$launcher"; then
        if $dry_run; then
            printf '%s: dry run, nothing installed\n' "$label"
        else
            printf '%s: installed\n' "$label"
        fi
    else
        printf '%s: install failed\n' "$label"
        failed=$((failed + 1))
    fi
done <<<"$selected"

[ "$failed" -eq 0 ]
