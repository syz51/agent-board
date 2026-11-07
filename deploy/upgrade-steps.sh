#!/bin/sh
# The steps deploy/upgrade.sh runs on each host, piped to `sh -s -- STEP ARGS...` locally
# or over ssh. upgrade.sh validates every argument before it sends them.
set -eu
PATH="$HOME/.cargo/bin:$PATH"
export PATH
fail() {
    printf 'upgrade: %s\n' "$*" >&2
    exit 1
}
installed() {
    cargo install --list 2>/dev/null |
        sed -n 's/^board v[^ ]* (.*[?&]rev=\([0-9a-f]\{40\}\)#.*$/\1/p' | head -n 1
}
health() {
    curl -fsS --max-time 3 "http://127.0.0.1:$1/healthz" 2>/dev/null || true
}
action=$1
shift
case "$action" in
server-status)
    cd "$1"
    commit=$(health "$2" | sed -n 's/.*"commit":"\([0-9a-f]*\)".*/\1/p')
    echo "$(git rev-parse HEAD) ${commit:-none}"
    ;;
server-upgrade)
    clone=$1 data=$2 port=$3 sha=$4
    cd "$clone"
    lock="$(git rev-parse --git-dir)/agent-board-upgrade.lock"
    mkdir "$lock" 2>/dev/null ||
        fail "another upgrade holds $clone/$lock: wait for it, or remove that directory if none is running"
    trap 'rmdir "$lock"' EXIT
    git diff --quiet HEAD -- ||
        fail "$clone has local changes: commit or discard them, then run again (nothing was changed)"
    ts=$(date -u +%Y%m%dT%H%M%SZ)
    backup="$data/pre-upgrade-$ts.db"
    docker compose -f deploy/compose.yaml exec -T board \
        board admin backup --db "$data/board.db" --out "$backup" >/dev/null ||
        fail "the backup failed, so nothing was changed: check 'docker compose -f $clone/deploy/compose.yaml ps' and the container's logs"
    [ -s "$backup" ] || fail "the backup $backup is missing or empty, so nothing was changed"
    echo "backup: $backup"
    # Keep the five newest pre-upgrade copies; the backup timer keeps the history. The
    # names hold a UTC timestamp, so the glob lists them oldest first.
    set -- "$data"/pre-upgrade-*.db
    while [ $# -gt 5 ]; do
        rm -f "$1"
        shift
    done
    git fetch --quiet --tags origin || fail "git fetch in $clone failed: the server is unchanged"
    git checkout --quiet --detach "$sha" ||
        fail "git checkout $sha in $clone failed: the server is unchanged"
    short=$(git rev-parse --short=12 HEAD)
    BOARD_COMMIT=$short docker compose -f deploy/compose.yaml up -d --build ||
        fail "the image build or start failed at $short: read the output above, fix it, and run again"
    i=0
    while [ $i -lt 60 ]; do
        body=$(health "$port")
        case "$body" in *"\"commit\":\"$short\""*)
            echo "healthz: $(printf '%s' "$body" | grep -o '"\(ok\|version\|commit\|schema_version\)":[^,}]*' | tr '\n' ' ')"
            exit 0
            ;;
        esac
        i=$((i + 1))
        sleep 2
    done
    fail "/healthz did not report commit $short within 2 minutes: read 'docker compose -f $clone/deploy/compose.yaml logs board'"
    ;;
device-status)
    rev=$(installed)
    echo "${rev:-unknown}"
    ;;
device-upgrade)
    url=$1 sha=$2
    BOARD_COMMIT=$(printf '%.12s' "$sha") cargo install --quiet --git "$url" --rev "$sha" --locked --force board ||
        fail "cargo install of $sha failed: read the output above; the previous board stays installed"
    case "$(uname -s)" in
    Darwin) launchctl kickstart -k "gui/$(id -u)/dev.agent-board.watch" ;;
    *) systemctl --user restart agent-board-watch.service ;;
    esac || fail "board $sha is installed but board watch did not restart: run 'board install launchd' (or systemd) and load it"
    rev=$(installed)
    [ "$rev" = "$sha" ] || fail "cargo install --list shows ${rev:-no board from git}, not $sha"
    echo "$rev"
    ;;
*) fail "unknown step $action" ;;
esac
