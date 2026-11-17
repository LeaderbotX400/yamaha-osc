#!/bin/sh
# Copyright (c) 2026 Eric Singer. All rights reserved.
# Proprietary and confidential; restricted distribution. No use without an explicit written grant - see LICENSE.
# Install tf-bridge on the booth Mac.
#
#   sh src/install.sh user@host [--no-restart] [--force]   push over ssh and install
#   sh src/install.sh --print                              write the self-contained installer to stdout
#
# --print output is a complete installer (the bridge and presets template are embedded), so it can
# be piped anywhere that has `sh`:
#
#   sh src/install.sh --print | ssh user@host sh -s --
#   sh src/install.sh --print > install-tf-bridge.sh     # carry it to the Mac on a stick
#
# What the installer does on the Mac (idempotent):
#   - checks the interpreter has python-osc and that the new bridge compiles
#   - keeps ~/Scripts/tf3.py.orig (the venue's original, once) and ~/Scripts/tf3.py.prev (what was
#     running before this install) so rollback is one `cp`
#   - installs the bridge as ~/Scripts/tf3.py (the file the tf3.server launchd agent runs)
#   - creates ~/Scripts/presets.json from the example only if you don't have one
#   - reloads the launchd agent, waits for "OSC server listening", and ROLLS BACK to .prev if it
#     doesn't come up
#
# NOTE: this restarts the live bridge (a second or two of no OSC). Don't run it mid-show.
# Installer flags: --no-restart (install files only), --force (reinstall even if identical).

set -eu

HERE=$(cd "$(dirname "$0")" && pwd)
BRIDGE="$HERE/tf-bridge.py"
PRESETS="$HERE/presets.example.json"

remote_body() {
    cat <<'REMOTE'
SCRIPTS="$HOME/Scripts"
TARGET="$SCRIPTS/tf3.py"
PLIST="$HOME/Library/LaunchAgents/tf3.server.plist"
LABEL="tf3.server"
FWPY="/Library/Frameworks/Python.framework/Versions/3.14/bin/python3"
LOG="${TF_LOG:-/tmp/tf3.out.log}"
WAIT="${TF_WAIT:-10}"
RESTART=1
FORCE=0
for a in "$@"; do
    case "$a" in
        --no-restart) RESTART=0 ;;
        --force) FORCE=1 ;;
        *) echo "unknown option: $a" >&2; exit 2 ;;
    esac
done

say() { printf '==> %s\n' "$*"; }
die() { printf 'ERROR: %s\n' "$*" >&2; exit 1; }

PY="$FWPY"
[ -x "$PY" ] || PY=$(command -v python3 || true)
[ -n "$PY" ] || die "no python3 found"
say "interpreter: $PY"
"$PY" -c 'import pythonosc' 2>/dev/null || die "python-osc is not installed for $PY"
"$PY" -m py_compile "$TMPD/tf-bridge.py" || die "the new bridge does not compile"
[ -f "$PLIST" ] || [ "$RESTART" = 0 ] || die "$PLIST not found - is this the booth Mac?"
mkdir -p "$SCRIPTS"

if [ -f "$TARGET" ] && cmp -s "$TMPD/tf-bridge.py" "$TARGET" && [ "$FORCE" = 0 ]; then
    say "$TARGET is already up to date (use --force to reinstall/restart)"
    exit 0
fi

if [ -f "$TARGET" ]; then
    if [ ! -f "$TARGET.orig" ]; then
        cp -p "$TARGET" "$TARGET.orig"
        say "saved the original as $TARGET.orig"
    fi
    cp -p "$TARGET" "$TARGET.prev"
    say "previous version kept as $TARGET.prev"
fi

cp "$TMPD/tf-bridge.py" "$TARGET.new"
mv "$TARGET.new" "$TARGET"
say "installed $TARGET"

if [ ! -f "$SCRIPTS/presets.json" ]; then
    cp "$TMPD/presets.example.json" "$SCRIPTS/presets.json"
    say "created $SCRIPTS/presets.json from the example - edit it for your show"
fi

reload() {
    command -v launchctl >/dev/null 2>&1 || die "launchctl not found"
    launchctl bootout "gui/$(id -u)/$LABEL" 2>/dev/null || true
    launchctl bootstrap "gui/$(id -u)" "$PLIST" || return 1
}

wait_for() {  # wait_for <pattern> <skip-lines>: poll the log until the pattern shows up
    i=0
    while [ "$i" -lt "$WAIT" ]; do
        if [ -f "$LOG" ] && tail -n +"$(($2 + 1))" "$LOG" | grep -q "$1"; then return 0; fi
        sleep 1
        i=$((i + 1))
    done
    return 1
}

if [ "$RESTART" = 0 ]; then
    say "files installed; not restarting (--no-restart). Reload with:"
    echo "    launchctl bootout gui/\$(id -u)/$LABEL; launchctl bootstrap gui/\$(id -u) $PLIST"
    exit 0
fi

before=0
[ -f "$LOG" ] && before=$(wc -l < "$LOG")
say "reloading $LABEL"
if reload && wait_for "OSC server listening" "$before"; then
    say "bridge is up on UDP :8000"
    if wait_for "TF CONNECTED" "$before"; then
        say "connected to the console"
    else
        say "WARNING: no console connection yet (console off / not on the 192.168.50.x network?)"
    fi
    say "done. Roll back with:  cp $TARGET.prev $TARGET  (original: $TARGET.orig)  then reload the agent"
    exit 0
fi

echo "ERROR: the new bridge did not start; rolling back" >&2
if [ -f "$TARGET.prev" ]; then
    cp "$TARGET.prev" "$TARGET"
    reload || true
    echo "rolled back to $TARGET.prev; see $LOG and /tmp/tf3.err.log" >&2
fi
exit 1
REMOTE
}

emit() {
    for f in "$BRIDGE" "$PRESETS"; do
        [ -f "$f" ] || { echo "missing $f" >&2; exit 1; }
    done
    for marker in __TF_BRIDGE_PY__ __TF_PRESETS_JSON__; do
        if grep -q "^$marker\$" "$BRIDGE" "$PRESETS"; then
            echo "payload contains the heredoc marker $marker" >&2
            exit 1
        fi
    done
    echo '#!/bin/sh'
    echo '# Copyright (c) 2026 Eric Singer. All rights reserved. Proprietary; see LICENSE.'
    echo '# tf-bridge installer (generated by src/install.sh) - safe to re-run'
    echo 'main() {'
    echo 'set -eu'
    echo 'TMPD=$(mktemp -d)'
    echo 'trap '"'"'rm -rf "$TMPD"'"'"' EXIT'
    echo "cat > \"\$TMPD/tf-bridge.py\" <<'__TF_BRIDGE_PY__'"
    cat "$BRIDGE"
    echo '__TF_BRIDGE_PY__'
    echo "cat > \"\$TMPD/presets.example.json\" <<'__TF_PRESETS_JSON__'"
    cat "$PRESETS"
    echo '__TF_PRESETS_JSON__'
    remote_body
    echo '}'
    echo 'main "$@"'
    echo 'exit $?'
}

case "${1:-}" in
    --print)
        emit
        ;;
    -h|--help|"")
        sed -n '2,/^set -eu/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'
        [ -n "${1:-}" ] || exit 2
        ;;
    -*)
        echo "unknown option: $1 (try --help)" >&2
        exit 2
        ;;
    *)
        host=$1
        shift
        echo "Installing to $host (this restarts the live bridge)..." >&2
        emit | ssh "$host" sh -s -- "$@"
        ;;
esac
