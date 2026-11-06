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
docker compose -f /opt/agent-board/deploy/compose.yaml up -d --build
tailscale serve --bg --https=443 http://127.0.0.1:7377
curl -fsS https://board-host.your-tailnet.ts.net/healthz
```

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

Upgrade: `git -C /opt/agent-board pull` and run the `docker compose ... up -d --build`
line again. Migrations run on start. Since pairing came in, `BOARD_TELEGRAM_TOKEN` alone
starts the notifier, without a `[telegram]` table; it sends nothing until a chat is paired
or configured.

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
snapshots, 30 daily, 26 weekly).

Restore drill, at least after setup and after each upgrade:

```sh
restic restore latest --tag agent-board --target /tmp/board-restore
docker run --rm --user 0 -v /tmp/board-restore:/r agent-board:local \
    admin backup --db /r/var/lib/agent-board/backup/board.db --out /r/check.db
```

A clean copy proves the backup opens and migrates. To restore for real, stop the
container, move the restored file to `/var/lib/agent-board/board.db` (owner 10001), and
start it again.
