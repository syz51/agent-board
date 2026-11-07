#!/usr/bin/env bash
# Upgrades the board server and every home device to one pinned revision.
#
#   deploy/upgrade.sh [--yes] [--rollback] [--config FILE] REV
#
# REV is a full commit SHA on origin/main or a v* release tag. Without --yes the script
# prints the plan (each host: current revision -> new revision) and changes nothing.
# With --yes it backs up the database, moves the server's clone to REV, rebuilds the
# container and waits for /healthz, then installs the board CLI at REV on each device
# and restarts its `board watch` daemon. It stops at the first failure and says what to
# do next. It refuses a REV older than a host's current one unless --rollback is given.
#
# Hosts come from a config file outside the repository (docs/DEPLOY.md, "Upgrade"):
#   server <ssh-target> [clone=/opt/agent-board] [data=/var/lib/agent-board] [port=7377] [identity=FILE]
#   device local
#   device <ssh-target> [identity=FILE]
set -euo pipefail

usage() {
    sed -n '2,16p' "$0" | sed 's/^# \{0,1\}//' >&2
    exit 2
}

die() {
    printf 'upgrade: %s\n' "$*" >&2
    exit 1
}

yes=0
rollback=0
config="${XDG_CONFIG_HOME:-$HOME/.config}/agent-board/upgrade.conf"
rev=""
while [ $# -gt 0 ]; do
    case "$1" in
    --yes) yes=1 ;;
    --rollback) rollback=1 ;;
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

git_url=${BOARD_GIT_URL:-https://github.com/syz51/agent-board}
[[ $git_url =~ ^https://[A-Za-z0-9./_-]+$ ]] || die "BOARD_GIT_URL must be a plain https URL"

repo=$(cd "$(dirname "$0")/.." && pwd)
git -C "$repo" fetch --quiet --tags origin ||
    die "git fetch in $repo failed: check that this clone can reach origin"
if [ "$kind" = sha ]; then
    git -C "$repo" cat-file -e "$rev^{commit}" 2>/dev/null ||
        die "$rev is not a commit in origin: pass a commit on origin/main"
    git -C "$repo" merge-base --is-ancestor "$rev" origin/main ||
        die "$rev is not on origin/main: merge it first, or pass a commit that is"
    sha=$rev
else
    sha=$(git -C "$repo" rev-parse --verify --quiet "refs/tags/$rev^{commit}") ||
        die "tag $rev does not exist in origin: pass an existing v* tag"
fi
short=${sha:0:12}

# --- Hosts ---------------------------------------------------------------------------

[ -f "$config" ] || die "no host list at $config: write one (docs/DEPLOY.md, \"Upgrade\") or pass --config"

target_re='^[A-Za-z0-9_][A-Za-z0-9._@-]*$'
path_re='^/[A-Za-z0-9._/-]+$'
server=""
clone=/opt/agent-board
data=/var/lib/agent-board
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
        server:data)
            [[ $value =~ $path_re ]] || die "$config line $n: data must be an absolute path"
            data=$value
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
    local opts=(-o BatchMode=yes -o StrictHostKeyChecking=yes -o ForwardAgent=no
        -o ForwardX11=no -o ClearAllForwardings=yes -o ConnectTimeout=15)
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

# --- Plan ------------------------------------------------------------------------------

status=$(run_on "$server" "$server_id" server-status "$clone" "$port") ||
    die "could not read the server's revision: check that 'ssh $server' works without a prompt"
server_prev=${status%% *}
server_running=${status#* }
[[ $server_prev =~ $sha_re ]] || die "the server reported '$status', not a revision"

device_prev=()
i=0
while [ $i -lt ${#devices[@]} ]; do
    prev=$(run_on "${devices[$i]}" "${device_ids[$i]}" device-status) ||
        die "could not read board's revision on ${devices[$i]}: check that 'ssh ${devices[$i]}' works without a prompt"
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
i=0
while [ $i -lt ${#devices[@]} ]; do
    prev=${device_prev[$i]}
    if [ "$prev" = "$sha" ]; then
        echo "  device ${devices[$i]}: $short (already there)"
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

rollback_hint="to roll back the code: $0 --rollback --yes $server_prev (the database backup is printed above; migrations do not run backwards)"

# --- Apply -----------------------------------------------------------------------------

if [ $server_skip = 0 ]; then
    echo "server $server: upgrading"
    run_on "$server" "$server_id" server-upgrade "$clone" "$data" "$port" "$sha" ||
        die "the server upgrade stopped (see above); no device was changed. Previous server revision: $server_prev. $rollback_hint"
fi

i=0
while [ $i -lt ${#devices[@]} ]; do
    host=${devices[$i]}
    if [ "${device_prev[$i]}" != "$sha" ]; then
        echo "device $host: installing"
        run_on "$host" "${device_ids[$i]}" device-upgrade "$git_url" "$sha" >/dev/null ||
            die "$host stopped (see above); the server and the devices before it run $short. Fix it and run the same command again: finished hosts are skipped. $rollback_hint"
    fi
    i=$((i + 1))
done

echo "done: the server runs $short; cargo install --list on each device shows:"
i=0
while [ $i -lt ${#devices[@]} ]; do
    now=$(run_on "${devices[$i]}" "${device_ids[$i]}" device-status) || now="(could not read)"
    echo "  ${devices[$i]}: $now"
    i=$((i + 1))
done
echo "previous revisions: server ${server_prev:0:12}$(
    i=0
    while [ $i -lt ${#devices[@]} ]; do
        printf ', %s %s' "${devices[$i]}" "${device_prev[$i]:0:12}"
        i=$((i + 1))
    done
)"
echo "$rollback_hint"
