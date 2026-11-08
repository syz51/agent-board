//! Tests for `deploy/upgrade.sh` against stub `ssh`, `gh`, `docker`, `curl`, `cargo` and
//! service managers. Git is real: an origin repository the hosts install from, the
//! operator's clone the script runs from, and the server's clone it moves.
#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
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
    // Logs the host and step, then runs the command locally, as the host would.
    (
        "ssh",
        r#"#!/bin/sh
echo "ssh-args $*" >> "$STATE/ssh-args"
while [ "$1" != "--" ]; do shift; done
shift
host=$1
shift
echo "$host $4" >> "$STATE/log"
STUB_HOST=$host exec "$@"
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
*) echo "unexpected docker $*" >&2; exit 9 ;;
esac
"#,
    ),
    (
        "curl",
        r#"#!/bin/sh
printf '{"ok":true,"version":"0.1.0","commit":"%s","schema_version":11}' "$(cat "$STATE/running")"
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

fn write_exec(path: &Path, text: &str) {
    fs::write(path, text).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
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
            write_exec(&root.join("bin").join(name), text);
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
            "BOARD_IMAGE",
            "STUB_DIGEST",
            "STUB_GH_LOGGED_OUT",
            "STUB_GH_VERIFY_FAIL",
            "STUB_FAIL_PULL",
            "STUB_FAIL_TAG",
            "STUB_FAIL_BUILD",
            "STUB_FAIL_BACKUP",
            "STUB_FAIL_UP",
            "STUB_FAIL_RM",
            "STUB_FAIL_LIST",
            "STUB_FAIL_RESTART",
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
            "gh is not logged in: run gh auth login, or pass --build",
        ),
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
