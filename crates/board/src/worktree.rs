//! The git worktree a coordinator or worker thread starts in, so it never works in the
//! project root that other threads share.
//!
//! When the project folder is the top of a git repository, T3 creates the thread's branch
//! from the default branch and runs the thread in a new worktree on it. The branch is
//! `coordinator/<slug>` for a coordinator and `task/<id>` for a worker, or that name with
//! `-<UTC time>` when it exists: `t3 thread start` can only create a branch, never check out
//! one that exists, and an earlier thread's worktree may still use it, maybe with a live
//! thread or files git ignores. So nothing here removes a worktree or changes a branch.

use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use board_proto::{BoardSlug, TaskId, Timestamp};

use crate::t3::NewWorktree;

/// How long a git command may run, and whether a timeout may be a macOS privacy prompt.
#[derive(Clone, Copy)]
struct Wait {
    limit: Duration,
    on_folder: bool,
}

/// A git command that only reads the repository answers in about a second, unless something
/// holds it, such as a macOS privacy prompt for a protected folder, which git waits on with
/// no end. So a stuck start fails soon instead of spending the daemon's reconcile on one
/// board, with room left for a slow disk or the first run of macOS's git shim.
const LOCAL: Wait = Wait {
    limit: Duration::from_secs(20),
    on_folder: true,
};
/// A fetch, which waits on the network; the commands before it already read the folder.
const FETCH: Wait = Wait {
    limit: Duration::from_secs(60),
    on_folder: false,
};

/// The branch the coordinator of `slug` runs on when it is free.
pub fn coordinator_branch(slug: &BoardSlug) -> String {
    format!("coordinator/{slug}")
}

/// The branch the worker of `task` runs on when it is free.
pub fn worker_branch(task: &TaskId) -> String {
    format!("task/{task}")
}

/// The worktree to start the coordinator of `slug` in; `None` when `project` is not the top
/// of a git repository, so the thread runs in the folder itself.
pub async fn coordinator_worktree(
    project: &Path,
    slug: &BoardSlug,
    now: Timestamp,
) -> Result<Option<NewWorktree>> {
    new_worktree(project, coordinator_branch(slug), now).await
}

/// The worktree to start the worker of `task` in, as [`coordinator_worktree`].
pub async fn worker_worktree(
    project: &Path,
    task: &TaskId,
    now: Timestamp,
) -> Result<Option<NewWorktree>> {
    new_worktree(project, worker_branch(task), now).await
}

/// The branch the review judge of `task` runs on when it is free.
pub fn review_branch(task: &TaskId) -> String {
    format!("review/{task}")
}

/// The worktree to start the review judge of `task` in, as [`coordinator_worktree`]: on
/// `head`, the branch of the task's pull request, fetched first. Without a pull request,
/// or when its branch cannot be fetched (a fork's), it starts from the default branch.
pub async fn review_worktree(
    project: &Path,
    task: &TaskId,
    head: Option<&str>,
    now: Timestamp,
) -> Result<Option<NewWorktree>> {
    let Some(mut worktree) = new_worktree(project, review_branch(task), now).await? else {
        return Ok(None);
    };
    // A name git would read as an option is no branch.
    if let Some(head) = head.filter(|h| !h.is_empty() && !h.starts_with('-')) {
        match git(project, &["fetch", "--quiet", "origin", head], FETCH).await {
            Ok(out) if out.ok => worktree.base = format!("origin/{head}"),
            Ok(out) => tracing::warn!(
                "fetching {head} in {}: {}; the review of {task} starts from {}",
                project.display(),
                out.stderr.trim(),
                worktree.base
            ),
            Err(err) => tracing::warn!(
                "fetching {head} in {}: {err:#}; the review of {task} starts from {}",
                project.display(),
                worktree.base
            ),
        }
    }
    Ok(Some(worktree))
}

/// A worktree on `branch`, or on `branch` with the time when that one exists, from the
/// default branch; `None` when `project` is not the top of a git repository.
async fn new_worktree(
    project: &Path,
    mut branch: String,
    now: Timestamp,
) -> Result<Option<NewWorktree>> {
    if !is_repo_top(project).await? {
        return Ok(None);
    }
    let base = default_base(project).await?;
    if has_ref(project, &format!("refs/heads/{branch}")).await? {
        branch = format!("{branch}-{}", now.format("%Y%m%d-%H%M%S"));
    }
    Ok(Some(NewWorktree { base, branch }))
}

/// Whether `project` is the top folder of a git work tree. A folder that does not exist is
/// not (T3 then reports the missing project), nor is one nested in another repository.
/// git alone answers, with a link followed as git follows it, so a folder macOS holds
/// for a prompt or refused fails here with the privacy hint instead of passing as no
/// repository, and nothing is left waiting on it once the command is stopped.
async fn is_repo_top(project: &Path) -> Result<bool> {
    let out = git(
        project,
        &["rev-parse", "--is-inside-work-tree", "--show-prefix"],
        LOCAL,
    )
    .await?;
    if !out.ok {
        if out.stderr.contains("not a git repository")
            || out.stderr.contains("No such file or directory")
        {
            return Ok(false);
        }
        bail!(
            "git rev-parse failed in {}: {}",
            project.display(),
            out.stderr.trim()
        );
    }
    // `true` and an empty prefix at the top; `false` inside `.git`.
    let mut lines = out.stdout.lines();
    Ok(lines.next() == Some("true") && lines.next().is_none_or(str::is_empty))
}

/// The ref new worktrees start from, fetched first when it is a remote branch:
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
        LOCAL,
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
            "{} has no origin/HEAD, origin/main, origin/master, main or master to start new worktrees from; set origin/HEAD (git remote set-head origin --auto)",
            project.display()
        );
    };
    if let Some(name) = base.strip_prefix("origin/") {
        // A stale remote branch would start the thread, and its children, on old code.
        match git(project, &["fetch", "--quiet", "origin", name], FETCH).await {
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
    let out = git(project, &["rev-parse", "--verify", "--quiet", full], LOCAL).await?;
    Ok(out.ok)
}

struct GitOutput {
    ok: bool,
    stdout: String,
    stderr: String,
}

/// Runs `git -C <project> <args>` and returns its output, stopping it after `wait.limit`.
async fn git(project: &Path, args: &[&str], wait: Wait) -> Result<GitOutput> {
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
    let Ok(out) = tokio::time::timeout(wait.limit, child.wait_with_output()).await else {
        let hint = if wait.on_folder {
            privacy_hint_for(project).await
        } else {
            ""
        };
        bail!(
            "git {} timed out in {}{hint}",
            args.join(" "),
            project.display()
        );
    };
    let out = out.context("running git")?;
    let ok = out.status.success();
    let mut stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    if denied(ok, &stderr) {
        stderr = format!("{}{}", stderr.trim_end(), privacy_hint_for(project).await);
    }
    Ok(GitOutput {
        ok,
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr,
    })
}

/// Whether git failed on a folder macOS denied it, as it does once the user refused the
/// privacy prompt.
fn denied(ok: bool, stderr: &str) -> bool {
    !ok && stderr.contains("Operation not permitted")
}

/// [`privacy_hint`] for `project` on this Mac.
async fn privacy_hint_for(project: &Path) -> &'static str {
    if !cfg!(target_os = "macos") {
        return "";
    }
    hint_through_links(dirs::home_dir().as_deref(), project)
        .await
        .unwrap_or_default()
}

/// [`privacy_hint`] for `project` with its links followed, so a link to a protected folder
/// counts as one. The path is read one folder at a time from the root and the hint checked
/// before each read, so nothing inside a protected folder is read: that read would wait on
/// the same prompt. A read that takes over a second gives up.
async fn hint_through_links(home: Option<&Path>, project: &Path) -> Option<&'static str> {
    const MAX_LINKS: usize = 16;
    let mut path = project.to_path_buf();
    for _ in 0..=MAX_LINKS {
        let parts: Vec<Component> = path.components().collect();
        let mut prefix = PathBuf::new();
        let mut next = None;
        for (i, part) in parts.iter().enumerate() {
            match part {
                Component::CurDir => continue,
                // `prefix` has no links, so going up is going to its parent.
                Component::ParentDir => {
                    prefix.pop();
                    continue;
                }
                Component::Normal(_) => prefix.push(part),
                Component::RootDir | Component::Prefix(_) => {
                    prefix.push(part);
                    continue;
                }
            }
            if let Some(hint) = privacy_hint(home, &prefix) {
                return Some(hint);
            }
            let read = tokio::time::timeout(Duration::from_secs(1), tokio::fs::read_link(&prefix));
            match read.await {
                Ok(Ok(target)) => {
                    let mut resolved = prefix.parent().unwrap_or(&prefix).join(target);
                    resolved.extend(&parts[i + 1..]);
                    next = Some(resolved);
                    break;
                }
                // Not a link, or not there.
                Ok(Err(_)) => {}
                Err(_) => return None,
            }
        }
        path = next?;
    }
    None
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
    use super::*;

    #[tokio::test]
    async fn a_link_to_a_protected_folder_gets_the_hint_without_reading_inside_it() {
        let dir = tempfile::tempdir().unwrap();
        // Resolved, as a home folder has no links on the way (/var is one, to /private/var).
        let home = &dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(home.join("Documents/code/repo")).unwrap();
        std::fs::create_dir(home.join("Developer")).unwrap();
        std::os::unix::fs::symlink("Documents/code", home.join("code")).unwrap();
        std::os::unix::fs::symlink("../code/repo", home.join("Developer/repo")).unwrap();
        let hint = |project: PathBuf| async move { hint_through_links(Some(home), &project).await };
        assert!(hint(home.join("Documents/x")).await.is_some(), "no link");
        assert!(
            hint(home.join("code/repo")).await.is_some(),
            "a folder on the way"
        );
        assert!(
            hint(home.join("Developer/repo")).await.is_some(),
            "relative, twice"
        );
        assert_eq!(hint(home.join("Developer")).await, None);
        assert_eq!(hint(home.join("gone/repo")).await, None);
        // A link to itself ends at the limit.
        std::os::unix::fs::symlink("loop", home.join("loop")).unwrap();
        assert_eq!(hint(home.join("loop/repo")).await, None);
    }

    #[tokio::test]
    async fn a_folder_git_cannot_enter_is_an_error_not_a_folder_outside_git() {
        let dir = tempfile::tempdir().unwrap();
        // A file stands in for the folder: git cannot change into it, as into one macOS
        // refused, and the start must stop rather than run in the project folder.
        let file = dir.path().join("file");
        std::fs::write(&file, "").unwrap();
        let err = is_repo_top(&file).await.expect_err("an error");
        assert!(
            format!("{err:#}").contains("git rev-parse failed in"),
            "{err:#}"
        );
    }

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
    fn only_a_failure_macos_denied_counts_as_denied() {
        let refused = "fatal: cannot change to '/Users/u/Documents/x': Operation not permitted\n";
        assert!(denied(false, refused));
        assert!(!denied(false, "fatal: not a git repository\n"));
        assert!(!denied(true, refused), "not on success");
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
        let wait = Wait {
            limit: Duration::from_millis(300),
            ..LOCAL
        };
        let err = git(dir.path(), &["hash-object", "fifo"], wait)
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
    async fn a_worker_branch_is_named_after_its_task_and_never_reused() {
        let dir = tempfile::tempdir().unwrap();
        let repo = clone(dir.path(), "main");
        let task: TaskId = "factory-7".parse().unwrap();
        let first = worker_worktree(&repo, &task, at()).await.unwrap();
        assert_eq!(
            first,
            Some(NewWorktree {
                base: "origin/main".into(),
                branch: "task/factory-7".into()
            })
        );
        run(&repo, &["branch", "task/factory-7", "origin/main"]);
        let again = worker_worktree(&repo, &task, at()).await.unwrap().unwrap();
        assert_eq!(again.branch, "task/factory-7-20261108-132005");
    }

    #[tokio::test]
    async fn a_review_starts_on_the_pull_requests_branch_once_fetched() {
        let dir = tempfile::tempdir().unwrap();
        let repo = clone(dir.path(), "main");
        let task: TaskId = "factory-7".parse().unwrap();
        // The branch exists on the remote only: the clone has not fetched it.
        run(
            &dir.path().join("origin"),
            &["branch", "task/factory-7", "main"],
        );
        let on_head = review_worktree(&repo, &task, Some("task/factory-7"), at())
            .await
            .unwrap();
        assert_eq!(
            on_head,
            Some(NewWorktree {
                base: "origin/task/factory-7".into(),
                branch: "review/factory-7".into()
            })
        );
        run(&repo, &["rev-parse", "--verify", "origin/task/factory-7"]);
        // No pull request, a branch the remote lacks, or a name that is an option: the
        // default branch.
        for head in [None, Some("task/gone"), Some("--upload-pack=x")] {
            let worktree = review_worktree(&repo, &task, head, at()).await.unwrap();
            assert_eq!(worktree.unwrap().base, "origin/main", "{head:?}");
        }
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
