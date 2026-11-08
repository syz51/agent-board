#!/usr/bin/env bash
# Upgrades the board server and every home device to one pinned revision.
#
#   deploy/upgrade.sh [--yes] [--rollback] [--build] [--config FILE] REV
#
# REV is a full commit SHA on main or a v* release tag whose commit is on main. The server
# runs the image CI published for that commit (ghcr.io/syz51/agent-board:sha-<commit>):
# this script checks its build provenance with `gh attestation verify` and the server pulls
# it by digest. --build builds the image on the server instead. Without --yes the script
# prints the plan (each host: current revision -> new revision) and changes nothing. With
# --yes it locks the server, pulls or builds the new image, backs up the database,
# restarts the server and waits for /healthz, then installs the board CLI at REV on each
# device and restarts its `board watch` daemon. It stops at the first failure and says
# what to do next. It refuses a REV older than a host's current one unless --rollback is
# given.
#
# Hosts come from a config file outside the repository (docs/DEPLOY.md, "Upgrade"):
#   server <ssh-target> [clone=/opt/agent-board] [db=/var/lib/agent-board/board.db] [port=7377] [identity=FILE]
#   device local
#   device <ssh-target> [identity=FILE]
set -euo pipefail

usage() {
    sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//' >&2
    exit 2
}

die() {
    printf 'upgrade: %s\n' "$*" >&2
    exit 1
}

yes=0
rollback=0
build=0
config="${XDG_CONFIG_HOME:-$HOME/.config}/agent-board/upgrade.conf"
rev=""
while [ $# -gt 0 ]; do
    case "$1" in
    --yes) yes=1 ;;
    --rollback) rollback=1 ;;
    --build) build=1 ;;
    --config)
        [ $# -ge 2 ] || usage
        config=$2
        shift
        ;;
    -h | --help) usage ;;
    -*) die "unknown option $1: see --help" ;;
    *)
        [ -z "$rev" ] || die "give one revision, not '$rev' and '$1'"
        rev=$1
        ;;
    esac
    shift
done
[ -n "$rev" ] || usage

# The revision is checked against a strict pattern before anything else runs, since it
# ends up in commands on other hosts.
sha_re='^[0-9a-f]{40}$'
tag_re='^v[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.]+)?$'
if [[ $rev =~ $sha_re ]]; then
    kind=sha
elif [[ $rev =~ $tag_re ]]; then
    kind=tag
else
    die "'$rev' is not a full 40-character commit SHA or a v* release tag (such as v0.2.0): pass one of those"
fi

# Devices install from this repository, and the server fetches from it, so the revision
# is checked against it rather than against this clone's own remotes.
git_url=${BOARD_GIT_URL:-https://github.com/syz51/agent-board}
[[ $git_url =~ ^(https://|/)[A-Za-z0-9./_-]+$ ]] ||
    die "BOARD_GIT_URL must be a plain https URL or an absolute path"

repo=$(cd "$(dirname "$0")/.." && pwd)
git -C "$repo" fetch --quiet "$git_url" main ||
    die "git fetch of main from $git_url failed: check the network and run again"
main_tip=$(git -C "$repo" rev-parse FETCH_HEAD)
if [ "$kind" = sha ]; then
    sha=$(git -C "$repo" rev-parse --verify --quiet "$rev^{commit}") ||
        die "$rev is not a commit on main of $git_url: pass one that is"
else
    refs=$(git -C "$repo" ls-remote "$git_url" "refs/tags/$rev" "refs/tags/$rev^{}") ||
        die "git ls-remote $git_url failed: check the network and run again"
    # An annotated tag lists its commit on the ^{} line.
    sha=$(printf '%s\n' "$refs" | awk '$2 ~ /\^\{\}$/ { print $1; exit }')
    [ -n "$sha" ] || sha=$(printf '%s\n' "$refs" | awk 'NR == 1 { print $1 }')
    [ -n "$sha" ] || die "tag $rev does not exist in $git_url: pass an existing v* tag"
    git -C "$repo" cat-file -e "$sha^{commit}" 2>/dev/null ||
        die "tag $rev points at $sha, which is not on main of $git_url"
fi
git -C "$repo" merge-base --is-ancestor "$sha" "$main_tip" ||
    die "$rev is not on main of $git_url: merge it first, or pass a commit that is"
short=${sha:0:12}

# --- Image -----------------------------------------------------------------------------

# The server pulls the image by digest, and only a digest whose provenance says it was
# built by this repository's CI from this commit on main, on a GitHub-hosted runner.
image=${BOARD_IMAGE:-ghcr.io/syz51/agent-board}
[[ $image =~ ^ghcr\.io/[a-z0-9_.-]+/[a-z0-9_.-]+$ ]] ||
    die "BOARD_IMAGE must be ghcr.io/<owner>/<repository>, the image CI publishes"
image_repo=${image#ghcr.io/}
if [ $build = 1 ]; then
    image_ref=build
else
    command -v gh >/dev/null ||
        die "gh checks the image's provenance and is not installed: install it, or pass --build to build on the server"
    digest=$(gh attestation verify "oci://$image:sha-$sha" --repo "$image_repo" \
        --signer-workflow "$image_repo/.github/workflows/ci.yml" --source-ref refs/heads/main \
        --source-digest "$sha" --deny-self-hosted-runners --format json \
        --jq '[.[].verificationResult.statement.subject[].digest.sha256] | unique | .[]') ||
        die "no image with verified provenance at $image:sha-$sha (see above). CI publishes one for each push to main in its 'image' job: wait for it, upgrade to a later commit on main, or pass --build to build on the server"
    [[ $digest =~ ^[0-9a-f]{64}$ ]] ||
        die "the provenance of $image:sha-$sha names '$digest', not one sha256 digest: pass --build to build on the server"
    image_ref="$image@sha256:$digest"
fi

# --- Hosts ---------------------------------------------------------------------------

[ -f "$config" ] || die "no host list at $config: write one (docs/DEPLOY.md, \"Upgrade\") or pass --config"

target_re='^[A-Za-z0-9_][A-Za-z0-9._@-]*$'
path_re='^/[A-Za-z0-9._/-]+$'
server=""
clone=/opt/agent-board
db=/var/lib/agent-board/board.db
port=7377
server_id=""
devices=()
device_ids=()
n=0
while read -r role target opts || [ -n "${role:-}" ]; do
    n=$((n + 1))
    case "${role:-}" in '' | '#'*) continue ;; esac
    [ -n "${target:-}" ] || die "$config line $n: '$role' needs a host"
    [[ $target =~ $target_re ]] || die "$config line $n: '$target' is not an ssh host name"
    identity=""
    for opt in ${opts:-}; do
        key=${opt%%=*}
        value=${opt#*=}
        [ "$key" != "$opt" ] || die "$config line $n: '$opt' is not key=value"
        case "$role:$key" in
        server:clone)
            [[ $value =~ $path_re ]] || die "$config line $n: clone must be an absolute path"
            clone=$value
            ;;
        server:db)
            [[ $value =~ $path_re ]] || die "$config line $n: db must be an absolute path"
            db=$value
            ;;
        server:port)
            [[ $value =~ ^[0-9]{1,5}$ ]] || die "$config line $n: port must be a number"
            port=$value
            ;;
        *:identity) identity=${value/#\~/$HOME} ;;
        *) die "$config line $n: unknown option '$key' for $role" ;;
        esac
    done
    case "$role" in
    server)
        [ -z "$server" ] || die "$config line $n: only one server line is allowed"
        [ "$target" != local ] || die "$config line $n: the server is reached over ssh, not 'local'"
        server=$target
        server_id=$identity
        ;;
    device)
        devices+=("$target")
        device_ids+=("$identity")
        ;;
    *) die "$config line $n: '$role' is not 'server' or 'device'" ;;
    esac
done <"$config"
[ -n "$server" ] || die "$config has no server line"

# --- Remote steps ----------------------------------------------------------------------

# The steps run on each host; the arguments were validated above.
remote=$(cat "$repo/deploy/upgrade-steps.sh")

# run_on HOST IDENTITY STEP ARGS...: runs a remote step locally or over ssh.
run_on() {
    local host=$1 identity=$2
    shift 2
    if [ "$host" = local ]; then
        printf '%s\n' "$remote" | sh -s -- "$@"
        return
    fi
    # Keepalives hold the connection through a long, quiet cargo build.
    local opts=(-o BatchMode=yes -o StrictHostKeyChecking=yes -o ForwardAgent=no
        -o ForwardX11=no -o ClearAllForwardings=yes -o ConnectTimeout=15
        -o ServerAliveInterval=30 -o ServerAliveCountMax=10)
    if [ -n "$identity" ]; then
        opts+=(-i "$identity" -o IdentitiesOnly=yes)
    fi
    printf '%s\n' "$remote" | ssh "${opts[@]}" -- "$host" sh -s -- "$@"
}

# older_than OLD: true when $sha is strictly older than OLD (an ancestor of it).
older_than() {
    [ "$1" != "$sha" ] &&
        git -C "$repo" cat-file -e "$1^{commit}" 2>/dev/null &&
        git -C "$repo" merge-base --is-ancestor "$sha" "$1"
}

# --- Lock ------------------------------------------------------------------------------

# An applying run holds the server's lock from before it reads the hosts' revisions until
# its last device, so a second run waits instead of racing it to an older build.
token="$(date -u +%Y%m%dT%H%M%SZ)-$$"
locked=0
release() {
    if [ $locked = 1 ]; then
        run_on "$server" "$server_id" server-unlock "$clone" "$token" ||
            echo "upgrade: could not release the lock: remove $clone/.git/agent-board-upgrade.lock on $server" >&2
    fi
}
if [ $yes = 1 ]; then
    trap release EXIT
    trap 'exit 130' INT TERM HUP
    run_on "$server" "$server_id" server-lock "$clone" "$token" ||
        die "could not take the upgrade lock on $server (see above): nothing was changed"
    locked=1
fi

# --- Plan ------------------------------------------------------------------------------

status=$(run_on "$server" "$server_id" server-status "$clone" "$port") ||
    die "could not read the server's revision: check that 'ssh $server' works without a prompt"
read -r server_prev server_running server_recorded <<<"$status"
[[ $server_prev =~ $sha_re ]] || die "the server reported '$status', not a revision"

device_prev=()
i=0
while [ $i -lt ${#devices[@]} ]; do
    prev=$(run_on "${devices[$i]}" "${device_ids[$i]}" device-status) ||
        die "could not read board's revision on ${devices[$i]} (see above): nothing was changed"
    device_prev+=("$prev")
    i=$((i + 1))
done

refuse=""
check_order() {
    if older_than "$2" && [ $rollback = 0 ]; then
        refuse="$refuse $1"
    fi
}

if [ "$kind" = tag ]; then
    echo "upgrade to $short ($rev, $sha)"
else
    echo "upgrade to $short ($sha)"
fi
server_skip=0
if [ "$server_prev" = "$sha" ] && [ "$server_running" = "$short" ]; then
    server_skip=1
    echo "  server $server: $short (already there)"
else
    echo "  server $server: ${server_prev:0:12} (running ${server_running}) -> $short"
fi
check_order "server" "$server_prev"
if [ "$image_ref" = build ]; then
    echo "  image: built on the server (--build)"
else
    echo "  image: $image_ref (provenance checked)"
fi
i=0
while [ $i -lt ${#devices[@]} ]; do
    prev=${device_prev[$i]}
    if [ "$prev" = "$sha" ]; then
        echo "  device ${devices[$i]}: $short (already there; board watch restarts)"
    else
        echo "  device ${devices[$i]}: ${prev:0:12} -> $short"
    fi
    check_order "${devices[$i]}" "$prev"
    i=$((i + 1))
done

[ -z "$refuse" ] ||
    die "$short is older than the revision on:$refuse. A newer build would be lost; pass --rollback to go back on purpose"

if [ $yes = 0 ]; then
    echo "plan only: nothing changed. Run again with --yes to apply."
    exit 0
fi

# The revision to roll back to: the server's before this run, or, when an earlier run
# already moved it, the one that run recorded.
back=$server_prev
if [ "$back" = "$sha" ]; then
    back=$server_recorded
fi
if [[ $back =~ $sha_re ]]; then
    rollback_hint="to roll back the code: $(printf '%q' "$0") --config $(printf '%q' "$config") --rollback --yes $back (pre-upgrade backups are in $(dirname "$db") on the server; migrations do not run backwards)"
else
    rollback_hint="the server records no earlier revision to roll back to"
fi

# --- Apply -----------------------------------------------------------------------------

if [ $server_skip = 0 ]; then
    echo "server $server: upgrading"
    run_on "$server" "$server_id" server-upgrade "$clone" "$port" "$db" "$git_url" "$sha" "$token" "$image_ref" ||
        die "the server upgrade stopped (see above); no device was changed. $rollback_hint"
fi

device_now=()
i=0
while [ $i -lt ${#devices[@]} ]; do
    host=${devices[$i]}
    if [ "${device_prev[$i]}" = "$sha" ]; then
        echo "device $host: restarting board watch"
    else
        echo "device $host: installing"
    fi
    now=$(run_on "$host" "${device_ids[$i]}" device-upgrade "$git_url" "$sha") ||
        die "$host stopped (see above); the server and the devices before it run $short. Fix it and run the same command again: finished hosts are skipped. $rollback_hint"
    device_now+=("$now")
    i=$((i + 1))
done

echo "done: the server runs $short; cargo install --list on each device shows:"
i=0
while [ $i -lt ${#devices[@]} ]; do
    echo "  ${devices[$i]}: ${device_now[$i]} (was ${device_prev[$i]:0:12})"
    i=$((i + 1))
done
echo "$rollback_hint"
