#!/usr/bin/env bash
# Tests for install.sh: --dry-run against fake IDE installations.
# Checks are strings that check() evals, so $status and $out expand late.
# shellcheck disable=SC2016,SC2034
set -uo pipefail

script="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/install.sh"
failures=0
work=""

setup() {
    work="$(cd "$(mktemp -d)" && pwd -P)"
    export HOME="$work/home"
    unset XDG_DATA_HOME
    mkdir -p "$HOME"
}

teardown() {
    rm -rf "$work"
}

# fake_ide <root> <dir> <name> <version> <launcher-basename>
fake_ide() {
    local dir="$1/$2"
    mkdir -p "$dir/bin"
    cat >"$dir/product-info.json" <<EOF
{
  "name": "$3",
  "version": "$4",
  "dataDirectoryName": "$3$4",
  "launch": [
    {
      "os": "Linux",
      "launcherPath": "bin/$5"
    }
  ]
}
EOF
    printf '#!/bin/sh\n' >"$dir/bin/$5"
    chmod +x "$dir/bin/$5"
}

# run_install <stdin> <args...>; sets $out and $status
run_install() {
    local input="$1"
    shift
    out="$(printf '%s' "$input" | ASCIIDOC_LSP_IDE_ROOTS="${roots:-$work/none}" "$script" --dry-run "$@" 2>&1)"
    status=$?
}

check() {
    local name="$1" condition="$2"
    if eval "$condition"; then
        printf 'ok   %s\n' "$name"
    else
        printf 'FAIL %s\n--- output ---\n%s\n--------------\n' "$name" "$out"
        failures=$((failures + 1))
    fi
}

contains() { grep -qF -- "$1" <<<"$out"; }
count() { grep -cF -- "$1" <<<"$out"; }

test_single_ide_installs_without_prompt() {
    setup
    fake_ide "$work/opt" rustrover RustRover 2025.2.3 rustrover.sh
    roots="$work/opt"
    run_install ""
    check "single IDE: exit 0" '[ "$status" -eq 0 ]'
    check "single IDE: no prompt" '! contains "Select"'
    check "single IDE: installs into it" 'contains "$work/opt/rustrover/bin/rustrover.sh installPlugins"'
    teardown
}

test_release_command() {
    setup
    fake_ide "$work/opt" rustrover RustRover 2025.2.3 rustrover.sh
    roots="$work/opt"
    run_install ""
    check "release: installs LSP4IJ and the plugin from the release" \
        'contains "installPlugins com.redhat.devtools.lsp4ij io.github.seflue.asciidoclsp https://github.com/seflue/asciidoc-lsp/releases/latest/download/updatePlugins.xml --give-consent-to-use-third-party-plugins"'
    check "release: says it gives the consent" 'contains "third-party"'
    teardown
}

test_dev_command() {
    setup
    fake_ide "$work/opt" rustrover RustRover 2025.2.3 rustrover.sh
    roots="$work/opt"
    touch "$work/asciidoc-lsp-1.2.3.zip"
    run_install "" --dev "$work/asciidoc-lsp-1.2.3.zip"
    check "dev: installs LSP4IJ and the plugin from a local repository" \
        'contains "installPlugins com.redhat.devtools.lsp4ij io.github.seflue.asciidoclsp file://" && contains "/updatePlugins.xml --give-consent-to-use-third-party-plugins"'
    check "dev: no release URL" '! contains "releases/latest"'
    repo="$(grep -o "file://[^ ]*updatePlugins.xml" <<<"$out")"
    check "dev: the repository points at the ZIP with its version" \
        'grep -qF "version=\"1.2.3\" url=\"file://$work/asciidoc-lsp-1.2.3.zip\"" "${repo#file://}"'
    teardown
}

# installPlugins skips a plugin id that is already installed, whatever its version.
test_dev_replaces_the_installed_plugin() {
    setup
    fake_ide "$work/opt" rustrover RustRover 2025.2.3 rustrover.sh
    roots="$work/opt"
    installed="$HOME/.local/share/JetBrains/RustRover2025.2.3/asciidoc-lsp"
    mkdir -p "$installed"
    touch "$work/asciidoc-lsp-1.2.3.zip"
    run_install "" --dev "$work/asciidoc-lsp-1.2.3.zip"
    check "dev: removes the installed plugin first" 'contains "+ rm -rf $installed"'
    check "dev: then installs" '[ "$(grep -n "rm -rf" <<<"$out" | cut -d: -f1)" -lt "$(grep -n installPlugins <<<"$out" | cut -d: -f1)" ]'
    run_install ""
    check "release: leaves the installed plugin alone" '! contains "rm -rf"'
    teardown
}

test_dev_without_installed_plugin_removes_nothing() {
    setup
    fake_ide "$work/opt" rustrover RustRover 2025.2.3 rustrover.sh
    roots="$work/opt"
    touch "$work/asciidoc-lsp-1.2.3.zip"
    run_install "" --dev "$work/asciidoc-lsp-1.2.3.zip"
    check "dev, nothing installed: removes nothing" '! contains "rm -rf"'
    teardown
}

test_several_ides_prompt_and_multi_select() {
    setup
    fake_ide "$work/opt" pycharm PyCharm 2025.2.1 pycharm.sh
    fake_ide "$work/apps" rustrover RustRover 2025.2.3 rustrover.sh
    roots="$work/opt:$work/apps"
    run_install $'1 2\n'
    check "several: lists both with version" 'contains "PyCharm 2025.2.1" && contains "RustRover 2025.2.3"'
    check "several: asks for a selection" 'contains "Select"'
    check "several: installs into both" '[ "$(count " installPlugins com.redhat.devtools.lsp4ij io.github")" -eq 2 ]'
    run_install $'2\n'
    check "several: installs only the chosen one" '[ "$(count " installPlugins com.redhat.devtools.lsp4ij io.github")" -eq 1 ]'
    run_install $'1 1\n'
    check "several: a number given twice installs once" '[ "$(count " installPlugins com.redhat.devtools.lsp4ij io.github")" -eq 1 ]'
    run_install $'1,2\n'
    check "several: accepts commas" '[ "$(count " installPlugins com.redhat.devtools.lsp4ij io.github")" -eq 2 ]'
    run_install $'\n'
    check "several: empty selection aborts" '[ "$status" -ne 0 ] && ! contains " installPlugins "'
    teardown
}

test_argument_skips_prompt() {
    setup
    fake_ide "$work/opt" pycharm PyCharm 2025.2.1 pycharm.sh
    fake_ide "$work/opt" rustrover RustRover 2025.2.3 rustrover.sh
    roots="$work/opt"
    run_install "" rustrover
    check "argument by name: no prompt" '! contains "Select"'
    check "argument by name: only that IDE" 'contains "rustrover.sh installPlugins" && ! contains "pycharm.sh installPlugins"'
    run_install "" "$work/opt/pycharm/bin/pycharm.sh"
    check "argument by path: only that IDE" 'contains "pycharm.sh installPlugins" && ! contains "rustrover.sh installPlugins"'
    run_install "" goland
    check "argument without match: fails" '[ "$status" -ne 0 ] && contains "goland"'
    teardown
}

test_duplicate_listed_once() {
    setup
    fake_ide "$work/opt" rustrover RustRover 2025.2.3 rustrover.sh
    mkdir -p "$work/other"
    ln -s "$work/opt/rustrover" "$work/other/rustrover"
    roots="$work/opt:$work/other"
    run_install ""
    check "duplicate: no prompt, installed once" '! contains "Select" && [ "$(count "installPlugins com.redhat.devtools.lsp4ij io.github")" -eq 1 ]'
    teardown
}

test_none_found_asks_for_path() {
    setup
    fake_ide "$work/elsewhere" idea IDEA 2025.2 idea.sh
    roots="$work/empty"
    mkdir -p "$roots"
    run_install "$work/elsewhere/idea"$'\n'
    check "none found: asks for a path" 'contains "No JetBrains IDE found"'
    check "none found: installs into the given path" 'contains "$work/elsewhere/idea/bin/idea.sh installPlugins"'
    run_install $'\n'
    check "none found, no path: fails" '[ "$status" -ne 0 ] && ! contains " installPlugins "'
    teardown
}

# start_fake_ide_process <install dir>; sets $pid to a process whose command line starts there
start_fake_ide_process() {
    (exec -a "$1/bin/java" sleep 30) &
    pid=$!
    local tries=0
    until pgrep -f -- "^$1/bin/java" >/dev/null || [ "$tries" -ge 50 ]; do
        tries=$((tries + 1))
    done
}

stop_fake_ide_process() {
    kill "$pid" 2>/dev/null
    wait "$pid" 2>/dev/null
}

test_running_ide_is_refused() {
    setup
    fake_ide "$work/opt" rustrover RustRover 2025.2.3 rustrover.sh
    roots="$work/opt"
    start_fake_ide_process "$work/opt/rustrover"
    run_install ""
    stop_fake_ide_process
    check "running IDE: fails" '[ "$status" -ne 0 ]'
    check "running IDE: says to close it" 'contains "running" && ! contains " installPlugins "'
    teardown
}

# --check runs before the dev build, so a running IDE fails before the build, not after it.
test_check_refuses_a_running_ide() {
    setup
    fake_ide "$work/opt" rustrover RustRover 2025.2.3 rustrover.sh
    roots="$work/opt"
    start_fake_ide_process "$work/opt/rustrover"
    run_install "" --check
    stop_fake_ide_process
    check "check, running IDE: fails" '[ "$status" -ne 0 ]'
    check "check, running IDE: says to close it" 'contains "running"'
    check "check, running IDE: installs nothing" '! contains "installPlugins"'
    teardown
}

test_check_passes_a_closed_ide() {
    setup
    fake_ide "$work/opt" rustrover RustRover 2025.2.3 rustrover.sh
    roots="$work/opt"
    run_install "" --check
    check "check, closed IDE: exit 0" '[ "$status" -eq 0 ]'
    check "check, closed IDE: installs nothing" '! contains "installPlugins"'
    teardown
}

# With several IDEs and no argument the selection comes after the build; --check does not ask twice.
test_check_does_not_prompt() {
    setup
    fake_ide "$work/opt" pycharm PyCharm 2025.2.1 pycharm.sh
    fake_ide "$work/opt" rustrover RustRover 2025.2.3 rustrover.sh
    roots="$work/opt"
    start_fake_ide_process "$work/opt/rustrover"
    run_install "" --check
    stop_fake_ide_process
    check "check, several IDEs: no prompt" '! contains "Select"'
    check "check, several IDEs: exit 0" '[ "$status" -eq 0 ]'
    teardown
}

test_reports_per_ide() {
    setup
    fake_ide "$work/opt" rustrover RustRover 2025.2.3 rustrover.sh
    roots="$work/opt"
    run_install ""
    check "report: names the IDE and its path" 'contains "RustRover 2025.2.3: $work/opt/rustrover"'
    check "report: dry run installs nothing" 'contains "RustRover 2025.2.3: dry run, nothing installed" && ! contains ": installed"'
    teardown
}

test_single_ide_installs_without_prompt
test_release_command
test_dev_command
test_dev_replaces_the_installed_plugin
test_dev_without_installed_plugin_removes_nothing
test_several_ides_prompt_and_multi_select
test_argument_skips_prompt
test_duplicate_listed_once
test_none_found_asks_for_path
test_running_ide_is_refused
test_check_refuses_a_running_ide
test_check_passes_a_closed_ide
test_check_does_not_prompt
test_reports_per_ide

if [ "$failures" -gt 0 ]; then
    printf '%d check(s) failed\n' "$failures"
    exit 1
fi
printf 'all checks passed\n'
