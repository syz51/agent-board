//! The git worktree a worker or judge runs in, so it never works in the project root that
//! other threads share (`docs/SCHEDULER_DESIGN.md`, Section 4.2).
//!
//! Includes both:
//! - Branch/ref resolution (`worker_worktree`, `review_worktree`) preserving `origin/HEAD` ->
//!   `origin/main` -> `origin/master` -> `main` -> `master` resolution, 60s network fetches,
//!   PR/task-branch fetches for review judges, and `-YYYYMMDD-HHMMSS` collision suffixes;
//! - Daemon-owned git worktree lifecycle ([`WorktreeManager`], [`WorktreeStore`],
//!   [`WorktreeRecord`], [`WorktreeHandle`]) that creates isolated worktrees under
//!   `$BOARD_HOME/worktrees/<repo-slug>/<run-or-task-id>`, tracks them behind a clean
//!   persistence boundary (ready for Slice 1.3 `daemon.db`), and safely cleans up clean
//!   ended worktrees while refusing to delete dirty worktrees silently (recording a Trail
//!   Rule entry in `$BOARD_HOME/state/worktree-events.jsonl`).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use board_proto::{TaskId, Timestamp};
use serde::{Deserialize, Serialize};

use crate::config::Home;
use crate::state;
use crate::t3::NewWorktree;

/// macOS TCC privacy hint appended to git failures or timeouts on protected folders.
pub const PRIVACY_HINT: &str = ": macOS may be keeping board out of this folder. Answer its \
    prompt if one is open, or turn on board under System Settings > Privacy & Security > Files \
    and Folders";

/// Typed error for a macOS TCC privacy refusal on a protected folder (`~/Documents`,
/// `~/Desktop`, `~/Downloads`, `~/Library/Mobile Documents`, `~/Library/CloudStorage`,
/// `/Volumes/...`, or a symlink into one): timeout, `Operation not permitted` (`EPERM`),
/// `Interrupted system call` (`EINTR`), or unreadable `.git`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrivacyRefusal {
    pub project: PathBuf,
    pub detail: String,
}

impl std::fmt::Display for PrivacyRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.detail)
    }
}

impl std::error::Error for PrivacyRefusal {}

/// Returns the [`PrivacyRefusal`] cause when `err` came from a macOS TCC privacy refusal on a
/// protected folder.
pub fn privacy_refusal(err: &anyhow::Error) -> Option<&PrivacyRefusal> {
    err.chain()
        .find_map(|cause| cause.downcast_ref::<PrivacyRefusal>())
}

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
/// Creating or removing a worktree on disk (may check out many files); the commands before it
/// already verified folder access.
const WORKTREE_OP: Wait = Wait {
    limit: Duration::from_secs(60),
    on_folder: false,
};

/// Named state file holding daemon-managed worktree records until Slice 1.3 migrates them
/// into `daemon.db`'s `worktrees` table.
pub const WORKTREES_STATE_NAME: &str = "worktrees";

/// Maximum number of dirty `git status --porcelain` lines recorded per Trail Rule entry.
const DIRTY_ENTRIES_MAX: usize = 50;

/// Handle to an isolated git worktree created and owned by [`WorktreeManager`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeHandle {
    /// Run or task identifier uniquely naming this managed worktree record.
    pub id: String,
    /// Normalized repository slug used as the parent folder under `$BOARD_HOME/worktrees/`.
    pub repo_slug: String,
    /// Path of the source git repository checkout (`git -C <repo_root>`).
    pub repo_root: PathBuf,
    /// Path of the isolated git worktree directory.
    pub path: PathBuf,
    /// Branch checked out in `path`.
    pub branch: String,
    /// Base ref `branch` was created from (e.g. `origin/main` or `origin/feat/x`).
    pub base: String,
}

/// Lifecycle state of a tracked daemon-owned worktree record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorktreeStatus {
    Active,
    Released,
    /// Worktree had uncommitted or untracked changes and was intentionally left untouched.
    DirtySkipped,
    /// A transient error (such as lock contention or timeout) prevented cleanup; eligible for
    /// retry on later [`WorktreeManager::cleanup_ended`] passes.
    CleanupError,
    #[serde(other)]
    Unknown,
}

/// Persisted metadata for one daemon-managed git worktree (`daemon.db` `worktrees` table
/// shape for Slice 1.3 `agent-board-679`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeRecord {
    pub id: String,
    pub repo_slug: String,
    pub repo_root: PathBuf,
    pub path: PathBuf,
    pub branch: String,
    pub base: String,
    pub status: WorktreeStatus,
    pub created_at: Timestamp,
    pub last_used_at: Timestamp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cleanup_note: Option<String>,
}

/// Result of [`WorktreeManager::prepare_review`]: a created worktree plus how its base ref
/// was chosen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedReviewWorktree {
    pub handle: WorktreeHandle,
    pub source: ReviewBase,
    pub fetch_error: Option<String>,
}

/// Result of [`WorktreeManager::cleanup`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CleanupOutcome {
    /// Clean worktree was removed via `git worktree remove` (preserving non-review branch
    /// refs and deleting daemon-created ephemeral `review/*` branch refs when they have no
    /// unique unpushed commits).
    Removed {
        id: String,
        path: PathBuf,
        branch: String,
    },
    /// Worktree directory was already missing on disk; git metadata pruned, any safe
    /// ephemeral `review/*` branch ref deleted, and record dropped.
    AlreadyGone {
        id: String,
        path: PathBuf,
        branch: String,
    },
    /// Worktree had uncommitted, staged, or untracked changes: left untouched on disk and
    /// recorded in `trail_path` (Trail Rule).
    SkippedDirty {
        id: String,
        path: PathBuf,
        branch: String,
        dirty_entries: Vec<String>,
        trail_path: PathBuf,
    },
    /// Checking or removing the worktree failed: left untouched on disk and recorded in
    /// `trail_path` (Trail Rule).
    SkippedError {
        id: String,
        path: PathBuf,
        branch: String,
        error: String,
        trail_path: PathBuf,
    },
}

/// Persistence boundary for daemon-managed [`WorktreeRecord`]s.
///
/// Backed by [`JsonWorktreeStore`] (`$BOARD_HOME/state/worktrees.json`) today and designed
/// to be backed by `daemon.db`'s `worktrees` table in Slice 1.3 (`agent-board-679`) with no
/// changes to [`WorktreeManager`] or callers.
pub trait WorktreeStore: Send + Sync {
    fn list(&self) -> Result<Vec<WorktreeRecord>>;
    fn get(&self, id: &str) -> Result<Option<WorktreeRecord>>;
    fn upsert(&self, record: WorktreeRecord) -> Result<()>;
    fn remove(&self, id: &str) -> Result<()>;
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct WorktreeFile {
    #[serde(default, deserialize_with = "lenient_worktree_map")]
    records: BTreeMap<String, WorktreeRecord>,
}

fn lenient_worktree_map<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<BTreeMap<String, WorktreeRecord>, D::Error> {
    let serde_json::Value::Object(entries) = serde_json::Value::deserialize(d)? else {
        return Ok(BTreeMap::new());
    };
    Ok(entries
        .into_iter()
        .filter_map(|(key, value)| match serde_json::from_value(value) {
            Ok(record) => Some((key, record)),
            Err(err) => {
                tracing::warn!("worktree state: skipped unreadable entry {key:?}: {err}");
                None
            }
        })
        .collect())
}

/// File-backed [`WorktreeStore`] storing records atomically in `$BOARD_HOME/state/worktrees.json`.
pub struct JsonWorktreeStore {
    home: Home,
    lock: Mutex<()>,
}

impl JsonWorktreeStore {
    pub fn new(home: Home) -> Self {
        Self {
            home,
            lock: Mutex::new(()),
        }
    }

    fn load_locked(&self) -> WorktreeFile {
        state::load_named_keeping_unreadable(&self.home, WORKTREES_STATE_NAME)
    }

    fn save_locked(&self, file: &WorktreeFile) -> Result<()> {
        state::save_named(&self.home, WORKTREES_STATE_NAME, file)
    }
}

impl WorktreeStore for JsonWorktreeStore {
    fn list(&self) -> Result<Vec<WorktreeRecord>> {
        let _guard = self.lock.lock().unwrap_or_else(PoisonError::into_inner);
        Ok(self.load_locked().records.into_values().collect())
    }

    fn get(&self, id: &str) -> Result<Option<WorktreeRecord>> {
        let _guard = self.lock.lock().unwrap_or_else(PoisonError::into_inner);
        Ok(self.load_locked().records.remove(id))
    }

    fn upsert(&self, record: WorktreeRecord) -> Result<()> {
        let _guard = self.lock.lock().unwrap_or_else(PoisonError::into_inner);
        let mut file = self.load_locked();
        file.records.insert(record.id.clone(), record);
        self.save_locked(&file)
    }

    fn remove(&self, id: &str) -> Result<()> {
        let _guard = self.lock.lock().unwrap_or_else(PoisonError::into_inner);
        let mut file = self.load_locked();
        if file.records.remove(id).is_some() {
            self.save_locked(&file)?;
        }
        Ok(())
    }
}

/// In-memory [`WorktreeStore`] for isolated unit tests.
#[derive(Default)]
pub struct InMemoryWorktreeStore {
    records: Mutex<BTreeMap<String, WorktreeRecord>>,
}

impl WorktreeStore for InMemoryWorktreeStore {
    fn list(&self) -> Result<Vec<WorktreeRecord>> {
        let guard = self.records.lock().unwrap_or_else(PoisonError::into_inner);
        Ok(guard.values().cloned().collect())
    }

    fn get(&self, id: &str) -> Result<Option<WorktreeRecord>> {
        let guard = self.records.lock().unwrap_or_else(PoisonError::into_inner);
        Ok(guard.get(id).cloned())
    }

    fn upsert(&self, record: WorktreeRecord) -> Result<()> {
        let mut guard = self.records.lock().unwrap_or_else(PoisonError::into_inner);
        guard.insert(record.id.clone(), record);
        Ok(())
    }

    fn remove(&self, id: &str) -> Result<()> {
        let mut guard = self.records.lock().unwrap_or_else(PoisonError::into_inner);
        guard.remove(id);
        Ok(())
    }
}

/// Normalizes a single directory-name segment for `$BOARD_HOME/worktrees/<repo-slug>/<id>`:
/// keeps ASCII alphanumerics, `-`, `_`, and `.`, maps everything else to `-`, trims leading
/// dots/hyphens so `.` / `..` / hidden names cannot appear, and falls back to `"unnamed"`.
pub fn safe_segment(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut last_dash = false;
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '_' | '.') {
            out.push(ch);
            last_dash = false;
        } else if !last_dash && !out.is_empty() {
            out.push('-');
            last_dash = true;
        }
    }
    let trimmed = out.trim_matches(|c| c == '-' || c == '.');
    if trimmed.is_empty() {
        "unnamed".to_owned()
    } else {
        trimmed.to_owned()
    }
}

/// Derives `<repo-slug>` from a normalized repo key (e.g. `"github.com/org/agent-board"`)
/// or, when none is configured, from `project`'s directory name.
pub fn repo_slug(repo_key: Option<&str>, project: &Path) -> String {
    if let Some(key) = repo_key.map(str::trim).filter(|k| !k.is_empty()) {
        return safe_segment(key);
    }
    let name = project
        .file_name()
        .map(|n| n.to_string_lossy())
        .unwrap_or_default();
    safe_segment(&name)
}

/// Creates, tracks, and safely cleans up daemon-owned git worktrees under
/// `$BOARD_HOME/worktrees/<repo-slug>/<run-or-task-id>`.
#[derive(Clone)]
pub struct WorktreeManager {
    home: Home,
    root: PathBuf,
    store: Arc<dyn WorktreeStore>,
}

impl WorktreeManager {
    /// Creates a manager storing worktrees under `$BOARD_HOME/worktrees` and persisting
    /// records in `$BOARD_HOME/state/daemon.db`'s `worktrees` table (importing any legacy
    /// `state/worktrees.json` on first open; falls back to [`JsonWorktreeStore`] only if
    /// SQLite cannot open).
    pub fn for_home(home: Home) -> Self {
        let store: Arc<dyn WorktreeStore> =
            match crate::local_db::SqliteWorktreeStore::open_for_home(&home) {
                Ok(sqlite) => Arc::new(sqlite),
                Err(err) => {
                    tracing::warn!(
                        "opening daemon.db for worktree persistence failed ({err:#}); falling back to worktrees.json"
                    );
                    Arc::new(JsonWorktreeStore::new(home.clone()))
                }
            };
        Self::with_store(home, store)
    }

    /// Creates a manager with an explicit [`WorktreeStore`] backend (for tests or Slice 1.3
    /// `daemon.db`).
    pub fn with_store(home: Home, store: Arc<dyn WorktreeStore>) -> Self {
        let root = home.root().join("worktrees");
        Self { home, root, store }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn store(&self) -> &Arc<dyn WorktreeStore> {
        &self.store
    }

    pub fn records(&self) -> Result<Vec<WorktreeRecord>> {
        self.store.list()
    }

    /// Creates an isolated git worktree for `spec` at
    /// `$BOARD_HOME/worktrees/<repo-slug>/<id>` via `git worktree add -b <branch> <path> <base>`.
    ///
    /// - If `spec.branch` already exists in `project`, strips any existing
    ///   `-YYYYMMDD-HHMMSS(-N)` suffix and appends `-YYYYMMDD-HHMMSS` (plus a numeric suffix
    ///   if called twice in the same second) so existing branches and worktrees are never
    ///   overwritten and retries never nest timestamps.
    /// - If the target directory already exists on disk, suffixes the directory name so an
    ///   earlier dirty worktree is never clobbered.
    /// - Persists the [`WorktreeRecord`] before calling `git worktree add` (write-before-act),
    ///   removing the record if `git` fails without creating the directory.
    pub async fn create(
        &self,
        project: &Path,
        repo_key: Option<&str>,
        id: &str,
        spec: &NewWorktree,
        now: Timestamp,
    ) -> Result<WorktreeHandle> {
        let slug = repo_slug(repo_key, project);
        let branch = unique_branch(project, spec.branch.clone(), now).await?;
        let base_dir = self.root.join(&slug);
        tokio::fs::create_dir_all(&base_dir)
            .await
            .with_context(|| format!("creating {}", base_dir.display()))?;

        let (record_id, path) = unique_worktree_entry(&self.store, &slug, &base_dir, id, now)?;
        let record = WorktreeRecord {
            id: record_id.clone(),
            repo_slug: slug.clone(),
            repo_root: project.to_path_buf(),
            path: path.clone(),
            branch: branch.clone(),
            base: spec.base.clone(),
            status: WorktreeStatus::Active,
            created_at: now,
            last_used_at: now,
            cleanup_note: None,
        };
        // Write-before-act: record intent before git creates files on disk.
        self.store.upsert(record)?;

        let path_str = path.to_string_lossy().into_owned();
        let out = git(
            project,
            &[
                "worktree", "add", "--quiet", "-b", &branch, &path_str, &spec.base,
            ],
            WORKTREE_OP,
        )
        .await;
        match out {
            Ok(out) if out.ok => Ok(WorktreeHandle {
                id: record_id,
                repo_slug: slug,
                repo_root: project.to_path_buf(),
                path,
                branch,
                base: spec.base.clone(),
            }),
            Ok(out) => {
                if !path.exists() {
                    let _ = self.store.remove(&record_id);
                }
                bail!(
                    "git worktree add -b {branch} {} {} in {} failed: {}",
                    path.display(),
                    spec.base,
                    project.display(),
                    out.stderr.trim()
                );
            }
            Err(err) => {
                if !path.exists() {
                    let _ = self.store.remove(&record_id);
                }
                Err(err)
            }
        }
    }

    /// Resolves and creates a daemon-owned worktree for a worker of `task`; returns
    /// `Ok(None)` when `project` is not the top of a git repository (non-git folders start
    /// in-place cleanly).
    pub async fn prepare_worker(
        &self,
        project: &Path,
        repo_key: Option<&str>,
        task: &TaskId,
        run_id: &str,
        now: Timestamp,
    ) -> Result<Option<WorktreeHandle>> {
        let Some(spec) = worker_worktree(project, task, now).await? else {
            return Ok(None);
        };
        let handle = self.create(project, repo_key, run_id, &spec, now).await?;
        Ok(Some(handle))
    }

    /// Resolves and creates a daemon-owned worktree for a review judge of `task`; returns
    /// `Ok(None)` when `project` is not the top of a git repository.
    pub async fn prepare_review(
        &self,
        project: &Path,
        repo_key: Option<&str>,
        task: &TaskId,
        run_id: &str,
        target: ReviewTarget<'_>,
        now: Timestamp,
    ) -> Result<Option<PreparedReviewWorktree>> {
        let Some(resolved) = review_worktree(project, task, target, now).await? else {
            return Ok(None);
        };
        let handle = self
            .create(project, repo_key, run_id, &resolved.worktree, now)
            .await?;
        Ok(Some(PreparedReviewWorktree {
            handle,
            source: resolved.source,
            fetch_error: resolved.fetch_error,
        }))
    }

    /// Safely cleans up the tracked worktree `id` after its run has ended.
    ///
    /// - Clean worktrees are removed with `git worktree remove <path>` (without `--force`)
    ///   and dropped from the store. Non-review branch refs (such as `task/*`) are preserved;
    ///   daemon-created ephemeral `review/*` branch refs are deleted via `git branch -D` once
    ///   verified to have no unique unpushed commits (otherwise preserved and recorded in
    ///   `$BOARD_HOME/state/worktree-events.jsonl` per the Trail Rule).
    /// - Dirty worktrees (uncommitted, staged, or untracked files) are **never** deleted
    ///   silently: they (and their local branches) are left intact on disk, marked
    ///   [`WorktreeStatus::DirtySkipped`], and recorded in
    ///   `$BOARD_HOME/state/worktree-events.jsonl` (Trail Rule).
    /// - Missing directories are pruned best-effort, any safe ephemeral `review/*` branch ref
    ///   is deleted, and the record is dropped from the store.
    pub async fn cleanup(&self, id: &str, now: Timestamp) -> Result<Option<CleanupOutcome>> {
        let Some(mut record) = self.store.get(id)? else {
            return Ok(None);
        };
        if !record.path.exists() {
            let branch_cleanup = if record.repo_root.exists() {
                let _ = git(&record.repo_root, &["worktree", "prune"], LOCAL).await;
                self.finalize_branch_cleanup(&record, now).await?
            } else {
                BranchCleanup::AlreadyAbsent
            };
            let _ = state::append_worktree_event(
                &self.home,
                &serde_json::json!({
                    "at": now,
                    "action": "already_gone",
                    "id": record.id,
                    "repo_slug": record.repo_slug,
                    "repo_root": record.repo_root,
                    "path": record.path,
                    "branch": record.branch,
                    "base": record.base,
                    "branch_deleted": matches!(branch_cleanup, BranchCleanup::Deleted),
                }),
            );
            self.store.remove(id)?;
            return Ok(Some(CleanupOutcome::AlreadyGone {
                id: record.id,
                path: record.path,
                branch: record.branch,
            }));
        }

        match git(
            &record.path,
            &["status", "--porcelain", "--untracked-files=normal"],
            LOCAL,
        )
        .await
        {
            Ok(out) if out.ok => {
                let all_dirty: Vec<String> = out
                    .stdout
                    .lines()
                    .filter(|l| !l.trim().is_empty())
                    .map(str::to_owned)
                    .collect();
                if !all_dirty.is_empty() {
                    let dirty_count = all_dirty.len();
                    let dirty_entries: Vec<String> =
                        all_dirty.into_iter().take(DIRTY_ENTRIES_MAX).collect();
                    let trail_path = state::append_worktree_event(
                        &self.home,
                        &serde_json::json!({
                            "at": now,
                            "action": "cleanup_skipped_dirty",
                            "id": record.id,
                            "repo_slug": record.repo_slug,
                            "repo_root": record.repo_root,
                            "path": record.path,
                            "branch": record.branch,
                            "base": record.base,
                            "dirty_count": dirty_count,
                            "dirty_entries": dirty_entries,
                            "reason": "refused to remove worktree with uncommitted or untracked files",
                        }),
                    )?;
                    record.status = WorktreeStatus::DirtySkipped;
                    record.last_used_at = now;
                    record.cleanup_note = Some(format!(
                        "dirty worktree left intact ({dirty_count} entries); see {}",
                        trail_path.display()
                    ));
                    self.store.upsert(record.clone())?;
                    tracing::warn!(
                        "worktree {} ({}) has {dirty_count} uncommitted/untracked entries; \
                         left intact on disk (trail: {})",
                        record.id,
                        record.path.display(),
                        trail_path.display()
                    );
                    return Ok(Some(CleanupOutcome::SkippedDirty {
                        id: record.id,
                        path: record.path,
                        branch: record.branch,
                        dirty_entries,
                        trail_path,
                    }));
                }
            }
            Ok(out) => {
                let error = format!("git status failed: {}", out.stderr.trim());
                return self.record_cleanup_error(record, error, now).map(Some);
            }
            Err(err) => {
                let error = format!("git status failed: {err:#}");
                return self.record_cleanup_error(record, error, now).map(Some);
            }
        }

        let path_str = record.path.to_string_lossy().into_owned();
        match git(
            &record.repo_root,
            &["worktree", "remove", &path_str],
            WORKTREE_OP,
        )
        .await
        {
            Ok(out) if out.ok => {
                let branch_cleanup = self.finalize_branch_cleanup(&record, now).await?;
                let _ = state::append_worktree_event(
                    &self.home,
                    &serde_json::json!({
                        "at": now,
                        "action": "removed",
                        "id": record.id,
                        "repo_slug": record.repo_slug,
                        "repo_root": record.repo_root,
                        "path": record.path,
                        "branch": record.branch,
                        "base": record.base,
                        "branch_deleted": matches!(branch_cleanup, BranchCleanup::Deleted),
                    }),
                );
                self.store.remove(id)?;
                Ok(Some(CleanupOutcome::Removed {
                    id: record.id,
                    path: record.path,
                    branch: record.branch,
                }))
            }
            Ok(out) => {
                let error = format!("git worktree remove failed: {}", out.stderr.trim());
                self.record_cleanup_error(record, error, now).map(Some)
            }
            Err(err) => {
                let error = format!("git worktree remove failed: {err:#}");
                self.record_cleanup_error(record, error, now).map(Some)
            }
        }
    }

    async fn finalize_branch_cleanup(
        &self,
        record: &WorktreeRecord,
        now: Timestamp,
    ) -> Result<BranchCleanup> {
        let outcome = cleanup_ephemeral_review_branch(record).await;
        match &outcome {
            BranchCleanup::KeptUnpushed {
                unique_commits,
                head_sha,
            } => {
                let trail_path = state::append_worktree_event(
                    &self.home,
                    &serde_json::json!({
                        "at": now,
                        "action": "branch_cleanup_skipped_unpushed",
                        "id": record.id,
                        "repo_slug": record.repo_slug,
                        "repo_root": record.repo_root,
                        "path": record.path,
                        "branch": record.branch,
                        "base": record.base,
                        "head_sha": head_sha,
                        "unique_commits": unique_commits,
                        "reason": "refused to delete local review branch with unique unpushed commits",
                    }),
                )?;
                tracing::warn!(
                    "review branch {} for worktree {} has {unique_commits} unique unpushed \
                     commit(s) (HEAD {head_sha}); left intact (trail: {})",
                    record.branch,
                    record.id,
                    trail_path.display()
                );
            }
            BranchCleanup::Failed { error } => {
                let trail_path = state::append_worktree_event(
                    &self.home,
                    &serde_json::json!({
                        "at": now,
                        "action": "branch_cleanup_failed",
                        "id": record.id,
                        "repo_slug": record.repo_slug,
                        "repo_root": record.repo_root,
                        "path": record.path,
                        "branch": record.branch,
                        "base": record.base,
                        "error": error,
                    }),
                )?;
                tracing::warn!(
                    "could not delete ephemeral review branch {} for worktree {} ({error}); \
                     left intact (trail: {})",
                    record.branch,
                    record.id,
                    trail_path.display()
                );
            }
            BranchCleanup::PreservedNonReview
            | BranchCleanup::AlreadyAbsent
            | BranchCleanup::Deleted => {}
        }
        Ok(outcome)
    }

    fn record_cleanup_error(
        &self,
        mut record: WorktreeRecord,
        error: String,
        now: Timestamp,
    ) -> Result<CleanupOutcome> {
        let trail_path = state::append_worktree_event(
            &self.home,
            &serde_json::json!({
                "at": now,
                "action": "cleanup_skipped_error",
                "id": record.id,
                "repo_slug": record.repo_slug,
                "repo_root": record.repo_root,
                "path": record.path,
                "branch": record.branch,
                "base": record.base,
                "error": error,
            }),
        )?;
        record.status = WorktreeStatus::CleanupError;
        record.last_used_at = now;
        record.cleanup_note = Some(format!("{error}; see {}", trail_path.display()));
        self.store.upsert(record.clone())?;
        tracing::warn!(
            "worktree {} ({}) could not be removed ({error}); left intact on disk (trail: {})",
            record.id,
            record.path.display(),
            trail_path.display()
        );
        Ok(CleanupOutcome::SkippedError {
            id: record.id,
            path: record.path,
            branch: record.branch,
            error,
            trail_path,
        })
    }

    /// Cleans up all tracked worktrees whose `id` is not in `active_ids` and that have not
    /// been marked [`WorktreeStatus::DirtySkipped`] (transient [`WorktreeStatus::CleanupError`]
    /// records are retried automatically).
    pub async fn cleanup_ended(
        &self,
        active_ids: &BTreeSet<String>,
        now: Timestamp,
    ) -> Result<Vec<CleanupOutcome>> {
        let mut outcomes = Vec::new();
        for record in self.store.list()? {
            if active_ids.contains(&record.id) || record.status == WorktreeStatus::DirtySkipped {
                continue;
            }
            if let Some(outcome) = self.cleanup(&record.id, now).await? {
                outcomes.push(outcome);
            }
        }
        Ok(outcomes)
    }
}

/// Whether `branch` is a daemon-created ephemeral review branch (`review/<task>...`) that
/// may be deleted locally once its worktree is cleaned up (provided it has no unique
/// unpushed commits).
fn is_ephemeral_review_branch(branch: &str) -> bool {
    branch.starts_with("review/") && valid_branch_name(branch)
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum BranchCleanup {
    /// Non-review branch (e.g. `task/*`): always preserved.
    PreservedNonReview,
    /// Local ref did not exist in `repo_root` (already removed).
    AlreadyAbsent,
    /// Local ephemeral `review/*` branch deleted via `git branch -D`.
    Deleted,
    /// Local `review/*` branch had commits not reachable from `base` or any remote ref;
    /// preserved so no commits are ever lost silently.
    KeptUnpushed {
        unique_commits: u64,
        head_sha: String,
    },
    /// Checking or deleting the local `review/*` branch failed; preserved and recorded.
    Failed { error: String },
}

async fn cleanup_ephemeral_review_branch(record: &WorktreeRecord) -> BranchCleanup {
    if !is_ephemeral_review_branch(&record.branch) {
        return BranchCleanup::PreservedNonReview;
    }
    if !record.repo_root.exists() {
        return BranchCleanup::Failed {
            error: format!("repo_root {} does not exist", record.repo_root.display()),
        };
    }
    let full_ref = format!("refs/heads/{}", record.branch);
    let head_sha = match git(
        &record.repo_root,
        &["rev-parse", "--verify", "--quiet", &full_ref],
        LOCAL,
    )
    .await
    {
        Ok(out) if out.ok => out.stdout.trim().to_owned(),
        Ok(out) if out.stderr.trim().is_empty() => return BranchCleanup::AlreadyAbsent,
        Ok(out) => {
            return BranchCleanup::Failed {
                error: format!("checking ref {full_ref}: {}", out.stderr.trim()),
            };
        }
        Err(err) => {
            return BranchCleanup::Failed {
                error: format!("checking ref {full_ref}: {err:#}"),
            };
        }
    };

    match count_unique_unpushed_commits(&record.repo_root, &record.base, &head_sha).await {
        Ok(0) => {}
        Ok(unique_commits) => {
            return BranchCleanup::KeptUnpushed {
                unique_commits,
                head_sha,
            };
        }
        Err(err) => {
            return BranchCleanup::Failed {
                error: format!(
                    "checking unpushed commits on {} against {}: {err:#}",
                    record.branch, record.base
                ),
            };
        }
    }

    match git(
        &record.repo_root,
        &["branch", "-D", "--", &record.branch],
        LOCAL,
    )
    .await
    {
        Ok(out) if out.ok => BranchCleanup::Deleted,
        Ok(out) => BranchCleanup::Failed {
            error: format!(
                "git branch -D {} failed: {}",
                record.branch,
                out.stderr.trim()
            ),
        },
        Err(err) => BranchCleanup::Failed {
            error: format!("git branch -D {} failed: {err:#}", record.branch),
        },
    }
}

/// Returns how many commits reachable from `head_sha` are NOT reachable from `base` (when
/// `base` resolves locally) AND NOT reachable from any remote-tracking ref (`--remotes`).
///
/// Returns `0` when the branch points at `base`, is an ancestor of `base`, or every commit on
/// it is already present on a remote ref.
async fn count_unique_unpushed_commits(
    repo_root: &Path,
    base: &str,
    head_sha: &str,
) -> Result<u64> {
    if valid_branch_name(base) {
        let base_commit_ref = format!("{base}^{{commit}}");
        let base_out = git(
            repo_root,
            &["rev-parse", "--verify", "--quiet", &base_commit_ref],
            LOCAL,
        )
        .await?;
        if base_out.ok {
            let base_sha = base_out.stdout.trim();
            if base_sha == head_sha {
                return Ok(0);
            }
            return git_rev_list_count(
                repo_root,
                &[
                    "rev-list",
                    "--count",
                    head_sha,
                    "--not",
                    base_sha,
                    "--remotes",
                ],
            )
            .await;
        }
        if !base_out.stderr.trim().is_empty() {
            bail!("resolving base ref {base}: {}", base_out.stderr.trim());
        }
    }

    // `base` no longer exists (e.g. remote PR branch deleted/pruned after merge): check
    // whether all commits are already present on a remote ref.
    git_rev_list_count(
        repo_root,
        &["rev-list", "--count", head_sha, "--not", "--remotes"],
    )
    .await
}

async fn git_rev_list_count(repo_root: &Path, args: &[&str]) -> Result<u64> {
    let out = git(repo_root, args, LOCAL).await?;
    if !out.ok {
        bail!("git {} failed: {}", args.join(" "), out.stderr.trim());
    }
    out.stdout.trim().parse::<u64>().with_context(|| {
        format!(
            "parsing git {} output {:?}",
            args.join(" "),
            out.stdout.trim()
        )
    })
}

/// Returns `true` when `s` is an 8-digit `YYYYMMDD` date and 6-digit `HHMMSS` time separated
/// by `-` (15 ASCII bytes total, with plausible month/day/clock ranges).
fn is_compact_timestamp(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() != 15 || b[8] != b'-' {
        return false;
    }
    if !b[..8].iter().all(u8::is_ascii_digit) || !b[9..].iter().all(u8::is_ascii_digit) {
        return false;
    }
    let month: u32 = s[4..6].parse().unwrap_or(0);
    let day: u32 = s[6..8].parse().unwrap_or(0);
    let hour: u32 = s[9..11].parse().unwrap_or(99);
    let minute: u32 = s[11..13].parse().unwrap_or(99);
    let second: u32 = s[13..15].parse().unwrap_or(99);
    (1..=12).contains(&month)
        && (1..=31).contains(&day)
        && hour <= 23
        && minute <= 59
        && second <= 59
}

/// Strips a single trailing `-YYYYMMDD-HHMMSS` from `s` when the remaining stem is non-empty
/// and does not end with `/`.
fn strip_trailing_timestamp(s: &str) -> Option<&str> {
    if s.len() <= 16 || !s.is_char_boundary(s.len() - 16) {
        return None;
    }
    let (stem, suffix) = s.split_at(s.len() - 16);
    (suffix.starts_with('-') && is_compact_timestamp(&suffix[1..]) && !stem.ends_with('/'))
        .then_some(stem)
}

/// Strips one trailing `-YYYYMMDD-HHMMSS` or `-YYYYMMDD-HHMMSS-N` disambiguation suffix from
/// `name` when the remaining stem is non-empty and does not end with `/`.
fn strip_one_timestamp_suffix(name: &str) -> &str {
    if let Some(stem) = strip_trailing_timestamp(name) {
        return stem;
    }
    if let Some((before_counter, counter)) = name.rsplit_once('-')
        && !counter.is_empty()
        && counter.bytes().all(|b| b.is_ascii_digit())
        && let Some(stem) = strip_trailing_timestamp(before_counter)
    {
        return stem;
    }
    name
}

/// Strips all trailing `-YYYYMMDD-HHMMSS` (and optional `-N` collision counter) suffixes so
/// repeated retries never nest disambiguation timestamps (for example
/// `review/agent-board-818-20261127-120129-20261127-120129-8` normalizes to
/// `review/agent-board-818`).
fn strip_timestamp_suffix(mut name: &str) -> &str {
    loop {
        let stripped = strip_one_timestamp_suffix(name);
        if stripped.len() == name.len() {
            return name;
        }
        name = stripped;
    }
}

/// Returns `branch`, or `<stem>-YYYYMMDD-HHMMSS` (with an incrementing `-2..=100` numeric
/// suffix if needed) when `branch` already exists in `project`, where `<stem>` strips any
/// existing `-YYYYMMDD-HHMMSS(-N)` disambiguation suffix so retries never nest timestamps.
async fn unique_branch(project: &Path, branch: String, now: Timestamp) -> Result<String> {
    if !has_ref(project, &format!("refs/heads/{branch}")).await? {
        return Ok(branch);
    }
    let stem = strip_timestamp_suffix(&branch);
    let stamped = format!("{stem}-{}", now.format("%Y%m%d-%H%M%S"));
    if !has_ref(project, &format!("refs/heads/{stamped}")).await? {
        return Ok(stamped);
    }
    for n in 2..=100u32 {
        let candidate = format!("{stamped}-{n}");
        if !has_ref(project, &format!("refs/heads/{candidate}")).await? {
            return Ok(candidate);
        }
    }
    bail!(
        "could not pick a free branch name for {branch} in {} at {now}",
        project.display()
    )
}

/// Picks a globally unique `(record_id, path)` pair scoped by `repo_slug` under `base_dir`,
/// checking both the filesystem and `store` so neither a dirty directory on disk nor an
/// existing store record for another repository can ever be overwritten.
fn unique_worktree_entry(
    store: &Arc<dyn WorktreeStore>,
    repo_slug: &str,
    base_dir: &Path,
    id: &str,
    now: Timestamp,
) -> Result<(String, PathBuf)> {
    let seg = safe_segment(id);
    let first_id = format!("{repo_slug}/{seg}");
    let first_path = base_dir.join(&seg);
    if !first_path.exists() && store.get(&first_id)?.is_none() {
        return Ok((first_id, first_path));
    }
    let stem = strip_timestamp_suffix(&seg);
    let stamped = format!("{stem}-{}", now.format("%Y%m%d-%H%M%S"));
    let second_id = format!("{repo_slug}/{stamped}");
    let second_path = base_dir.join(&stamped);
    if !second_path.exists() && store.get(&second_id)?.is_none() {
        return Ok((second_id, second_path));
    }
    for n in 2..=100u32 {
        let candidate = format!("{stamped}-{n}");
        let candidate_id = format!("{repo_slug}/{candidate}");
        let path = base_dir.join(&candidate);
        if !path.exists() && store.get(&candidate_id)?.is_none() {
            return Ok((candidate_id, path));
        }
    }
    bail!(
        "could not pick a free worktree directory for {id:?} under {}",
        base_dir.display()
    )
}

/// The branch the worker of `task` runs on when it is free.
pub fn worker_branch(task: &TaskId) -> String {
    format!("task/{task}")
}

/// The worktree to start the worker of `task` in; `None` when `project` is not the top of a
/// git repository, so the thread runs in the folder itself.
pub async fn worker_worktree(
    project: &Path,
    task: &TaskId,
    now: Timestamp,
) -> Result<Option<NewWorktree>> {
    worker_worktree_with_base(project, task, None, now).await
}

/// Like [`worker_worktree`], starting from `base_branch` (fetched as `origin/<base_branch>`
/// when configured) instead of `origin/HEAD`.
pub async fn worker_worktree_with_base(
    project: &Path,
    task: &TaskId,
    base_branch: Option<&str>,
    now: Timestamp,
) -> Result<Option<NewWorktree>> {
    new_worktree_with_base(project, worker_branch(task), base_branch, now).await
}

/// The branch the review judge of `task` runs on when it is free.
pub fn review_branch(task: &TaskId) -> String {
    format!("review/{task}")
}

/// What branch [`review_worktree`] should try to fetch from `origin` before falling back
/// to the repository's default base.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewTarget<'a> {
    /// The pull request's head branch (or its base branch when already merged).
    PullRequest(&'a str),
    /// The worker's task branch (`task/<id>` or its time-suffixed form) when no pull
    /// request is recorded yet.
    TaskBranch(&'a str),
    /// No branch to fetch (for example, `pr: none`, an open fork PR, or a failed PR read).
    None,
}

/// How [`review_worktree`] chose its worktree's base ref.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewBase {
    /// Started on the requested pull request head (or base branch for a merged PR).
    PullRequest,
    /// Started on the worker's task branch fetched from `origin`.
    TaskBranch,
    /// Fell back to the repository's default base because no PR head or task branch
    /// resolved.
    DefaultBase,
}

/// The worktree prepared for a review judge, and how its base ref was chosen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewWorktree {
    pub worktree: NewWorktree,
    pub source: ReviewBase,
    /// Why a requested branch could not be fetched, when `source` fell back to
    /// [`ReviewBase::DefaultBase`].
    pub fetch_error: Option<String>,
}

/// Whether `name` is a safe git branch name to use in a fetch refspec.
pub(crate) fn valid_branch_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('-')
        && !name.starts_with('/')
        && !name.ends_with('/')
        && !name.contains("..")
        && !name.contains(':')
        && !name.contains("@{")
        && !name.chars().any(|c| {
            c.is_whitespace() || c.is_control() || matches!(c, '~' | '^' | '?' | '*' | '[' | '\\')
        })
}

/// The worktree to start the review judge of `task` in, as [`worker_worktree`]: on
/// `target`, fetched from `origin`, or on the repository's default base when no target was
/// given or fetching it failed (recorded in [`ReviewWorktree::source`] and
/// [`ReviewWorktree::fetch_error`]).
pub async fn review_worktree(
    project: &Path,
    task: &TaskId,
    target: ReviewTarget<'_>,
    now: Timestamp,
) -> Result<Option<ReviewWorktree>> {
    review_worktree_with_base(project, task, target, None, now).await
}

/// Like [`review_worktree`], using `base_branch` as the default fallback base when set.
pub async fn review_worktree_with_base(
    project: &Path,
    task: &TaskId,
    target: ReviewTarget<'_>,
    base_branch: Option<&str>,
    now: Timestamp,
) -> Result<Option<ReviewWorktree>> {
    let Some((mut worktree, default_fetch_error)) =
        new_worktree_with_fetch(project, review_branch(task), base_branch, now).await?
    else {
        return Ok(None);
    };
    let (head, resolved_source) = match target {
        ReviewTarget::PullRequest(head) => (head, ReviewBase::PullRequest),
        ReviewTarget::TaskBranch(head) => (head, ReviewBase::TaskBranch),
        ReviewTarget::None => {
            return Ok(Some(ReviewWorktree {
                worktree,
                source: ReviewBase::DefaultBase,
                fetch_error: default_fetch_error,
            }));
        }
    };
    if !valid_branch_name(head) {
        return Ok(Some(ReviewWorktree {
            worktree,
            source: ReviewBase::DefaultBase,
            fetch_error: Some(format!("invalid branch name {head:?}")),
        }));
    }
    let remote_ref = format!("origin/{head}");
    // Skip a second fetch when `default_base` already fetched the same `origin/<head>`,
    // surfacing any fetch failure so callers know the local ref may be stale.
    if worktree.base == remote_ref {
        let source = if default_fetch_error.is_some() {
            ReviewBase::DefaultBase
        } else {
            resolved_source
        };
        return Ok(Some(ReviewWorktree {
            worktree,
            source,
            fetch_error: default_fetch_error,
        }));
    }
    let refspec = format!("+refs/heads/{head}:refs/remotes/origin/{head}");
    match git(project, &["fetch", "--quiet", "origin", &refspec], FETCH).await {
        Ok(out) if out.ok => {
            worktree.base = remote_ref;
            Ok(Some(ReviewWorktree {
                worktree,
                source: resolved_source,
                fetch_error: None,
            }))
        }
        Ok(out) => Ok(Some(ReviewWorktree {
            worktree,
            source: ReviewBase::DefaultBase,
            fetch_error: Some(format!(
                "fetching {head} in {}: {}",
                project.display(),
                out.stderr.trim()
            )),
        })),
        Err(err) => Ok(Some(ReviewWorktree {
            worktree,
            source: ReviewBase::DefaultBase,
            fetch_error: Some(format!("fetching {head} in {}: {err:#}", project.display())),
        })),
    }
}

/// A worktree on `branch`, or on `branch` with the time when that one exists, from
/// `base_branch` (or the repository's default branch when `None`); `None` when `project` is
/// not the top of a git repository.
async fn new_worktree_with_base(
    project: &Path,
    branch: String,
    base_branch: Option<&str>,
    now: Timestamp,
) -> Result<Option<NewWorktree>> {
    Ok(new_worktree_with_fetch(project, branch, base_branch, now)
        .await?
        .map(|(wt, _)| wt))
}

async fn new_worktree_with_fetch(
    project: &Path,
    branch: String,
    base_branch: Option<&str>,
    now: Timestamp,
) -> Result<Option<(NewWorktree, Option<String>)>> {
    if !is_repo_top(project).await? {
        return Ok(None);
    }
    let (base, fetch_error) = resolve_base_with_fetch(project, base_branch).await?;
    let branch = unique_branch(project, branch, now).await?;
    Ok(Some((NewWorktree { base, branch }, fetch_error)))
}

/// Whether `project` is the top folder of a git work tree. A folder that does not exist is
/// not (T3 then reports the missing project), nor is one nested in another repository.
/// git answers, with a link followed as git follows it, so a folder macOS holds for a
/// prompt or refused fails here with the privacy hint instead of passing as no
/// repository, and nothing is left waiting on it once the command is stopped. Only where
/// git finds no repository is `.git` read as well.
pub(crate) async fn is_repo_top(project: &Path) -> Result<bool> {
    let out = git(
        project,
        &["rev-parse", "--is-inside-work-tree", "--show-prefix"],
        LOCAL,
    )
    .await?;
    if !out.ok {
        if folder_missing(&out.stderr) {
            return Ok(false);
        }
        if out.stderr.contains("not a git repository") {
            // git says the same when macOS refuses it `.git`: it then looks in the folders
            // above and finds no repository.
            let git_dir = project.join(".git");
            let probe = tokio::time::timeout(PROBE, tokio::fs::metadata(&git_dir)).await;
            let Some(refused) = git_dir_refused(probe) else {
                return Ok(false);
            };
            let hint = privacy_hint_for(project).await;
            let detail = format!("cannot read {}: {refused}{hint}", git_dir.display());
            if !hint.is_empty() {
                return Err(PrivacyRefusal {
                    project: project.to_path_buf(),
                    detail,
                }
                .into());
            }
            bail!("{detail}");
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

/// How long the read of `.git` may take; macOS holds it with no end while a prompt is open.
const PROBE: Duration = Duration::from_secs(1);

/// Whether git failed because the folder it was sent to does not exist. Both parts are
/// needed: other failures name a missing file too.
fn folder_missing(stderr: &str) -> bool {
    stderr.contains("cannot change to") && stderr.contains("No such file or directory")
}

/// Why the read of `.git` shows a folder kept from board, from that read's result under
/// its timeout; `None` when `.git` is not there or is something git does not take.
fn git_dir_refused(
    probe: Result<std::io::Result<std::fs::Metadata>, tokio::time::error::Elapsed>,
) -> Option<String> {
    match probe {
        Ok(Err(err))
            if matches!(
                err.kind(),
                std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::Interrupted
            ) =>
        {
            Some(err.to_string())
        }
        Ok(_) => None,
        Err(_) => Some(format!("no answer in {}s", PROBE.as_secs())),
    }
}

/// Resolves an explicit `base_branch` override (fetched from `origin` when a remote exists,
/// falling back to a local branch), or delegates to [`default_base_with_fetch`] when `None`.
async fn resolve_base_with_fetch(
    project: &Path,
    base_branch: Option<&str>,
) -> Result<(String, Option<String>)> {
    let Some(raw) = base_branch.map(str::trim).filter(|s| !s.is_empty()) else {
        return default_base_with_fetch(project).await;
    };
    let name = raw.strip_prefix("origin/").unwrap_or(raw);
    if !valid_branch_name(name) {
        bail!("invalid configured base branch {raw:?}");
    }
    let remote_ref = format!("origin/{name}");
    let remote_full = format!("refs/remotes/origin/{name}");
    let local_full = format!("refs/heads/{name}");
    let refspec = format!("+refs/heads/{name}:{remote_full}");
    let mut fetch_error = None;
    match git(project, &["fetch", "--quiet", "origin", &refspec], FETCH).await {
        Ok(out) if out.ok => {
            if has_ref(project, &remote_full).await? {
                return Ok((remote_ref, None));
            }
        }
        Ok(out) => {
            let err = format!(
                "fetching {remote_ref} in {}: {}",
                project.display(),
                out.stderr.trim()
            );
            tracing::warn!("{err}; checking local ref");
            fetch_error = Some(err);
        }
        Err(err) => {
            let msg = format!("fetching {remote_ref} in {}: {err:#}", project.display());
            tracing::warn!("{msg}; checking local ref");
            fetch_error = Some(msg);
        }
    }
    if has_ref(project, &remote_full).await? {
        return Ok((remote_ref, fetch_error));
    }
    if has_ref(project, &local_full).await? {
        return Ok((name.to_owned(), fetch_error));
    }
    bail!(
        "configured base branch {name:?} does not exist in {} (neither {remote_ref} nor local {name})",
        project.display()
    );
}

/// The ref new worktrees start from, fetched first when it is a remote branch:
/// the remote's default branch (`origin/HEAD`), else the first of `origin/main`,
/// `origin/master`, `main` and `master` that exists. Returns any fetch error so callers
/// that require fresh remote state (such as after-merge reviews) can warn.
async fn default_base_with_fetch(project: &Path) -> Result<(String, Option<String>)> {
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
    let mut fetch_error = None;
    if let Some(name) = base.strip_prefix("origin/") {
        // A stale remote branch would start the thread, and its children, on old code.
        match git(project, &["fetch", "--quiet", "origin", name], FETCH).await {
            Ok(out) if out.ok => {}
            Ok(out) => {
                let err = format!(
                    "fetching {base} in {}: {}",
                    project.display(),
                    out.stderr.trim()
                );
                tracing::warn!("{err}; starting from the local copy");
                fetch_error = Some(err);
            }
            Err(err) => {
                let msg = format!("fetching {base} in {}: {err:#}", project.display());
                tracing::warn!("{msg}; starting from the local copy");
                fetch_error = Some(msg);
            }
        }
    }
    Ok((base, fetch_error))
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
        if wait.on_folder {
            let hint = privacy_hint_for(project).await;
            if !hint.is_empty() {
                let detail = format!(
                    "git {} timed out in {}{hint}",
                    args.join(" "),
                    project.display()
                );
                return Err(PrivacyRefusal {
                    project: project.to_path_buf(),
                    detail,
                }
                .into());
            }
        }
        bail!("git {} timed out in {}", args.join(" "), project.display());
    };
    let out = out.context("running git")?;
    let ok = out.status.success();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    if cfg!(target_os = "macos")
        && let Some(refusal) = classify_denied_with_home(
            dirs::home_dir().as_deref(),
            project,
            args,
            wait,
            ok,
            &stderr,
        )
        .await
    {
        return Err(refusal.into());
    }
    Ok(GitOutput {
        ok,
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr,
    })
}

/// Whether git failed on a folder macOS TCC denied or interrupted, as it does once the user
/// refused the privacy prompt (`Operation not permitted`) or after replacing the `board`
/// binary with a new ad-hoc code signature (`Interrupted system call` / `EINTR` while macOS
/// holds/interrupts syscalls in a protected folder). Not for a fetch: the commands before it
/// read the folder, so there the message is about something else, such as the network.
fn denied(wait: Wait, ok: bool, stderr: &str) -> bool {
    wait.on_folder
        && !ok
        && (stderr.contains("Operation not permitted")
            || stderr.contains("Interrupted system call"))
}

/// Classifies a non-zero `git` failure in `project` (using `home` as the user's home folder)
/// as a macOS TCC [`PrivacyRefusal`] when `wait.on_folder` is set, `stderr` indicates a TCC
/// denial (`Operation not permitted`) or interruption (`Interrupted system call`), and
/// `project` (following symlinks safely) lies inside a protected folder.
async fn classify_denied_with_home(
    home: Option<&Path>,
    project: &Path,
    args: &[&str],
    wait: Wait,
    ok: bool,
    stderr: &str,
) -> Option<PrivacyRefusal> {
    if !denied(wait, ok, stderr) {
        return None;
    }
    let hint = hint_through_links(home, project).await?;
    let detail = format!(
        "git {} failed in {}: {}{hint}",
        args.join(" "),
        project.display(),
        stderr.trim()
    );
    Some(PrivacyRefusal {
        project: project.to_path_buf(),
        detail,
    })
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
    protected.then_some(PRIVACY_HINT)
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
        let eintr = "fatal: Unable to read current working directory: Interrupted system call\n";
        assert!(denied(LOCAL, false, refused));
        assert!(denied(LOCAL, false, eintr));
        assert!(!denied(LOCAL, false, "fatal: not a git repository\n"));
        assert!(!denied(LOCAL, true, refused), "not on success");
        assert!(!denied(LOCAL, true, eintr), "not on success");
        assert!(!denied(FETCH, false, refused), "not for a fetch");
        assert!(!denied(FETCH, false, eintr), "not for a fetch");
        assert!(!denied(WORKTREE_OP, false, eintr), "not for worktree op");
    }

    #[tokio::test]
    async fn eintr_in_protected_macos_folders_is_classified_as_privacy_refusal() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(home.join("Documents/vps")).unwrap();
        std::fs::create_dir_all(home.join("Desktop/app")).unwrap();
        std::fs::create_dir_all(home.join("Downloads/repo")).unwrap();
        std::fs::create_dir_all(home.join("Developer/safe")).unwrap();
        std::os::unix::fs::symlink("../Documents/vps", home.join("Developer/vps-link")).unwrap();

        let eintr = "fatal: Unable to read current working directory: Interrupted system call\n";
        let args = &["rev-parse", "--is-inside-work-tree", "--show-prefix"];

        for rel in [
            "Documents/vps",
            "Desktop/app",
            "Downloads/repo",
            "Developer/vps-link",
        ] {
            let project = home.join(rel);
            let refusal =
                classify_denied_with_home(Some(&home), &project, args, LOCAL, false, eintr)
                    .await
                    .unwrap_or_else(|| panic!("expected PrivacyRefusal for {rel}"));
            assert_eq!(refusal.project, project);
            assert!(
                refusal.detail.contains("Interrupted system call"),
                "{}",
                refusal.detail
            );
            assert!(
                refusal
                    .detail
                    .contains("Privacy & Security > Files and Folders"),
                "{}",
                refusal.detail
            );

            // Typed error survives anyhow `.context(...)` wrapping.
            let wrapped: anyhow::Error = anyhow::Error::new(refusal).context("preparing worker");
            let found = privacy_refusal(&wrapped).expect("downcast PrivacyRefusal through context");
            assert_eq!(found.project, project);
        }

        // Unprotected folder, missing home, fetch, or unrelated stderr must NOT classify as TCC refusal.
        assert_eq!(
            classify_denied_with_home(
                Some(&home),
                &home.join("Developer/safe"),
                args,
                LOCAL,
                false,
                eintr,
            )
            .await,
            None
        );
        assert_eq!(
            classify_denied_with_home(None, &home.join("Documents/vps"), args, LOCAL, false, eintr)
                .await,
            None
        );
        assert_eq!(
            classify_denied_with_home(
                Some(&home),
                &home.join("Documents/vps"),
                &["fetch", "--quiet", "origin"],
                FETCH,
                false,
                eintr,
            )
            .await,
            None
        );
        assert_eq!(
            classify_denied_with_home(
                Some(&home),
                &home.join("Documents/vps"),
                args,
                LOCAL,
                false,
                "fatal: not a git repository\n",
            )
            .await,
            None
        );
    }

    #[test]
    fn only_a_folder_git_could_not_change_to_counts_as_missing() {
        assert!(folder_missing(
            "fatal: cannot change to '/Users/u/gone': No such file or directory\n"
        ));
        assert!(!folder_missing(
            "fatal: unable to access '.git/config': No such file or directory\n"
        ));
        assert!(!folder_missing(
            "fatal: cannot change to '/Users/u/file': Not a directory\n"
        ));
    }

    #[tokio::test]
    async fn a_git_folder_that_cannot_be_read_is_an_error_not_a_folder_outside_git() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        std::fs::create_dir(&project).unwrap();
        // No repository, so git answers "not a git repository" and `.git` is read: absent.
        assert!(!is_repo_top(&project).await.unwrap());
        // The read of `.git` as macOS refuses it; without search permission on the folder
        // git itself cannot enter, so the result of the read is tested apart from git.
        std::fs::set_permissions(&project, std::fs::Permissions::from_mode(0o600)).unwrap();
        let read = std::fs::metadata(project.join(".git"));
        std::fs::set_permissions(&project, std::fs::Permissions::from_mode(0o700)).unwrap();
        // root reads it anyway.
        if read
            .as_ref()
            .is_err_and(|err| err.kind() == std::io::ErrorKind::PermissionDenied)
        {
            assert!(
                git_dir_refused(Ok(read))
                    .unwrap()
                    .contains("Permission denied")
            );
        }
        let absent = std::fs::metadata(project.join(".git"));
        assert_eq!(git_dir_refused(Ok(absent)), None);
        let interrupted: std::io::Result<std::fs::Metadata> = Err(std::io::Error::new(
            std::io::ErrorKind::Interrupted,
            "Interrupted system call",
        ));
        assert!(
            git_dir_refused(Ok(interrupted))
                .unwrap()
                .contains("Interrupted system call")
        );
        let slow = tokio::time::timeout(Duration::ZERO, std::future::pending()).await;
        assert_eq!(git_dir_refused(slow).unwrap(), "no answer in 1s");
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

    fn first_task() -> TaskId {
        "factory-1".parse().unwrap()
    }

    fn at() -> Timestamp {
        "2026-11-08T13:20:05Z".parse().unwrap()
    }

    async fn start(project: &Path) -> Option<NewWorktree> {
        worker_worktree(project, &first_task(), at()).await.unwrap()
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
                branch: "task/factory-1".into()
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
        // The branches exist on the remote only: the clone has not fetched them.
        run(
            &dir.path().join("origin"),
            &["branch", "task/factory-7", "main"],
        );
        run(
            &dir.path().join("origin"),
            &["branch", "task/factory-7-20261108-132005", "main"],
        );
        let on_pr = review_worktree(
            &repo,
            &task,
            ReviewTarget::PullRequest("task/factory-7"),
            at(),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(
            on_pr.worktree,
            NewWorktree {
                base: "origin/task/factory-7".into(),
                branch: "review/factory-7".into()
            }
        );
        assert_eq!(on_pr.source, ReviewBase::PullRequest);
        assert_eq!(on_pr.fetch_error, None);
        run(&repo, &["rev-parse", "--verify", "origin/task/factory-7"]);

        // Task branch fallback (including time-suffixed worker branches).
        let on_task = review_worktree(
            &repo,
            &task,
            ReviewTarget::TaskBranch("task/factory-7-20261108-132005"),
            at(),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(
            on_task.worktree.base,
            "origin/task/factory-7-20261108-132005"
        );
        assert_eq!(on_task.source, ReviewBase::TaskBranch);
        assert_eq!(on_task.fetch_error, None);

        // A merged PR whose base is the default branch still counts as resolved PullRequest.
        let on_merged_default =
            review_worktree(&repo, &task, ReviewTarget::PullRequest("main"), at())
                .await
                .unwrap()
                .unwrap();
        assert_eq!(on_merged_default.worktree.base, "origin/main");
        assert_eq!(on_merged_default.source, ReviewBase::PullRequest);
        assert_eq!(on_merged_default.fetch_error, None);

        // No target, a branch the remote lacks, or an invalid/option name: default base.
        for (target, expects_err) in [
            (ReviewTarget::None, false),
            (ReviewTarget::PullRequest("task/gone"), true),
            (ReviewTarget::TaskBranch("task/gone"), true),
            (ReviewTarget::PullRequest("--upload-pack=x"), true),
            (ReviewTarget::PullRequest("a:b"), true),
        ] {
            let prepared = review_worktree(&repo, &task, target, at())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(prepared.worktree.base, "origin/main", "{target:?}");
            assert_eq!(prepared.source, ReviewBase::DefaultBase, "{target:?}");
            assert_eq!(prepared.fetch_error.is_some(), expects_err, "{target:?}");
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
        let err = worker_worktree(&repo, &first_task(), at())
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
                "task/factory-1",
                old_s,
                "origin/main",
            ],
        );
        std::fs::write(old.join(".env"), "ignored, maybe").unwrap();
        assert_eq!(
            start(&repo).await.unwrap().branch,
            "task/factory-1-20261108-132005"
        );
        assert!(old.join(".env").exists(), "the old worktree is untouched");
        assert_eq!(
            run(&repo, &["branch", "--format=%(refname:short)"]),
            "main\ntask/factory-1\n"
        );
    }

    #[tokio::test]
    async fn worktree_manager_creates_tracks_collides_and_safely_cleans_up() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::at(dir.path().join("home"));
        let repo = clone(dir.path(), "main");
        let manager = WorktreeManager::for_home(home.clone());
        let task: TaskId = "factory-42".parse().unwrap();

        // Non-git folder starts in-place (Ok(None)).
        let plain = dir.path().join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        assert_eq!(
            manager
                .prepare_worker(&plain, None, &task, "run-plain", at())
                .await
                .unwrap(),
            None
        );

        // Git repo gets an isolated worktree under `$BOARD_HOME/worktrees/<repo-slug>/<id>`.
        let h1 = manager
            .prepare_worker(
                &repo,
                Some("github.com/org/agent-board"),
                &task,
                "run-1",
                at(),
            )
            .await
            .unwrap()
            .expect("managed worktree created");
        assert_eq!(h1.repo_slug, "github.com-org-agent-board");
        assert_eq!(h1.branch, "task/factory-42");
        assert_eq!(h1.base, "origin/main");
        assert_eq!(
            h1.path,
            home.root()
                .join("worktrees")
                .join("github.com-org-agent-board")
                .join("run-1")
        );
        assert!(h1.path.exists());
        assert_eq!(
            run(&h1.path, &["rev-parse", "--abbrev-ref", "HEAD"]).trim(),
            "task/factory-42"
        );

        // Persisted in JsonWorktreeStore and readable by a fresh manager instance.
        let reloaded = WorktreeManager::for_home(home.clone());
        let records = reloaded.records().unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].id, "github.com-org-agent-board/run-1");
        assert_eq!(records[0].status, WorktreeStatus::Active);

        // Second start for the same task gets the `-YYYYMMDD-HHMMSS` branch suffix and never
        // touches the first worktree.
        let h2 = manager
            .prepare_worker(
                &repo,
                Some("github.com/org/agent-board"),
                &task,
                "run-2",
                at(),
            )
            .await
            .unwrap()
            .expect("second managed worktree");
        assert_eq!(h2.branch, "task/factory-42-20261108-132005");
        assert_ne!(h1.path, h2.path);
        assert!(h2.path.exists());

        // Make `h1` dirty with an untracked file; cleanup MUST refuse and write a Trail Rule record.
        std::fs::write(h1.path.join("wip.txt"), "unsaved work").unwrap();
        let dirty_outcome = manager.cleanup(&h1.id, at()).await.unwrap().unwrap();
        match dirty_outcome {
            CleanupOutcome::SkippedDirty {
                id,
                path,
                dirty_entries,
                trail_path,
                ..
            } => {
                assert_eq!(id, "github.com-org-agent-board/run-1");
                assert_eq!(path, h1.path);
                assert!(dirty_entries.iter().any(|l| l.contains("wip.txt")));
                assert!(trail_path.exists());
                let trail = std::fs::read_to_string(&trail_path).unwrap();
                assert!(trail.contains("cleanup_skipped_dirty"), "{trail}");
                assert!(trail.contains("wip.txt"), "{trail}");
            }
            other => panic!("expected SkippedDirty, got {other:?}"),
        }
        assert!(
            h1.path.join("wip.txt").exists(),
            "dirty worktree must remain untouched on disk"
        );
        assert_eq!(
            manager.store().get(&h1.id).unwrap().unwrap().status,
            WorktreeStatus::DirtySkipped
        );

        // `h2` is clean: cleanup removes the directory and store record while keeping the branch ref.
        let clean_outcome = manager.cleanup(&h2.id, at()).await.unwrap().unwrap();
        assert_eq!(
            clean_outcome,
            CleanupOutcome::Removed {
                id: h2.id.clone(),
                path: h2.path.clone(),
                branch: h2.branch.clone(),
            }
        );
        assert!(!h2.path.exists());
        assert_eq!(manager.store().get(&h2.id).unwrap(), None);
        assert!(
            run(&repo, &["branch", "--format=%(refname:short)"])
                .lines()
                .any(|b| b == h2.branch),
            "git branch ref must remain intact after clean worktree removal"
        );

        // `cleanup_ended` skips active ids and already-DirtySkipped worktrees without re-spamming.
        let h3 = manager
            .prepare_worker(
                &repo,
                Some("github.com/org/agent-board"),
                &"factory-43".parse().unwrap(),
                "run-3",
                at(),
            )
            .await
            .unwrap()
            .unwrap();
        let active = BTreeSet::from([h3.id.clone()]);
        let ended = manager.cleanup_ended(&active, at()).await.unwrap();
        assert!(
            ended.is_empty(),
            "h1 is DirtySkipped and h3 is active: {ended:?}"
        );
        let ended = manager.cleanup_ended(&BTreeSet::new(), at()).await.unwrap();
        assert_eq!(ended.len(), 1);
        assert!(matches!(ended[0], CleanupOutcome::Removed { .. }));
        assert!(!h3.path.exists());
    }

    #[tokio::test]
    async fn worktree_manager_prepare_review_fetches_pr_head_and_checks_it_out() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::at(dir.path().join("home"));
        let repo = clone(dir.path(), "main");
        let origin = dir.path().join("origin");
        run(&origin, &["checkout", "-b", "feat/review-me"]);
        std::fs::write(origin.join("feature.txt"), "hello\n").unwrap();
        run(&origin, &["add", "feature.txt"]);
        run(&origin, &["commit", "--quiet", "-m", "add feature"]);
        run(&origin, &["checkout", "main"]);

        let manager = WorktreeManager::for_home(home);
        let task: TaskId = "factory-9".parse().unwrap();
        let prepared = manager
            .prepare_review(
                &repo,
                Some("github.com/org/demo"),
                &task,
                "review-run-1",
                ReviewTarget::PullRequest("feat/review-me"),
                at(),
            )
            .await
            .unwrap()
            .expect("review worktree created");
        assert_eq!(prepared.source, ReviewBase::PullRequest);
        assert_eq!(prepared.fetch_error, None);
        assert_eq!(prepared.handle.base, "origin/feat/review-me");
        assert_eq!(prepared.handle.branch, "review/factory-9");
        assert_eq!(
            std::fs::read_to_string(prepared.handle.path.join("feature.txt")).unwrap(),
            "hello\n"
        );
        let removed = manager
            .cleanup(&prepared.handle.id, at())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(removed, CleanupOutcome::Removed { .. }));
        assert!(
            !run(&repo, &["branch", "--format=%(refname:short)"])
                .lines()
                .any(|b| b == "review/factory-9"),
            "ephemeral review branch should be deleted after clean cleanup"
        );
    }

    #[tokio::test]
    async fn worktree_manager_scopes_store_keys_per_repo_and_retries_cleanup_errors() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::at(dir.path().join("home"));
        let dir_a = dir.path().join("a");
        let dir_b = dir.path().join("b");
        std::fs::create_dir_all(&dir_a).unwrap();
        std::fs::create_dir_all(&dir_b).unwrap();
        let repo_a = clone(&dir_a, "main");
        let repo_b = clone(&dir_b, "main");

        let manager = WorktreeManager::for_home(home);
        let task: TaskId = "factory-1".parse().unwrap();

        // Same run_id across two different repos must NOT overwrite each other in WorktreeStore.
        let ha = manager
            .prepare_worker(&repo_a, Some("github.com/org/alpha"), &task, "run-1", at())
            .await
            .unwrap()
            .unwrap();
        let hb = manager
            .prepare_worker(&repo_b, Some("github.com/org/beta"), &task, "run-1", at())
            .await
            .unwrap()
            .unwrap();
        assert_ne!(ha.id, hb.id);
        assert_eq!(manager.records().unwrap().len(), 2);

        // Mark `ha` as CleanupError (simulating a prior transient failure): cleanup_ended MUST retry it.
        let mut rec_a = manager.store().get(&ha.id).unwrap().unwrap();
        rec_a.status = WorktreeStatus::CleanupError;
        manager.store().upsert(rec_a).unwrap();

        let active = BTreeSet::from([hb.id.clone()]);
        let outcomes = manager.cleanup_ended(&active, at()).await.unwrap();
        assert_eq!(outcomes.len(), 1);
        assert!(matches!(outcomes[0], CleanupOutcome::Removed { ref id, .. } if id == &ha.id));
        assert!(!ha.path.exists());
        assert!(hb.path.exists());
        assert_eq!(manager.records().unwrap().len(), 1);

        // Clean up `hb` too.
        let outcomes = manager.cleanup_ended(&BTreeSet::new(), at()).await.unwrap();
        assert_eq!(outcomes.len(), 1);
        assert!(!hb.path.exists());
    }

    #[tokio::test]
    async fn worker_worktree_with_base_uses_configured_branch_over_origin_head() {
        let dir = tempfile::tempdir().unwrap();
        let repo = clone(dir.path(), "main");
        let origin = dir.path().join("origin");
        run(&origin, &["checkout", "-b", "fork"]);
        std::fs::write(origin.join("fork-only.txt"), "fork\n").unwrap();
        run(&origin, &["add", "fork-only.txt"]);
        run(&origin, &["commit", "--quiet", "-m", "fork commit"]);
        run(&origin, &["checkout", "main"]);

        let task: TaskId = "app-321".parse().unwrap();
        // Default (`None`) still follows `origin/HEAD` (`origin/main`).
        let default_wt = worker_worktree_with_base(&repo, &task, None, at())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(default_wt.base, "origin/main");

        // Explicit `Some("fork")` fetches and branches from `origin/fork` despite `origin/HEAD -> origin/main`.
        let fork_wt = worker_worktree_with_base(&repo, &task, Some("fork"), at())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(fork_wt.base, "origin/fork");
        assert_eq!(fork_wt.branch, "task/app-321");

        // Leading `origin/` is normalized too.
        let prefixed_wt = worker_worktree_with_base(&repo, &task, Some("origin/fork"), at())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(prefixed_wt.base, "origin/fork");

        // Missing configured base branch fails with an actionable error instead of silently falling back to main.
        let err = worker_worktree_with_base(&repo, &task, Some("nonexistent"), at())
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("nonexistent"),
            "expected missing branch in error: {err:#}"
        );

        // `review_worktree_with_base` falls back to configured base when `ReviewTarget::None`
        // and skips a redundant fetch when `ReviewTarget::PullRequest` matches that base.
        let rev_none =
            review_worktree_with_base(&repo, &task, ReviewTarget::None, Some("fork"), at())
                .await
                .unwrap()
                .unwrap();
        assert_eq!(rev_none.source, ReviewBase::DefaultBase);
        assert_eq!(rev_none.worktree.base, "origin/fork");
        assert!(rev_none.fetch_error.is_none());

        let rev_same = review_worktree_with_base(
            &repo,
            &task,
            ReviewTarget::PullRequest("fork"),
            Some("fork"),
            at(),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(rev_same.source, ReviewBase::PullRequest);
        assert_eq!(rev_same.worktree.base, "origin/fork");
        assert!(rev_same.fetch_error.is_none());

        let rev_err =
            review_worktree_with_base(&repo, &task, ReviewTarget::None, Some("nonexistent"), at())
                .await
                .unwrap_err();
        assert!(
            rev_err.to_string().contains("nonexistent"),
            "expected missing branch error from review_worktree_with_base: {rev_err:#}"
        );
    }

    #[tokio::test]
    async fn worktree_manager_cleanup_deletes_ephemeral_review_branch() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::at(dir.path().join("home"));
        let repo = clone(dir.path(), "main");
        let manager = WorktreeManager::for_home(home.clone());
        let task: TaskId = "agent-board-832".parse().unwrap();

        let prepared = manager
            .prepare_review(
                &repo,
                Some("github.com/org/agent-board"),
                &task,
                "review-run-1",
                ReviewTarget::None,
                at(),
            )
            .await
            .unwrap()
            .expect("review worktree created");
        assert_eq!(prepared.handle.branch, "review/agent-board-832");
        assert!(
            run(&repo, &["branch", "--format=%(refname:short)"])
                .lines()
                .any(|b| b == "review/agent-board-832")
        );

        let outcome = manager
            .cleanup(&prepared.handle.id, at())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(outcome, CleanupOutcome::Removed { .. }));
        assert!(
            !run(&repo, &["branch", "--format=%(refname:short)"])
                .lines()
                .any(|b| b == "review/agent-board-832"),
            "ephemeral review/* branch must be deleted on clean worktree removal"
        );
        let events =
            std::fs::read_to_string(home.root().join("state/worktree-events.jsonl")).unwrap();
        assert!(
            events.contains("\"action\":\"removed\"") && events.contains("\"branch_deleted\":true"),
            "{events}"
        );

        // `cleanup_ended` deletes ephemeral review/* branches too.
        let ended_prep = manager
            .prepare_review(
                &repo,
                Some("github.com/org/agent-board"),
                &"agent-board-833".parse().unwrap(),
                "review-run-2",
                ReviewTarget::None,
                at(),
            )
            .await
            .unwrap()
            .unwrap();
        let outcomes = manager.cleanup_ended(&BTreeSet::new(), at()).await.unwrap();
        assert_eq!(outcomes.len(), 1);
        assert!(matches!(outcomes[0], CleanupOutcome::Removed { .. }));
        assert!(
            !run(&repo, &["branch", "--format=%(refname:short)"])
                .lines()
                .any(|b| b == ended_prep.handle.branch),
            "cleanup_ended must delete ephemeral review/* branch"
        );

        // AlreadyGone worktree directory still deletes the leftover ephemeral review/* branch.
        let gone_prep = manager
            .prepare_review(
                &repo,
                Some("github.com/org/agent-board"),
                &"agent-board-834".parse().unwrap(),
                "review-run-3",
                ReviewTarget::None,
                at(),
            )
            .await
            .unwrap()
            .unwrap();
        std::fs::remove_dir_all(&gone_prep.handle.path).unwrap();
        let gone_outcome = manager
            .cleanup(&gone_prep.handle.id, at())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(gone_outcome, CleanupOutcome::AlreadyGone { .. }));
        assert!(
            !run(&repo, &["branch", "--format=%(refname:short)"])
                .lines()
                .any(|b| b == gone_prep.handle.branch),
            "AlreadyGone cleanup must prune ephemeral review/* branch too"
        );

        // Local-only repo (no `origin` remote) still deletes a clean ephemeral review/* branch.
        let local_repo = dir.path().join("local-only");
        std::fs::create_dir(&local_repo).unwrap();
        run(&local_repo, &["init", "--quiet", "--initial-branch=main"]);
        run(
            &local_repo,
            &["commit", "--quiet", "--allow-empty", "-m", "init"],
        );
        let local_prep = manager
            .prepare_review(
                &local_repo,
                Some("local/only"),
                &"agent-board-835".parse().unwrap(),
                "review-local-1",
                ReviewTarget::None,
                at(),
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(local_prep.handle.base, "main");
        let local_removed = manager
            .cleanup(&local_prep.handle.id, at())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(local_removed, CleanupOutcome::Removed { .. }));
        assert!(
            !run(&local_repo, &["branch", "--format=%(refname:short)"])
                .lines()
                .any(|b| b == local_prep.handle.branch),
            "local-only repo should delete clean ephemeral review/* branch"
        );

        // Non-review worker `task/*` branch is still preserved on cleanup.
        let worker = manager
            .prepare_worker(
                &repo,
                Some("github.com/org/agent-board"),
                &"agent-board-836".parse().unwrap(),
                "worker-run-1",
                at(),
            )
            .await
            .unwrap()
            .unwrap();
        let worker_removed = manager.cleanup(&worker.id, at()).await.unwrap().unwrap();
        assert!(matches!(worker_removed, CleanupOutcome::Removed { .. }));
        assert!(
            run(&repo, &["branch", "--format=%(refname:short)"])
                .lines()
                .any(|b| b == worker.branch),
            "non-review worker branch must remain intact after cleanup"
        );
    }

    #[tokio::test]
    async fn worktree_manager_cleanup_preserves_dirty_unpushed_and_records_branch_failure() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::at(dir.path().join("home"));
        let repo = clone(dir.path(), "main");
        let origin = dir.path().join("origin");
        let manager = WorktreeManager::for_home(home.clone());

        // 1. Dirty review worktree: neither worktree nor branch is removed.
        let dirty_prep = manager
            .prepare_review(
                &repo,
                Some("github.com/org/agent-board"),
                &"factory-10".parse().unwrap(),
                "review-dirty",
                ReviewTarget::None,
                at(),
            )
            .await
            .unwrap()
            .unwrap();
        std::fs::write(dirty_prep.handle.path.join("notes.tmp"), "untracked").unwrap();
        let dirty_outcome = manager
            .cleanup(&dirty_prep.handle.id, at())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(dirty_outcome, CleanupOutcome::SkippedDirty { .. }));
        assert!(dirty_prep.handle.path.exists());
        assert!(
            run(&repo, &["branch", "--format=%(refname:short)"])
                .lines()
                .any(|b| b == dirty_prep.handle.branch),
            "dirty review worktree must keep its local branch"
        );

        // 2. Clean review worktree with a unique unpushed local commit: worktree is removed,
        //    branch is preserved, and a Trail Rule record is written.
        let unpushed_prep = manager
            .prepare_review(
                &repo,
                Some("github.com/org/agent-board"),
                &"factory-11".parse().unwrap(),
                "review-unpushed",
                ReviewTarget::None,
                at(),
            )
            .await
            .unwrap()
            .unwrap();
        std::fs::write(unpushed_prep.handle.path.join("local.txt"), "precious\n").unwrap();
        run(&unpushed_prep.handle.path, &["add", "local.txt"]);
        run(
            &unpushed_prep.handle.path,
            &["commit", "--quiet", "-m", "unpushed review edit"],
        );
        let head_sha = run(&unpushed_prep.handle.path, &["rev-parse", "HEAD"])
            .trim()
            .to_owned();
        let unpushed_outcome = manager
            .cleanup(&unpushed_prep.handle.id, at())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(unpushed_outcome, CleanupOutcome::Removed { .. }));
        assert!(!unpushed_prep.handle.path.exists());
        assert!(
            run(&repo, &["branch", "--format=%(refname:short)"])
                .lines()
                .any(|b| b == unpushed_prep.handle.branch),
            "review branch with unique unpushed commits must be preserved"
        );
        let events =
            std::fs::read_to_string(home.root().join("state/worktree-events.jsonl")).unwrap();
        assert!(
            events.contains("\"action\":\"branch_cleanup_skipped_unpushed\"")
                && events.contains(&head_sha)
                && events.contains("\"unique_commits\":1"),
            "expected Trail Rule record for preserved unpushed review branch: {events}"
        );

        // 3. Merged PR branch pruned locally after merge into origin/main: base ref is gone,
        //    but commit is on origin/main, so cleanup still deletes the ephemeral review branch.
        run(&origin, &["checkout", "-b", "feat/merged-pr"]);
        std::fs::write(origin.join("merged.txt"), "ok\n").unwrap();
        run(&origin, &["add", "merged.txt"]);
        run(&origin, &["commit", "--quiet", "-m", "pr commit"]);
        run(&origin, &["checkout", "main"]);
        let merged_prep = manager
            .prepare_review(
                &repo,
                Some("github.com/org/agent-board"),
                &"factory-12".parse().unwrap(),
                "review-merged",
                ReviewTarget::PullRequest("feat/merged-pr"),
                at(),
            )
            .await
            .unwrap()
            .unwrap();
        run(
            &origin,
            &["merge", "--quiet", "--ff-only", "feat/merged-pr"],
        );
        run(&origin, &["branch", "-d", "feat/merged-pr"]);
        run(&repo, &["fetch", "--quiet", "--prune", "origin"]);
        let merged_outcome = manager
            .cleanup(&merged_prep.handle.id, at())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(merged_outcome, CleanupOutcome::Removed { .. }));
        assert!(
            !run(&repo, &["branch", "--format=%(refname:short)"])
                .lines()
                .any(|b| b == merged_prep.handle.branch),
            "ephemeral review branch whose base was merged & pruned should still be deleted"
        );

        // 4. Branch deletion failure (e.g. branch checked out in repo_root): recorded via
        //    Trail Rule (`branch_cleanup_failed`) without losing info.
        let fail_prep = manager
            .prepare_review(
                &repo,
                Some("github.com/org/agent-board"),
                &"factory-13".parse().unwrap(),
                "review-branch-fail",
                ReviewTarget::None,
                at(),
            )
            .await
            .unwrap()
            .unwrap();
        std::fs::remove_dir_all(&fail_prep.handle.path).unwrap();
        run(&repo, &["worktree", "prune"]);
        run(&repo, &["checkout", "--quiet", &fail_prep.handle.branch]);
        let fail_outcome = manager
            .cleanup(&fail_prep.handle.id, at())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(fail_outcome, CleanupOutcome::AlreadyGone { .. }));
        let events =
            std::fs::read_to_string(home.root().join("state/worktree-events.jsonl")).unwrap();
        assert!(
            events.contains("\"action\":\"branch_cleanup_failed\"")
                && events.contains(&fail_prep.handle.branch),
            "expected Trail Rule record for branch deletion failure: {events}"
        );

        // 5. Fatal git error while probing the branch ref (e.g. non-git repo_root) must record
        //    `branch_cleanup_failed` instead of silently treating it as AlreadyAbsent.
        let non_git = dir.path().join("not-a-repo");
        std::fs::create_dir(&non_git).unwrap();
        let bad_record = WorktreeRecord {
            id: "bad-repo/review-bad".into(),
            repo_slug: "bad-repo".into(),
            repo_root: non_git,
            path: dir.path().join("missing-worktree"),
            branch: "review/factory-14".into(),
            base: "origin/main".into(),
            status: WorktreeStatus::Active,
            created_at: at(),
            last_used_at: at(),
            cleanup_note: None,
        };
        manager.store().upsert(bad_record.clone()).unwrap();
        let bad_outcome = manager
            .cleanup(&bad_record.id, at())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(bad_outcome, CleanupOutcome::AlreadyGone { .. }));
        let events =
            std::fs::read_to_string(home.root().join("state/worktree-events.jsonl")).unwrap();
        assert!(
            events.contains("\"action\":\"branch_cleanup_failed\"")
                && events.contains("\"branch\":\"review/factory-14\"")
                && events.contains("not a git repository"),
            "expected Trail Rule record for fatal rev-parse failure: {events}"
        );
    }

    #[test]
    fn strip_timestamp_suffix_handles_nested_and_edge_cases() {
        assert_eq!(
            strip_timestamp_suffix("review/agent-board-818"),
            "review/agent-board-818"
        );
        assert_eq!(
            strip_timestamp_suffix("review/agent-board-818-20261127-120129"),
            "review/agent-board-818"
        );
        assert_eq!(
            strip_timestamp_suffix("review/agent-board-818-20261127-120129-8"),
            "review/agent-board-818"
        );
        assert_eq!(
            strip_timestamp_suffix("review/agent-board-818-20261127-120129-20261127-120129-8"),
            "review/agent-board-818"
        );
        assert_eq!(
            strip_timestamp_suffix("review/agent-board-818-20261127-120129-2-20261127-120130-3"),
            "review/agent-board-818"
        );
        assert_eq!(
            strip_timestamp_suffix("task/factory-7-20261108-132005"),
            "task/factory-7"
        );
        assert_eq!(
            strip_timestamp_suffix("review/-20261127-120129"),
            "review/-20261127-120129"
        );
        assert_eq!(
            strip_timestamp_suffix("review/20261127-120129"),
            "review/20261127-120129"
        );
        assert_eq!(
            strip_timestamp_suffix("review/proj-99999999-999999"),
            "review/proj-99999999-999999"
        );
    }

    #[tokio::test]
    async fn branch_collision_suffixing_never_nests_timestamps() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::at(dir.path().join("home"));
        let repo = clone(dir.path(), "main");
        let manager = WorktreeManager::for_home(home);
        let task: TaskId = "agent-board-818".parse().unwrap();
        let t1: Timestamp = "2026-11-27T12:01:29Z".parse().unwrap();
        let t2: Timestamp = "2026-11-27T12:01:30Z".parse().unwrap();

        run(&repo, &["branch", "review/agent-board-818", "origin/main"]);
        run(
            &repo,
            &[
                "branch",
                "review/agent-board-818-20261127-120129",
                "origin/main",
            ],
        );

        let prepared = manager
            .prepare_review(
                &repo,
                Some("github.com/org/agent-board"),
                &task,
                "review-retry-1",
                ReviewTarget::None,
                t1,
            )
            .await
            .unwrap()
            .expect("retry review worktree");
        assert_eq!(
            prepared.handle.branch,
            "review/agent-board-818-20261127-120129-2"
        );

        // Passing an already-suffixed branch at a later timestamp replaces the suffix cleanly.
        let next_same_sec = unique_branch(
            &repo,
            "review/agent-board-818-20261127-120129-2".to_owned(),
            t1,
        )
        .await
        .unwrap();
        assert_eq!(next_same_sec, "review/agent-board-818-20261127-120129-3");

        let next_later_sec = unique_branch(
            &repo,
            "review/agent-board-818-20261127-120129-2".to_owned(),
            t2,
        )
        .await
        .unwrap();
        assert_eq!(next_later_sec, "review/agent-board-818-20261127-120130");

        // Legacy already-nested branch name normalizes back to a single timestamp.
        run(
            &repo,
            &[
                "branch",
                "review/agent-board-818-20261127-120129-20261127-120129-8",
                "origin/main",
            ],
        );
        let normalized = unique_branch(
            &repo,
            "review/agent-board-818-20261127-120129-20261127-120129-8".to_owned(),
            t2,
        )
        .await
        .unwrap();
        assert_eq!(normalized, "review/agent-board-818-20261127-120130");
    }
}
