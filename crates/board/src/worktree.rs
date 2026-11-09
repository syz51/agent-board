//! The git worktree a coordinator thread starts in, so it never works in the project root
//! that other threads share.
//!
//! When the project folder is the top of a git repository, T3 creates the coordinator's
//! branch from the default branch and runs the thread in a new worktree on it. The branch
//! is `coordinator/<slug>`, or `coordinator/<slug>-<UTC time>` when that one exists: `t3
//! thread start` can only create a branch, never check out one that exists, and an earlier
//! coordinator's worktree may still use it, maybe with a live thread or files git ignores.
//! So nothing here removes a worktree or changes a branch.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use board_proto::{BoardSlug, Timestamp};

use crate::t3::NewWorktree;

/// A git command that only reads the repository answers in well under a second, unless
/// something holds it, such as a macOS privacy prompt for a protected folder, which git
/// waits on with no end. So a stuck start fails soon instead of spending the daemon's
/// reconcile on one board.
const LOCAL_GIT_TIMEOUT: Duration = Duration::from_secs(10);
/// A fetch, which waits on the network.
const FETCH_TIMEOUT: Duration = Duration::from_secs(60);

/// The branch the coordinator of `slug` runs on when it is free.
pub fn coordinator_branch(slug: &BoardSlug) -> String {
    format!("coordinator/{slug}")
}

/// The worktree to start the coordinator of `slug` in; `None` when `project` is not the top
/// of a git repository, so the thread runs in the folder itself.
pub async fn coordinator_worktree(
    project: &Path,
    slug: &BoardSlug,
    now: Timestamp,
) -> Result<Option<NewWorktree>> {
    if !is_repo_top(project).await? {
        return Ok(None);
    }
    let base = default_base(project).await?;
    let mut branch = coordinator_branch(slug);
    if has_ref(project, &format!("refs/heads/{branch}")).await? {
        branch = format!("{branch}-{}", now.format("%Y%m%d-%H%M%S"));
    }
    Ok(Some(NewWorktree { base, branch }))
}

/// Whether `project` is the top folder of a git work tree. A folder that does not exist is
/// not (T3 then reports the missing project), nor is one nested in another repository.
async fn is_repo_top(project: &Path) -> Result<bool> {
    let Ok(folder) = project.canonicalize() else {
        return Ok(false);
    };
    let out = git(
        project,
        &["rev-parse", "--show-toplevel"],
        LOCAL_GIT_TIMEOUT,
    )
    .await?;
    if !out.ok {
        if out.stderr.contains("not a git repository") {
            return Ok(false);
        }
        bail!(
            "git rev-parse failed in {}: {}",
            project.display(),
            out.stderr.trim()
        );
    }
    let top = Path::new(out.stdout.trim()).canonicalize();
    Ok(top.is_ok_and(|top| top == folder))
}

/// The ref new coordinator worktrees start from, fetched first when it is a remote branch:
/// the remote's default branch (`origin/HEAD`), else the first of `origin/main`,
/// `origin/master`, `main` and `master` that exists.
async fn default_base(project: &Path) -> Result<String> {
    let head = git(
        project,
        &[
            "symbolic-ref",
            "--quiet",
            "--short",
            "refs/remotes/origin/HEAD",
        ],
        LOCAL_GIT_TIMEOUT,
    )
    .await?;
    let mut base = head
        .ok
        .then(|| head.stdout.trim().to_owned())
        .filter(|b| !b.is_empty());
    if base.is_none() {
        for candidate in ["origin/main", "origin/master", "main", "master"] {
            let full = match candidate.strip_prefix("origin/") {
                Some(name) => format!("refs/remotes/origin/{name}"),
                None => format!("refs/heads/{candidate}"),
            };
            if has_ref(project, &full).await? {
                base = Some(candidate.to_owned());
                break;
            }
        }
    }
    let Some(base) = base else {
        bail!(
            "{} has no origin/HEAD, origin/main, origin/master, main or master to start the coordinator's worktree from; set origin/HEAD (git remote set-head origin --auto)",
            project.display()
        );
    };
    if let Some(name) = base.strip_prefix("origin/") {
        // A stale remote branch would start the coordinator, and its children, on old code.
        match git(
            project,
            &["fetch", "--quiet", "origin", name],
            FETCH_TIMEOUT,
        )
        .await
        {
            Ok(out) if out.ok => {}
            Ok(out) => tracing::warn!(
                "fetching {base} in {}: {}; starting from the local copy",
                project.display(),
                out.stderr.trim()
            ),
            Err(err) => tracing::warn!(
                "fetching {base} in {}: {err:#}; starting from the local copy",
                project.display()
            ),
        }
    }
    Ok(base)
}

async fn has_ref(project: &Path, full: &str) -> Result<bool> {
    let out = git(
        project,
        &["rev-parse", "--verify", "--quiet", full],
        LOCAL_GIT_TIMEOUT,
    )
    .await?;
    Ok(out.ok)
}

struct GitOutput {
    ok: bool,
    stdout: String,
    stderr: String,
}

/// Runs `git -C <project> <args>` and returns its output, stopping it after `timeout`.
async fn git(project: &Path, args: &[&str], timeout: Duration) -> Result<GitOutput> {
    let child = tokio::process::Command::new("git")
        .arg("--no-optional-locks")
        .arg("-C")
        .arg(project)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // English messages, which `is_repo_top` reads.
        .env("LC_ALL", "C")
        .kill_on_drop(true)
        .spawn()
        .context("running git")?;
    // Only on macOS, and resolved, so a link to a protected folder counts as one.
    let hint = || {
        if !cfg!(target_os = "macos") {
            return "";
        }
        let project = project
            .canonicalize()
            .unwrap_or_else(|_| project.to_path_buf());
        privacy_hint(dirs::home_dir().as_deref(), &project).unwrap_or_default()
    };
    let out = tokio::time::timeout(timeout, child.wait_with_output())
        .await
        .with_context(|| {
            format!(
                "git {} timed out in {}{}",
                args.join(" "),
                project.display(),
                hint()
            )
        })??;
    let ok = out.status.success();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    Ok(GitOutput {
        ok,
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: with_denial_hint(ok, stderr, hint),
    })
}

/// Git's error output, with the privacy hint after it when git failed on a folder macOS
/// denied it, as it does once the user refused the prompt.
fn with_denial_hint(ok: bool, stderr: String, hint: impl FnOnce() -> &'static str) -> String {
    if ok || !stderr.contains("Operation not permitted") {
        return stderr;
    }
    format!("{}{}", stderr.trim_end(), hint())
}

/// What a git timeout or denial in `project` adds on macOS when the folder is one macOS
/// protects: there git waits, with no end, until the user answers the privacy prompt for
/// board, and is denied once they refused it.
fn privacy_hint(home: Option<&Path>, project: &Path) -> Option<&'static str> {
    const PROTECTED: [&str; 5] = [
        "Documents",
        "Desktop",
        "Downloads",
        "Library/Mobile Documents",
        "Library/CloudStorage",
    ];
    // Compared without case, as APFS compares names by default.
    let lower = |path: &Path| PathBuf::from(path.to_string_lossy().to_lowercase());
    let project = lower(project);
    let protected = project.starts_with("/volumes")
        || home.is_some_and(|home| {
            PROTECTED
                .iter()
                .any(|folder| project.starts_with(lower(&home.join(folder))))
        });
    protected.then_some(
        ": macOS may be keeping board out of this folder. Answer its prompt if one is open, \
         or turn on board under System Settings > Privacy & Security > Files and Folders",
    )
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn a_timeout_in_a_protected_folder_names_the_privacy_setting() {
        let home = Path::new("/Users/u");
        let hint = |project: &str| privacy_hint(Some(home), Path::new(project));
        assert!(
            hint("/Users/u/Documents/vps")
                .unwrap()
                .contains("Privacy & Security > Files and Folders")
        );
        assert!(hint("/Users/u/Library/Mobile Documents/x").is_some());
        assert!(hint("/Users/u/Library/CloudStorage/Dropbox/x").is_some());
        assert!(hint("/Volumes/External/repo").is_some());
        assert!(hint("/users/u/documents/vps").is_some(), "without case");
        assert_eq!(hint("/Users/u/Developer/repo"), None);
        assert_eq!(hint("/Users/u/DocumentsOld/repo"), None);
        assert_eq!(privacy_hint(None, Path::new("/Users/u/Documents/x")), None);
    }

    #[test]
    fn a_denied_folder_adds_the_hint_on_the_same_line() {
        let hint = || ": macOS may be keeping board out of this folder";
        let denied =
            "fatal: cannot change to '/Users/u/Documents/x': Operation not permitted\n".to_owned();
        assert_eq!(
            with_denial_hint(false, denied, hint),
            "fatal: cannot change to '/Users/u/Documents/x': Operation not permitted: macOS may be keeping board out of this folder"
        );
        let other = "fatal: not a git repository\n".to_owned();
        assert_eq!(with_denial_hint(false, other.clone(), hint), other);
        assert_eq!(
            with_denial_hint(true, "Operation not permitted\n".into(), hint),
            "Operation not permitted\n",
            "not on success"
        );
    }

    #[tokio::test]
    async fn a_git_command_that_hangs_is_stopped_at_its_timeout() {
        let dir = tempfile::tempdir().unwrap();
        // Reading a FIFO with no writer blocks, as git does on an unanswered macOS prompt.
        let fifo = dir.path().join("fifo");
        let made = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap();
        assert!(made.success());
        let err = git(
            dir.path(),
            &["hash-object", "fifo"],
            Duration::from_millis(300),
        )
        .await
        .err()
        .expect("stopped");
        assert!(
            format!("{err:#}").contains("git hash-object fifo timed out in"),
            "{err:#}"
        );
    }

    /// Runs git in `dir` with no user or system config, so signing or hooks set there do not
    /// apply, and panics when it fails.
    fn run(dir: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
        String::from_utf8(out.stdout).unwrap()
    }

    /// A clone of a repository with one commit on `branch`, with `origin/HEAD` set.
    fn clone(dir: &Path, branch: &str) -> PathBuf {
        let origin = dir.join("origin");
        std::fs::create_dir(&origin).unwrap();
        run(
            &origin,
            &["init", "--quiet", &format!("--initial-branch={branch}")],
        );
        run(
            &origin,
            &["commit", "--quiet", "--allow-empty", "-m", "first"],
        );
        run(dir, &["clone", "--quiet", "origin", "repo"]);
        dir.join("repo")
    }

    fn slug() -> BoardSlug {
        "factory".parse().unwrap()
    }

    fn at() -> Timestamp {
        "2026-11-08T13:20:05Z".parse().unwrap()
    }

    async fn start(project: &Path) -> Option<NewWorktree> {
        coordinator_worktree(project, &slug(), at()).await.unwrap()
    }

    #[tokio::test]
    async fn folders_outside_git_or_missing_start_in_place() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(start(dir.path()).await, None);
        assert_eq!(start(&dir.path().join("gone")).await, None);
    }

    #[tokio::test]
    async fn a_folder_nested_in_a_repository_starts_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let repo = clone(dir.path(), "main");
        let nested = repo.join("notes");
        std::fs::create_dir(&nested).unwrap();
        assert_eq!(start(&nested).await, None);
        assert!(start(&repo).await.is_some());
    }

    #[tokio::test]
    async fn a_first_start_creates_the_branch_from_the_remotes_default_branch() {
        let dir = tempfile::tempdir().unwrap();
        let repo = clone(dir.path(), "trunk");
        assert_eq!(
            start(&repo).await,
            Some(NewWorktree {
                base: "origin/trunk".into(),
                branch: "coordinator/factory".into()
            })
        );
    }

    #[tokio::test]
    async fn without_origin_head_the_base_is_the_first_branch_that_exists() {
        let dir = tempfile::tempdir().unwrap();
        let repo = clone(dir.path(), "master");
        run(&repo, &["remote", "set-head", "origin", "--delete"]);
        assert_eq!(start(&repo).await.unwrap().base, "origin/master");
        run(&repo, &["remote", "remove", "origin"]);
        assert_eq!(start(&repo).await.unwrap().base, "master");
        run(&repo, &["branch", "-m", "master", "develop"]);
        let err = coordinator_worktree(&repo, &slug(), at())
            .await
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("git remote set-head origin --auto")
        );
    }

    #[tokio::test]
    async fn a_restart_leaves_the_old_worktree_and_branch_and_takes_a_new_branch() {
        let dir = tempfile::tempdir().unwrap();
        let repo = clone(dir.path(), "main");
        let old = dir.path().join("old");
        let old_s = old.to_str().unwrap();
        run(
            &repo,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                "coordinator/factory",
                old_s,
                "origin/main",
            ],
        );
        std::fs::write(old.join(".env"), "ignored, maybe").unwrap();
        assert_eq!(
            start(&repo).await.unwrap().branch,
            "coordinator/factory-20261108-132005"
        );
        assert!(old.join(".env").exists(), "the old worktree is untouched");
        assert_eq!(
            run(&repo, &["branch", "--format=%(refname:short)"]),
            "coordinator/factory\nmain\n"
        );
    }
}
