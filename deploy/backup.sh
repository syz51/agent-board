#!/bin/sh
# Copies the live database consistently (VACUUM INTO) and stores it with restic.
# Run by agent-board-backup.timer; settings come from /etc/agent-board/restic.env.
set -eu

set -a
# shellcheck source=/dev/null
. /etc/agent-board/restic.env
set +a

dir=/var/lib/agent-board/backup
mkdir -p "$dir"
chown 10001:10001 "$dir"
rm -f "$dir/board.db"

docker compose -f /opt/agent-board/deploy/compose.yaml exec -T board \
    board admin backup --db /var/lib/agent-board/board.db --out "$dir/board.db"
# Extra restic options, such as `-o s3.bucket-lookup=dns` for stores that need
# virtual-hosted bucket names.
# shellcheck disable=SC2086
restic ${BOARD_RESTIC_OPTIONS:-} backup --quiet --tag agent-board --host board "$dir/board.db"
rm -f "$dir/board.db"

# Thin out old snapshots once a day.
if [ "$(date -u +%H%M)" -lt 0015 ]; then
    # shellcheck disable=SC2086
    restic ${BOARD_RESTIC_OPTIONS:-} forget --quiet --tag agent-board --keep-within 2d --keep-daily 30 \
        --keep-weekly 26 --prune
fi
