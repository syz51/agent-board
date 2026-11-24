//! The pull requests of daemon dispatch (`docs/DESIGN.md`, "Daemon dispatch", Merging and
//! finishing), read and merged with the `gh` CLI.

use std::collections::{BTreeMap, BTreeSet};
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
    /// The branch it merges into.
    pub base_ref: String,
    /// The branch it merges from.
    pub head_ref: String,
    /// The commit its branch is at.
    pub head_oid: String,
    /// Its branch is in another repository (a fork) than its base.
    pub cross_repository: bool,
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
    /// repository allows one, else squashed, else rebased. Only while its branch is still
    /// at `head`, the commit whose checks were read.
    fn merge<'a>(
        &'a self,
        project: &'a Path,
        pr: &'a str,
        head: &'a str,
    ) -> BoxFuture<'a, Result<()>>;
}

/// The fields [`parse_pull_request`] reads.
const VIEW_FIELDS: &str = "url,state,isDraft,baseRefName,headRefName,headRefOid,isCrossRepository,mergeCommit,mergeStateStatus,mergeable,statusCheckRollup";

const COMMAND_TIMEOUT: Duration = Duration::from_secs(60);

/// GitHub records each Actions job it never started for billing as a failed check run with no
/// steps and one failure-level annotation, on path `.github`, whose message starts with this
/// text. GitHub may add notice-level annotations on `.github`, such as a runner label's
/// migration notice.
const BILLING_ANNOTATION_PREFIX: &str =
    "The job was not started because recent account payments have failed or your spending limit";

/// Failed check runs in the head commit's GitHub Actions suites (`appId: 15368` on github.com),
/// with their step count and annotations. On another host the call finds no suites, so failed
/// jobs stay failed.
const FAILED_RUNS_QUERY: &str = "\
query($owner: String!, $name: String!, $oid: GitObjectID!) { \
  repository(owner: $owner, name: $name) { \
    object(oid: $oid) { \
      ... on Commit { \
        checkSuites(first: 20, filterBy: {appId: 15368}) { \
          nodes { \
            app { slug } \
            checkRuns(first: 30, filterBy: {checkType: ALL, conclusions: [FAILURE]}) { \
              nodes { \
                databaseId \
                steps(first: 1) { totalCount } \
                annotations(first: 10) { \
                  totalCount \
                  nodes { path message annotationLevel } \
                } \
              } \
            } \
          } \
        } \
      } \
    } \
  } \
}";

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

    /// The check-run ids of failed Actions jobs on `head_oid` that GitHub never started for
    /// billing. Called only when every failed check in `statusCheckRollup` is a failed Actions
    /// job, so ordinary pull requests incur no extra GraphQL call.
    async fn billing_skipped_jobs(
        &self,
        project: &Path,
        pr: &str,
        head_oid: &str,
    ) -> Result<BTreeSet<String>> {
        let Some((host, owner, name)) = repo_parts(pr) else {
            return Ok(BTreeSet::new());
        };
        let query_arg = format!("query={FAILED_RUNS_QUERY}");
        let owner_arg = format!("owner={owner}");
        let name_arg = format!("name={name}");
        let oid_arg = format!("oid={head_oid}");
        let args = [
            "api",
            "--hostname",
            host,
            "graphql",
            "-f",
            &query_arg,
            "-f",
            &owner_arg,
            "-f",
            &name_arg,
            "-f",
            &oid_arg,
        ];
        let (ok, stdout, stderr) = self.run(project, &args).await?;
        if !ok {
            bail!(
                "gh api graphql failed reading Actions check annotations: {}",
                stderr.trim()
            );
        }
        let value: Value = serde_json::from_str(&stdout).context("gh api graphql")?;
        if value["data"]["repository"]["object"].is_null()
            && value["errors"]
                .as_array()
                .is_some_and(|errors| !errors.is_empty())
        {
            bail!("gh api graphql returned errors reading Actions check annotations");
        }
        Ok(parse_billing_skipped_jobs(&value))
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
            let empty = BTreeSet::new();
            let (mut parsed, all_failed_are_actions) =
                parse_pull_request_classified(&value, &empty)?;
            if parsed.state == PrState::Open && all_failed_are_actions {
                let skipped = self
                    .billing_skipped_jobs(project, &parsed.url, &parsed.head_oid)
                    .await?;
                if !skipped.is_empty() {
                    parsed.checks = classify_checks(&value["statusCheckRollup"], &skipped).checks;
                }
            }
            Ok(parsed)
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

    fn merge<'a>(
        &'a self,
        project: &'a Path,
        pr: &'a str,
        head: &'a str,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let flag = self.merge_flag(project, pr).await?;
            let args = ["pr", "merge", pr, flag, "--match-head-commit", head];
            let (ok, _, stderr) = self.run(project, &args).await?;
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
pub fn repo_of(pr: &str) -> Option<String> {
    let (host, owner, name) = repo_parts(pr)?;
    Some(format!("{host}/{owner}/{name}"))
}

/// `(host, owner, name)` from a pull request URL (`https://<host>/<owner>/<name>/pull/<number>`).
fn repo_parts(pr: &str) -> Option<(&str, &str, &str)> {
    let (repo, _) = pr.strip_prefix("https://")?.split_once("/pull/")?;
    let mut parts = repo.split('/');
    let host = parts.next().filter(|s| !s.is_empty())?;
    let owner = parts.next().filter(|s| !s.is_empty())?;
    let name = parts.next().filter(|s| !s.is_empty())?;
    parts.next().is_none().then_some((host, owner, name))
}

/// The pull request in `gh pr view --json` output with [`VIEW_FIELDS`].
pub fn parse_pull_request(value: &Value) -> Result<PullRequest> {
    let empty = BTreeSet::new();
    Ok(parse_pull_request_classified(value, &empty)?.0)
}

/// The pull request in `gh pr view --json` output, resolving any GitHub Actions jobs in
/// `billing_skipped` against matching non-Actions fallback checks on the same head commit.
pub fn parse_pull_request_with_billing_skipped(
    value: &Value,
    billing_skipped: &BTreeSet<String>,
) -> Result<PullRequest> {
    Ok(parse_pull_request_classified(value, billing_skipped)?.0)
}

fn parse_pull_request_classified(
    value: &Value,
    billing_skipped: &BTreeSet<String>,
) -> Result<(PullRequest, bool)> {
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
    let classified = classify_checks(&value["statusCheckRollup"], billing_skipped);
    Ok((
        PullRequest {
            url: text("url").to_owned(),
            state,
            draft,
            base_ref: text("baseRefName").to_owned(),
            head_ref: text("headRefName").to_owned(),
            head_oid: text("headRefOid").to_owned(),
            cross_repository: value["isCrossRepository"].as_bool().unwrap_or(false),
            merge_commit: value["mergeCommit"]["oid"].as_str().map(str::to_owned),
            merge_state,
            checks: classified.checks,
        },
        classified.all_failed_are_actions_failures,
    ))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ClassifiedChecks {
    checks: Checks,
    all_failed_are_actions_failures: bool,
}

/// `(run_id, job_id)` from a GitHub Actions job URL (`.../actions/runs/<run>/job/<job>`).
fn parse_actions_job_url(url: &str) -> Option<(&str, &str)> {
    let (_, after_runs) = url.split_once("/actions/runs/")?;
    let (run_id, after_job) = after_runs.split_once("/job/")?;
    if run_id.is_empty() || !run_id.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let job_end = after_job.bytes().take_while(u8::is_ascii_digit).count();
    if job_end == 0 {
        return None;
    }
    let job_id = &after_job[..job_end];
    if let Some(next) = after_job.as_bytes().get(job_end)
        && !matches!(next, b'/' | b'?' | b'#')
    {
        return None;
    }
    Some((run_id, job_id))
}

/// `(run_id, job_id)` when `check` is a GitHub Actions `CheckRun`.
fn actions_run_and_job(check: &Value) -> Option<(&str, &str)> {
    if check["__typename"].as_str() != Some("CheckRun") {
        return None;
    }
    parse_actions_job_url(check["detailsUrl"].as_str()?)
}

/// The Actions job id when `check` is a GitHub Actions `CheckRun`.
fn actions_job(check: &Value) -> Option<&str> {
    actions_run_and_job(check).map(|(_, job)| job)
}

/// Within one Actions workflow run (which a re-run reuses), keeps only the latest attempt of
/// each job; all other checks are preserved.
fn latest_checks(rollup: &[Value]) -> Vec<&Value> {
    fn started(check: &Value) -> &str {
        match check["startedAt"].as_str() {
            Some(ts) if !ts.is_empty() && !ts.starts_with("0001-") => ts,
            _ => "9999",
        }
    }
    let mut kept: Vec<&Value> = Vec::with_capacity(rollup.len());
    let mut positions: BTreeMap<(&str, &str, &str), usize> = BTreeMap::new();
    for check in rollup {
        if let Some((run_id, _)) = actions_run_and_job(check) {
            let key = (
                run_id,
                check["workflowName"].as_str().unwrap_or_default(),
                check["name"].as_str().unwrap_or_default(),
            );
            if let Some(&idx) = positions.get(&key) {
                if started(check) >= started(kept[idx]) {
                    kept[idx] = check;
                }
                continue;
            }
            positions.insert(key, kept.len());
        }
        kept.push(check);
    }
    kept
}

/// Whether `other` is a non-Actions fallback check named `<context>: <job_name>`.
fn is_fallback_for(other: &Value, job_name: &str) -> bool {
    if actions_job(other).is_some() {
        return false;
    }
    let other_name = other["context"]
        .as_str()
        .or_else(|| other["name"].as_str())
        .unwrap_or_default();
    other_name
        .strip_suffix(job_name)
        .and_then(|prefix| prefix.strip_suffix(": "))
        .is_some_and(|context| !context.is_empty())
}

fn check_passed(check: &Value) -> bool {
    match check["state"].as_str() {
        Some(state) => state == "SUCCESS",
        None => {
            check["status"].as_str() == Some("COMPLETED")
                && check["conclusion"].as_str() == Some("SUCCESS")
        }
    }
}

fn check_running(check: &Value) -> bool {
    match check["state"].as_str() {
        Some(state) => matches!(state, "PENDING" | "EXPECTED" | ""),
        None => check["status"].as_str() != Some("COMPLETED"),
    }
}

/// The check-run ids in `FAILED_RUNS_QUERY` output that GitHub never started for billing:
/// a `FAILURE` check run of the `github-actions` app with zero steps and all annotations on
/// path `.github`, containing exactly one `FAILURE` billing-refusal annotation and only
/// `NOTICE` annotations besides.
pub fn parse_billing_skipped_jobs(graphql: &Value) -> BTreeSet<String> {
    let mut skipped = BTreeSet::new();
    let Some(suites) = graphql["data"]["repository"]["object"]["checkSuites"]["nodes"].as_array()
    else {
        return skipped;
    };
    for suite in suites {
        if suite["app"]["slug"].as_str() != Some("github-actions") {
            continue;
        }
        let Some(runs) = suite["checkRuns"]["nodes"].as_array() else {
            continue;
        };
        for run in runs {
            if run["steps"]["totalCount"].as_u64() != Some(0) {
                continue;
            }
            let Some(annotations) = run["annotations"]["nodes"].as_array() else {
                continue;
            };
            if run["annotations"]["totalCount"].as_u64() != Some(annotations.len() as u64) {
                continue;
            }
            if !annotations
                .iter()
                .all(|ann| ann["path"].as_str() == Some(".github"))
            {
                continue;
            }
            let mut billing_failures = 0usize;
            let mut all_billing_or_notice = true;
            for ann in annotations {
                let level = ann["annotationLevel"].as_str();
                let is_billing = level == Some("FAILURE")
                    && ann["message"]
                        .as_str()
                        .is_some_and(|msg| msg.starts_with(BILLING_ANNOTATION_PREFIX));
                if is_billing {
                    billing_failures += 1;
                } else if level != Some("NOTICE") {
                    all_billing_or_notice = false;
                    break;
                }
            }
            if billing_failures != 1 || !all_billing_or_notice {
                continue;
            }
            let id = run["databaseId"]
                .as_u64()
                .map(|n| n.to_string())
                .or_else(|| {
                    run["databaseId"]
                        .as_str()
                        .filter(|s| !s.is_empty())
                        .map(str::to_owned)
                });
            if let Some(id) = id {
                skipped.insert(id);
            }
        }
    }
    skipped
}

/// The checks in `statusCheckRollup`: check runs (`status`, `conclusion`) and commit
/// statuses (`state`). Billing-skipped Actions jobs in `billing_skipped` pass when an
/// unambiguous non-Actions check named `<context>: <job>` passed on the same head commit,
/// wait while one is pending, and fail otherwise.
fn classify_checks(rollup: &Value, billing_skipped: &BTreeSet<String>) -> ClassifiedChecks {
    let checks = latest_checks(rollup.as_array().map(Vec::as_slice).unwrap_or(&[]));
    let is_skipped = |check: &Value| -> bool {
        check["state"].as_str().is_none()
            && check["status"].as_str() == Some("COMPLETED")
            && check["conclusion"].as_str() == Some("FAILURE")
            && actions_job(check).is_some_and(|job| billing_skipped.contains(job))
    };
    let mut workflows_of: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for check in checks.iter().copied().filter(|c| is_skipped(c)) {
        if let Some(name) = check["name"].as_str() {
            workflows_of
                .entry(name)
                .or_default()
                .insert(check["workflowName"].as_str().unwrap_or_default());
        }
    }
    let mut failed = Vec::new();
    let mut actions_failed = 0usize;
    let mut pending = false;
    for &check in &checks {
        let name = check["name"]
            .as_str()
            .or_else(|| check["context"].as_str())
            .unwrap_or("a check");
        if let Some(state) = check["state"].as_str() {
            match state {
                "SUCCESS" | "NEUTRAL" | "SKIPPED" => {}
                "PENDING" | "EXPECTED" | "" => pending = true,
                _ => failed.push(name.to_owned()),
            }
            continue;
        }
        if check["status"].as_str() != Some("COMPLETED") {
            pending = true;
            continue;
        }
        if is_skipped(check) {
            let ambiguous = workflows_of
                .get(name)
                .is_some_and(|workflows| workflows.len() > 1);
            if ambiguous {
                failed.push(name.to_owned());
                continue;
            }
            let passed = checks
                .iter()
                .copied()
                .any(|other| is_fallback_for(other, name) && check_passed(other));
            let running = checks
                .iter()
                .copied()
                .any(|other| is_fallback_for(other, name) && check_running(other));
            if passed {
                continue;
            }
            if running {
                pending = true;
            } else {
                failed.push(name.to_owned());
            }
            continue;
        }
        let outcome = check["conclusion"].as_str().unwrap_or_default();
        match outcome {
            "SUCCESS" | "NEUTRAL" | "SKIPPED" => {}
            "PENDING" | "EXPECTED" | "" => pending = true,
            _ => {
                failed.push(name.to_owned());
                if outcome == "FAILURE" && actions_job(check).is_some() {
                    actions_failed += 1;
                }
            }
        }
    }
    let all_failed_are_actions_failures = !failed.is_empty() && failed.len() == actions_failed;
    let checks = if !failed.is_empty() {
        Checks::Failed(failed)
    } else if pending {
        Checks::Pending
    } else {
        Checks::Passed
    };
    ClassifiedChecks {
        checks,
        all_failed_are_actions_failures,
    }
}

/// The last pull request URL in `text` (`https://<host>/<owner>/<repo>/pull/<number>`).
pub fn pull_request_url(text: &str) -> Option<String> {
    pull_request_url_in(text, &[])
}

/// The last pull request URL in `text` belonging to one of `repos` (or any repository when
/// `repos` is empty).
pub fn pull_request_url_in(text: &str, repos: &[String]) -> Option<String> {
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
            let known = repos.is_empty() || repos.iter().any(|k| k.eq_ignore_ascii_case(repo));
            if repo.split('/').count() == 3 && !digits.is_empty() && known {
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
            "baseRefName": "main",
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
        assert_eq!(pr.base_ref, "main");
        assert_eq!(pr.head_ref, "task/factory-7");
        assert_eq!(pr.head_oid, "abc123");
        assert_eq!(pr.merge_state, MergeState::Clean);
        assert_eq!(pr.checks, Checks::Passed);
        assert_eq!(pr.merge_commit, None);

        let merged = parse_pull_request(&view(json!({
            "state": "MERGED", "baseRefName": "fork",
            "mergeCommit": {"oid": "def456"}, "mergeStateStatus": "UNKNOWN",
        })))
        .unwrap();
        assert_eq!(merged.state, PrState::Merged);
        assert_eq!(merged.base_ref, "fork");
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

    fn actions_check(workflow: &str, name: &str, run: u64, job: u64, conclusion: &str) -> Value {
        json!({
            "__typename": "CheckRun",
            "workflowName": workflow,
            "name": name,
            "status": "COMPLETED",
            "conclusion": conclusion,
            "startedAt": "2026-11-24T04:59:42Z",
            "detailsUrl": format!("https://github.com/o/r/actions/runs/{run}/job/{job}"),
        })
    }

    #[test]
    fn billing_skipped_actions_checks_pass_with_matching_circleci_fallback() {
        let pr_json = view(json!({
            "mergeStateStatus": "UNSTABLE",
            "statusCheckRollup": [
                actions_check("Infrastructure checks", "rust-controllers", 10, 101, "FAILURE"),
                actions_check("Infrastructure checks", "validate", 10, 102, "FAILURE"),
                {"__typename": "StatusContext", "context": "ci/circleci: rust-controllers", "state": "SUCCESS"},
                {"__typename": "StatusContext", "context": "ci/circleci: validate", "state": "SUCCESS"},
            ],
        }));
        // Without billing-refusal proof, the failed Actions checks stay failed.
        assert_eq!(
            parse_pull_request(&pr_json).unwrap().checks,
            Checks::Failed(vec!["rust-controllers".into(), "validate".into()])
        );
        let skipped: BTreeSet<String> = ["101".into(), "102".into()].into_iter().collect();
        let pr = parse_pull_request_with_billing_skipped(&pr_json, &skipped).unwrap();
        assert_eq!(pr.merge_state, MergeState::Clean);
        assert_eq!(pr.checks, Checks::Passed);
    }

    #[test]
    fn latest_attempt_within_one_actions_workflow_run_wins() {
        let rollup = json!([
            {
                "__typename": "CheckRun",
                "workflowName": "CI",
                "name": "rust",
                "status": "COMPLETED",
                "conclusion": "FAILURE",
                "startedAt": "2026-11-24T04:00:00Z",
                "detailsUrl": "https://github.com/o/r/actions/runs/55/job/501",
            },
            {
                "__typename": "CheckRun",
                "workflowName": "CI",
                "name": "rust",
                "status": "COMPLETED",
                "conclusion": "SUCCESS",
                "startedAt": "2026-11-24T04:10:00Z",
                "detailsUrl": "https://github.com/o/r/actions/runs/55/job/502",
            },
        ]);
        let pr = parse_pull_request(&view(json!({"statusCheckRollup": rollup}))).unwrap();
        assert_eq!(pr.checks, Checks::Passed);
    }

    #[test]
    fn billing_skipped_actions_checks_wait_while_fallback_runs_and_fail_without_one() {
        let skipped: BTreeSet<String> = ["101".into(), "102".into()].into_iter().collect();
        // Pending fallback -> Pending.
        let pending = parse_pull_request_with_billing_skipped(
            &view(json!({
                "statusCheckRollup": [
                    actions_check("Infrastructure checks", "rust-controllers", 10, 101, "FAILURE"),
                    actions_check("Infrastructure checks", "validate", 10, 102, "FAILURE"),
                    {"__typename": "StatusContext", "context": "ci/circleci: rust-controllers", "state": "SUCCESS"},
                    {"__typename": "StatusContext", "context": "ci/circleci: validate", "state": "PENDING"},
                ],
            })),
            &skipped,
        )
        .unwrap();
        assert_eq!(pending.checks, Checks::Pending);

        // Missing fallback -> Failed.
        let missing = parse_pull_request_with_billing_skipped(
            &view(json!({
                "statusCheckRollup": [
                    actions_check("Infrastructure checks", "rust-controllers", 10, 101, "FAILURE"),
                    actions_check("Infrastructure checks", "validate", 10, 102, "FAILURE"),
                    {"__typename": "StatusContext", "context": "ci/circleci: rust-controllers", "state": "SUCCESS"},
                ],
            })),
            &skipped,
        )
        .unwrap();
        assert_eq!(missing.checks, Checks::Failed(vec!["validate".into()]));

        // Real Actions failure (id not in billing_skipped) stays Failed even with a CircleCI status.
        let real_failure = parse_pull_request_with_billing_skipped(
            &view(json!({
                "statusCheckRollup": [
                    actions_check("Infrastructure checks", "rust-controllers", 10, 999, "FAILURE"),
                    {"__typename": "StatusContext", "context": "ci/circleci: rust-controllers", "state": "SUCCESS"},
                ],
            })),
            &skipped,
        )
        .unwrap();
        assert_eq!(
            real_failure.checks,
            Checks::Failed(vec!["rust-controllers".into()])
        );

        // Ambiguous job name skipped across two workflows stays Failed.
        let ambiguous_skipped: BTreeSet<String> =
            ["101".into(), "201".into()].into_iter().collect();
        let ambiguous = parse_pull_request_with_billing_skipped(
            &view(json!({
                "statusCheckRollup": [
                    actions_check("Workflow A", "validate", 10, 101, "FAILURE"),
                    actions_check("Workflow B", "validate", 11, 201, "FAILURE"),
                    {"__typename": "StatusContext", "context": "ci/circleci: validate", "state": "SUCCESS"},
                ],
            })),
            &ambiguous_skipped,
        )
        .unwrap();
        assert_eq!(
            ambiguous.checks,
            Checks::Failed(vec!["validate".into(), "validate".into()])
        );
    }

    #[test]
    fn parse_billing_skipped_jobs_accepts_only_zero_step_billing_annotations() {
        let graphql = json!({
            "data": {
                "repository": {
                    "object": {
                        "checkSuites": {
                            "nodes": [
                                {
                                    "app": {"slug": "github-actions"},
                                    "checkRuns": {
                                        "nodes": [
                                            // Valid: 0 steps, 1 billing FAILURE + 1 NOTICE on .github.
                                            {
                                                "databaseId": 101,
                                                "steps": {"totalCount": 0},
                                                "annotations": {
                                                    "totalCount": 2,
                                                    "nodes": [
                                                        {
                                                            "path": ".github",
                                                            "annotationLevel": "FAILURE",
                                                            "message": "The job was not started because recent account payments have failed or your spending limit needs to be increased."
                                                        },
                                                        {
                                                            "path": ".github",
                                                            "annotationLevel": "NOTICE",
                                                            "message": "Runner notice"
                                                        }
                                                    ]
                                                }
                                            },
                                            // Rejected: ran steps.
                                            {
                                                "databaseId": 102,
                                                "steps": {"totalCount": 3},
                                                "annotations": {
                                                    "totalCount": 1,
                                                    "nodes": [{
                                                        "path": ".github",
                                                        "annotationLevel": "FAILURE",
                                                        "message": "The job was not started because recent account payments have failed or your spending limit needs to be increased."
                                                    }]
                                                }
                                            },
                                            // Rejected: non-.github annotation path.
                                            {
                                                "databaseId": 103,
                                                "steps": {"totalCount": 0},
                                                "annotations": {
                                                    "totalCount": 1,
                                                    "nodes": [{
                                                        "path": "src/main.rs",
                                                        "annotationLevel": "FAILURE",
                                                        "message": "The job was not started because recent account payments have failed or your spending limit needs to be increased."
                                                    }]
                                                }
                                            },
                                            // Rejected: truncated annotations.
                                            {
                                                "databaseId": 104,
                                                "steps": {"totalCount": 0},
                                                "annotations": {
                                                    "totalCount": 2,
                                                    "nodes": [{
                                                        "path": ".github",
                                                        "annotationLevel": "FAILURE",
                                                        "message": "The job was not started because recent account payments have failed or your spending limit needs to be increased."
                                                    }]
                                                }
                                            }
                                        ]
                                    }
                                },
                                // Rejected: non-github-actions app.
                                {
                                    "app": {"slug": "other-ci"},
                                    "checkRuns": {
                                        "nodes": [{
                                            "databaseId": 105,
                                            "steps": {"totalCount": 0},
                                            "annotations": {
                                                "totalCount": 1,
                                                "nodes": [{
                                                    "path": ".github",
                                                    "annotationLevel": "FAILURE",
                                                    "message": "The job was not started because recent account payments have failed or your spending limit needs to be increased."
                                                }]
                                            }
                                        }]
                                    }
                                }
                            ]
                        }
                    }
                }
            }
        });
        let skipped = parse_billing_skipped_jobs(&graphql);
        assert_eq!(skipped, BTreeSet::from(["101".to_owned()]));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn gh_cli_view_queries_graphql_only_when_all_failed_checks_are_actions() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("calls.log");
        let view_json = dir.path().join("view.json");
        let graphql_json = dir.path().join("graphql.json");
        let fake_gh = dir.path().join("gh");
        let script = format!(
            "#!/bin/sh\necho \"$*\" >> \"{}\"\nif [ \"$1\" = \"pr\" ]; then\n  cat \"{}\"\nelse\n  cat \"{}\"\nfi\n",
            log.display(),
            view_json.display(),
            graphql_json.display(),
        );
        std::fs::write(&fake_gh, script).unwrap();
        std::fs::set_permissions(&fake_gh, std::fs::Permissions::from_mode(0o755)).unwrap();

        let cli = GhCli { bin: fake_gh };

        // Case 1: passing checks -> exactly 1 gh call (no GraphQL call).
        std::fs::write(
            &view_json,
            view(json!({
                "statusCheckRollup": [
                    actions_check("CI", "rust", 10, 101, "SUCCESS"),
                ],
            }))
            .to_string(),
        )
        .unwrap();
        let pr = cli
            .view(dir.path(), "https://github.com/o/r/pull/7")
            .await
            .unwrap();
        assert_eq!(pr.checks, Checks::Passed);
        let calls = std::fs::read_to_string(&log).unwrap();
        assert_eq!(calls.lines().count(), 1);
        assert!(calls.starts_with("pr view "));

        // Case 2: billing-refused Actions jobs + passing CircleCI -> 2 calls (pr view + graphql) -> Passed.
        std::fs::write(&log, "").unwrap();
        std::fs::write(
            &view_json,
            view(json!({
                "mergeStateStatus": "UNSTABLE",
                "statusCheckRollup": [
                    actions_check("Infrastructure checks", "rust-controllers", 10, 101, "FAILURE"),
                    actions_check("Infrastructure checks", "validate", 10, 102, "FAILURE"),
                    {"__typename": "StatusContext", "context": "ci/circleci: rust-controllers", "state": "SUCCESS"},
                    {"__typename": "StatusContext", "context": "ci/circleci: validate", "state": "SUCCESS"},
                ],
            }))
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            &graphql_json,
            json!({
                "data": {
                    "repository": {
                        "object": {
                            "checkSuites": {
                                "nodes": [{
                                    "app": {"slug": "github-actions"},
                                    "checkRuns": {
                                        "nodes": [
                                            {
                                                "databaseId": 101,
                                                "steps": {"totalCount": 0},
                                                "annotations": {
                                                    "totalCount": 1,
                                                    "nodes": [{
                                                        "path": ".github",
                                                        "annotationLevel": "FAILURE",
                                                        "message": "The job was not started because recent account payments have failed or your spending limit needs to be increased."
                                                    }]
                                                }
                                            },
                                            {
                                                "databaseId": 102,
                                                "steps": {"totalCount": 0},
                                                "annotations": {
                                                    "totalCount": 1,
                                                    "nodes": [{
                                                        "path": ".github",
                                                        "annotationLevel": "FAILURE",
                                                        "message": "The job was not started because recent account payments have failed or your spending limit needs to be increased."
                                                    }]
                                                }
                                            }
                                        ]
                                    }
                                }]
                            }
                        }
                    }
                }
            })
            .to_string(),
        )
        .unwrap();
        let pr = cli
            .view(dir.path(), "https://github.com/o/r/pull/7")
            .await
            .unwrap();
        assert_eq!(pr.checks, Checks::Passed);
        let calls: Vec<String> = std::fs::read_to_string(&log)
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect();
        assert_eq!(calls.len(), 2);
        assert!(calls[1].starts_with("api --hostname github.com graphql "));

        // Case 3: non-Actions failure alongside Actions failure -> 1 call only (no GraphQL call).
        std::fs::write(&log, "").unwrap();
        std::fs::write(
            &view_json,
            view(json!({
                "statusCheckRollup": [
                    actions_check("Infrastructure checks", "validate", 10, 102, "FAILURE"),
                    {"__typename": "StatusContext", "context": "ci/circleci: validate", "state": "FAILURE"},
                ],
            }))
            .to_string(),
        )
        .unwrap();
        let pr = cli
            .view(dir.path(), "https://github.com/o/r/pull/7")
            .await
            .unwrap();
        assert_eq!(
            pr.checks,
            Checks::Failed(vec!["validate".into(), "ci/circleci: validate".into()])
        );
        assert_eq!(std::fs::read_to_string(&log).unwrap().lines().count(), 1);
    }
}
