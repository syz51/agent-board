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
# The commit board was installed from with `cargo install --git`, or nothing.
installed() {
    command -v cargo >/dev/null ||
        fail "cargo is not on PATH or in ~/.cargo/bin: install Rust with rustup first"
    list=$(cargo install --list) || fail "cargo install --list failed: see above"
    printf '%s\n' "$list" |
        sed -n 's/^board v[^ ]* (.*[?&]rev=\([0-9a-f]\{40\}\)#.*$/\1/p' | head -n 1
}
# The /healthz body, or nothing while the server does not answer.
health() {
    curl -fsS --max-time 3 "http://127.0.0.1:$1/healthz" 2>/dev/null || true
}
compose() {
    docker compose -f deploy/compose.yaml "$@"
}
# Signs board in cargo's bin folder with this Mac's own code-signing identity, made on first
# use, so macOS privacy grants survive upgrades. `cargo install` leaves an ad-hoc signature
# that macOS knows only by its hash, so every build lost the grant (such as Documents access,
# which board watch needs for repositories there) and git waited on a new prompt. The key
# is in its own keychain, unlocked with a random password in a 0600 file beside it.
# Each failure says what failed.
sign_board() {
    dir="$HOME/Library/Application Support/agent-board/signing"
    keychain="$dir/signing.keychain-db"
    name="agent-board code signing"
    bin="${CARGO_HOME:-$HOME/.cargo}/bin/board"
    (umask 077 && mkdir -p "$dir") || return 1
    # A keychain that does not unlock or holds no identity, as a run cut short leaves it,
    # is made again.
    if ! id=$(signing_id); then
        echo "upgrade: making the code-signing identity in $dir"
        rm -f "$keychain" "$dir/password"
        if ! make_signing_identity; then
            rm -f "$keychain" "$dir/password"
            return 1
        fi
        id=$(signing_id) || {
            echo "upgrade: $keychain holds no usable identity named \"$name\" right after it was made"
            return 1
        }
    fi
    codesign --force --keychain "$keychain" -s "$id" --identifier dev.agent-board.board "$bin" || {
        echo "upgrade: codesign could not sign $bin"
        return 1
    }
    codesign -d -r- "$bin" 2>&1 | grep -q 'certificate leaf' || {
        echo "upgrade: codesign ran, but the signature of $bin names no certificate"
        return 1
    }
}
# Unlocks the keychain and prints the hash of its identity; fails without one.
signing_id() {
    [ -s "$dir/password" ] && [ -f "$keychain" ] || return 1
    security unlock-keychain -p "$(cat "$dir/password")" "$keychain" 2>/dev/null || return 1
    # Listed as untrusted, which codesign accepts: macOS needs only the same key each time.
    security find-identity -p codesigning "$keychain" |
        grep -F "\"$name\"" | awk '{ print $2; found = 1; exit } END { exit !found }'
}
# Makes the password and a keychain that holds the identity. A subshell, so its trap
# removes the unencrypted key however it ends.
make_signing_identity() (
    (umask 077 && /usr/bin/openssl rand -hex 24 >"$dir/password") || {
        echo "upgrade: openssl could not make the keychain password"
        exit 1
    }
    password=$(cat "$dir/password")
    tmp=$(mktemp -d) || exit 1
    trap 'rm -rf "$tmp"' EXIT
    trap 'exit 1' HUP INT TERM
    cat >"$tmp/cert.cnf" <<'EOF'
[req]
distinguished_name = dn
x509_extensions = ext
prompt = no
[dn]
CN = agent-board code signing
[ext]
basicConstraints = critical,CA:false
keyUsage = critical,digitalSignature
extendedKeyUsage = critical,codeSigning
EOF
    # /usr/bin/openssl is LibreSSL, whose PKCS#12 files `security import` reads. Its
    # progress dots are shown only when it fails.
    /usr/bin/openssl req -x509 -newkey rsa:2048 -nodes -days 36500 -config "$tmp/cert.cnf" \
        -keyout "$tmp/key.pem" -out "$tmp/cert.pem" 2>"$tmp/err" || {
        cat "$tmp/err"
        echo "upgrade: openssl could not make the signing certificate"
        exit 1
    }
    /usr/bin/openssl pkcs12 -export -inkey "$tmp/key.pem" -in "$tmp/cert.pem" -name "$name" \
        -out "$tmp/id.p12" -passout "file:$dir/password" || {
        echo "upgrade: openssl could not bundle the signing key"
        exit 1
    }
    # A new keychain is unlocked; set-keychain-settings with no options keeps it so.
    { security create-keychain -p "$password" "$keychain" &&
        security set-keychain-settings "$keychain" &&
        security import "$tmp/id.p12" -k "$keychain" -P "$password" \
            -T /usr/bin/codesign >/dev/null &&
        security set-key-partition-list -S apple-tool:,apple:,codesign: -s -k "$password" \
            "$keychain" >/dev/null; } || {
        echo "upgrade: security could not store the signing identity in $keychain"
        exit 1
    }
)
action=$1
shift
case "$action" in
server-status)
    cd "$1"
    git_dir=$(git rev-parse --git-dir)
    commit=$(health "$2" | sed -n 's/.*"commit":"\([0-9a-f]*\)".*/\1/p')
    previous=none
    if [ -f "$git_dir/agent-board-previous" ]; then
        previous=$(cat "$git_dir/agent-board-previous")
    fi
    echo "$(git rev-parse HEAD) ${commit:-none} $previous"
    ;;
server-lock)
    cd "$1"
    lock="$(git rev-parse --git-dir)/agent-board-upgrade.lock"
    if ! mkdir "$lock" 2>/dev/null; then
        owner=$(cat "$lock/owner" 2>/dev/null || echo unknown)
        if [ -n "$(find "$lock" -maxdepth 0 -mmin +120)" ]; then
            echo "upgrade: replacing the lock of upgrade $owner, which is over 2 hours old" >&2
            rm -rf "$lock"
            mkdir "$lock"
        else
            fail "upgrade $owner holds $1/$lock: wait for it, or remove that directory if it is not running"
        fi
    fi
    echo "$2" >"$lock/owner"
    ;;
server-unlock)
    cd "$1"
    lock="$(git rev-parse --git-dir)/agent-board-upgrade.lock"
    if [ "$(cat "$lock/owner" 2>/dev/null)" = "$2" ]; then
        rm -rf "$lock"
    fi
    ;;
server-upgrade)
    clone=$1 port=$2 db=$3 url=$4 sha=$5 token=$6 image=$7
    cd "$clone"
    git_dir=$(git rev-parse --git-dir)
    [ "$(cat "$git_dir/agent-board-upgrade.lock/owner" 2>/dev/null)" = "$token" ] ||
        fail "this run no longer holds the upgrade lock in $clone: run again"
    data=$(dirname "$db")
    if [ ! -r "$data" ] || [ ! -w "$data" ]; then
        fail "$(id -un) cannot read and write $data, where the backup goes: run the server steps as root"
    fi
    git diff --quiet HEAD -- ||
        fail "$clone has local changes: commit or discard them, then run again (nothing was changed)"
    prev=$(git rev-parse HEAD)
    short=$(printf '%.12s' "$sha")
    git fetch --quiet "$url" main || fail "git fetch of $url in $clone failed: the server is unchanged"
    git checkout --quiet --detach "$sha" ||
        fail "git checkout $sha in $clone failed: the server is unchanged"
    # The image agent-board:local names now, if any: a failure below names it again, and
    # a healthy start removes it.
    prev_image=$(docker image inspect -f '{{.Id}}' agent-board:local 2>/dev/null) || prev_image=""
    # undo: puts the clone and agent-board:local back where they were.
    undo() {
        git checkout --quiet --detach "$prev"
        if [ -n "$prev_image" ] && ! docker tag "$prev_image" agent-board:local; then
            echo "upgrade: could not name $prev_image agent-board:local again: run 'docker tag $prev_image agent-board:local' on the server" >&2
        fi
    }
    # Get the image before the backup, so the copy misses only the seconds of the restart.
    # The running container keeps its image when agent-board:local moves.
    if [ "$image" = build ]; then
        if ! BOARD_COMMIT=$short BOARD_BUILT=$(git log -1 --format=%ct HEAD) compose build board; then
            undo
            fail "the image build failed at $short: the server still runs the old image and the clone is back at $prev. Read the output above, fix it, and run again"
        fi
    elif ! docker pull --quiet "$image" >/dev/null || ! docker tag "$image" agent-board:local; then
        undo
        fail "pulling $image failed: the server still runs the old image and the clone is back at $prev. The server may not reach ghcr.io, or may not be linux/amd64, the only platform CI publishes: fix the first and run again, or pass --build to build on the server"
    fi
    ts=$(date -u +%Y%m%dT%H%M%SZ)
    backup="$data/pre-upgrade-$ts.db"
    if ! compose exec -T board board admin backup --db "$db" --out "$backup" >/dev/null ||
        [ ! -s "$backup" ]; then
        undo
        fail "the backup to $backup failed, so the old server keeps running and the clone is back at $prev: check 'docker compose -f $clone/deploy/compose.yaml ps' and the container's logs. A server that is not running needs the manual steps in docs/DEPLOY.md"
    fi
    echo "backup: $backup"
    # Keep the five newest pre-upgrade copies; the backup timer keeps the history. The
    # names hold a UTC timestamp, so the glob lists them oldest first.
    set -- "$data"/pre-upgrade-*.db
    while [ $# -gt 5 ]; do
        rm -f "$1"
        shift
    done
    if [ "$prev" != "$sha" ]; then
        echo "$prev" >"$git_dir/agent-board-previous"
    fi
    BOARD_COMMIT=$short compose up -d --no-build board ||
        fail "the new container did not start at $short: read 'docker compose -f $clone/deploy/compose.yaml logs board', fix it and run again, or roll back"
    i=0
    while [ $i -lt 60 ]; do
        body=$(health "$port")
        case "$body" in *"\"commit\":\"$short\""*)
            echo "healthz: $(printf '%s' "$body" | grep -o '"\(ok\|version\|commit\|schema_version\)":[^,}]*' | tr '\n' ' ')"
            # Remove the image the old container ran, so images do not pile up. Docker
            # refuses while another container or tag still uses it; that is fine.
            new_image=$(docker image inspect -f '{{.Id}}' agent-board:local 2>/dev/null) || new_image=""
            if [ -n "$prev_image" ] && [ -n "$new_image" ] && [ "$prev_image" != "$new_image" ] &&
                docker image rm "$prev_image" >/dev/null 2>&1; then
                echo "removed the previous image $prev_image"
            fi
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
    if [ "$(installed)" != "$sha" ]; then
        BOARD_COMMIT=$(printf '%.12s' "$sha") cargo install --quiet --git "$url" --rev "$sha" --locked --force board ||
            fail "cargo install of $sha failed: read the output above; the previous board stays installed"
    fi
    # Signed again even when the install was already there: a run that failed here earlier
    # may have left the ad-hoc signature. A failure costs only a new privacy prompt. Its
    # output goes to stderr: upgrade.sh reads this step's stdout as the revision.
    if [ "$(uname -s)" = Darwin ] && ! sign_board >&2; then
        echo "upgrade: could not sign board with a lasting identity (see above), so macOS may ask again for folder access" >&2
    fi
    # Restart even when the install was already there: a run that failed here earlier
    # left the old daemon running.
    case "$(uname -s)" in
    Darwin) launchctl kickstart -k "gui/$(id -u)/dev.agent-board.watch" ;;
    *) systemctl --user restart agent-board-watch.service ;;
    esac || fail "board $sha is installed but board watch did not restart: run 'board install launchd' (or systemd) and load it, then run again"
    rev=$(installed)
    [ "$rev" = "$sha" ] || fail "cargo install --list shows ${rev:-no board from git}, not $sha"
    echo "$rev"
    ;;
*) fail "unknown step $action" ;;
esac
