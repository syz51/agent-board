//! Tests for `deploy/upgrade.sh` against stub `ssh`, `gh`, `docker`, `curl`, `cargo` and
//! service managers. Git is real: an origin repository the hosts install from, the
//! operator's clone the script runs from, and the server's clone it moves.
#![cfg(unix)]

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const IMAGE: &str = "ghcr.io/syz51/agent-board";

/// The digest the stub `gh` verifies for a commit: its first 12 characters, padded. The
/// stub registry's image at a digest runs the commit in the digest's first 12 characters.
fn digest(sha: &str) -> String {
    format!("{}{}", &sha[..12], "0".repeat(52))
}

const STUBS: &[(&str, &str)] = &[
    // Answers `attestation verify` with the digest for the commit in the image tag,
    // STUB_DIGEST instead when set, or a failure.
    (
        "gh",
        r#"#!/bin/sh
case "$1 $2" in
"auth status")
    echo "gh auth status" >> "$STATE/log"
    [ -z "${STUB_GH_LOGGED_OUT:-}" ] || { echo "You are not logged into any GitHub hosts." >&2; exit 1; }
    [ -z "${STUB_GH_OFFLINE:-}" ] || { echo "error connecting to api.github.com" >&2; exit 1; }
    ;;
"attestation verify")
    echo "gh attestation verify $3" >> "$STATE/log"
    echo "$*" >> "$STATE/gh-args"
    [ -z "${STUB_GH_VERIFY_FAIL:-}" ] || { echo "no attestations found" >&2; exit 1; }
    if [ -n "${STUB_DIGEST+set}" ]; then
        printf '%s\n' "$STUB_DIGEST"
    else
        printf '%.12s%052d\n' "${3##*:sha-}" 0
    fi
    ;;
*) echo "unexpected gh $*" >&2; exit 9 ;;
esac
"#,
    ),
    // Logs the host and step, then runs the command locally, as the host would. The host
    // STUB_UNREACHABLE names fails as ssh does when it cannot connect.
    (
        "ssh",
        r#"#!/bin/sh
echo "ssh-args $*" >> "$STATE/ssh-args"
while [ "$1" != "--" ]; do shift; done
shift
host=$1
shift
if [ "$host" = "${STUB_UNREACHABLE:-}" ]; then
    echo "ssh: connect to host $host port 22: Operation timed out" >&2
    exit 255
fi
if [ $# -ge 4 ] && [ "$1" = "sh" ] && [ "$2" = "-s" ] && [ "$3" = "--" ]; then
    echo "$host $4" >> "$STATE/log"
else
    echo "$host raw" >> "$STATE/log"
fi
# Real OpenSSH joins remote arguments with spaces and runs them through the remote shell.
STUB_HOST=$host exec sh -c "$*"
"#,
    ),
    // $STATE/image holds the id agent-board:local names: built-<commit> after a build, or
    // the pulled reference after a tag. A container runs that image's commit.
    (
        "docker",
        r#"#!/bin/sh
case "$*" in
*"admin backup"*)
    [ -z "${STUB_FAIL_BACKUP:-}" ] || { echo "backup failed" >&2; exit 1; }
    while [ "$1" != "--out" ]; do shift; done
    echo db > "$2"
    echo "server backup" >> "$STATE/log"
    ;;
*" build board")
    [ -z "${STUB_FAIL_BUILD:-}" ] || { echo "build failed" >&2; exit 1; }
    echo "built-$BOARD_COMMIT" > "$STATE/image"
    echo "server build $BOARD_COMMIT" >> "$STATE/log"
    ;;
*" up -d --no-build board")
    [ -z "${STUB_FAIL_UP:-}" ] || { echo "start failed" >&2; exit 1; }
    id=$(cat "$STATE/image")
    case "$id" in
    built-*) echo "${id#built-}" > "$STATE/running" ;;
    *@sha256:*) printf '%.12s\n' "${id#*@sha256:}" > "$STATE/running" ;;
    esac
    echo "server up $(cat "$STATE/running")" >> "$STATE/log"
    ;;
*"--profile mcp-http ps -q board-mcp")
    [ -s "$STATE/mcp-running" ] && echo "mcp-cid-1" || true
    ;;
"inspect -f {{.State.Running}} mcp-cid-1")
    [ -s "$STATE/mcp-running" ] && echo "true" || echo "false"
    ;;
*"--profile mcp-http up -d --no-build board-mcp")
    [ -z "${STUB_FAIL_MCP_UP:-}" ] || { echo "mcp start failed" >&2; exit 1; }
    id=$(cat "$STATE/image")
    case "$id" in
    built-*) echo "${id#built-}" > "$STATE/mcp-running" ;;
    *@sha256:*) printf '%.12s\n' "${id#*@sha256:}" > "$STATE/mcp-running" ;;
    esac
    echo "server up-mcp $(cat "$STATE/mcp-running")" >> "$STATE/log"
    ;;
"pull --quiet "*)
    [ -z "${STUB_FAIL_PULL:-}" ] || { echo "pull failed" >&2; exit 1; }
    echo "$3" >> "$STATE/pulled"
    echo "server pull $3" >> "$STATE/log"
    ;;
"tag "*" agent-board:local")
    case "$2" in *@sha256:*)
        [ -z "${STUB_FAIL_TAG:-}" ] || { echo "tag failed" >&2; exit 1; }
        grep -qxF "$2" "$STATE/pulled" || { echo "no such image $2" >&2; exit 1; }
        ;;
    esac
    echo "$2" > "$STATE/image"
    echo "server tag $2" >> "$STATE/log"
    ;;
"image inspect -f {{.Id}} agent-board:local")
    [ -s "$STATE/image" ] || { echo "Error: No such image: agent-board:local" >&2; exit 1; }
    cat "$STATE/image"
    ;;
"image rm "*)
    echo "server rm $3" >> "$STATE/log"
    [ -z "${STUB_FAIL_RM:-}" ] || { echo "image is being used by a container" >&2; exit 1; }
    ;;
"create agent-board:local")
    echo "server create-extract" >> "$STATE/log"
    echo "extract-cid-1"
    ;;
"cp extract-cid-1:/usr/local/bin/board "*)
    id=$(cat "$STATE/image")
    case "$id" in
    built-*) commit="${id#built-}" ;;
    *@sha256:*) commit=$(printf '%.12s' "${id#*@sha256:}") ;;
    *) commit="unknown" ;;
    esac
    printf '#!/bin/sh\necho "board 0.1.0 (%s)"\n' "$commit" > "$3"
    chmod 0755 "$3"
    echo "server cp-bin $commit" >> "$STATE/log"
    ;;
"rm -f extract-cid-1")
    echo "server rm-extract" >> "$STATE/log"
    ;;
*) echo "unexpected docker $*" >&2; exit 9 ;;
esac
"#,
    ),
    (
        "curl",
        r#"#!/bin/sh
case "$*" in
*:7378/healthz*)
    printf '{"ok":true,"service":"agent-board-mcp-http","version":"0.1.0","commit":"%s"}' "$(cat "$STATE/mcp-running")"
    ;;
*--unix-socket*/relay*)
    f="$STATE/watch-running-${STUB_HOST:-local}"
    [ -s "$f" ] || exit 1
    printf '{"version":"0.1.0","commit":"%s","server":"https://board.example.test"}' "$(cat "$f")"
    ;;
*)
    printf '{"ok":true,"version":"0.1.0","commit":"%s","schema_version":11}' "$(cat "$STATE/running")"
    ;;
esac
"#,
    ),
    (
        "cargo",
        r#"#!/bin/sh
f="$STATE/cargo-${STUB_HOST:-local}"
case "$*" in
"install --list")
    [ -z "${STUB_FAIL_LIST:-}" ] || { echo "error: cannot read the install list" >&2; exit 101; }
    [ -s "$f" ] || exit 0
    sha=$(cat "$f")
    printf 'board v0.1.0 (https://github.com/syz51/agent-board?rev=%s#%.8s):\n    board\n' "$sha" "$sha"
    ;;
install*)
    while [ "$1" != "--rev" ]; do shift; done
    echo "$2" > "$f"
    echo "${STUB_HOST:-local} install $2 commit=$BOARD_COMMIT" >> "$STATE/log"
    ;;
*) echo "unexpected cargo $*" >&2; exit 9 ;;
esac
"#,
    ),
    (
        "launchctl",
        "#!/bin/sh\n[ -z \"${STUB_FAIL_RESTART:-}\" ] || exit 1\necho \"${STUB_HOST:-local} restart\" >> \"$STATE/log\"\n",
    ),
    (
        "systemctl",
        "#!/bin/sh\n[ -z \"${STUB_FAIL_RESTART:-}\" ] || exit 1\necho \"${STUB_HOST:-local} restart\" >> \"$STATE/log\"\n",
    ),
    // Linux, unless STUB_OS names another system, so a run on a Mac stays off its keychain.
    ("uname", "#!/bin/sh\necho \"${STUB_OS:-Linux}\"\n"),
    // A keychain is a file, empty until the identity is imported; one that holds only
    // `certificate` has a certificate but no identity. Unlocking, locking and deleting are
    // logged, and so is the name of the keychain whose partition list is set.
    (
        "security",
        r#"#!/bin/sh
case "$1" in
create-keychain) : > "$4" ;;
import) echo identity > "$4" ;;
set-key-partition-list)
    [ -z "${STUB_FAIL_PARTITION:-}" ] || { echo "security: SecItemCopyMatching: failed" >&2; exit 1; }
    for last; do :; done
    echo "${STUB_HOST:-local} partition-list ${last##*/}" >> "$STATE/log"
    ;;
unlock-keychain)
    [ -z "${STUB_FAIL_UNLOCK:-}" ] || { echo "security: The user name or passphrase you entered is not correct." >&2; exit 51; }
    for last; do :; done
    echo "${STUB_HOST:-local} unlock ${last##*/}" >> "$STATE/log"
    ;;
lock-keychain) echo "${STUB_HOST:-local} lock" >> "$STATE/log" ;;
delete-keychain)
    rm -f "$2"
    echo "${STUB_HOST:-local} delete-keychain" >> "$STATE/log"
    ;;
find-certificate)
    [ -z "${STUB_FAIL_FIND:-}" ] || { echo "security: SecKeychainSearchCopyNext: Internal error" >&2; exit 1; }
    [ -s "$4" ] || { echo "security: SecKeychainSearchCopyNext: The specified item could not be found in the keychain." >&2; exit 44; }
    ;;
find-identity)
    grep -qx identity "$4" || { echo '     0 valid identities found'; exit 0; }
    printf '  1) 0123ABCD "agent-board code signing"\n     1 valid identities found\n'
    ;;
esac
"#,
    ),
    // Logs `<host> sign <identity> <identifier> <path>`; `-d` shows the requirement of a
    // signed binary.
    (
        "codesign",
        r#"#!/bin/sh
case "$1" in
-d) echo 'designated => identifier "dev.agent-board.board" and certificate leaf = H"00"' ;;
*)
    [ -z "${STUB_FAIL_SIGN:-}" ] || { echo "codesign: no identity found" >&2; exit 1; }
    echo "${STUB_HOST:-local} sign $5 $7 $8" >> "$STATE/log"
    ;;
esac
"#,
    ),
];

struct Env {
    _dir: tempfile::TempDir,
    root: PathBuf,
    /// Commits on origin's main, oldest first.
    main: Vec<String>,
    /// A commit pushed to a side branch only.
    side: String,
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

impl Env {
    fn new() -> Env {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        for sub in ["bin", "state", "data", "home"] {
            fs::create_dir(root.join(sub)).unwrap();
        }
        git(&root, &["init", "-q", "--bare", "-b", "main", "origin.git"]);
        git(&root, &["clone", "-q", "origin.git", "work"]);
        let work = root.join("work");
        let mut main = Vec::new();
        for n in 0..2 {
            fs::write(work.join("file"), format!("{n}")).unwrap();
            git(&work, &["add", "file"]);
            git(&work, &["commit", "-q", "-m", &format!("c{n}")]);
            main.push(git(&work, &["rev-parse", "HEAD"]));
        }
        git(&work, &["push", "-q", "origin", "main"]);
        git(&work, &["switch", "-q", "-c", "side"]);
        fs::write(work.join("file"), "side").unwrap();
        git(&work, &["commit", "-q", "-am", "side"]);
        let side = git(&work, &["rev-parse", "HEAD"]);
        git(&work, &["push", "-q", "origin", "side"]);
        git(&work, &["switch", "-q", "main"]);

        // The scripts under test, beside the operator's clone as in the repository.
        let deploy = work.join("deploy");
        fs::create_dir(&deploy).unwrap();
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../deploy");
        for name in ["upgrade.sh", "upgrade-steps.sh"] {
            fs::copy(src.join(name), deploy.join(name)).unwrap();
        }
        git(&root, &["clone", "-q", "origin.git", "server"]);
        for (name, text) in STUBS {
            common::script::write_script(&root.join("bin").join(name), text);
        }
        fs::write(
            root.join("hosts.conf"),
            format!(
                "# hosts\nserver board-host clone={} db={} port=7377\n\ndevice local\ndevice laptop identity=~/.ssh/key\n",
                root.join("server").display(),
                root.join("data/board.db").display()
            ),
        )
        .unwrap();
        let env = Env {
            _dir: dir,
            root,
            main,
            side,
        };
        env.at(&env.main[0].clone());
        env
    }

    /// Puts the server and both devices at `sha`.
    fn at(&self, sha: &str) {
        git(
            &self.root.join("server"),
            &["checkout", "-q", "--detach", sha],
        );
        fs::write(self.state("running"), &sha[..12]).unwrap();
        fs::write(self.state("image"), format!("built-{}", &sha[..12])).unwrap();
        for host in ["local", "laptop"] {
            fs::write(self.state(&format!("cargo-{host}")), sha).unwrap();
        }
    }

    fn state(&self, name: &str) -> PathBuf {
        self.root.join("state").join(name)
    }

    fn read(&self, name: &str) -> String {
        fs::read_to_string(self.state(name)).unwrap_or_default()
    }

    fn clear_log(&self) {
        fs::write(self.state("log"), "").unwrap();
    }

    fn server_head(&self) -> String {
        git(&self.root.join("server"), &["rev-parse", "HEAD"])
    }

    fn lock(&self) -> PathBuf {
        self.root.join("server/.git/agent-board-upgrade.lock")
    }

    fn backups(&self) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(self.root.join("data"))
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    fn run(&self, args: &[&str], extra: &[(&str, &str)]) -> Output {
        let path = format!(
            "{}:{}",
            self.root.join("bin").display(),
            std::env::var("PATH").unwrap()
        );
        let mut cmd = Command::new("bash");
        cmd.arg(self.root.join("work/deploy/upgrade.sh"))
            .arg("--config")
            .arg(self.root.join("hosts.conf"))
            .args(args)
            .env("PATH", path)
            .env("HOME", self.root.join("home"))
            .env("STATE", self.root.join("state"))
            .env("BOARD_GIT_URL", self.root.join("origin.git"))
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1");
        for var in [
            "BOARD_HOME",
            "BOARD_IMAGE",
            "STUB_DIGEST",
            "STUB_GH_LOGGED_OUT",
            "STUB_GH_OFFLINE",
            "STUB_GH_VERIFY_FAIL",
            "STUB_FAIL_PULL",
            "STUB_FAIL_TAG",
            "STUB_FAIL_BUILD",
            "STUB_FAIL_BACKUP",
            "STUB_FAIL_UP",
            "STUB_FAIL_RM",
            "STUB_FAIL_LIST",
            "STUB_FAIL_RESTART",
            "STUB_UNREACHABLE",
            "STUB_OS",
            "STUB_FAIL_SIGN",
            "STUB_FAIL_UNLOCK",
            "STUB_FAIL_FIND",
            "CARGO_HOME",
            "CARGO_INSTALL_ROOT",
        ] {
            cmd.env_remove(var);
        }
        for (k, v) in extra {
            cmd.env(k, v);
        }
        cmd.output().unwrap()
    }
}

fn text(out: &Output) -> (String, String) {
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// Runs and expects failure; returns stderr.
fn fails(out: Output) -> String {
    let (stdout, err) = text(&out);
    assert!(!out.status.success(), "succeeded: {stdout}");
    err
}

/// Runs and expects success; returns stdout.
fn succeeds(out: Output) -> String {
    let (stdout, err) = text(&out);
    assert!(out.status.success(), "{stdout}\n{err}");
    stdout
}

#[test]
fn a_rev_that_is_not_a_full_sha_or_release_tag_is_refused_before_any_remote_command() {
    let env = Env::new();
    let upper = env.main[1].to_uppercase();
    let short = &env.main[1][..12];
    for rev in [
        "main",
        "HEAD",
        short,
        upper.as_str(),
        "v1",
        "v1.2",
        "1.2.3",
        "v1.2.3;id",
        "$(id)",
        "x v1.2.3",
    ] {
        let err = fails(env.run(&["--yes", rev], &[]));
        assert!(
            err.contains("not a full 40-character commit SHA"),
            "{rev}: {err}"
        );
    }
    assert_eq!(env.read("log"), "", "a refused rev reached a host");
}

#[test]
fn a_commit_off_main_a_missing_commit_and_an_unpushed_tag_are_refused() {
    let env = Env::new();
    let err = fails(env.run(&["--yes", &env.side], &[]));
    assert!(err.contains("is not on main of"), "{err}");

    let err = fails(env.run(&["--yes", &"a".repeat(40)], &[]));
    assert!(err.contains("is not a commit on main of"), "{err}");

    let err = fails(env.run(&["--yes", "v9.9.9"], &[]));
    assert!(err.contains("tag v9.9.9 does not exist"), "{err}");

    // A tag in the operator's clone that the hosts' repository does not have.
    git(&env.root.join("work"), &["tag", "v0.3.0", &env.main[1]]);
    let err = fails(env.run(&["--yes", "v0.3.0"], &[]));
    assert!(err.contains("tag v0.3.0 does not exist"), "{err}");
    assert_eq!(env.read("log"), "");
}

#[test]
fn a_release_tag_resolves_to_its_commit() {
    let env = Env::new();
    let work = env.root.join("work");
    git(
        &work,
        &["tag", "-a", "-m", "release", "v0.2.0", &env.main[1]],
    );
    git(&work, &["push", "-q", "origin", "v0.2.0"]);
    git(&work, &["tag", "-d", "v0.2.0"]);
    let stdout = succeeds(env.run(&["v0.2.0"], &[]));
    let new = &env.main[1];
    assert!(
        stdout.contains(&format!("upgrade to {} (v0.2.0, {new})", &new[..12])),
        "{stdout}"
    );
}

#[test]
fn without_yes_it_prints_the_plan_and_changes_nothing() {
    let env = Env::new();
    let (old, new) = (&env.main[0], &env.main[1]);
    let stdout = succeeds(env.run(&[new], &[]));
    let (o, n) = (&old[..12], &new[..12]);
    assert!(
        stdout.contains(&format!("server board-host: {o} (running {o}) -> {n}")),
        "{stdout}"
    );
    assert!(
        stdout.contains(&format!("device local: {o} -> {n}")),
        "{stdout}"
    );
    assert!(
        stdout.contains(&format!("device laptop: {o} -> {n}")),
        "{stdout}"
    );
    assert!(
        stdout.contains(&format!(
            "  image: {IMAGE}:sha-{new} (provenance checked when applied)"
        )),
        "{stdout}"
    );
    assert!(stdout.contains("plan only: nothing changed"), "{stdout}");

    // Only the read-only status steps ran, without the lock or gh.
    assert_eq!(
        env.read("log"),
        "board-host server-status\nlaptop device-status\n"
    );
    assert_eq!(&env.server_head(), old);
    assert_eq!(env.read("running"), o);
    assert_eq!(env.read("cargo-local").trim(), old);
    assert_eq!(env.read("cargo-laptop").trim(), old);
    assert!(env.backups().is_empty());

    env.clear_log();
    let stdout = succeeds(env.run(&["--build", new], &[]));
    assert!(
        stdout.contains("  image: built on the server (--build)"),
        "{stdout}"
    );
    assert!(!env.read("log").contains("gh "), "{}", env.read("log"));
}

#[test]
fn with_yes_it_verifies_pulls_backs_up_and_upgrades_the_server_then_every_device() {
    let env = Env::new();
    let (old, new) = (&env.main[0], &env.main[1]);
    let (o, pulled) = (&old[..12], format!("{IMAGE}@sha256:{}", digest(new)));
    // Six older copies: the run adds one and keeps the newest five.
    for day in 1..=6 {
        fs::write(
            env.root
                .join(format!("data/pre-upgrade-2026010{day}T000000Z.db")),
            "old",
        )
        .unwrap();
    }
    let stdout = succeeds(env.run(&["--yes", new], &[]));

    let n = &new[..12];
    let log = env.read("log");
    let steps: Vec<&str> = log.lines().collect();
    assert_eq!(
        steps,
        [
            "board-host server-lock".to_string(),
            "board-host server-status".to_string(),
            "laptop device-status".to_string(),
            "gh auth status".to_string(),
            format!("gh attestation verify oci://{IMAGE}:sha-{new}"),
            "board-host server-upgrade".to_string(),
            format!("server pull {pulled}"),
            format!("server tag {pulled}"),
            "server backup".to_string(),
            format!("server up {n}"),
            format!("server rm built-{o}"),
            format!("local install {new} commit={n}"),
            "local restart".to_string(),
            "laptop device-upgrade".to_string(),
            format!("laptop install {new} commit={n}"),
            "laptop restart".to_string(),
            "board-host server-unlock".to_string(),
        ]
    );
    assert_eq!(&env.server_head(), new);
    assert_eq!(env.read("image").trim(), pulled);
    assert!(!env.lock().exists(), "the lock was not released");
    let gh = env.read("gh-args");
    assert!(
        gh.contains(&format!(
            "--repo syz51/agent-board --signer-workflow syz51/agent-board/.github/workflows/ci.yml \
             --source-ref refs/heads/main --source-digest {new} --deny-self-hosted-runners"
        )),
        "{gh}"
    );
    assert!(
        stdout.contains(&format!("image: provenance verified for {pulled}")),
        "{stdout}"
    );
    assert!(
        stdout.contains(&format!("removed the previous image built-{o}")),
        "{stdout}"
    );
    assert!(stdout.contains(&format!("\"commit\":\"{n}\"")), "{stdout}");
    assert!(
        stdout.contains(&format!("  local: {new} (was {o})")),
        "{stdout}"
    );
    assert!(
        stdout.contains(&format!("  laptop: {new} (was ")),
        "{stdout}"
    );
    // The rollback builds, since an older commit may have no image to pull.
    let config = env.root.join("hosts.conf");
    assert!(
        stdout.contains(&format!(
            "--config {} --rollback --build --yes {old}",
            config.display()
        )),
        "{stdout}"
    );

    let backups = env.backups();
    assert_eq!(backups.len(), 5, "{backups:?}");
    assert!(
        backups[0].starts_with("pre-upgrade-20260103"),
        "{backups:?}"
    );
    assert!(
        !backups[4].starts_with("pre-upgrade-202601"),
        "the fresh backup was pruned: {backups:?}"
    );

    let ssh = env.read("ssh-args");
    for opt in [
        "BatchMode=yes",
        "StrictHostKeyChecking=yes",
        "ForwardAgent=no",
        "ClearAllForwardings=yes",
        "ServerAliveInterval=30",
    ] {
        assert!(ssh.lines().all(|l| l.contains(opt)), "{opt} missing: {ssh}");
    }
    let key = env.root.join("home/.ssh/key");
    assert!(
        ssh.contains(&format!(
            "-i {} -o IdentitiesOnly=yes -- laptop",
            key.display()
        )),
        "{ssh}"
    );

    // A second run finds every host at the rev: it installs nothing but restarts the
    // daemons, needs no gh, and still names the revision before the first run for a
    // rollback.
    env.clear_log();
    let stdout = succeeds(env.run(&["--yes", new], &[("STUB_GH_LOGGED_OUT", "1")]));
    let log = env.read("log");
    assert!(
        !log.contains("install") && !log.contains("server-upgrade") && !log.contains("gh "),
        "{log}"
    );
    assert!(
        log.contains("local restart") && log.contains("laptop restart"),
        "{log}"
    );
    assert!(!stdout.contains("image:"), "{stdout}");
    assert!(
        stdout.contains(&format!("--rollback --build --yes {old}")),
        "{stdout}"
    );
}

#[test]
fn with_build_it_builds_on_the_server_without_gh() {
    let env = Env::new();
    let (o, new) = (&env.main[0][..12], &env.main[1]);
    let n = &new[..12];
    let stdout = succeeds(env.run(&["--yes", "--build", new], &[("STUB_GH_LOGGED_OUT", "1")]));
    let log = env.read("log");
    assert!(!log.contains("gh ") && !log.contains("pull"), "{log}");
    assert!(
        log.contains(&format!(
            "server build {n}\nserver backup\nserver up {n}\nserver rm built-{o}\n"
        )),
        "{log}"
    );
    assert_eq!(env.read("image").trim(), format!("built-{n}"));
    assert_eq!(env.read("running").trim(), n);
    assert!(
        stdout.contains("  image: built on the server (--build)"),
        "{stdout}"
    );
}

#[test]
fn a_failed_provenance_check_stops_before_any_change() {
    let env = Env::new();
    let (old, new) = (&env.main[0], &env.main[1]);
    let two = format!("{}\n{}", digest(old), digest(new));
    for (var, value, want) in [
        (
            "STUB_GH_LOGGED_OUT",
            "1",
            "You are not logged into any GitHub hosts.",
        ),
        ("STUB_GH_OFFLINE", "1", "error connecting to api.github.com"),
        (
            "STUB_GH_VERIFY_FAIL",
            "1",
            "no image with verified provenance",
        ),
        (
            "STUB_DIGEST",
            "not-a-digest",
            "names 'not-a-digest', not one sha256 digest",
        ),
        ("STUB_DIGEST", two.as_str(), "not one sha256 digest"),
        ("STUB_DIGEST", "", "not one sha256 digest"),
    ] {
        env.clear_log();
        let err = fails(env.run(&["--yes", new], &[(var, value)]));
        assert!(err.contains(want), "{var}={value}: {err}");
        assert!(err.contains("--build"), "{var}={value}: {err}");
        let log = env.read("log");
        assert!(!log.contains("server-upgrade"), "{var}={value}: {log}");
        assert!(log.contains("server-unlock"), "{var}={value}: {log}");
        assert_eq!(&env.server_head(), old);
        assert_eq!(env.read("image").trim(), format!("built-{}", &old[..12]));
        assert_eq!(env.read("cargo-local").trim(), old);
        assert_eq!(env.read("cargo-laptop").trim(), old);
        assert!(env.backups().is_empty());
        assert!(!env.lock().exists(), "the lock was not released");
    }
    // A failed `gh auth status` names both causes and their fixes, whichever it was.
    for var in ["STUB_GH_LOGGED_OUT", "STUB_GH_OFFLINE"] {
        let err = fails(env.run(&["--yes", new], &[(var, "1")]));
        for want in [
            "gh auth status failed",
            "gh is not logged in: run gh auth login",
            "gh cannot reach GitHub",
            "pass --build",
            "nothing was changed",
        ] {
            assert!(err.contains(want), "{var}: {want}: {err}");
        }
    }
    // A verification failure names its other causes too.
    let err = fails(env.run(&["--yes", new], &[("STUB_GH_VERIFY_FAIL", "1")]));
    assert!(err.contains("no attestations found"), "{err}");
    assert!(err.contains("gh too old"), "{err}");
    assert!(err.contains("nothing was changed"), "{err}");
}

#[test]
fn board_image_must_name_a_ghcr_repository() {
    let env = Env::new();
    let new = &env.main[1];
    for bad in [
        "docker.io/syz51/agent-board",
        "ghcr.io/syz51",
        "ghcr.io/syz51/agent-board:latest",
        "ghcr.io/Syz51/agent-board",
        "ghcr.io/syz51/agent-board;id",
        "ghcr.io/a/b/c",
    ] {
        let err = fails(env.run(&[new], &[("BOARD_IMAGE", bad)]));
        assert!(err.contains("BOARD_IMAGE must be ghcr.io/"), "{bad}: {err}");
    }
    assert_eq!(env.read("log"), "", "a refused image reached a host");

    // A fork's image is verified against the fork's workflow.
    let fork = "ghcr.io/fork/agent-board";
    succeeds(env.run(&["--yes", new], &[("BOARD_IMAGE", fork)]));
    let gh = env.read("gh-args");
    assert!(
        gh.contains(&format!(
            "oci://{fork}:sha-{new} --repo fork/agent-board --signer-workflow fork/agent-board/.github/workflows/ci.yml"
        )),
        "{gh}"
    );
    let pulled = format!("{fork}@sha256:{}", digest(new));
    assert!(env.read("log").contains(&format!("server pull {pulled}")));
    assert_eq!(env.read("image").trim(), pulled);
}

#[test]
fn a_failed_pull_or_tag_puts_the_clone_and_the_tag_back() {
    for var in ["STUB_FAIL_PULL", "STUB_FAIL_TAG"] {
        let env = Env::new();
        let (old, new) = (&env.main[0], &env.main[1]);
        let o = &old[..12];
        let err = fails(env.run(&["--yes", new], &[(var, "1")]));
        assert!(
            err.contains("pulling ghcr.io/syz51/agent-board@sha256:"),
            "{var}: {err}"
        );
        assert!(err.contains("reach ghcr.io"), "{var}: {err}");
        assert!(err.contains("linux/amd64"), "{var}: {err}");
        assert!(err.contains("pass --build"), "{var}: {err}");
        assert!(err.contains("no device was changed"), "{var}: {err}");
        assert_eq!(&env.server_head(), old, "{var}");
        assert!(
            env.read("log").contains(&format!("server tag built-{o}")),
            "{var}: {}",
            env.read("log")
        );
        assert_eq!(env.read("image").trim(), format!("built-{o}"), "{var}");
        assert_eq!(env.read("running").trim(), o, "{var}");
        assert!(env.backups().is_empty(), "{var}");
        assert_eq!(env.read("cargo-local").trim(), old, "{var}");
        assert!(!env.lock().exists(), "{var}: the lock was not released");
    }
}

#[test]
fn a_failed_backup_puts_the_clone_and_the_tag_back() {
    for args in [&["--yes"][..], &["--yes", "--build"][..]] {
        let env = Env::new();
        let (old, new) = (&env.main[0], &env.main[1]);
        let o = &old[..12];
        let mut args = args.to_vec();
        args.push(new);
        let err = fails(env.run(&args, &[("STUB_FAIL_BACKUP", "1")]));
        assert!(err.contains("the backup to"), "{args:?}: {err}");
        assert!(err.contains("no device was changed"), "{args:?}: {err}");
        assert_eq!(&env.server_head(), old, "{args:?}");
        // The pull or build moved agent-board:local; the failure moved it back.
        let log = env.read("log");
        assert!(
            log.contains("server build") || log.contains("server pull"),
            "{args:?}: {log}"
        );
        assert!(
            log.ends_with(&format!("server tag built-{o}\nboard-host server-unlock\n")),
            "{args:?}: {log}"
        );
        assert_eq!(env.read("image").trim(), format!("built-{o}"), "{args:?}");
        assert_eq!(env.read("running").trim(), o, "{args:?}");
        assert_eq!(env.read("cargo-local").trim(), old, "{args:?}");
    }
}

#[test]
fn a_previous_image_that_cannot_be_removed_does_not_fail_the_upgrade() {
    let env = Env::new();
    let new = &env.main[1];
    let stdout = succeeds(env.run(&["--yes", new], &[("STUB_FAIL_RM", "1")]));
    assert!(
        env.read("log").contains("server rm built-"),
        "{}",
        env.read("log")
    );
    assert!(!stdout.contains("removed the previous image"), "{stdout}");
    assert_eq!(env.read("running").trim(), &new[..12]);
    assert_eq!(env.read("cargo-laptop").trim(), new);
}

#[test]
fn an_older_rev_is_refused_unless_rolling_back() {
    let env = Env::new();
    let (old, new) = (&env.main[0], &env.main[1]);
    env.at(new);
    let err = fails(env.run(&["--yes", old], &[]));
    assert!(
        err.contains("older than the revision on: server local laptop"),
        "{err}"
    );
    assert!(err.contains("--rollback"), "{err}");
    assert_eq!(&env.server_head(), new);
    assert!(!env.lock().exists(), "a refused run kept the lock");

    succeeds(env.run(&["--rollback", "--yes", old], &[]));
    assert_eq!(&env.server_head(), old);
    assert_eq!(env.read("cargo-laptop").trim(), old);
}

#[test]
fn a_failed_build_puts_the_clone_back_and_changes_no_device() {
    let env = Env::new();
    let (old, new) = (&env.main[0], &env.main[1]);
    let err = fails(env.run(&["--yes", "--build", new], &[("STUB_FAIL_BUILD", "1")]));
    assert!(err.contains("the image build failed"), "{err}");
    assert!(err.contains("no device was changed"), "{err}");
    assert!(
        err.contains(&format!("--rollback --build --yes {old}")),
        "{err}"
    );
    assert_eq!(&env.server_head(), old, "the clone was left at the new rev");
    assert_eq!(env.read("image").trim(), format!("built-{}", &old[..12]));
    assert!(env.backups().is_empty(), "a failed build took a backup");
    assert_eq!(env.read("cargo-local").trim(), old);
    assert_eq!(env.read("cargo-laptop").trim(), old);
    assert!(!env.lock().exists(), "the lock was not released");
}

#[test]
fn after_a_failed_start_the_rerun_still_rolls_back_to_the_old_rev() {
    let env = Env::new();
    let (old, new) = (&env.main[0], &env.main[1]);
    let err = fails(env.run(&["--yes", new], &[("STUB_FAIL_UP", "1")]));
    assert!(err.contains("the new container did not start"), "{err}");
    assert!(
        err.contains(&format!("--rollback --build --yes {old}")),
        "{err}"
    );
    assert_eq!(&env.server_head(), new);
    // The old image stays until a new container is healthy.
    assert!(
        !env.read("log").contains("server rm"),
        "{}",
        env.read("log")
    );

    // The clone is at the new rev now; the rollback target comes from the server's record.
    // agent-board:local already names the new image, so nothing is removed.
    env.clear_log();
    let stdout = succeeds(env.run(&["--yes", new], &[]));
    assert_eq!(env.read("running").trim(), &new[..12]);
    assert!(
        !env.read("log").contains("server rm"),
        "{}",
        env.read("log")
    );
    assert!(
        stdout.contains(&format!("--rollback --build --yes {old}")),
        "{stdout}"
    );
}

#[test]
fn a_device_whose_daemon_did_not_restart_is_restarted_on_the_rerun() {
    let env = Env::new();
    let new = &env.main[1];
    let err = fails(env.run(&["--yes", new], &[("STUB_FAIL_RESTART", "1")]));
    assert!(err.contains("board watch did not restart"), "{err}");
    assert!(err.contains("local stopped"), "{err}");
    assert_eq!(env.read("cargo-local").trim(), new);

    env.clear_log();
    succeeds(env.run(&["--yes", new], &[]));
    let log = env.read("log");
    assert!(log.contains("local restart"), "{log}");
    assert!(!log.contains("local install"), "{log}");
}

#[test]
fn on_macos_each_device_signs_board_with_one_lasting_identity_before_the_restart() {
    let env = Env::new();
    let new = &env.main[1];
    let stdout = succeeds(env.run(&["--yes", new], &[("STUB_OS", "Darwin")]));

    let log = env.read("log");
    let device: Vec<&str> = log
        .lines()
        .skip_while(|l| !l.starts_with("local install"))
        .take_while(|l| !l.contains("server-unlock"))
        .collect();
    // CARGO_HOME is unset, so board is in ~/.cargo/bin.
    let sign = format!(
        "sign 0123ABCD dev.agent-board.board {}",
        env.root.join("home/.cargo/bin/board").display()
    );
    assert_eq!(
        device,
        [
            format!("local install {new} commit={}", &new[..12]),
            "local delete-keychain".into(),
            // Made beside its place and moved there, then unlocked at its place.
            "local partition-list making.keychain-db".into(),
            "local unlock signing.keychain-db".into(),
            format!("local {sign}"),
            "local lock".into(),
            "local restart".into(),
            "laptop device-upgrade".into(),
            format!("laptop install {new} commit={}", &new[..12]),
            "laptop unlock signing.keychain-db".into(),
            format!("laptop {sign}"),
            "laptop lock".into(),
            "laptop restart".into(),
        ]
    );
    // The first device made the identity; its message stays out of the revision summary.
    let dir = env
        .root
        .join("home/Library/Application Support/agent-board/signing");
    let mode = fs::metadata(dir.join("password")).unwrap().permissions();
    assert_eq!(
        std::os::unix::fs::PermissionsExt::mode(&mode) & 0o777,
        0o600
    );
    assert!(dir.join("signing.keychain-db").exists());
    assert!(
        stdout.contains(&format!("  local: {new} (was ")),
        "{stdout}"
    );

    // A device already at the revision signs again, with the same identity.
    env.clear_log();
    succeeds(env.run(&["--yes", new], &[("STUB_OS", "Darwin")]));
    let log = env.read("log");
    assert!(log.contains(&format!("local {sign}")), "{log}");
    assert!(!log.contains("local install"), "{log}");
    assert!(!log.contains("delete-keychain"), "{log}");
}

/// Writes a signing folder as an earlier run left it: this password and keychain content.
fn signing_folder(env: &Env, keychain: &str) -> PathBuf {
    let dir = env
        .root
        .join("home/Library/Application Support/agent-board/signing");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("password"), "old\n").unwrap();
    fs::write(dir.join("signing.keychain-db"), keychain).unwrap();
    dir
}

#[test]
fn a_failed_signing_warns_and_still_restarts_the_device() {
    let env = Env::new();
    let new = &env.main[1];
    let out = env.run(
        &["--yes", new],
        &[("STUB_OS", "Darwin"), ("STUB_FAIL_SIGN", "1")],
    );
    let (_, err) = text(&out);
    assert!(out.status.success(), "{err}");
    assert!(err.contains("codesign could not sign"), "{err}");
    assert!(err.contains("could not sign board"), "{err}");
    let log = env.read("log");
    assert!(log.contains("local restart"), "{log}");
    assert!(!log.contains(" sign "), "{log}");
    assert!(
        log.contains("local lock"),
        "the keychain stays unlocked: {log}"
    );
}

#[test]
fn a_signing_identity_left_half_made_is_made_again() {
    let env = Env::new();
    let new = &env.main[1];
    // As a run cut short after `security create-keychain` leaves it: no certificate inside.
    let dir = signing_folder(&env, "");

    let out = env.run(&["--yes", new], &[("STUB_OS", "Darwin")]);
    let (_, err) = text(&out);
    assert!(out.status.success(), "{err}");
    assert!(err.contains("making the code-signing identity"), "{err}");
    assert!(!err.contains("could not sign"), "{err}");
    assert_ne!(fs::read_to_string(dir.join("password")).unwrap(), "old\n");
    let log = env.read("log");
    assert!(log.contains("local delete-keychain"), "{log}");
    assert!(
        log.contains("local sign 0123ABCD dev.agent-board.board"),
        "{log}"
    );
}

fn signing_files(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

#[test]
fn a_run_that_fails_while_making_the_identity_leaves_no_keychain_and_the_next_run_signs() {
    let env = Env::new();
    let new = &env.main[1];
    let out = env.run(
        &["--yes", new],
        &[("STUB_OS", "Darwin"), ("STUB_FAIL_PARTITION", "1")],
    );
    let (_, err) = text(&out);
    assert!(out.status.success(), "{err}");
    assert!(
        err.contains("could not store the signing identity"),
        "{err}"
    );
    assert!(!env.read("log").contains(" sign "));
    let dir = env
        .root
        .join("home/Library/Application Support/agent-board/signing");
    assert_eq!(signing_files(&dir), [] as [&str; 0]);

    env.clear_log();
    let out = env.run(&["--yes", new], &[("STUB_OS", "Darwin")]);
    let (_, err) = text(&out);
    assert!(out.status.success(), "{err}");
    assert!(!err.contains("could not sign"), "{err}");
    let log = env.read("log");
    assert!(
        log.contains("local sign 0123ABCD dev.agent-board.board"),
        "{log}"
    );
    assert_eq!(signing_files(&dir), ["password", "signing.keychain-db"]);
}

#[test]
fn a_keychain_a_killed_run_left_unfinished_is_removed_and_the_identity_made_again() {
    let env = Env::new();
    let new = &env.main[1];
    // As a run killed after `security import` leaves it: the certificate is inside, but the
    // keychain and its password are not yet in place.
    let dir = env
        .root
        .join("home/Library/Application Support/agent-board/signing");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("making.password"), "old\n").unwrap();
    fs::write(dir.join("making.keychain-db"), "certificate\n").unwrap();

    let out = env.run(&["--yes", new], &[("STUB_OS", "Darwin")]);
    let (_, err) = text(&out);
    assert!(out.status.success(), "{err}");
    assert!(err.contains("making the code-signing identity"), "{err}");
    assert!(!err.contains("could not sign"), "{err}");
    let log = env.read("log");
    assert!(
        log.contains("local sign 0123ABCD dev.agent-board.board"),
        "{log}"
    );
    assert_eq!(signing_files(&dir), ["password", "signing.keychain-db"]);
    assert_ne!(fs::read_to_string(dir.join("password")).unwrap(), "old\n");
}

#[test]
fn a_keychain_that_does_not_unlock_is_kept_and_reported() {
    let env = Env::new();
    let new = &env.main[1];
    let dir = signing_folder(&env, "identity\n");

    let out = env.run(
        &["--yes", new],
        &[("STUB_OS", "Darwin"), ("STUB_FAIL_UNLOCK", "1")],
    );
    let (_, err) = text(&out);
    assert!(out.status.success(), "{err}");
    assert!(
        err.contains("passphrase you entered is not correct"),
        "{err}"
    );
    assert!(err.contains("could not unlock"), "{err}");
    assert!(err.contains("could not sign board"), "{err}");
    // A new key would lose the privacy grants made for this one.
    assert_eq!(fs::read_to_string(dir.join("password")).unwrap(), "old\n");
    assert_eq!(
        fs::read_to_string(dir.join("signing.keychain-db")).unwrap(),
        "identity\n"
    );
    let log = env.read("log");
    assert!(!log.contains(" sign "), "{log}");
    assert!(!log.contains("delete-keychain"), "{log}");
    assert!(log.contains("local restart"), "{log}");
}

#[test]
fn a_certificate_without_a_usable_identity_is_kept_and_reported() {
    let env = Env::new();
    let new = &env.main[1];
    let dir = signing_folder(&env, "certificate\n");

    let out = env.run(&["--yes", new], &[("STUB_OS", "Darwin")]);
    let (_, err) = text(&out);
    assert!(out.status.success(), "{err}");
    assert!(err.contains("no identity codesign can use"), "{err}");
    assert!(!err.contains("making the code-signing identity"), "{err}");
    assert_eq!(fs::read_to_string(dir.join("password")).unwrap(), "old\n");
    assert_eq!(
        fs::read_to_string(dir.join("signing.keychain-db")).unwrap(),
        "certificate\n"
    );
    let log = env.read("log");
    assert!(!log.contains(" sign "), "{log}");
    assert!(log.contains("local lock"), "{log}");
}

#[test]
fn a_keychain_security_cannot_search_is_kept_and_reported() {
    let env = Env::new();
    let new = &env.main[1];
    let dir = signing_folder(&env, "identity\n");

    let out = env.run(
        &["--yes", new],
        &[("STUB_OS", "Darwin"), ("STUB_FAIL_FIND", "1")],
    );
    let (_, err) = text(&out);
    assert!(out.status.success(), "{err}");
    assert!(err.contains("Internal error"), "{err}");
    assert!(err.contains("could not search"), "{err}");
    assert!(err.contains("could not sign board"), "{err}");
    assert!(!err.contains("making the code-signing identity"), "{err}");
    assert_eq!(fs::read_to_string(dir.join("password")).unwrap(), "old\n");
    assert_eq!(
        fs::read_to_string(dir.join("signing.keychain-db")).unwrap(),
        "identity\n"
    );
    let log = env.read("log");
    assert!(!log.contains(" sign "), "{log}");
    assert!(!log.contains("delete-keychain"), "{log}");
    // The trap locks the keychain the failed run unlocked.
    assert!(log.contains("local lock"), "{log}");
    assert!(log.contains("local restart"), "{log}");
}

#[test]
fn a_keychain_with_the_certificate_but_no_password_is_kept_and_reported() {
    let env = Env::new();
    let new = &env.main[1];
    let dir = signing_folder(&env, "identity\n");
    fs::remove_file(dir.join("password")).unwrap();

    let out = env.run(&["--yes", new], &[("STUB_OS", "Darwin")]);
    let (_, err) = text(&out);
    assert!(out.status.success(), "{err}");
    assert!(err.contains("password file"), "{err}");
    assert!(err.contains("Restore that file"), "{err}");
    assert!(err.contains("could not sign board"), "{err}");
    assert!(!err.contains("making the code-signing identity"), "{err}");
    assert!(!dir.join("password").exists());
    assert_eq!(
        fs::read_to_string(dir.join("signing.keychain-db")).unwrap(),
        "identity\n"
    );
    let log = env.read("log");
    assert!(!log.contains("local unlock"), "{log}");
    assert!(!log.contains(" sign "), "{log}");
    assert!(!log.contains("delete-keychain"), "{log}");
    assert!(log.contains("local restart"), "{log}");
}

#[test]
fn board_is_signed_where_cargo_install_root_puts_it() {
    let env = Env::new();
    let new = &env.main[1];
    let root = env.root.join("tools");
    let root_text = root.display().to_string();
    let out = env.run(
        &["--yes", new],
        &[("STUB_OS", "Darwin"), ("CARGO_INSTALL_ROOT", &root_text)],
    );
    let (_, err) = text(&out);
    assert!(out.status.success(), "{err}");
    assert!(!err.contains("could not sign"), "{err}");
    let log = env.read("log");
    assert!(
        log.contains(&format!(
            "local sign 0123ABCD dev.agent-board.board {}",
            root.join("bin/board").display()
        )),
        "{log}"
    );
}

#[test]
fn a_device_whose_cargo_fails_stops_the_plan_before_any_change() {
    let env = Env::new();
    let err = fails(env.run(&["--yes", &env.main[1]], &[("STUB_FAIL_LIST", "1")]));
    assert!(err.contains("cannot read the install list"), "{err}");
    assert!(
        err.contains("could not read board's revision on local"),
        "{err}"
    );
    assert_eq!(env.server_head(), env.main[0]);
    assert!(!env.read("log").contains("server-upgrade"));
    assert!(!env.lock().exists(), "the lock was not released");

    // A device that answers but fails its step is not left out.
    let err = fails(env.run(
        &["--yes", "--skip-unreachable", &env.main[1]],
        &[("STUB_FAIL_LIST", "1")],
    ));
    assert!(
        err.contains("could not read board's revision on local"),
        "{err}"
    );
    assert_eq!(env.server_head(), env.main[0]);
}

#[test]
fn an_unreachable_device_stops_the_run_unless_it_is_left_out() {
    let env = Env::new();
    let (old, new) = (&env.main[0], &env.main[1]);
    let off = [("STUB_UNREACHABLE", "laptop")];
    let err = fails(env.run(&["--yes", new], &off));
    assert!(err.contains("ssh could not connect to laptop"), "{err}");
    assert!(err.contains("--skip-unreachable"), "{err}");
    assert_eq!(&env.server_head(), old);
    assert_eq!(env.read("cargo-local").trim(), old);
    assert!(!env.lock().exists(), "the lock was not released");

    // The plan names the device it leaves out and exits 3.
    let out = env.run(&["--skip-unreachable", new], &off);
    let (stdout, err) = text(&out);
    assert_eq!(out.status.code(), Some(3), "{stdout}\n{err}");
    assert!(
        stdout.contains("device laptop: left out (ssh could not connect)"),
        "{stdout}"
    );
    assert_eq!(&env.server_head(), old);

    // Applied, it upgrades the other hosts and prints the command for the one left out.
    let out = env.run(&["--yes", "--skip-unreachable", new], &off);
    let (stdout, err) = text(&out);
    assert_eq!(out.status.code(), Some(3), "{stdout}\n{err}");
    assert_eq!(&env.server_head(), new);
    assert_eq!(env.read("cargo-local").trim(), new);
    assert_eq!(env.read("cargo-laptop").trim(), old);
    assert!(
        stdout.contains("  laptop: left out (ssh could not connect)"),
        "{stdout}"
    );
    let again = format!(
        "--config {} --yes {new}",
        env.root.join("hosts.conf").display()
    );
    assert!(
        stdout.contains("left out because ssh could not connect: laptop. Once they answer, run: ")
            && stdout.contains(&again),
        "{stdout}"
    );
    assert!(!env.lock().exists(), "the lock was not released");

    // Once the device answers, that command finishes it.
    succeeds(env.run(&["--yes", new], &[]));
    assert_eq!(env.read("cargo-laptop").trim(), new);
}

#[test]
fn a_device_with_watch_no_gets_board_but_no_restart() {
    let env = Env::new();
    let new = &env.main[1];
    let conf = env.root.join("hosts.conf");
    let hosts = fs::read_to_string(&conf).unwrap();
    fs::write(
        &conf,
        hosts.replace("~/.ssh/key\n", "~/.ssh/key watch=no\n"),
    )
    .unwrap();
    let stdout = succeeds(env.run(&[new], &[]));
    assert!(
        stdout.contains(&format!(
            "device laptop: {} -> {} (no board watch)",
            &env.main[0][..12],
            &new[..12]
        )),
        "{stdout}"
    );

    let stdout = succeeds(env.run(&["--yes", new], &[]));
    let log = env.read("log");
    assert!(
        log.contains("laptop install") && !log.contains("laptop restart"),
        "{log}"
    );
    assert!(log.contains("local restart"), "{log}");
    assert!(stdout.contains(&format!("  laptop: {new}")), "{stdout}");

    let stdout = succeeds(env.run(&["--yes", new], &[]));
    assert!(
        stdout.contains(&format!("device laptop: already at {}", &new[..12])),
        "{stdout}"
    );

    fs::write(
        &conf,
        hosts.replace("~/.ssh/key\n", "~/.ssh/key watch=maybe\n"),
    )
    .unwrap();
    let err = fails(env.run(&[new], &[]));
    assert!(err.contains("watch must be yes or no"), "{err}");
}

#[test]
fn a_held_lock_stops_the_run_and_a_stale_one_is_replaced() {
    let env = Env::new();
    fs::create_dir(env.lock()).unwrap();
    fs::write(env.lock().join("owner"), "20260101T000000Z-1").unwrap();
    let err = fails(env.run(&["--yes", &env.main[1]], &[]));
    assert!(err.contains("upgrade 20260101T000000Z-1 holds"), "{err}");
    assert!(!env.read("log").contains("backup"));
    assert_eq!(env.server_head(), env.main[0]);
    assert!(env.lock().exists(), "another run's lock was removed");

    // A lock over two hours old is from a run that died.
    let touched = Command::new("touch")
        .args(["-t", "202001010000"])
        .arg(env.lock())
        .status()
        .unwrap();
    assert!(touched.success());
    let out = env.run(&["--yes", &env.main[1]], &[]);
    let (_, err) = text(&out);
    assert!(out.status.success(), "{err}");
    assert!(err.contains("replacing the lock"), "{err}");
    assert_eq!(env.server_head(), env.main[1]);
    assert!(!env.lock().exists());
}

#[test]
fn active_board_mcp_sidecar_is_upgraded_and_verified_alongside_board() {
    let env = Env::new();
    let new = &env.main[1];
    // Mark board-mcp as running at the old commit before upgrading.
    fs::write(env.root.join("state/mcp-running"), &env.main[0][..12]).unwrap();

    let stdout = succeeds(env.run(&["--yes", new], &[]));
    let log = env.read("log");
    assert!(
        log.contains(&format!("server up-mcp {}", &new[..12])),
        "expected board-mcp recreate in log:\n{log}"
    );
    assert!(
        stdout.contains("board-mcp healthz:")
            && stdout.contains(&format!("\"commit\":\"{}\"", &new[..12])),
        "expected board-mcp healthz output:\n{stdout}"
    );
    assert_eq!(
        fs::read_to_string(env.root.join("state/mcp-running"))
            .unwrap()
            .trim(),
        &new[..12]
    );

    // When board-mcp is not running, upgrade leaves mcp-http untouched.
    let env2 = Env::new();
    let _ = succeeds(env2.run(&["--yes", &env2.main[1]], &[]));
    let log2 = env2.read("log");
    assert!(
        !log2.contains("server up-mcp"),
        "inactive board-mcp must not be started:\n{log2}"
    );
}

#[test]
fn install_image_upgrades_linux_telemetry_hosts_without_cargo() {
    let env = Env::new();
    let old = &env.main[0];
    let new = &env.main[1];

    // Seed two prebuilt host binaries at the old commit (simulating Linux hosts without cargo).
    let bin_a = env.root.join("state/board-host-a");
    let bin_b = env.root.join("state/board-host-b");
    for bin in [&bin_a, &bin_b] {
        common::script::write_script(
            bin,
            &format!("#!/bin/sh\necho \"board 0.1.0 ({})\"\n", &old[..12]),
        );
    }

    let conf = env.root.join("hosts.conf");
    fs::write(
        &conf,
        format!(
            "server board-host clone={} db={} port=7377\n\
             device local\n\
             device telemetry-a watch=no install=image bin={}\n\
             device telemetry-b watch=no install=image bin={}\n",
            env.root.join("server").display(),
            env.root.join("data/board.db").display(),
            bin_a.display(),
            bin_b.display(),
        ),
    )
    .unwrap();

    // Plan shows both image-installed hosts moving from old -> new.
    let plan = succeeds(env.run(&[new], &[]));
    assert!(
        plan.contains(&format!(
            "device telemetry-a: {} -> {} (no board watch)",
            &old[..12],
            &new[..12]
        )),
        "{plan}"
    );
    assert!(
        plan.contains(&format!(
            "device telemetry-b: {} -> {} (no board watch)",
            &old[..12],
            &new[..12]
        )),
        "{plan}"
    );

    // Applying extracts once from server image and updates both binaries atomically.
    let stdout = succeeds(env.run(&["--yes", new], &[]));
    let log = env.read("log");
    assert_eq!(
        log.matches("server create-extract").count(),
        1,
        "binary should be extracted once from server image:\n{log}"
    );
    assert!(
        !log.contains("telemetry-a install") && !log.contains("telemetry-a restart"),
        "image hosts must not run cargo install or restart when watch=no:\n{log}"
    );
    assert!(
        stdout.contains(&format!("  telemetry-a: {}", &new[..12])),
        "{stdout}"
    );
    assert!(
        stdout.contains(&format!("  telemetry-b: {}", &new[..12])),
        "{stdout}"
    );
    for bin in [&bin_a, &bin_b] {
        let ver = Command::new(bin).arg("--version").output().unwrap();
        assert!(ver.status.success());
        assert_eq!(
            String::from_utf8_lossy(&ver.stdout).trim(),
            format!("board 0.1.0 ({})", &new[..12])
        );
    }

    // Rerunning at the same revision skips extraction and reports already at target.
    env.clear_log();
    let rerun = succeeds(env.run(&["--yes", new], &[]));
    assert!(
        rerun.contains(&format!("device telemetry-a: already at {}", &new[..12])),
        "{rerun}"
    );
    assert!(
        !env.read("log").contains("server create-extract"),
        "already-current hosts must not extract from server:\n{}",
        env.read("log")
    );

    // Refuses downgrading an install=image host without --rollback.
    let err = fails(env.run(&[old], &[]));
    assert!(err.contains("telemetry-a"), "{err}");

    // Rejects bin= when install is not image.
    fs::write(
        &conf,
        format!(
            "server board-host clone={} db={} port=7377\n\
             device laptop bin={}\n",
            env.root.join("server").display(),
            env.root.join("data/board.db").display(),
            bin_a.display(),
        ),
    )
    .unwrap();
    let err = fails(env.run(&[new], &[]));
    assert!(
        err.contains("bin= is only valid with install=image"),
        "{err}"
    );
}

#[test]
fn no_restart_local_installs_local_and_restarts_remote_without_restarting_local() {
    let env = Env::new();
    let (old, new) = (&env.main[0], &env.main[1]);
    let (o, n) = (&old[..12], &new[..12]);

    // Plan notes that local board watch restart is skipped while remote restarts normally.
    let plan = succeeds(env.run(&["--no-restart-local", new], &[]));
    assert!(
        plan.contains(&format!(
            "device local: {o} -> {n} (local board watch restart skipped)"
        )),
        "{plan}"
    );
    assert!(
        plan.contains(&format!("device laptop: {o} -> {n}")),
        "{plan}"
    );

    // Apply with --no-restart-local installs and signs local, restarts laptop, never restarts local.
    let stdout = succeeds(env.run(
        &["--yes", "--no-restart-local", new],
        &[("STUB_OS", "Darwin")],
    ));
    let log = env.read("log");
    assert!(
        log.contains(&format!("local install {new} commit={n}")),
        "{log}"
    );
    assert!(
        log.contains("local sign 0123ABCD dev.agent-board.board"),
        "{log}"
    );
    assert!(
        !log.contains("local restart"),
        "--no-restart-local must not restart local board watch:\n{log}"
    );
    assert!(
        log.contains(&format!("laptop install {new} commit={n}")),
        "{log}"
    );
    assert!(
        log.contains("laptop restart"),
        "remote devices must still restart under --no-restart-local:\n{log}"
    );
    assert!(!env.lock().exists(), "server lock was not released");
    assert!(
        stdout.contains(&format!("  local: {new} (was {o})")),
        "{stdout}"
    );

    // Rerunning at the same revision with --no-restart-local still skips restarting local.
    env.clear_log();
    let rerun = succeeds(env.run(&["--yes", "--no-restart-local", new], &[]));
    let log = env.read("log");
    assert!(
        rerun.contains(&format!(
            "device local: already at {n} (local board watch restart skipped)"
        )),
        "{rerun}"
    );
    assert!(
        !log.contains("local restart"),
        "rerun with --no-restart-local must not restart local:\n{log}"
    );
    assert!(log.contains("laptop restart"), "{log}");
}

#[test]
fn devices_already_running_target_commit_skip_watch_restart() {
    use std::os::unix::net::UnixListener;

    let env = Env::new();
    let (old, new) = (&env.main[0], &env.main[1]);
    let (o, n) = (&old[..12], &new[..12]);
    // Both devices already have `new` installed on disk.
    env.at(new);

    // Bind a real hook.sock under a short BOARD_HOME so `[ -S "$sock" ]` holds on macOS/Linux.
    let board_home = env.root.join("bh");
    let run_dir = board_home.join("run");
    fs::create_dir_all(&run_dir).unwrap();
    let sock_path = run_dir.join("hook.sock");
    let _listener = UnixListener::bind(&sock_path).unwrap();
    let bh_str = board_home.display().to_string();

    // `local` is already running `new`, while `laptop` is still running `old`.
    fs::write(env.state("watch-running-local"), n).unwrap();
    fs::write(env.state("watch-running-laptop"), o).unwrap();

    // Plan distinguishes already-running target from a device needing restart.
    let plan = succeeds(env.run(&[new], &[("BOARD_HOME", &bh_str)]));
    assert!(
        plan.contains(&format!(
            "device local: {n} (already there; board watch already running {n})"
        )),
        "{plan}"
    );
    assert!(
        plan.contains(&format!(
            "device laptop: {n} (already there; board watch restarts)"
        )),
        "{plan}"
    );

    // Apply skips restarting `local` (already running `new`) and restarts `laptop` (running `old`).
    env.clear_log();
    let stdout = succeeds(env.run(&["--yes", new], &[("BOARD_HOME", &bh_str)]));
    let log = env.read("log");
    assert!(
        stdout.contains(&format!(
            "device local: already at {n} (board watch already running {n})"
        )),
        "{stdout}"
    );
    assert!(
        !log.contains("local restart"),
        "must skip no-op restart when board watch already runs target commit:\n{log}"
    );
    assert!(
        log.contains("laptop restart"),
        "must still restart device running an older commit:\n{log}"
    );

    // Same check for `install=image` with `watch=yes`.
    let bin = env.root.join("state/board-img");
    common::script::write_script(&bin, &format!("#!/bin/sh\necho \"board 0.1.0 ({n})\"\n"));
    fs::write(
        env.root.join("hosts.conf"),
        format!(
            "server board-host clone={} db={} port=7377\n\
             device local install=image bin={}\n\
             device laptop install=image bin={}\n",
            env.root.join("server").display(),
            env.root.join("data/board.db").display(),
            bin.display(),
            bin.display(),
        ),
    )
    .unwrap();
    env.clear_log();
    let stdout = succeeds(env.run(&["--yes", new], &[("BOARD_HOME", &bh_str)]));
    let log = env.read("log");
    assert!(
        stdout.contains(&format!(
            "device local: already at {n} (board watch already running {n})"
        )),
        "{stdout}"
    );
    assert!(
        !log.contains("local restart"),
        "install=image must skip no-op restart when already running target commit:\n{log}"
    );
    assert!(
        log.contains("laptop restart"),
        "install=image must still restart device running older commit:\n{log}"
    );
}

#[test]
fn at_least_succeeds_when_all_hosts_already_run_newer_main_descendant_and_rejects_partial_or_diverged()
 {
    use std::os::unix::net::UnixListener;

    let env = Env::new();
    let work = env.root.join("work");
    let c0 = env.main[0].clone();
    let c1 = env.main[1].clone();
    // Push a third commit `c2` on main so c0 < c1 < c2 on main, while `env.side` diverges from c1.
    fs::write(work.join("file"), "2").unwrap();
    git(&work, &["commit", "-q", "-am", "c2"]);
    let c2 = git(&work, &["rev-parse", "HEAD"]);
    git(&work, &["push", "-q", "origin", "main"]);
    git(&env.root.join("server"), &["fetch", "-q", "origin", "main"]);

    // 1. `--at-least` and `--rollback` cannot be combined.
    let err = fails(env.run(&["--at-least", "--rollback", &c1], &[]));
    assert!(
        err.contains("--at-least and --rollback cannot be used together"),
        "{err}"
    );
    assert_eq!(env.read("log"), "");

    // 2. All hosts behind `c1` (at `c0`): `--yes --at-least c1` runs the normal upgrade to `c1`.
    env.at(&c0);
    env.clear_log();
    let stdout = succeeds(env.run(&["--yes", "--no-restart-local", "--at-least", &c1], &[]));
    assert!(
        !stdout.contains("already satisfied (--at-least)"),
        "{stdout}"
    );
    assert_eq!(env.server_head(), c1);
    assert_eq!(env.read("cargo-local").trim(), c1);
    assert_eq!(env.read("cargo-laptop").trim(), c1);

    // 3. All hosts already run `c2` (newer main descendant of `c1`):
    //    plan-only and `--yes --no-restart-local --at-least c1` both succeed without touching
    //    any host, calling `gh`, or leaving the server lock behind.
    env.at(&c2);
    env.clear_log();
    let plan = succeeds(env.run(&["--at-least", &c1], &[("STUB_GH_LOGGED_OUT", "1")]));
    assert!(
        plan.contains(&format!(
            "already satisfied (--at-least): every host runs {c2} on main (descendant of {} via git merge-base --is-ancestor)",
            &c1[..12]
        )),
        "{plan}"
    );
    assert!(!plan.contains("  image:"), "{plan}");
    assert_eq!(
        env.read("log"),
        "board-host server-status\nlaptop device-status\n"
    );

    env.clear_log();
    let applied = succeeds(env.run(
        &["--yes", "--no-restart-local", "--at-least", &c1],
        &[("STUB_GH_LOGGED_OUT", "1")],
    ));
    assert!(
        applied.contains(&format!(
            "already satisfied (--at-least): every host runs {c2} on main (descendant of {} via git merge-base --is-ancestor)",
            &c1[..12]
        )),
        "{applied}"
    );
    let log = env.read("log");
    assert_eq!(
        log,
        "board-host server-lock\nboard-host server-status\nlaptop device-status\nboard-host server-unlock\n"
    );
    assert_eq!(env.server_head(), c2, "server must not be downgraded");
    assert_eq!(env.read("cargo-local").trim(), c2);
    assert_eq!(env.read("cargo-laptop").trim(), c2);
    assert!(!env.lock().exists(), "server lock must be released");

    // 4. Mixed at-or-ahead hosts on main (`server` + `local` at `c2`, `laptop` at `c1`):
    //    succeeds with per-host summary without downgrading `server` or `local`.
    fs::write(env.state("cargo-laptop"), &c1).unwrap();
    env.clear_log();
    let mixed = succeeds(env.run(
        &["--yes", "--no-restart-local", "--at-least", &c1],
        &[("STUB_GH_LOGGED_OUT", "1")],
    ));
    assert!(
        mixed.contains(&format!(
            "already satisfied (--at-least): every host runs a main commit at or ahead of {} (git merge-base --is-ancestor; server={} local={} laptop={})",
            &c1[..12],
            &c2[..12],
            &c2[..12],
            &c1[..12]
        )),
        "{mixed}"
    );
    assert_eq!(env.server_head(), c2);
    assert!(!env.lock().exists());

    // 5. One host behind `c1` (`laptop` at `c0`) while `server` and `local` are at `c2`:
    //    must fail cleanly without downgrading or false-passing.
    fs::write(env.state("cargo-laptop"), &c0).unwrap();
    let err = fails(env.run(&["--yes", "--no-restart-local", "--at-least", &c1], &[]));
    assert!(
        err.contains("not every host is at or ahead of")
            && err.contains(&format!("laptop({})", &c0[..12])),
        "{err}"
    );
    assert_eq!(env.server_head(), c2);
    assert!(!env.lock().exists());

    // 6. Unreachable device (`laptop`) while `server` and `local` are at `c2`:
    //    fails cleanly both with and without `--skip-unreachable` (no false pass).
    env.at(&c2);
    let err = fails(env.run(
        &["--yes", "--no-restart-local", "--at-least", &c1],
        &[("STUB_UNREACHABLE", "laptop")],
    ));
    assert!(err.contains("ssh could not connect to laptop"), "{err}");
    let err_skip = fails(env.run(
        &[
            "--yes",
            "--no-restart-local",
            "--skip-unreachable",
            "--at-least",
            &c1,
        ],
        &[("STUB_UNREACHABLE", "laptop")],
    ));
    assert!(err_skip.contains("laptop(unreachable)"), "{err_skip}");
    assert!(!env.lock().exists());

    // 7. Diverged descendant commit off `main` (`env.side`, whose parent is `c1`):
    //    must fail cleanly without false-passing or downgrading.
    env.at(&c2);
    fs::write(env.state("cargo-laptop"), &env.side).unwrap();
    let err_div = fails(env.run(&["--yes", "--no-restart-local", "--at-least", &c1], &[]));
    assert!(
        err_div.contains("not every host is at or ahead of")
            && err_div.contains(&format!("laptop({})", &env.side[..12])),
        "{err_div}"
    );
    assert_eq!(env.server_head(), c2);

    // 8. Server clone at `c2` while running container reports older `c0`:
    //    must fail cleanly (no false pass).
    env.at(&c2);
    fs::write(env.state("running"), &c0[..12]).unwrap();
    let err_srv = fails(env.run(&["--yes", "--no-restart-local", "--at-least", &c1], &[]));
    assert!(
        err_srv.contains(&format!(
            "server(running={},clone={})",
            &c0[..12],
            &c2[..12]
        )),
        "{err_srv}"
    );

    // 9. Device binary installed at `c2`, but running `board watch` on `hook.sock` is still `c0`:
    //    must fail cleanly; once running `c2`, succeeds.
    env.at(&c2);
    let board_home = env.root.join("bh-at-least");
    let run_dir = board_home.join("run");
    fs::create_dir_all(&run_dir).unwrap();
    let _listener = UnixListener::bind(run_dir.join("hook.sock")).unwrap();
    let bh_str = board_home.display().to_string();
    fs::write(env.state("watch-running-local"), &c2[..12]).unwrap();
    fs::write(env.state("watch-running-laptop"), &c0[..12]).unwrap();
    let err_watch = fails(env.run(
        &["--yes", "--no-restart-local", "--at-least", &c1],
        &[("BOARD_HOME", &bh_str)],
    ));
    assert!(
        err_watch.contains(&format!(
            "laptop(running={},installed={})",
            &c0[..12],
            &c2[..12]
        )),
        "{err_watch}"
    );
    fs::write(env.state("watch-running-laptop"), &c2[..12]).unwrap();
    succeeds(env.run(
        &["--yes", "--no-restart-local", "--at-least", &c1],
        &[("BOARD_HOME", &bh_str)],
    ));

    // 10. `install=image` host running newer main descendant `c2` (12-char `--version`) succeeds.
    let bin = env.root.join("state/board-at-least-img");
    common::script::write_script(
        &bin,
        &format!("#!/bin/sh\necho \"board 0.1.0 ({})\"\n", &c2[..12]),
    );
    fs::write(
        env.root.join("hosts.conf"),
        format!(
            "server board-host clone={} db={} port=7377\n\
             device local\n\
             device telemetry watch=no install=image bin={}\n",
            env.root.join("server").display(),
            env.root.join("data/board.db").display(),
            bin.display(),
        ),
    )
    .unwrap();
    let img_ok = succeeds(env.run(
        &["--yes", "--no-restart-local", "--at-least", &c1],
        &[("BOARD_HOME", &bh_str), ("STUB_GH_LOGGED_OUT", "1")],
    ));
    assert!(
        img_ok.contains(&format!("every host runs {c2} on main")),
        "{img_ok}"
    );
}
