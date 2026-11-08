//! The git worktree a coordinator thread starts in, so it never works in the project root
//! that other threads share.
//!
//! In a git project the coordinator of board `<slug>` runs on branch `coordinator/<slug>`,
//! which T3 creates from the default branch (`origin/HEAD`, else `origin/main`, else `main`)
//! in a new worktree. `t3 thread start` can only create that branch, never check out one that
//! exists, so before each start the branch is freed here, without losing work:
//!
//! - the worktree an earlier coordinator left on it is removed when clean (git refuses a
//!   dirty or locked one, and the main checkout);
//! - the branch is then deleted when the default branch contains all its commits, else
//!   renamed to `coordinator/<slug>-<UTC time>`, which keeps them.
//!
//! When the old worktree cannot be removed, it keeps the branch, and this start uses
//! `coordinator/<slug>-<UTC time>` instead.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use board_proto::{BoardSlug, Timestamp};

use crate::t3::NewWorktree;

const GIT_TIMEOUT: Duration = Duration::from_secs(60);

/// The branch the coordinator of `slug` runs on.
pub fn coordinator_branch(slug: &BoardSlug) -> String {
    format!("coordinator/{slug}")
}

/// The worktree to start the coordinator of `slug` in, with its branch freed for T3 to
/// create; `None` when `project` is not in a git repository (or git is not installed), so
/// the thread runs in the folder itself.
pub async fn coordinator_worktree(
    project: &Path,
    slug: &BoardSlug,
    now: Timestamp,
) -> Result<Option<NewWorktree>> {
    if !is_repo(project).await? {
        return Ok(None);
    }
    let base = default_base(project).await?;
    let branch = coordinator_branch(slug);
    let set_aside = format!("{branch}-{}", now.format("%Y%m%d-%H%M%S"));
    let branch = free_branch(project, &branch, &base, set_aside).await?;
    Ok(Some(NewWorktree { base, branch }))
}

/// Makes `branch` free for T3 to create and returns it, or returns `set_aside` when an old
/// worktree that cannot be removed holds it.
async fn free_branch(
    project: &Path,
    branch: &str,
    base: &str,
    set_aside: String,
) -> Result<String> {
    let full = format!("refs/heads/{branch}");
    if !git(project, &["rev-parse", "--verify", "--quiet", &full])
        .await?
        .ok
    {
        return Ok(branch.to_owned());
    }
    // Forget worktrees whose folder is gone, so they do not hold the branch.
    git_ok(project, &["worktree", "prune"]).await?;
    let list = git_ok(project, &["worktree", "list", "--porcelain"]).await?;
    if let Some(path) = worktree_on(&list, &full) {
        let removed = git(project, &["worktree", "remove", "--", &path]).await?;
        if !removed.ok {
            tracing::warn!(
                "the worktree {path} on {branch} was kept ({}); the coordinator starts on {set_aside}",
                removed.stderr.trim()
            );
            return Ok(set_aside);
        }
        tracing::info!("removed the worktree {path} an earlier coordinator left on {branch}");
    }
    if git(project, &["merge-base", "--is-ancestor", &full, base])
        .await?
        .ok
    {
        git_ok(project, &["branch", "-D", branch]).await?;
        tracing::info!("deleted {branch}: {base} has all its commits");
    } else {
        git_ok(project, &["branch", "-m", branch, &set_aside]).await?;
        tracing::warn!("renamed {branch} to {set_aside}: it has commits {base} lacks");
    }
    Ok(branch.to_owned())
}

/// The path of the worktree that has `full_ref` checked out, from `git worktree list
/// --porcelain`: records split by blank lines, each with `worktree <path>` and, when on a
/// branch, `branch <ref>`.
fn worktree_on(list: &str, full_ref: &str) -> Option<String> {
    list.split("\n\n").find_map(|record| {
        let mut path = None;
        let mut on = false;
        for line in record.lines() {
            if let Some(p) = line.strip_prefix("worktree ") {
                path = Some(p.to_owned());
            } else if line.strip_prefix("branch ") == Some(full_ref) {
                on = true;
            }
        }
        path.filter(|_| on)
    })
}

/// Whether `project` is in a git work tree; false when git is not installed.
async fn is_repo(project: &Path) -> Result<bool> {
    match git(project, &["rev-parse", "--is-inside-work-tree"]).await {
        Ok(out) => Ok(out.ok && out.stdout.trim() == "true"),
        Err(err)
            if err
                .downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
        {
            Ok(false)
        }
        Err(err) => Err(err),
    }
}

/// The ref new coordinator worktrees start from: the remote's default branch
/// (`origin/HEAD`), else `origin/main`, else `main`.
async fn default_base(project: &Path) -> Result<String> {
    let head = git(
        project,
        &[
            "symbolic-ref",
            "--quiet",
            "--short",
            "refs/remotes/origin/HEAD",
        ],
    )
    .await?;
    if head.ok && !head.stdout.trim().is_empty() {
        return Ok(head.stdout.trim().to_owned());
    }
    let main = git(
        project,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            "refs/remotes/origin/main",
        ],
    )
    .await?;
    Ok(if main.ok { "origin/main" } else { "main" }.to_owned())
}

struct GitOutput {
    ok: bool,
    stdout: String,
    stderr: String,
}

/// Runs `git -C <project> <args>`. Spawn errors keep their `io::Error` as the source.
async fn git(project: &Path, args: &[&str]) -> Result<GitOutput> {
    let child = tokio::process::Command::new("git")
        .arg("-C")
        .arg(project)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let out = tokio::time::timeout(GIT_TIMEOUT, child.wait_with_output())
        .await
        .with_context(|| format!("git {} timed out in {}", args.join(" "), project.display()))??;
    Ok(GitOutput {
        ok: out.status.success(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    })
}

/// [`git`] that must succeed; returns its output.
async fn git_ok(project: &Path, args: &[&str]) -> Result<String> {
    let out = git(project, args).await?;
    if !out.ok {
        bail!(
            "git {} failed in {}: {}",
            args.join(" "),
            project.display(),
            out.stderr.trim()
        );
    }
    Ok(out.stdout)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

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

    /// A clone of a repository with one commit on `main`, with `origin/HEAD` set.
    fn clone(dir: &Path) -> PathBuf {
        let origin = dir.join("origin");
        std::fs::create_dir(&origin).unwrap();
        run(&origin, &["init", "--quiet", "--initial-branch=main"]);
        run(
            &origin,
            &["commit", "--quiet", "--allow-empty", "-m", "first"],
        );
        let repo = dir.join("repo");
        run(dir, &["clone", "--quiet", "origin", "repo"]);
        repo
    }

    fn slug() -> BoardSlug {
        "factory".parse().unwrap()
    }

    fn at() -> Timestamp {
        "2026-11-08T13:20:05Z".parse().unwrap()
    }

    fn branches(repo: &Path) -> String {
        run(repo, &["branch", "--format=%(refname:short)"])
    }

    #[tokio::test]
    async fn folders_outside_git_start_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let start = coordinator_worktree(dir.path(), &slug(), at()).await;
        assert_eq!(start.unwrap(), None);
    }

    #[tokio::test]
    async fn a_first_start_creates_the_branch_from_the_default_branch() {
        let dir = tempfile::tempdir().unwrap();
        let repo = clone(dir.path());
        let start = coordinator_worktree(&repo, &slug(), at()).await.unwrap();
        assert_eq!(
            start,
            Some(NewWorktree {
                base: "origin/main".into(),
                branch: "coordinator/factory".into()
            })
        );
    }

    #[tokio::test]
    async fn without_origin_head_the_base_falls_back_to_main() {
        let dir = tempfile::tempdir().unwrap();
        let repo = clone(dir.path());
        run(&repo, &["remote", "set-head", "origin", "--delete"]);
        let start = coordinator_worktree(&repo, &slug(), at()).await.unwrap();
        assert_eq!(start.unwrap().base, "origin/main");
        run(&repo, &["remote", "remove", "origin"]);
        let start = coordinator_worktree(&repo, &slug(), at()).await.unwrap();
        assert_eq!(start.unwrap().base, "main");
    }

    #[tokio::test]
    async fn a_restart_removes_the_clean_old_worktree_and_reuses_the_branch() {
        let dir = tempfile::tempdir().unwrap();
        let repo = clone(dir.path());
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
        let start = coordinator_worktree(&repo, &slug(), at()).await.unwrap();
        assert_eq!(start.unwrap().branch, "coordinator/factory");
        assert!(!old.exists(), "the clean worktree is removed");
        assert_eq!(
            branches(&repo),
            "main\n",
            "the merged branch is deleted for T3"
        );
    }

    #[tokio::test]
    async fn a_restart_keeps_unmerged_commits_on_a_renamed_branch() {
        let dir = tempfile::tempdir().unwrap();
        let repo = clone(dir.path());
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
        run(&old, &["commit", "--quiet", "--allow-empty", "-m", "work"]);
        let start = coordinator_worktree(&repo, &slug(), at()).await.unwrap();
        assert_eq!(start.unwrap().branch, "coordinator/factory");
        assert!(!old.exists());
        assert_eq!(
            branches(&repo),
            "coordinator/factory-20261108-132005\nmain\n"
        );
        assert_eq!(
            run(
                &repo,
                &[
                    "log",
                    "-1",
                    "--format=%s",
                    "coordinator/factory-20261108-132005"
                ]
            ),
            "work\n"
        );
    }

    #[tokio::test]
    async fn a_dirty_old_worktree_keeps_the_branch_and_the_start_uses_another() {
        let dir = tempfile::tempdir().unwrap();
        let repo = clone(dir.path());
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
        std::fs::write(old.join("draft.txt"), "unsaved").unwrap();
        let start = coordinator_worktree(&repo, &slug(), at()).await.unwrap();
        assert_eq!(start.unwrap().branch, "coordinator/factory-20261108-132005");
        assert!(old.join("draft.txt").exists(), "nothing is lost");
        assert_eq!(branches(&repo), "coordinator/factory\nmain\n");
    }

    #[tokio::test]
    async fn a_branch_whose_worktree_folder_is_gone_is_reused() {
        let dir = tempfile::tempdir().unwrap();
        let repo = clone(dir.path());
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
        std::fs::remove_dir_all(&old).unwrap();
        let start = coordinator_worktree(&repo, &slug(), at()).await.unwrap();
        assert_eq!(start.unwrap().branch, "coordinator/factory");
        assert_eq!(branches(&repo), "main\n");
    }

    #[test]
    fn worktrees_are_found_by_their_branch() {
        let list = "worktree /src/p\nHEAD 1\nbranch refs/heads/main\n\n\
                    worktree /wt/a\nHEAD 2\ndetached\n\n\
                    worktree /wt/b\nHEAD 3\nbranch refs/heads/coordinator/factory\n";
        assert_eq!(
            worktree_on(list, "refs/heads/coordinator/factory").as_deref(),
            Some("/wt/b")
        );
        assert_eq!(worktree_on(list, "refs/heads/other"), None);
    }
}
