//! The pull requests of daemon dispatch (`docs/DESIGN.md`, "Daemon dispatch", Merging and
//! finishing), read and merged with the `gh` CLI.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use futures_util::future::BoxFuture;
use serde_json::Value;

/// A pull request as `gh pr view --json` gives it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequest {
    pub url: String,
    pub state: PrState,
    pub draft: bool,
    /// The branch it merges from.
    pub head_ref: String,
    /// The commit its branch is at.
    pub head_oid: String,
    /// The commit that merged it.
    pub merge_commit: Option<String>,
    pub merge_state: MergeState,
    pub checks: Checks,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrState {
    Open,
    Merged,
    Closed,
}

/// Whether GitHub would merge the pull request now (`mergeStateStatus`, `mergeable`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeState {
    /// Nothing holds the merge.
    Clean,
    /// The base branch requires the head to be up to date, and it is not.
    Behind,
    /// The head conflicts with the base.
    Conflict,
    /// Held: required checks or reviews are missing, it is a draft, or GitHub has not
    /// worked it out yet.
    Waits,
}

/// The pull request's checks, taken together.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Checks {
    /// Every check ended well, or there is none.
    Passed,
    /// At least one still runs, and none failed.
    Pending,
    /// These failed.
    Failed(Vec<String>),
}

/// What `gh pr update-branch` did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Updated {
    /// The base branch was merged into the head.
    Done,
    /// The base branch conflicts with the head.
    Conflict,
}

/// The pull request commands the dispatcher needs. Each runs in `project`, a checkout of
/// the repository, and names the pull request by its URL.
pub trait Gh: Send + Sync {
    fn view<'a>(&'a self, project: &'a Path, pr: &'a str) -> BoxFuture<'a, Result<PullRequest>>;

    /// Merges the base branch into the pull request's branch.
    fn update_branch<'a>(
        &'a self,
        project: &'a Path,
        pr: &'a str,
    ) -> BoxFuture<'a, Result<Updated>>;

    /// Merges the pull request by the repository's rules: with a merge commit where the
    /// repository allows one, else squashed, else rebased.
    fn merge<'a>(&'a self, project: &'a Path, pr: &'a str) -> BoxFuture<'a, Result<()>>;
}

/// The fields [`parse_pull_request`] reads.
const VIEW_FIELDS: &str = "url,state,isDraft,headRefName,headRefOid,mergeCommit,mergeStateStatus,mergeable,statusCheckRollup";

const COMMAND_TIMEOUT: Duration = Duration::from_secs(60);

/// The real `gh` binary.
#[derive(Debug, Clone)]
pub struct GhCli {
    pub bin: PathBuf,
}

impl GhCli {
    /// `gh` from `PATH`, else from where Homebrew or a package puts it: a daemon started by
    /// the system has a short `PATH`.
    pub fn locate() -> Self {
        let on_path = std::env::var_os("PATH")
            .map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
            .unwrap_or_default();
        let bin = on_path
            .into_iter()
            .chain(["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin"].map(PathBuf::from))
            .map(|dir| dir.join("gh"))
            .find(|bin| bin.is_file())
            .unwrap_or_else(|| PathBuf::from("gh"));
        Self { bin }
    }

    /// Runs `gh <args>` in `project` and returns (exit success, stdout, stderr).
    async fn run(&self, project: &Path, args: &[&str]) -> Result<(bool, String, String)> {
        let child = tokio::process::Command::new(&self.bin)
            .args(args)
            .current_dir(project)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Never waits on a prompt.
            .env("GH_PROMPT_DISABLED", "1")
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("running {}", self.bin.display()))?;
        let Ok(out) = tokio::time::timeout(COMMAND_TIMEOUT, child.wait_with_output()).await else {
            bail!("gh {} timed out", args.join(" "));
        };
        let out = out.with_context(|| format!("running {}", self.bin.display()))?;
        Ok((
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        ))
    }

    /// The flag of the first merge method the pull request's repository allows.
    async fn merge_flag(&self, project: &Path, pr: &str) -> Result<&'static str> {
        let repo = repo_of(pr).with_context(|| format!("{pr} is no pull request URL"))?;
        let args = [
            "repo",
            "view",
            &repo,
            "--json",
            "mergeCommitAllowed,squashMergeAllowed,rebaseMergeAllowed",
        ];
        let (ok, stdout, stderr) = self.run(project, &args).await?;
        if !ok {
            bail!("gh repo view failed: {}", stderr.trim());
        }
        let value: Value = serde_json::from_str(&stdout).context("gh repo view --json")?;
        Ok(merge_flag(&value))
    }
}

impl Gh for GhCli {
    fn view<'a>(&'a self, project: &'a Path, pr: &'a str) -> BoxFuture<'a, Result<PullRequest>> {
        Box::pin(async move {
            let (ok, stdout, stderr) = self
                .run(project, &["pr", "view", pr, "--json", VIEW_FIELDS])
                .await?;
            if !ok {
                bail!("gh pr view failed: {}", stderr.trim());
            }
            let value: Value = serde_json::from_str(&stdout).context("gh pr view --json")?;
            parse_pull_request(&value)
        })
    }

    fn update_branch<'a>(
        &'a self,
        project: &'a Path,
        pr: &'a str,
    ) -> BoxFuture<'a, Result<Updated>> {
        Box::pin(async move {
            let (ok, _, stderr) = self.run(project, &["pr", "update-branch", pr]).await?;
            if ok {
                return Ok(Updated::Done);
            }
            if stderr.to_lowercase().contains("conflict") {
                return Ok(Updated::Conflict);
            }
            bail!("gh pr update-branch failed: {}", stderr.trim());
        })
    }

    fn merge<'a>(&'a self, project: &'a Path, pr: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let flag = self.merge_flag(project, pr).await?;
            let (ok, _, stderr) = self.run(project, &["pr", "merge", pr, flag]).await?;
            if !ok {
                bail!("gh pr merge {flag} failed: {}", stderr.trim());
            }
            Ok(())
        })
    }
}

/// `--merge` where the repository allows merge commits, else `--squash`, else `--rebase`.
pub fn merge_flag(repo: &Value) -> &'static str {
    if repo["mergeCommitAllowed"].as_bool().unwrap_or(true) {
        "--merge"
    } else if repo["squashMergeAllowed"].as_bool().unwrap_or(true) {
        "--squash"
    } else {
        "--rebase"
    }
}

/// `owner/repo` with its host, as `gh` takes a repository, from a pull request URL.
fn repo_of(pr: &str) -> Option<String> {
    let (repo, _) = pr.strip_prefix("https://")?.split_once("/pull/")?;
    (repo.split('/').count() == 3).then(|| repo.to_owned())
}

/// The pull request in `gh pr view --json` output with [`VIEW_FIELDS`].
pub fn parse_pull_request(value: &Value) -> Result<PullRequest> {
    let text = |field: &str| value[field].as_str().unwrap_or_default();
    let state = match text("state") {
        "OPEN" => PrState::Open,
        "MERGED" => PrState::Merged,
        "CLOSED" => PrState::Closed,
        other => bail!("gh pr view gave the state {other:?}"),
    };
    let draft = value["isDraft"].as_bool().unwrap_or(false);
    let merge_state = match (text("mergeable"), text("mergeStateStatus")) {
        ("CONFLICTING", _) | (_, "DIRTY") => MergeState::Conflict,
        (_, "BEHIND") => MergeState::Behind,
        // UNSTABLE: mergeable, with a check that is not required failing or pending; the
        // checks decide.
        (_, "CLEAN" | "HAS_HOOKS" | "UNSTABLE") if !draft => MergeState::Clean,
        _ => MergeState::Waits,
    };
    Ok(PullRequest {
        url: text("url").to_owned(),
        state,
        draft,
        head_ref: text("headRefName").to_owned(),
        head_oid: text("headRefOid").to_owned(),
        merge_commit: value["mergeCommit"]["oid"].as_str().map(str::to_owned),
        merge_state,
        checks: parse_checks(&value["statusCheckRollup"]),
    })
}

/// The checks in `statusCheckRollup`: check runs (`status`, `conclusion`) and commit
/// statuses (`state`).
fn parse_checks(rollup: &Value) -> Checks {
    let mut failed = Vec::new();
    let mut pending = false;
    for check in rollup.as_array().into_iter().flatten() {
        let name = check["name"]
            .as_str()
            .or_else(|| check["context"].as_str())
            .unwrap_or("a check");
        let outcome = match check["state"].as_str() {
            // A commit status.
            Some(state) => state,
            None if check["status"].as_str() == Some("COMPLETED") => {
                check["conclusion"].as_str().unwrap_or_default()
            }
            None => "PENDING",
        };
        match outcome {
            "SUCCESS" | "NEUTRAL" | "SKIPPED" => {}
            "PENDING" | "EXPECTED" | "" => pending = true,
            _ => failed.push(name.to_owned()),
        }
    }
    if !failed.is_empty() {
        Checks::Failed(failed)
    } else if pending {
        Checks::Pending
    } else {
        Checks::Passed
    }
}

/// The last pull request URL in `text` (`https://<host>/<owner>/<repo>/pull/<number>`).
pub fn pull_request_url(text: &str) -> Option<String> {
    let mut found = None;
    let mut rest = text;
    while let Some(at) = rest.find("https://") {
        let from = &rest[at..];
        let end = from
            .find(|c: char| !(c.is_ascii_alphanumeric() || "/:.-_~%".contains(c)))
            .unwrap_or(from.len());
        let url = &from[..end];
        if let Some((repo, number)) = url["https://".len()..].split_once("/pull/") {
            let digits: String = number.chars().take_while(char::is_ascii_digit).collect();
            if repo.split('/').count() == 3 && !digits.is_empty() {
                found = Some(format!("https://{repo}/pull/{digits}"));
            }
        }
        rest = &from[end.max("https://".len())..];
    }
    found
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn view(extra: Value) -> Value {
        let mut value = json!({
            "url": "https://github.com/o/r/pull/7",
            "state": "OPEN",
            "isDraft": false,
            "headRefName": "task/factory-7",
            "headRefOid": "abc123",
            "mergeCommit": null,
            "mergeStateStatus": "CLEAN",
            "mergeable": "MERGEABLE",
            "statusCheckRollup": [],
        });
        for (key, v) in extra.as_object().unwrap() {
            value[key] = v.clone();
        }
        value
    }

    #[test]
    fn a_pull_request_reads_its_state_head_and_merge_commit() {
        let pr = parse_pull_request(&view(json!({}))).unwrap();
        assert_eq!(pr.url, "https://github.com/o/r/pull/7");
        assert_eq!(pr.state, PrState::Open);
        assert_eq!(pr.head_ref, "task/factory-7");
        assert_eq!(pr.head_oid, "abc123");
        assert_eq!(pr.merge_state, MergeState::Clean);
        assert_eq!(pr.checks, Checks::Passed);
        assert_eq!(pr.merge_commit, None);

        let merged = parse_pull_request(&view(json!({
            "state": "MERGED", "mergeCommit": {"oid": "def456"}, "mergeStateStatus": "UNKNOWN",
        })))
        .unwrap();
        assert_eq!(merged.state, PrState::Merged);
        assert_eq!(merged.merge_commit.as_deref(), Some("def456"));

        assert!(parse_pull_request(&view(json!({"state": "ODD"}))).is_err());
    }

    #[test]
    fn the_merge_state_says_what_holds_the_merge() {
        let state = |extra| parse_pull_request(&view(extra)).unwrap().merge_state;
        assert_eq!(
            state(json!({"mergeStateStatus": "BEHIND"})),
            MergeState::Behind
        );
        assert_eq!(
            state(json!({"mergeStateStatus": "DIRTY"})),
            MergeState::Conflict
        );
        assert_eq!(
            state(json!({"mergeable": "CONFLICTING", "mergeStateStatus": "BEHIND"})),
            MergeState::Conflict
        );
        assert_eq!(
            state(json!({"mergeStateStatus": "BLOCKED"})),
            MergeState::Waits
        );
        assert_eq!(
            state(json!({"mergeStateStatus": "UNKNOWN"})),
            MergeState::Waits
        );
        assert_eq!(state(json!({"isDraft": true})), MergeState::Waits);
        assert_eq!(
            state(json!({"mergeStateStatus": "UNSTABLE"})),
            MergeState::Clean
        );
    }

    #[test]
    fn checks_pass_only_when_each_ended_well() {
        let checks = |rollup| {
            parse_pull_request(&view(json!({"statusCheckRollup": rollup})))
                .unwrap()
                .checks
        };
        assert_eq!(
            checks(json!([
                {"__typename": "CheckRun", "name": "rust", "status": "COMPLETED", "conclusion": "SUCCESS"},
                {"__typename": "CheckRun", "name": "web", "status": "COMPLETED", "conclusion": "SKIPPED"},
                {"__typename": "StatusContext", "context": "deploy", "state": "SUCCESS"},
            ])),
            Checks::Passed
        );
        assert_eq!(
            checks(json!([
                {"__typename": "CheckRun", "name": "rust", "status": "IN_PROGRESS", "conclusion": ""},
                {"__typename": "CheckRun", "name": "web", "status": "COMPLETED", "conclusion": "SUCCESS"},
            ])),
            Checks::Pending
        );
        assert_eq!(
            checks(
                json!([{"__typename": "StatusContext", "context": "deploy", "state": "PENDING"}])
            ),
            Checks::Pending
        );
        assert_eq!(
            checks(json!([
                {"__typename": "CheckRun", "name": "rust", "status": "COMPLETED", "conclusion": "FAILURE"},
                {"__typename": "CheckRun", "name": "web", "status": "QUEUED"},
                {"__typename": "StatusContext", "context": "deploy", "state": "ERROR"},
            ])),
            Checks::Failed(vec!["rust".into(), "deploy".into()])
        );
    }

    #[test]
    fn the_merge_method_is_the_first_the_repository_allows() {
        assert_eq!(merge_flag(&json!({"mergeCommitAllowed": true})), "--merge");
        assert_eq!(
            merge_flag(&json!({"mergeCommitAllowed": false, "squashMergeAllowed": true})),
            "--squash"
        );
        assert_eq!(
            merge_flag(&json!({
                "mergeCommitAllowed": false, "squashMergeAllowed": false, "rebaseMergeAllowed": true,
            })),
            "--rebase"
        );
    }

    #[test]
    fn the_last_pull_request_url_in_a_text_is_found() {
        assert_eq!(
            pull_request_url("note: parked, PR https://github.com/o/r/pull/12."),
            Some("https://github.com/o/r/pull/12".to_owned())
        );
        assert_eq!(
            pull_request_url(
                "see https://github.com/o/r/pull/3 and (https://github.com/o/r/pull/45/files)"
            ),
            Some("https://github.com/o/r/pull/45".to_owned())
        );
        assert_eq!(pull_request_url("https://github.com/o/r/issues/3"), None);
        assert_eq!(pull_request_url("https://github.com/o/pull/3"), None);
        assert_eq!(pull_request_url("PR #12"), None);
        assert_eq!(
            repo_of("https://github.com/o/r/pull/12").as_deref(),
            Some("github.com/o/r")
        );
    }
}
