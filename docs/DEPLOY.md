# Deploying the server

One server per setup, on a host in your tailnet. Every device and agent reaches it over
Tailscale; nothing listens on a public address. Values below are placeholders.

## 1. Host

Any Linux host with Docker and Tailscale. Join it to the tailnet with MagicDNS left as it
is for the host's other services:

```sh
tailscale up --hostname=board-host --accept-dns=false
```

## 2. Server

```sh
git clone https://github.com/syz51/agent-board /opt/agent-board
install -d -m 0755 /etc/agent-board
install -d -o 10001 -g 10001 -m 0700 /var/lib/agent-board
install -m 0644 /opt/agent-board/deploy/server.example.toml /etc/agent-board/server.toml
# edit /etc/agent-board/server.toml: users, public_url, optional [telegram] chat_id
BOARD_COMMIT=$(git -C /opt/agent-board rev-parse --short=12 HEAD) \
    docker compose -f /opt/agent-board/deploy/compose.yaml up -d --build
tailscale serve --bg --https=443 http://127.0.0.1:7377
curl -fsS https://board-host.your-tailnet.ts.net/healthz
```

The image has no `.git`, so the build reads the commit from `BOARD_COMMIT`. Verify the
deploy with `/healthz`: `commit` matches `git -C /opt/agent-board rev-parse --short=12
HEAD`, `version` matches the workspace version, and `schema_version` is the number of files
in `crates/board-server/migrations`. Each device's `board --version` prints the version
and commit it was built from; when they differ from the server's, the CLI and MCP warn.

The container uses the host network and listens on `127.0.0.1:7377`, so only processes on
the host reach it. `tailscale serve` terminates TLS and adds `Tailscale-User-Login`, which
the server trusts for the logins in `users` (`trust_identity_header`). Do not publish the
port another way while that setting is on.

Telegram (optional): create a bot with @BotFather, put `BOARD_TELEGRAM_TOKEN=<token>` in
`/etc/agent-board/secrets.env` (mode 0600) and restart the container. Then pair your chat:

```sh
docker compose -f /opt/agent-board/deploy/compose.yaml exec board \
    board admin telegram-pair --db /var/lib/agent-board/board.db
```

It prints a one-time code, valid for 15 minutes. In Telegram, open a private chat with the
bot and send `/start <code>`; the bot answers "Paired", and from then on only that chat
receives asks and may answer them. Asks created before pairing are sent then. Messages from
other chats are ignored. To move to another chat, run the command again with `--replace`
and send the new code from that chat; the paired chat keeps working until then, and open
asks are sent again to the new one. Setting `[telegram] chat_id` in `server.toml` instead
skips pairing and always wins over a paired chat.

The same chat gets two scheduled alerts (docs/DESIGN.md, "Scheduled alerts"). Both are on
by default; set them in `[telegram]` and restart the container:

```toml
[telegram]
stuck_after_secs = 14400   # report a doing or review task quiet this long, once; 0 = off
stalled_after_secs = 7200  # report an autopilot board whose ready work sat unclaimed this long; 0 = off
digest_at = "09:00"        # daily Needs you digest at this local time; "off" = off
utc_offset = "+08:00"      # the local time's offset from UTC (fixed: no daylight saving)
```

The first digest goes out at the first check past `digest_at` after the upgrade, which can
be right away; days with nothing to list send nothing. Set `utc_offset`: without it,
`digest_at` is read as UTC. The first check after the upgrade also reports every task
already quiet past `stuck_after_secs`, in one message. A board's stalled clock starts at the
first check that sees it stalled, so an autopilot board already stalled at the upgrade is
reported `stalled_after_secs` later.

To upgrade, use `deploy/upgrade.sh` (section 5). Migrations run on start. Since pairing
came in, `BOARD_TELEGRAM_TOKEN` alone starts the notifier, without a `[telegram]` table; it
sends nothing until a chat is paired or configured.

## 3. Devices

On the server host, create a token per device (`--user` for the token you answer asks
with), and paste each into `board login` on that device:

```sh
docker compose -f /opt/agent-board/deploy/compose.yaml exec board \
    board admin device laptop --db /var/lib/agent-board/board.db
```

On each device:

```sh
board login --server https://board-host.your-tailnet.ts.net --device laptop   # paste the token
board install claude --write ~/.claude/settings.json   # hooks; prints the MCP command
board install launchd                                   # or systemd: runs `board watch`
```

`board install codex` prints the Codex `hooks.json` and `config.toml` entries.

On macOS the token goes to the login Keychain. Where the Keychain cannot be used without
a desktop session (a login over SSH, or a locked Keychain), `board login` stores it in
`$BOARD_HOME/token` (mode 0600) instead and prints that path; `board` reads it from there.
Each `$BOARD_HOME` keeps its own token, so a second login with another `BOARD_HOME` (for
the `--user` token you answer asks with) does not replace the device's token.

### Coordinators and the feedback board

A board with autopilot on gets a coordinator thread on one of its home devices, in a T3
project on that device. `board watch` finds the project in the device's
`$BOARD_HOME/config.toml`: `[projects]` maps a board repo to its folder, and
`[board_projects]` maps a board slug to a folder for a board without a mapped repo.
A running `board watch` re-reads the file within a minute of an edit and applies these two
tables and `feedback_board` without a restart; a change to `server`, `device` or `t3_bin`
needs one, and the daemon's log says so.

The feedback board (`board-feedback` unless `feedback_board` says otherwise) has no repo,
and its coordinator triages: it resolves each feedback item as a duplicate, routes it to a
task on the owner's board, forwards it to the setup's own channel, declines it with a
reason, or marks it fixed when the cause is already gone (`prompts/triage.md`). To give it one, pick a home device and a T3 project folder on
it (for example the folder holding your `setups.toml`, the registry of setups and their
owning boards), then:

```toml
# config.toml on the home device
feedback_board = "board-feedback"   # optional; this is the default

[board_projects]
board-feedback = "/path/to/setups"
```

```sh
board board update board-feedback --home-device laptop --autopilot on
```

The daemon on that device starts the `board-feedback coordinator` thread within a minute
and wakes it as feedback arrives. Use the same `feedback_board` on every device, so the
feedback agents file and the board the coordinator triages are one.

## 4. Backups

`deploy/backup.sh` copies the database consistently with `board admin backup` and stores
it with [restic](https://restic.net) in any S3-compatible bucket, encrypted with the
restic password. Use a bucket and a key that only this job uses.

```sh
install -m 0600 /opt/agent-board/deploy/restic.example.env /etc/agent-board/restic.env
# edit restic.env; write a long random password to /etc/agent-board/restic.password (0600)
set -a; . /etc/agent-board/restic.env; set +a; restic init
cp /opt/agent-board/deploy/agent-board-backup.* /etc/systemd/system/
systemctl daemon-reload && systemctl enable --now agent-board-backup.timer
```

The timer runs every 15 minutes and thins snapshots once a day (2 days of all
snapshots, 30 daily, 26 weekly). The unit names are the ones in `deploy/`; a host set up
by other tooling may run its own backup unit under another name, so check with
`systemctl list-timers 'agent-board*'` on the host.

Restore drill, at least after setup and after each upgrade:

```sh
restic restore latest --tag agent-board --target /tmp/board-restore
docker run --rm --user 0 -v /tmp/board-restore:/r agent-board:local \
    admin backup --db /r/var/lib/agent-board/backup/board.db --out /r/check.db
```

A clean copy proves the backup opens and migrates. To restore for real, stop the
container, move the restored file to `/var/lib/agent-board/board.db` (owner 10001), and
start it again.

## 5. Upgrade

Run `deploy/upgrade.sh` from a clone of this repository on a device that reaches every
host over SSH. It moves the server and every home device to one pinned revision: a full
commit SHA on `main`, or a `v*` release tag whose commit is on `main`. Both are checked
against the repository the hosts install from (`BOARD_GIT_URL`, default this one on
GitHub), not against the clone's own remotes.

The server runs the image CI publishes for each push to `main`:
`ghcr.io/syz51/agent-board:sha-<commit>` (linux/amd64, built on a GitHub-hosted runner,
with build provenance). The script checks that provenance on the device it runs on with
`gh attestation verify`: the image must come from this repository's `ci.yml`, from that
commit on `main`, on a GitHub-hosted runner. The server then pulls the image by the
verified digest, so it runs exactly what was checked. The device needs `gh` (logged in);
the server needs to reach `ghcr.io`. A commit whose CI run was cancelled or has not
finished has no image yet: wait for the `image` job, upgrade to a later commit, or pass
`--build`, which builds the image on the server as before (minutes of full CPU on a small
host). `BOARD_IMAGE` (default `ghcr.io/syz51/agent-board`) names another image for a fork.

```sh
deploy/upgrade.sh 0123456789abcdef0123456789abcdef01234567          # print the plan only
deploy/upgrade.sh --yes 0123456789abcdef0123456789abcdef01234567    # apply it
```

The hosts come from `~/.config/agent-board/upgrade.conf` (or `--config FILE`), which stays
outside the repository:

```text
# server <ssh-target> [clone=/opt/agent-board] [db=/var/lib/agent-board/board.db] [port=7377] [identity=FILE]
server root@board-host
# device local | <ssh-target> [identity=FILE]
device local
device user@laptop identity=~/.ssh/id_ed25519_board
```

`db` is the server's database (`db` in `server.toml`); it must sit in the directory
`compose.yaml` mounts, which has the same path inside the container.

Without `--yes` the script prints each host's current and new revision and changes
nothing. With `--yes` it:

1. Takes a lock in the server's clone and holds it until the last device is done, so a
   second run waits instead of racing this one. A lock over two hours old is from a run
   that died, and is replaced.
2. Refuses a revision older than any host's current one, so a late run cannot install an
   older build. `--rollback` allows it on purpose.
3. On the server, checks out the revision (a detached `HEAD`) and pulls the verified image
   (or builds it with `--build`) while the old server keeps running. A failed pull or build
   puts the clone back.
4. Writes a fresh backup to `pre-upgrade-<UTC time>.db` beside the database with `board
   admin backup` in the running container, and checks it is not empty. It keeps the five
   newest of these copies; the backup timer keeps the history.
5. Starts the new container with `BOARD_COMMIT=<commit>` and waits until `/healthz`
   reports that commit.
6. On each device, runs `cargo install --git ... --rev <sha> --locked --force board` unless
   it is already installed, restarts `board watch` (launchd or the systemd user unit), and
   checks `cargo install --list`.
7. Prints each device's revision, the previous ones, and the rollback command.

It stops at the first failed step and says what to do next. A failed server step leaves
every device unchanged. After any failure, fix the cause and run the same command again:
the server is skipped once it runs the revision, devices already at it only restart `board
watch`, and the rollback command still names the revision from before the first run.
Rolling back moves the code only; migrations do not run backwards, so a rollback across a
migration needs the pre-upgrade backup restored as in section 4.

The backup runs in the server's container, so a server whose container is not running
cannot be upgraded this way: stop it, copy the database, and run the section 2 commands
by hand.

SSH runs with `BatchMode=yes`, `StrictHostKeyChecking=yes`, keepalives and no agent or
port forwarding, so each host must already be in `known_hosts` and reachable with a key.
The server target must run `docker compose`, own the clone, and read and write the data
directory (mode 0700, owned by the container's user), so it is `root`. Devices need
`cargo` (looked up in `~/.cargo/bin` too) and the `board watch` service from `board
install launchd` or `board install systemd`; a service that runs another `board` binary
than cargo's is not upgraded by this.
