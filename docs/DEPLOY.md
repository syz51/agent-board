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
BOARD_BUILT=$(git -C /opt/agent-board log -1 --format=%ct HEAD) \
    docker compose -f /opt/agent-board/deploy/compose.yaml up -d --build
tailscale serve --bg --https=443 http://127.0.0.1:7377
curl -fsS https://board-host.your-tailnet.ts.net/healthz
```

The image has no `.git`, so the build reads the commit from `BOARD_COMMIT` and its order
stamp, the commit's committer time, from `BOARD_BUILT`. Verify the
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

Where another tool manages the settings file (it is tracked in a dotfiles repository, or is
a link into a git checkout), do not use `--write` there: it changes a tracked file. Run
`board install claude --check ~/.claude/settings.json` instead. It writes nothing, lists
each hook the file lacks (`missing:`) or holds twice (`runs more than once:`), and exits 1
when it lists any. Make the listed changes in the tool that owns the file. Such a file may
run board through a wrapper script, as `/bin/sh "$HOME/.claude/hooks/board-hook.sh" stop
claude`. An entry counts for a hook when all of these hold:

- Its command names a program or script whose file name holds `board` as a word of its own
  (`board-hook.sh`; not `dashboard.sh`, and not a script in a folder `agent-board/`).
- Its command holds the event as `board hook` takes it (`stop`, `pre-tool-use`), as a word
  of its own. A wrapper that takes no event argument does not count.
- It is under the same event, with the same matcher text or a matcher for every value (none,
  `""` or `*`). Whether another pattern, such as `Bash|Edit`, covers the hook's matcher is
  not worked out: such an entry does not count, so `--check` lists the hook as missing and
  `--write` would add a second entry.

`--write` keeps such an entry, adds no second one beside it, and removes a direct
`board hook` entry that repeats it. `--check` compares events and matchers only, not the
binary's path or the timeouts. `board install codex` takes `--check` the same way.

On macOS the token goes to the login Keychain. Where the Keychain cannot be used without
a desktop session (a login over SSH, or a locked Keychain), `board login` stores it in
`$BOARD_HOME/token` (mode 0600) instead and prints that path; `board` reads it from there.
Each `$BOARD_HOME` keeps its own token, so a second login with another `BOARD_HOME` (for
the `--user` token you answer asks with) does not replace the device's token.

While `board watch` runs, hooks reach the server through it, on the socket
`$BOARD_HOME/run/hook.sock`, and skip the TLS handshake and the token lookup. It follows a
new `board login` by itself. After upgrading `board`, restart it: until then hooks pass the
older daemon by and call the server directly, as they do without one.

### Dispatch and the feedback board

A board with autopilot on is dispatched by `board watch` on one of its home devices: it
starts worker and judge threads in a T3 project on that device. `board watch` finds the project in the device's
`$BOARD_HOME/config.toml`: `[projects]` maps a board repo to its folder,
`[board_projects]` maps a board slug to a folder for a board without a mapped repo, and
`[project_base_branches]` optionally overrides the base branch by repo key (or board slug)
for fork repositories whose `origin/HEAD` tracks another branch.
A running `board watch` re-reads the file within a minute of an edit and applies these
tables, `feedback_board`, `runs_at_once` and `reserve_runs` without a restart; a change to `server`,
`device`, `t3_bin`, `[thread_model]`, `[judge_model]` or `[judge_models]` needs one, and the daemon's log says so.

`runs_at_once` is how many agent turns the device runs at once. Without the setting it is
one per 4 GiB of host memory, at least 2. `reserve_runs` (alias `reserve`, default `1`) is
how many of those turns `board watch` leaves free for threads it did not start, such as an
owner's thread calling `delegate_task` or `t3_thread_launch`; set `reserve_runs = 0` to let
the daemon fill every slot. Every thread of the configured T3 projects whose
run has not ended counts, whoever started it: you, a coordinator, a delegate or the daemon.
At its limit (`runs_at_once - reserve_runs`) the daemon starts no worker and no judge on any of its boards, and at `runs_at_once` the
`PreToolUse` hook refuses the threads agents start through T3 (`delegate_task`,
`t3_thread_launch`, `create_threads`); a turn you start yourself always runs. The hook
checks on every logged-in device, also one without daemon-dispatch boards. Every hold and
refusal is recorded: holds in `state/device-holds.jsonl`, refused calls in
`state/refused-launches/`, both under `$BOARD_HOME`; the note on the task names the file.
When T3 cannot list the threads of a configured project (T3 is down or answers with an
error), the device's load is unknown: the daemon starts no
worker and no judge on any board until T3 lists every project, and the hold's line names
the projects and their errors (`unread`). So a daemon that starts nothing with free slots
has a T3 that is down or that fails for one entry: start T3, or take the entry out. The hook
lets a launch run in that case and keeps each one in `state/unchecked-launches.jsonl`. An
entry whose folder T3 has no project for is no such failure: T3 answered, so the entry
counts as no threads, and `watch.log` names it once. After
an upgrade that adds a hook, run `board install claude --write ~/.claude/settings.json`
again so that Claude Code has it; on a device whose settings file another tool manages, run
`board install claude --check ~/.claude/settings.json` and add what it lists there
(section 3, Devices). Each run is an agent session of some hundred MiB, and
its builds take more, so lower the limit on a device you also work on:

```toml
# config.toml, above the first table
runs_at_once = 3
```

`board watch` starts its threads (workers and judges) with the project's
default provider and model, which the user's own chats use too. To give them settings of
their own, add a provider instance in T3 Code and name it in `config.toml`. This example
keeps the claude.ai connectors (such as Gmail and Docs) out of agent threads, where their
tool lists cost tokens at each reconnect, and leaves them in the user's chats:

1. In T3 Code's `settings.json`, add an instance of the Claude driver under
   `providerInstances`:

   ```json
   "claude-agents": {
     "driver": "claudeAgent",
     "displayName": "Claude (agents)",
     "environment": [
       { "name": "ENABLE_CLAUDEAI_MCP_SERVERS", "value": "false", "sensitive": false }
     ]
   }
   ```

   Without `options` in `config.toml`, T3 Code starts these threads at the model's default
   options rather than the project's default selection; set model options (such as `effort`
   or `thought_level`) per role in step 3.
2. T3 Code applies the change within seconds, without a restart. Check that the instance
   shows in its provider list as signed in (`~/.t3/caches/claude-agents.json` has
   `"status": "ready"`).
3. Name the instance and a model of it, then restart `board watch`:

   ```toml
   # config.toml
   [thread_model]
   provider = "claude-agents"
   model = "<model slug>"
   # optional: forwarded as repeated `--option <key>=<value>` flags
   options = { effort = "high" }

   # optional: override model/options for judge threads (review, verdict, triage, goal);
   # falls back to [thread_model], then project defaults when unset
   [judge_model]
   provider = "claude-agents"
   model = "<model slug>"
   options = { effort = "medium" }

   # optional: per-kind judge overrides (`review`, `verdict`, `triage`, `goal`);
   # each unset kind falls back to [judge_model], then [thread_model], then project defaults
   [judge_models.review]
   provider = "claude-agents"
   model = "<model slug>"
   options = { effort = "high" }
   ```

Both `provider` and `model` are required on `[thread_model]`, `[judge_model]`, and each `[judge_models.<kind>]`: T3 Code takes a provider only with a model, so these threads use this model and not the project's default. Optional `options` entries (inline table or `<table_name>.options`) are validated at load time (non-empty keys without `=`, non-empty values) and passed in key order as `--option <key>=<value>`. Threads started before the change keep their provider; each thread the daemon starts from then on uses this instance. T3 Code keeps each instance's usage readings in its own file, and the daemon reads the `[thread_model]` instance's file (else `[judge_model]`, else the first `[judge_models.<kind>]`) next to the default instance's.

The feedback board (`board-feedback` unless `feedback_board` says otherwise) has no repo,
and a triage judge handles its items: it resolves each feedback item as a duplicate, routes it to a
task on the owner's board, forwards it to the setup's own channel, declines it with a
reason, or marks it fixed when the cause is already gone (`prompts/judges/triage.md`). To give it a device, pick a home device and a T3 project folder on
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

The daemon on that device starts a triage judge within a minute of a feedback item
becoming ready. Use the same `feedback_board` on every device, so the feedback agents
file and the board the judges triage are one.

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

The server runs the image CI publishes for each push to `main`, once every other check
passes: `ghcr.io/syz51/agent-board:sha-<commit>` (built on a GitHub-hosted runner, smoke
tested, with build provenance). When a run with `--yes` upgrades the server, the script
checks that provenance on the device it runs on with `gh attestation verify`: the image
must come from this repository's `ci.yml`, from that commit on `main`, on a GitHub-hosted
runner. The server then pulls the image by the verified digest, so it runs exactly what
was checked. The device needs `gh`, logged in (`gh auth login`); the server needs to reach
`ghcr.io`. A plan without `--yes`, and a rerun that only finishes the devices, do not call
`gh`. `BOARD_IMAGE` (default `ghcr.io/syz51/agent-board`) names another image for a fork.

`--build` builds the image on the server instead, as before (minutes of full CPU on a
small host). Pass it when there is no image to pull:

- The server is not linux/amd64. CI publishes that platform only.
- The commit's CI run was cancelled or has not finished: wait for the `image` job, upgrade
  to a later commit, or build.
- The commit was never the head of a push to `main`: a commit in the middle of a pushed
  range, a `v*` tag on such a commit, or a commit from before CI published images.

The rollback command the script prints passes `--build`, so it works for any earlier
commit.

```sh
deploy/upgrade.sh 0123456789abcdef0123456789abcdef01234567          # print the plan only
deploy/upgrade.sh --yes 0123456789abcdef0123456789abcdef01234567    # apply it
```

The hosts come from `~/.config/agent-board/upgrade.conf` (or `--config FILE`), which stays
outside the repository:

```text
# server <ssh-target> [clone=/opt/agent-board] [db=/var/lib/agent-board/board.db] [port=7377] [identity=FILE]
server root@board-host
# device local | <ssh-target> [identity=FILE] [watch=no] [install=cargo|image] [bin=/usr/local/bin/board]
device local
device user@laptop identity=~/.ssh/id_ed25519_board
device user@spare watch=no
device user@linux-telemetry watch=no install=image
```

`watch=no` marks a device that runs no `board watch` service and only needs a current
`board` CLI: the upgrade installs and signs `board` there but restarts nothing.
`install=image` copies `/usr/local/bin/board` out of the server's `agent-board:local` image
onto Linux hosts that run `board report-host` without Rust/`cargo` (installing atomically to
`bin`, default `/usr/local/bin/board`, using `sudo -n` if the target directory is not
writable by the SSH user).

`db` is the server's database (`db` in `server.toml`); it must sit in the directory
`compose.yaml` mounts, which has the same path inside the container.

Without `--yes` the script prints each host's current and new revision and changes
nothing. With `--yes` it:

1. Takes a lock in the server's clone and holds it until the last device is done, so a
   second run waits instead of racing this one. A lock over two hours old is from a run
   that died, and is replaced.
2. Refuses a revision older than any host's current one, so a late run cannot install an
   older build. `--rollback` allows it on purpose.
3. Verifies the image's provenance, unless the server already runs the revision or
   `--build` is given.
4. On the server, checks out the revision (a detached `HEAD`) and pulls the verified image
   and tags it `agent-board:local` (or builds it with `--build`) while the old server keeps
   running. A failed pull or build puts the clone and the tag back.
5. Writes a fresh backup to `pre-upgrade-<UTC time>.db` beside the database with `board
   admin backup` in the running container, and checks it is not empty. It keeps the five
   newest of these copies; the backup timer keeps the history. A failed backup puts the
   clone and the tag back.
6. Starts the new container with `BOARD_COMMIT=<commit>` and waits until `/healthz`
   reports that commit. Then it removes the image the old container ran; it removes no
   other image.
7. On each device, runs `cargo install --git ... --rev <sha> --locked --force board` unless
   it is already installed, signs it on macOS (see below), restarts `board watch` (launchd
   or the systemd user unit) unless the device has `watch=no`, and checks `cargo install
   --list`.
8. Prints each device's revision, the previous ones, and the rollback command.

It stops at the first failed step and says what to do next. A failed server step leaves
every device unchanged. After any failure, fix the cause and run the same command again:
the server is skipped once it runs the revision, devices already at it only restart `board
watch`, and the rollback command still names the revision from before the first run.
Rolling back moves the code only; migrations do not run backwards, so a rollback across a
migration needs the pre-upgrade backup restored as in section 4.

A device that ssh cannot connect to, such as a laptop that is asleep, stops even the plan.
With `--skip-unreachable`, the script leaves out each such device and upgrades the other
hosts. It names the devices it left out, prints the command to run once they answer, and
exits 3, not 0 or the failure code 1. It leaves out only a device where ssh itself fails
(exit 255, such as a timeout or a refused key); a device whose step fails still stops the
run.

On macOS, `cargo install` leaves an ad-hoc signature, which macOS knows only by the
binary's hash. Each build then loses the privacy grants of the last one, such as access to
`~/Documents`, and `board watch` cannot start a thread for a project there: git waits
on the new prompt until it times out. So step 7 signs `~/.cargo/bin/board` with a
self-signed identity that stays the same across builds, as `dev.agent-board.board`. The
first run makes the identity in its own keychain,
`~/Library/Application Support/agent-board/signing/signing.keychain-db`, unlocked with a
random password in the 0600 file `password` beside it, and locked again however the step
ends. It signs `bin/board` under `CARGO_INSTALL_ROOT`, else `CARGO_HOME`, else `~/.cargo`;
cargo's `install.root` setting is not followed, so set the variable instead.
`codesign -d -r- ~/.cargo/bin/board` then shows `certificate leaf`. macOS asks once more
for the first signed build; allow it, or turn on `board` under System Settings > Privacy
& Security > Files and Folders. A failed signing step only prints a warning that says
which part failed. The keychain and its password are made as `making.keychain-db` and
`making.password` and moved into place when complete, so a run cut short leaves no
keychain in place; the next run removes what it left and makes both again. A keychain that
holds the certificate is never replaced, since a new key loses the grants: when its
password file is gone, it does not unlock, `security` cannot search it, or codesign
cannot use it, the warning says so and you fix or remove it. Only
this step signs: after installing `board` by hand with `cargo install`, run
`./deploy/upgrade.sh --yes <rev>` with the installed revision, which signs it again
without rebuilding. Any process of the same user can read that key and sign code that
macOS then trusts with board's grants. To stop signing, run `security delete-keychain`
on that keychain and delete the folder; the next upgrade makes a new identity, so remove
the step from `deploy/upgrade-steps.sh` as well to keep the ad-hoc signature.

The backup runs in the server's container, so a server whose container is not running
cannot be upgraded this way: stop it, copy the database, and run the section 2 commands
by hand.

SSH runs with `BatchMode=yes`, `StrictHostKeyChecking=yes`, keepalives and no agent or
port forwarding, so each host must already be in `known_hosts` and reachable with a key.
The server target must run `docker compose`, own the clone, and read and write the data
directory (mode 0700, owned by the container's user), so it is `root`. Devices need
`cargo` (looked up in `~/.cargo/bin` too) and, unless they have `watch=no`, the `board
watch` service from `board install launchd` or `board install systemd`; a service that runs another `board` binary
than cargo's is not upgraded by this.

## 6. Fleet Isolation, Multi-Mac Pull Pool, and Linux Headless Runners

In a multi-machine fleet (`docs/SCHEDULER_DESIGN.md`, Sections 2–3), keep infrastructure hosts
isolated from coding agent builds by assigning each registered device to one of three planes
and enforcing Kubernetes-style taints with a `--user` token. Values below are placeholders.

### Multi-Mac shared pull pool onboarding

When the server supports `dispatch-claim-v1`, every awake `board watch` daemon pulls
capability-matched work atomically via `POST /v1/dispatch/claim` (`dispatch_mode = "auto"`,
the default) instead of pinning entire boards behind 45-minute coordinator leases. Multiple
Macs (for example a heavy `mac-pro` and a lighter `mac-air`) share the same boards with zero
manual board splitting: if one Mac fills its `weight_units` budget or sleeps, the other Mac
immediately pulls remaining work and recovers orphaned runs within 3 minutes.

1. **Create a device token on `control-host` for the secondary Mac:**

   ```sh
   docker compose -f /opt/agent-board/deploy/compose.yaml exec board \
       board admin device mac-air --db /var/lib/agent-board/board.db
   ```

2. **Install `board`, log in, and clone the shared repositories on `mac-air`:**

   ```sh
   cargo install --git https://github.com/syz51/agent-board --locked board
   board login --server https://board-host.your-tailnet.ts.net --device mac-air
   git clone https://github.com/org/repo ~/src/repo
   ```

3. **Configure `$BOARD_HOME/config.toml` on each Mac** (`mac-pro` with larger `weight_units`,
   `mac-air` with smaller `weight_units` and reserve for interactive work):

   ```toml
   # mac-pro: $BOARD_HOME/config.toml
   server = "https://board-host.your-tailnet.ts.net"
   device = "mac-pro"
   dispatch_mode = "auto"     # "auto" (default: pull when server supports dispatch-claim-v1), "pull", or "legacy"
   weight_units = 12          # light=1, standard=2, heavy=4
   reserve_runs = 1           # reserves 2 weight units for interactive turns
   capabilities = ["tool:xcode", "git:cloneable"]
   # Optional: when omitted, board watch auto-advertises configured runners ("t3", plus "acp"
   # when `[acp_runner]` is configured) alongside built-in "deterministic". Only list runners
   # actually configured on this host.
   # runners = ["acp", "t3", "deterministic"]

   [projects]
   "github.com/org/repo" = "/Users/you/src/repo"
   ```

   ```toml
   # mac-air: $BOARD_HOME/config.toml
   server = "https://board-host.your-tailnet.ts.net"
   device = "mac-air"
   dispatch_mode = "auto"
   weight_units = 4
   reserve_runs = 1
   capabilities = ["git:cloneable"]
   # Omit `runners` to auto-advertise configured runners + "deterministic".

   [projects]
   "github.com/org/repo" = "/Users/you/src/repo"
   ```

   Every daemon always executes `merge` and `housekeeping` runs natively in Rust via
   `DeterministicRunner` (zero LLM tokens, respecting `repo:<repo>:merge` max 1).

4. **Start `board watch` under launchd on each Mac:**

   ```sh
   board install launchd --write
   ```

5. **Set fleet preferences and soft taints with a `--user` token** so heavy builds prefer
   `mac-pro` while spilling over to `mac-air` only when `mac-pro` is full or asleep:

   ```sh
   board admin device-update mac-pro \
       --role runner \
       --preference 10 \
       --clear-taints

   board admin device-update mac-air \
       --role runner \
       --preference 0 \
       --taint weight=heavy:prefer_no_schedule

   # Verify both Macs are online with live weight_units, capabilities, runners, and repos
   board devices
   ```

### Machine-local custody capability routing

In a multi-runner fleet, `board.home_devices` is only a soft scoring preference in
`POST /v1/dispatch/claim`; a higher-preference runner with more free units can still pull work
unless the task carries a hard capability predicate in `requires`. When a task needs
machine-local credentials or key custody (for example a local operator PKI or bootstrap store
present only on `mac-air`), declare a custody capability in that host's `$BOARD_HOME/config.toml`
and pin tasks with `--requires`:

1. **Advertise the custody capability only on devices that hold the local custody:**

   ```toml
   # mac-air (holds local operator secrets): $BOARD_HOME/config.toml
   capabilities = ["git:cloneable", "custody:vps-operator"]
   ```

   Leave `custody:vps-operator` out of `capabilities` on other runners (such as `mac-pro`), then
   verify with `board devices` that only the custody host advertises it.

2. **Pin custody-bound tasks using `board` CLI or MCP:**

   ```sh
   # Create a task pinned to runners advertising `custody:vps-operator` (repeat `--requires` for more)
   board add "Rotate cluster certificates" --board ops --requires custody:vps-operator

   # Update or clear capability requirements on an existing task
   board update ops-12 --requires custody:vps-operator --requires os:macos
   board update ops-12 --clear-requires    # or: board update ops-12 --requires ""

   # Inspect the task (non-empty `requires` prints on the task line)
   board show ops-12
   ```

   Agents using MCP can set `requires: ["custody:vps-operator"]` on `task_add` or `task_update`
   (and pass `requires: []` to `task_update` to clear it). `POST /v1/dispatch/claim` holds the
   task with reason `requires` for any runner that does not advertise every required capability.

### 3-plane fleet isolation and taints

1. **Control & Trust Plane** (`role=control`, taint `role=control:no_schedule`): hosts running
   `board-server`, primary SQLite backups, or a secrets authority. Never run untrusted agent
   builds here.
2. **Elastic Runner Plane** (`role=runner`, untainted or soft-tainted): laptops and dedicated
   disposable Linux runner nodes executing agent builds via `AcpRunner` or `T3Runner`.
3. **Service Hosting Plane** (`role=service_host`, taint `role=service-host:no_schedule`):
   hosts running deployed applications. Updated via CI/CD deploys, never targeted for builds.

Apply roles and taints from a workstation logged in with a `--user` token (`$BOARD_USER_HOME`
or `BOARD_HOME` holding the `--user` token):

```sh
# 1. Control & Trust Plane: hard NoSchedule isolation
board admin device-update control-host \
    --role control \
    --taint role=control:no_schedule

board admin device-update trust-host \
    --role control \
    --taint role=control:no_schedule

# 2. Service Hosting Plane: hard NoSchedule isolation
board admin device-update service-host-1 \
    --role service_host \
    --taint role=service-host:no_schedule

# 3. Elastic Runner Plane: untainted Linux runner + soft-tainted light laptop
board admin device-update runner-linux-1 \
    --role runner \
    --preference 10 \
    --clear-taints

board admin device-update laptop-light \
    --role runner \
    --preference 0 \
    --taint weight=heavy:prefer_no_schedule

# Verify the fleet inventory, roles, taints, and live heartbeats
board devices
```

A device token can update only its own live heartbeat (`PUT /v1/devices/{name}/heartbeat`) and
cannot clear taints or change its `role`; only a `--user` token can call `board admin
device-update`.

### Linux Runner Plane node setup (`AcpRunner` + systemd cgroup limits)

Dedicated Linux runner nodes run headless under `board watch` using `AcpRunner` against an
**official, unmodified** vendor binary speaking standard ACP JSON-RPC over stdio—no GUI, no
Electron shell, and no binary patching.

1. **Install `board` and log the runner in:**

   ```sh
   # On the server host (once per runner node):
   docker compose -f /opt/agent-board/deploy/compose.yaml exec board \
       board admin device runner-linux-1 --db /var/lib/agent-board/board.db

   # On runner-linux-1 (as a dedicated non-root runner user):
   cargo install --git https://github.com/syz51/agent-board --locked board
   board login --server https://board-host.your-tailnet.ts.net --device runner-linux-1
   ```

2. **Install and authenticate the official ACP binary** (for example the official Antigravity
   binary in sanctioned ACP mode, or any official ACP-compliant agent CLI) under the runner
   user, and verify it responds on your host before enabling autopilot.

3. **Configure `$BOARD_HOME/config.toml`** (default `~/.local/share/agent-board/config.toml`)
   with `[acp_runner]` and `[acp_runner.cgroup]`:

   ```toml
   server = "https://board-host.your-tailnet.ts.net"
   device = "runner-linux-1"
   runs_at_once = 4
   reserve_runs = 0
   weight_units = 8
   capabilities = ["os:linux", "arch:x86_64", "git:cloneable"]
   # Order controls default LLM runner selection when a task has no `preferred_runner`
   # (e.g. `["acp", "t3", "deterministic"]` prefers `acp`; `["t3", "acp", "deterministic"]`
   # prefers `t3`; omitting `runners` advertises registered runners with `acp` before `t3`).
   runners = ["acp", "deterministic"]

   [projects]
   "github.com/your-org/your-repo" = "/srv/repos/your-repo"

   [acp_runner]
   # Path to the official, unmodified ACP-compliant vendor binary.
   # Can also be overridden at runtime via BOARD_ACP_BIN.
   bin = "/usr/local/bin/official-acp-agent"
   args = ["acp"]
   model = "<model-slug>"
   turn_timeout_secs = 1800
   cancel_grace_secs = 15
   # Policy for incoming ACP `session/request_permission` requests: "allow" (default) or "deny".
   permission_policy = "allow"

   [acp_runner.options]
   thought_level = "high"

   # Optional extra environment variables passed to the child process.
   # Values are never logged or written to transcripts; keep config.toml mode 0600 if used.
   # [acp_runner.env]
   # EXAMPLE_FLAG = "1"

   # Linux systemd user-scope cgroup isolation: wraps each run in
   # `systemd-run --user --scope --quiet -p MemoryMax=... -p CPUQuota=... -p TasksMax=...`
   [acp_runner.cgroup]
   enabled = true
   memory_max = "4G"
   cpu_quota = "200%"
   tasks_max = 256
   slice = "agent-board-runs.slice"
   ```

   **Headless ACP permission handling (`permission_policy` vs `--dangerously-skip-permissions`):**
   - `permission_policy = "allow"` (default): `AcpRunner` handles incoming ACP
     `session/request_permission` requests during both the handshake and active turns by
     selecting the first `allow_always` option (falling back to the first `allow_once` option,
     or replying `cancelled` when no allow option is offered). Every decision (tool name/title,
     policy, chosen `optionId`/outcome) is recorded in `$BOARD_HOME/state/transcripts/<run-id>.jsonl`
     and the `RunEvent` stream per the Trail Rule. This lets default `args = ["acp"]` runs
     complete autonomously while keeping every permission approval auditable.
   - `permission_policy = "deny"`: selects the first `reject_once` option (falling back to
     `reject_always`, or `cancelled` when none is offered) and records every decision in the
     transcript and `RunEvent` stream.
   - `args = ["acp", "--dangerously-skip-permissions"]`: bypasses tool permission prompts inside
     the vendor ACP binary itself so the agent never emits `session/request_permission`. Prefer
     `args = ["acp"]` with `permission_policy = "allow"` when you want Trail Rule auditing of
     each permission decision; use `--dangerously-skip-permissions` only when a vendor binary
     requires it or you explicitly want vendor-side bypass.

   **Runner preference order (`runners`):**
   When `POST /v1/dispatch/claim` schedules an LLM run (`worker`, `review`, `verdict`, `triage`,
   `goal`) whose task has no `preferred_runner` (or whose `preferred_runner` is not advertised
   by the device), the server iterates `runners` in order and picks the first supported LLM
   runner (`"acp"` or `"t3"`). For example, set `runners = ["t3", "acp", "deterministic"]` on a
   workstation where you want T3 Code threads preferred over headless `AcpRunner` while keeping
   `acp` available for tasks that explicitly request it. When `runners` is omitted, `board watch`
   advertises all configured runners in default order (`"acp"` before `"t3"`, plus
   `"deterministic"`).

4. **Enable systemd user lingering, cgroup v2 delegation, and `board watch`:**

   ```sh
   # Allow the runner user's systemd --user instance to stay alive after SSH logout
   sudo loginctl enable-linger "$USER"

   # Optional: create a parent user slice to bound aggregate runner usage across all runs
   mkdir -p ~/.config/systemd/user
   cat <<'EOF' > ~/.config/systemd/user/agent-board-runs.slice
   [Unit]
   Description=Isolated slice for agent-board AcpRunner runs

   [Slice]
   MemoryHigh=12G
   MemoryMax=14G
   EOF

   # Verify user cgroup delegation works with systemd-run --user --scope
   systemd-run --user --scope --quiet \
       -p MemoryMax=512M -p CPUQuota=100% -p TasksMax=64 \
       -p Slice=agent-board-runs.slice -- true

   # Install and start the board watch user service
   board install systemd --write
   systemctl --user daemon-reload
   systemctl --user enable --now agent-board-watch.service
   systemctl --user status agent-board-watch.service
   ```

   If `systemd-run --user --scope` reports missing controllers (`cpu`, `memory`, or `pids`),
   enable user delegation in systemd (for example via `/etc/systemd/system/user@.service.d/delegate.conf`
   with `[Service]\nDelegate=cpu cpuset io memory pids`, followed by `systemctl daemon-reload`).
   Each run executes in its own process group inside the scope, writes its raw JSONL transcript
   to `$BOARD_HOME/state/transcripts/<run-id>.jsonl`, persists its `session_id` and `pid` in
   `$BOARD_HOME/state/daemon.db`, and tears down the entire process group on cancel or turn
   timeout after writing a Trail Rule entry to `$BOARD_HOME/state/runner-cancels.jsonl`.

## 7. Remote Streamable HTTP MCP gateway (`board mcp-http`)

To control your fleet from a remote MCP client (such as Gemini web/mobile Connected Apps)
without exposing the core Tailscale-only server, run `board mcp-http` as a separate sidecar
listening on loopback (`127.0.0.1:7378`). It talks to `http://127.0.0.1:7377/v1` with a
dedicated revocable `--user` device token and never sends or trusts `Tailscale-User-Login`.

1. Mint a dedicated `--user` device token and mark it as a `control` plane device so runner
   jobs are never scheduled on it:

   ```sh
   docker compose -f /opt/agent-board/deploy/compose.yaml exec board \
       board admin device mcp-http --user --db /var/lib/agent-board/board.db
   ```

2. Create `/etc/agent-board/mcp-http.toml` from `deploy/mcp-http.example.toml` and write
   `/etc/agent-board/mcp-secrets.env` (mode `0600`) with `BOARD_MCP_TOKEN` and a high-entropy
   `BOARD_MCP_PASSPHRASE` (at least 12 characters; optionally also `BOARD_MCP_STATIC_CLIENT_ID`
   and `BOARD_MCP_STATIC_CLIENT_SECRET` if a client uses static OAuth credentials instead of
   Dynamic Client Registration).

3. Start the sidecar using the `mcp-http` Compose profile:

   ```sh
   docker compose -f /opt/agent-board/deploy/compose.yaml --profile mcp-http up -d
   curl -fsS http://127.0.0.1:7378/healthz
   ```

4. Publish **only** these exact routes through your HTTPS reverse proxy (stripping any
   `Tailscale-User-Login` header and returning `404` for everything else):
   - `/.well-known/oauth-protected-resource` and `/.well-known/oauth-protected-resource/mcp`
   - `/.well-known/oauth-authorization-server`
   - `/oauth/register`, `/oauth/authorize`, `/oauth/token`
   - `/mcp`
   - `/healthz`

Never expose port `7377` directly through a public reverse proxy while `trust_identity_header`
is enabled on `board serve`.
