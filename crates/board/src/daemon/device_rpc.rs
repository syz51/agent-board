//! Background device-RPC bridge for `board watch`.
//!
//! Long-polls `GET /v1/devices/{device}/rpc/poll` and executes T3 thread operations locally
//! via [`T3Cli`], posting compact, unwrapped, bounded results back to
//! `POST /v1/devices/{device}/rpc/{id}/reply`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, bail};
use board_proto::{
    DEVICE_RPC_ERROR_CHARS_MAX, DEVICE_RPC_POLL_WAIT_SECS_DEFAULT, DeviceName, DeviceRpcCall,
    DeviceRpcReply,
};
use futures_util::stream::{self, StreamExt as _};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::{Semaphore, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::client::{Client, is_endpoint_unsupported};
use crate::config::Config;
use crate::daemon::Shutdown;
use crate::t3::{self, T3, T3Cli};

/// Maximum concurrent RPC handlers running on one device so bursts do not spawn excessive
/// `t3` subprocesses.
const MAX_CONCURRENT_RPCS: usize = 4;
/// Maximum concurrent `t3 thread list` calls when listing threads across all projects.
const LIST_CONCURRENCY: usize = 4;
/// Backoff when talking to an older server that returns 404/405 for `/rpc/poll`.
const UNSUPPORTED_BACKOFF: Duration = Duration::from_secs(60);
/// Initial and maximum backoff on transient poll failures.
const ERROR_BACKOFF_FIRST: Duration = Duration::from_secs(1);
const ERROR_BACKOFF_MAX: Duration = Duration::from_secs(30);
/// Maximum prompt/message length accepted by `t3.thread.launch` and `t3.thread.send`.
const MAX_PROMPT_CHARS: usize = 16_000;
/// Maximum `wait_secs` for synchronous reply polling on launch/send.
const MAX_WAIT_SECS: u64 = 25;
/// Maximum assistant reply length returned inline from launch/send `wait_secs`.
const MAX_INLINE_REPLY_CHARS: usize = 4_000;

/// Spawns the background device-RPC long-poll loop until `shutdown` resolves.
/// Reads live `Config` snapshots from `config_rx` so `config.toml` reloads take effect
/// immediately without restarting `board watch`.
pub fn spawn(
    client: Client,
    device: DeviceName,
    config_rx: watch::Receiver<Config>,
    t3: T3Cli,
    mut shutdown: Shutdown,
) -> JoinHandle<()> {
    let t3 = Arc::new(t3);
    let permits = Arc::new(Semaphore::new(MAX_CONCURRENT_RPCS));
    tokio::spawn(async move {
        let mut backoff = ERROR_BACKOFF_FIRST;
        let mut logged_unsupported = false;
        loop {
            let polled = tokio::select! {
                () = shutdown.wait() => break,
                res = client.poll_device_rpc(&device, DEVICE_RPC_POLL_WAIT_SECS_DEFAULT) => res,
            };
            match polled {
                Ok(calls) => {
                    backoff = ERROR_BACKOFF_FIRST;
                    logged_unsupported = false;
                    for call in calls {
                        let client = client.clone();
                        let device = device.clone();
                        let config_rx = config_rx.clone();
                        let t3 = Arc::clone(&t3);
                        let permits = Arc::clone(&permits);
                        // Start the per-RPC deadline immediately when polled so queued
                        // mutating RPCs cannot run after the server has already timed out.
                        let deadline =
                            Instant::now() + Duration::from_secs(call.timeout_secs.max(1));
                        tokio::spawn(async move {
                            let permit = match tokio::time::timeout_at(deadline, permits.acquire())
                                .await
                            {
                                Ok(Ok(p)) => p,
                                Ok(Err(_)) => return,
                                Err(_) => {
                                    let reply = error_reply(format!(
                                        "rpc {} timed out waiting for local concurrency slot after {}s",
                                        call.method, call.timeout_secs
                                    ));
                                    let _ =
                                        client.reply_device_rpc(&device, &call.id, &reply).await;
                                    return;
                                }
                            };
                            let cfg = config_rx.borrow().clone();
                            let reply = match tokio::time::timeout_at(
                                deadline,
                                handle_call(&t3, &cfg, &call),
                            )
                            .await
                            {
                                Ok(reply) => reply,
                                Err(_) => error_reply(format!(
                                    "rpc {} timed out locally after {}s",
                                    call.method, call.timeout_secs
                                )),
                            };
                            drop(permit);
                            if let Err(err) =
                                client.reply_device_rpc(&device, &call.id, &reply).await
                            {
                                tracing::debug!(
                                    call_id = %call.id,
                                    method = %call.method,
                                    "posting device RPC reply: {err:#}"
                                );
                            }
                        });
                    }
                }
                Err(err) if is_endpoint_unsupported(&err) => {
                    if !logged_unsupported {
                        tracing::info!(
                            "board server does not expose /v1/devices/{device}/rpc/poll yet; \
                             device RPC bridge idle"
                        );
                        logged_unsupported = true;
                    }
                    tokio::select! {
                        () = shutdown.wait() => break,
                        () = tokio::time::sleep(UNSUPPORTED_BACKOFF) => {}
                    }
                }
                Err(err) => {
                    tracing::warn!("device RPC poll failed: {err:#}; retrying in {backoff:?}");
                    tokio::select! {
                        () = shutdown.wait() => break,
                        () = tokio::time::sleep(backoff) => {}
                    }
                    backoff = (backoff * 2).min(ERROR_BACKOFF_MAX);
                }
            }
        }
    })
}

/// Dispatches one polled [`DeviceRpcCall`] locally using `t3` and `config`.
pub async fn handle_call(t3: &T3Cli, config: &Config, call: &DeviceRpcCall) -> DeviceRpcReply {
    let outcome = match call.method.as_str() {
        "t3.projects.list" => handle_projects_list(config),
        "t3.threads.list" => handle_threads_list(t3, config, call.params.clone()).await,
        "t3.thread.read" => handle_thread_read(t3, call.params.clone()).await,
        "t3.thread.launch" => handle_thread_launch(t3, config, call.params.clone()).await,
        "t3.thread.send" => handle_thread_send(t3, call.params.clone()).await,
        other => Err(anyhow::anyhow!("unknown device RPC method {other:?}")),
    };
    match outcome {
        Ok(result) => DeviceRpcReply {
            ok: true,
            result: Some(result),
            error: None,
        },
        Err(err) => error_reply(format!("{err:#}")),
    }
}

fn error_reply(msg: String) -> DeviceRpcReply {
    let trimmed = msg.trim();
    let bounded = if trimmed.is_empty() {
        "device RPC failed".to_owned()
    } else {
        truncate_chars(trimmed, DEVICE_RPC_ERROR_CHARS_MAX)
    };
    DeviceRpcReply {
        ok: false,
        result: None,
        error: Some(bounded),
    }
}

/// Validates a CLI argument token that must not be empty, contain whitespace/control chars,
/// or begin with `-` (which prevents CLI flag injection when passed to `t3`).
fn validate_cli_token<'a>(field: &str, raw: &'a str) -> Result<&'a str> {
    let trimmed = raw.trim();
    if trimmed.is_empty()
        || trimmed.starts_with('-')
        || trimmed.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        bail!("invalid {field}");
    }
    Ok(trimmed)
}

#[derive(Debug, Clone)]
struct ProjectEntry {
    name: String,
    key: String,
    path: PathBuf,
    kind: &'static str,
    exists: bool,
}

fn configured_projects(config: &Config) -> Vec<ProjectEntry> {
    let mut by_path: BTreeMap<PathBuf, ProjectEntry> = BTreeMap::new();
    for (repo_key, path) in &config.projects {
        let name = short_project_name(repo_key, path);
        by_path.entry(path.clone()).or_insert_with(|| ProjectEntry {
            name,
            key: repo_key.clone(),
            path: path.clone(),
            kind: "repo",
            exists: path.is_dir(),
        });
    }
    for (slug, path) in &config.board_projects {
        by_path.entry(path.clone()).or_insert_with(|| ProjectEntry {
            name: slug.to_string(),
            key: slug.to_string(),
            path: path.clone(),
            kind: "board",
            exists: path.is_dir(),
        });
    }
    let mut list: Vec<ProjectEntry> = by_path.into_values().collect();
    list.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.path.cmp(&b.path)));
    list
}

fn short_project_name(key: &str, path: &Path) -> String {
    key.rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .or_else(|| path.file_name().and_then(|n| n.to_str()))
        .unwrap_or(key)
        .to_owned()
}

fn resolve_project(config: &Config, query: &str) -> Result<(String, PathBuf)> {
    let q = query.trim();
    if q.is_empty() || q.starts_with('-') {
        bail!("invalid project {q:?}");
    }
    let q_lower = q.to_ascii_lowercase();
    let projects = configured_projects(config);

    // 1. Exact path match.
    if let Some(p) = projects.iter().find(|p| p.path == Path::new(q)) {
        return Ok((p.name.clone(), p.path.clone()));
    }
    // 2. Exact repo key or board slug match (case-insensitive).
    if let Some((k, path)) = config
        .projects
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(q))
    {
        return Ok((short_project_name(k, path), path.clone()));
    }
    if let Some((slug, path)) = config
        .board_projects
        .iter()
        .find(|(s, _)| s.as_str().eq_ignore_ascii_case(q))
    {
        return Ok((slug.to_string(), path.clone()));
    }
    // 3. Exact short name match (e.g. "agent-board" for "github.com/syz51/agent-board").
    let short_matches: Vec<&ProjectEntry> = projects
        .iter()
        .filter(|p| p.name.eq_ignore_ascii_case(&q_lower))
        .collect();
    if short_matches.len() == 1 {
        let p = short_matches[0];
        return Ok((p.name.clone(), p.path.clone()));
    }
    // 4. Direct existing absolute path fallback (user-authorized).
    let as_path = Path::new(q);
    if as_path.is_absolute() && as_path.is_dir() {
        let label = as_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(q)
            .to_owned();
        return Ok((label, as_path.to_path_buf()));
    }

    let known: Vec<&str> = projects.iter().map(|p| p.name.as_str()).collect();
    if known.is_empty() {
        bail!("unknown project {q:?} (no projects configured in config.toml)");
    }
    bail!(
        "unknown project {q:?}; available on this device: {}",
        known.join(", ")
    )
}

fn handle_projects_list(config: &Config) -> Result<Value> {
    let projects: Vec<Value> = configured_projects(config)
        .into_iter()
        .map(|p| {
            let mut obj = json!({
                "name": p.name,
                "key": p.key,
                "path": p.path.display().to_string(),
                "kind": p.kind,
            });
            if !p.exists {
                obj["exists"] = json!(false);
            }
            obj
        })
        .collect();
    Ok(json!({ "projects": projects }))
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct ThreadsListParams {
    project: Option<String>,
    status: Option<String>,
    limit: Option<u32>,
}

async fn handle_threads_list(t3: &T3Cli, config: &Config, params: Value) -> Result<Value> {
    let p: ThreadsListParams = if params.is_null() {
        ThreadsListParams::default()
    } else {
        serde_json::from_value(params)?
    };
    let status_filter = p.status.as_deref().unwrap_or("all");
    if !matches!(status_filter, "all" | "running" | "idle") {
        bail!("invalid status filter {status_filter:?}; use all, running, or idle");
    }
    let limit = p.limit.unwrap_or(15).clamp(1, 50) as usize;

    let targets: Vec<(String, PathBuf)> = match p.project.as_deref() {
        Some(proj) => vec![resolve_project(config, proj)?],
        None => configured_projects(config)
            .into_iter()
            .filter(|e| e.exists)
            .map(|e| (e.name, e.path))
            .collect(),
    };

    let results: Vec<(String, Result<Option<Value>>)> = stream::iter(targets)
        .map(|(label, path)| async move {
            let res = t3.list_project_threads_raw(&path).await;
            (label, res)
        })
        .buffer_unordered(LIST_CONCURRENCY)
        .collect()
        .await;

    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut rows: Vec<(String, Value)> = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    let mut listed_ok = 0usize;

    for (label, res) in results {
        match res {
            Ok(Some(val)) => {
                listed_ok += 1;
                for item in val.as_array().into_iter().flatten() {
                    let Some(id) = item["threadId"].as_str() else {
                        continue;
                    };
                    if !seen.insert(id.to_owned()) {
                        continue;
                    }
                    let status = item["status"].as_str().unwrap_or("idle");
                    let busy = t3::is_busy_status(status);
                    if (status_filter == "running" && !busy) || (status_filter == "idle" && busy) {
                        continue;
                    }
                    let updated = item["updatedAt"]
                        .as_str()
                        .or_else(|| item["createdAt"].as_str())
                        .unwrap_or("")
                        .to_owned();
                    let title = compact_one_line(item["title"].as_str().unwrap_or(""), 80);
                    let mut entry = json!({
                        "thread_id": id,
                        "title": title,
                        "status": status,
                        "project": label,
                        "updated_at": updated,
                    });
                    if item["settled"].as_bool() == Some(true) {
                        entry["settled"] = json!(true);
                    }
                    if let Some(open_prs) = item["openPullRequestCount"].as_u64()
                        && open_prs > 0
                    {
                        entry["open_prs"] = json!(open_prs);
                    }
                    if let Some(parent) = item["parentThreadId"].as_str().filter(|s| !s.is_empty())
                    {
                        entry["parent_thread_id"] = json!(parent);
                    }
                    rows.push((updated, entry));
                }
            }
            Ok(None) => {
                listed_ok += 1;
            }
            Err(err) => errors.push(format!("{label}: {err:#}")),
        }
    }

    if listed_ok == 0 && !errors.is_empty() {
        bail!("{}", errors.join("; "));
    }

    rows.sort_by(|a, b| b.0.cmp(&a.0));
    let total = rows.len();
    let threads: Vec<Value> = rows.into_iter().take(limit).map(|(_, v)| v).collect();
    let mut out = json!({
        "threads": threads,
        "total": total,
    });
    if !errors.is_empty() {
        out["unread_projects"] = json!(errors.len());
    }
    Ok(out)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ThreadReadParams {
    thread_id: String,
    messages: Option<u32>,
    max_chars: Option<u32>,
}

async fn handle_thread_read(t3: &T3Cli, params: Value) -> Result<Value> {
    let p: ThreadReadParams = serde_json::from_value(params)?;
    let thread_id = validate_cli_token("thread_id", &p.thread_id)?;
    let msg_count = p.messages.unwrap_or(6).clamp(1, 30) as usize;
    let max_chars = p.max_chars.unwrap_or(6_000).clamp(500, 20_000) as usize;

    let Some(raw) = t3.show_thread_raw(thread_id, msg_count).await? else {
        bail!("thread {thread_id} not found");
    };

    let thread = &raw["thread"];
    let status = thread["status"].as_str().unwrap_or("unknown");
    let busy = t3::is_busy_status(status) || !thread["activeRunId"].is_null();
    let title = compact_one_line(thread["title"].as_str().unwrap_or(""), 100);

    let cleaned: Vec<(String, String)> = raw["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| {
            let role = m["role"].as_str()?.to_owned();
            let text = t3::clean_t3_message_text(m["text"].as_str()?);
            (!text.is_empty()).then_some((role, text))
        })
        .collect();

    // Keep a contiguous suffix of newest messages within `max_chars` so the latest reply
    // is never dropped and no middle message is silently skipped.
    let total_cleaned = cleaned.len();
    let mut kept_rev: Vec<Value> = Vec::new();
    let mut remaining = max_chars;
    for (idx, (role, text)) in cleaned.into_iter().rev().enumerate() {
        let len = text.chars().count();
        if len <= remaining {
            remaining = remaining.saturating_sub(len);
            kept_rev.push(json!({ "role": role, "text": text }));
        } else if idx == 0 {
            let cut = truncate_chars(&text, remaining.max(64));
            kept_rev.push(json!({ "role": role, "text": cut, "truncated": true }));
            break;
        } else if remaining >= 200 {
            let cut = truncate_chars(&text, remaining);
            kept_rev.push(json!({ "role": role, "text": cut, "truncated": true }));
            break;
        } else {
            break;
        }
    }
    let omitted = total_cleaned.saturating_sub(kept_rev.len());
    kept_rev.reverse();

    let mut out = json!({
        "thread_id": thread["threadId"].as_str().unwrap_or(thread_id),
        "title": title,
        "status": status,
        "busy": busy,
        "messages": kept_rev,
    });
    if thread["archived"].as_bool() == Some(true) {
        out["archived"] = json!(true);
    }
    if thread["settled"].as_bool() == Some(true) {
        out["settled"] = json!(true);
    }
    if let Some(runs) = thread["runCount"].as_u64() {
        out["run_count"] = json!(runs);
    }
    if let Some(wt) = thread["worktreePath"].as_str().filter(|s| !s.is_empty()) {
        out["worktree"] = json!(wt);
    }
    if let Some(br) = thread["branch"].as_str().filter(|s| !s.is_empty()) {
        out["branch"] = json!(br);
    }
    if let Some(updated) = thread["updatedAt"].as_str() {
        out["updated_at"] = json!(updated);
    }
    if omitted > 0 {
        out["omitted_messages"] = json!(omitted);
    }
    Ok(out)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ThreadLaunchParams {
    project: String,
    prompt: String,
    title: Option<String>,
    worktree_base: Option<String>,
    branch: Option<String>,
    #[serde(default)]
    local: bool,
    wait_secs: Option<u64>,
}

async fn handle_thread_launch(t3: &T3Cli, config: &Config, params: Value) -> Result<Value> {
    let p: ThreadLaunchParams = serde_json::from_value(params)?;
    let prompt = p.prompt.trim();
    if prompt.is_empty() {
        bail!("prompt must not be empty");
    }
    if prompt.chars().count() > MAX_PROMPT_CHARS {
        bail!("prompt exceeds {MAX_PROMPT_CHARS} characters");
    }
    if p.local && (p.worktree_base.is_some() || p.branch.is_some()) {
        bail!("local=true cannot be combined with worktree_base or branch");
    }
    if p.branch.is_some() && p.worktree_base.is_none() {
        bail!("worktree_base is required when branch is set");
    }
    let worktree_base = p
        .worktree_base
        .as_deref()
        .map(|v| validate_cli_token("worktree_base", v))
        .transpose()?;
    let branch = p
        .branch
        .as_deref()
        .map(|v| validate_cli_token("branch", v))
        .transpose()?;
    let title = p
        .title
        .as_deref()
        .map(|t| compact_one_line(t, 80))
        .filter(|t| !t.is_empty());
    if title.as_deref().is_some_and(|t| t.starts_with('-')) {
        bail!("invalid title: must not start with '-'");
    }

    let (label, path) = resolve_project(config, &p.project)?;
    let thread_id = t3
        .start_thread_with_options(
            &path,
            title.as_deref(),
            prompt,
            worktree_base,
            branch,
            p.local,
            config.thread_model.as_ref(),
        )
        .await?;

    let wait_secs = p.wait_secs.unwrap_or(0).min(MAX_WAIT_SECS);
    let mut out = json!({
        "thread_id": thread_id,
        "project": label,
        "status": "starting",
        "busy": true,
    });
    if wait_secs > 0
        && let Some(snap) = wait_for_reply(t3, &thread_id, 0, 0, None, wait_secs).await
    {
        out["status"] = json!(snap.status);
        out["busy"] = json!(snap.busy);
        if let Some(reply) = snap.reply {
            out["reply"] = json!(reply);
        }
    }
    Ok(out)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ThreadSendParams {
    thread_id: String,
    message: String,
    wait_secs: Option<u64>,
}

async fn handle_thread_send(t3: &T3Cli, params: Value) -> Result<Value> {
    let p: ThreadSendParams = serde_json::from_value(params)?;
    let thread_id = validate_cli_token("thread_id", &p.thread_id)?;
    let message = p.message.trim();
    if message.is_empty() {
        bail!("message must not be empty");
    }
    if message.chars().count() > MAX_PROMPT_CHARS {
        bail!("message exceeds {MAX_PROMPT_CHARS} characters");
    }

    let wait_secs = p.wait_secs.unwrap_or(0).min(MAX_WAIT_SECS);
    let (before_runs, before_asst_count, before_asst_run) = if wait_secs > 0 {
        let Some(before) = t3.show_thread_raw(thread_id, 8).await? else {
            bail!("thread {thread_id} not found");
        };
        let runs = before["thread"]["runCount"].as_u64().unwrap_or(0);
        let (asst_count, latest_asst) = assistant_message_state(&before);
        let asst_run = latest_asst.and_then(|(run_id, _)| run_id);
        (runs, asst_count, asst_run)
    } else {
        (0, 0, None)
    };

    if let Err(err) = t3.send(thread_id, message).await {
        let msg = format!("{err:#}");
        if t3::is_unknown_thread(&msg, thread_id) {
            bail!("thread {thread_id} not found");
        }
        return Err(err);
    }

    let mut out = json!({
        "thread_id": thread_id,
        "sent": true,
        "status": "queued",
        "busy": true,
    });
    if wait_secs > 0
        && let Some(snap) = wait_for_reply(
            t3,
            thread_id,
            before_runs,
            before_asst_count,
            before_asst_run.as_deref(),
            wait_secs,
        )
        .await
    {
        out["status"] = json!(snap.status);
        out["busy"] = json!(snap.busy);
        if let Some(reply) = snap.reply {
            out["reply"] = json!(reply);
        }
    }
    Ok(out)
}

struct WaitSnapshot {
    status: String,
    busy: bool,
    reply: Option<String>,
}

async fn wait_for_reply(
    t3: &T3Cli,
    thread_id: &str,
    before_runs: u64,
    before_asst_count: usize,
    before_asst_run: Option<&str>,
    wait_secs: u64,
) -> Option<WaitSnapshot> {
    let deadline = Instant::now() + Duration::from_secs(wait_secs);
    let mut last: Option<WaitSnapshot> = None;
    loop {
        if let Ok(Some(raw)) = t3.show_thread_raw(thread_id, 8).await {
            let thread = &raw["thread"];
            let status = thread["status"].as_str().unwrap_or("unknown").to_owned();
            let busy = t3::is_busy_status(&status) || !thread["activeRunId"].is_null();
            let runs = thread["runCount"].as_u64().unwrap_or(0);
            let (asst_count, latest_asst) = assistant_message_state(&raw);
            // Only count an assistant message as a new reply when its runId changed from the
            // prior assistant message's runId, or when a new assistant message appeared where
            // none existed before. Never mistake a bumped `runCount` with the previous turn's
            // assistant message for a new reply.
            let has_new_reply = latest_asst.as_ref().is_some_and(|(run_id, _)| {
                match (before_asst_run, run_id.as_deref()) {
                    (Some(prev), Some(curr)) => curr != prev,
                    (None, _) => before_asst_count == 0 || asst_count > before_asst_count,
                    (Some(_), None) => asst_count > before_asst_count,
                }
            });
            let reply = if has_new_reply {
                latest_asst.map(|(_, text)| truncate_chars(&text, MAX_INLINE_REPLY_CHARS))
            } else {
                None
            };
            let done = !busy && (has_new_reply || runs > before_runs);
            last = Some(WaitSnapshot {
                status,
                busy,
                reply,
            });
            if done {
                return last;
            }
        }
        let now = Instant::now();
        if now >= deadline {
            return last;
        }
        tokio::time::sleep(Duration::from_secs(1).min(deadline - now)).await;
    }
}

fn assistant_message_state(raw: &Value) -> (usize, Option<(Option<String>, String)>) {
    let Some(msgs) = raw["messages"].as_array() else {
        return (0, None);
    };
    let mut count = 0usize;
    let mut latest = None;
    for m in msgs {
        if m["role"].as_str() == Some("assistant")
            && let Some(raw_text) = m["text"].as_str()
        {
            let text = t3::clean_t3_message_text(raw_text);
            if !text.is_empty() {
                count += 1;
                latest = Some((m["runId"].as_str().map(str::to_owned), text));
            }
        }
    }
    (count, latest)
}

fn compact_one_line(raw: &str, max_chars: usize) -> String {
    let collapsed = raw
        .split(|c: char| c.is_whitespace() || c.is_control())
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    truncate_chars(&collapsed, max_chars)
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_owned();
    }
    let keep = max_chars.saturating_sub(1);
    let mut out: String = text.chars().take(keep).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn fake_t3(dir: &Path, script: &str) -> T3Cli {
        let bin = dir.join("t3");
        crate::test_script::write_script(&bin, script);
        T3Cli::locate(Some(&bin))
    }

    #[test]
    fn projects_list_deduplicates_and_resolves_short_names() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("agent-board");
        std::fs::create_dir_all(&repo).unwrap();
        let mut config = Config::default();
        config
            .projects
            .insert("github.com/syz51/agent-board".into(), repo.clone());
        config
            .board_projects
            .insert("board-feedback".parse().unwrap(), repo.clone());

        let listed = handle_projects_list(&config).unwrap();
        assert_eq!(listed["projects"].as_array().unwrap().len(), 1);
        assert_eq!(listed["projects"][0]["name"], "agent-board");

        let (label, resolved) = resolve_project(&config, "agent-board").unwrap();
        assert_eq!(label, "agent-board");
        assert_eq!(resolved, repo);

        let (board_label, board_resolved) = resolve_project(&config, "board-feedback").unwrap();
        assert_eq!(board_label, "board-feedback");
        assert_eq!(board_resolved, repo);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn thread_read_unwraps_t3_prompt_and_keeps_contiguous_newest_suffix() {
        let dir = tempfile::tempdir().unwrap();
        let wrapped = "<t3_code_instructions>\nHUGE SYSTEM PROMPT\n</t3_code_instructions>\n\n<user_request>\nCheck disk space\n</user_request>\n<runtime_info>info</runtime_info>";
        // M0 is tiny (2 chars), M1 is 300 chars, M2 is 400 chars: with max_chars=500,
        // M2 fits (remaining=100 < 200), so M1 AND M0 must both be omitted (no middle gap!).
        let payload = json!({
            "thread": {
                "threadId": "thr-1",
                "title": "Disk Check",
                "status": "idle",
                "activeRunId": null,
                "runCount": 2,
                "archived": false,
                "settled": false,
                "updatedAt": "2026-11-25T10:00:00Z"
            },
            "messages": [
                { "role": "user", "runId": "r0", "text": "ok" },
                { "role": "user", "runId": "r1", "text": "m".repeat(300) },
                { "role": "user", "runId": "r2", "text": wrapped },
                { "role": "assistant", "runId": "r2", "text": "a".repeat(400) }
            ]
        });
        let json_path = dir.path().join("show.json");
        std::fs::write(&json_path, payload.to_string()).unwrap();
        let script = format!("#!/bin/sh\ncat '{}'\n", json_path.display());
        let t3 = fake_t3(dir.path(), &script);
        let call = DeviceRpcCall {
            id: "rpc-1".into(),
            method: "t3.thread.read".into(),
            params: json!({ "thread_id": "thr-1", "messages": 6, "max_chars": 500 }),
            timeout_secs: 10,
        };
        let reply = handle_call(&t3, &Config::default(), &call).await;
        assert!(reply.ok, "{reply:?}");
        let res = reply.result.unwrap();
        let msgs = res["messages"].as_array().unwrap();
        // Contiguous newest suffix: unwrapped user message + 400-char assistant reply fit (416 <= 500);
        // remaining is 84 (< 200), so BOTH the 300-char middle message AND the 2-char "ok" are omitted!
        assert_eq!(msgs.len(), 2, "{res}");
        assert_eq!(msgs[0]["text"], "Check disk space");
        assert_eq!(msgs[1]["text"].as_str().unwrap().len(), 400);
        assert_eq!(res["omitted_messages"], 2);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rpc_rejects_leading_hyphen_flag_injection_before_spawning_t3() {
        let dir = tempfile::tempdir().unwrap();
        let t3 = fake_t3(dir.path(), "#!/bin/sh\necho SHOULD_NOT_RUN >&2; exit 0\n");

        for (method, params) in [
            ("t3.thread.read", json!({ "thread_id": "--help" })),
            (
                "t3.thread.send",
                json!({ "thread_id": "--help", "message": "hi" }),
            ),
            (
                "t3.thread.launch",
                json!({ "project": "--base-dir", "prompt": "hi" }),
            ),
            (
                "t3.thread.launch",
                json!({ "project": "p", "prompt": "hi", "worktree_base": "--local" }),
            ),
            (
                "t3.thread.launch",
                json!({ "project": "p", "prompt": "hi", "worktree_base": "main", "branch": "--json" }),
            ),
            (
                "t3.thread.launch",
                json!({ "project": "p", "prompt": "hi", "title": "--output-schema" }),
            ),
        ] {
            let call = DeviceRpcCall {
                id: "rpc-flag".into(),
                method: method.into(),
                params,
                timeout_secs: 5,
            };
            let reply = handle_call(&t3, &Config::default(), &call).await;
            assert!(
                !reply.ok,
                "{method} should reject flag injection: {reply:?}"
            );
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn thread_send_wait_does_not_return_previous_turns_assistant_reply() {
        let dir = tempfile::tempdir().unwrap();
        // Simulates r2 starting (runCount increments from 1 -> 2) and finishing without a new
        // assistant message while r1's old assistant message is still in `messages`.
        let count_file = dir.path().join("calls");
        let script = format!(
            r#"#!/bin/sh
cmd="$1 $2"
if [ "$cmd" = "thread send" ]; then
  cat > /dev/null
  exit 0
fi
n=0
if [ -f '{cf}' ]; then n=$(cat '{cf}'); fi
n=$((n + 1))
echo "$n" > '{cf}'
if [ "$n" -eq 1 ]; then
  printf '%s\n' '{{"thread":{{"threadId":"thr-1","status":"idle","activeRunId":null,"runCount":1}},"messages":[{{"role":"assistant","runId":"r1","text":"OLD REPLY FROM TURN 1"}}]}}'
else
  printf '%s\n' '{{"thread":{{"threadId":"thr-1","status":"failed","activeRunId":null,"runCount":2}},"messages":[{{"role":"assistant","runId":"r1","text":"OLD REPLY FROM TURN 1"}}]}}'
fi
"#,
            cf = count_file.display()
        );
        let t3 = fake_t3(dir.path(), &script);
        let call = DeviceRpcCall {
            id: "rpc-send".into(),
            method: "t3.thread.send".into(),
            params: json!({
                "thread_id": "thr-1",
                "message": "Second turn",
                "wait_secs": 2
            }),
            timeout_secs: 5,
        };
        let reply = handle_call(&t3, &Config::default(), &call).await;
        assert!(reply.ok, "{reply:?}");
        let res = reply.result.unwrap();
        assert_eq!(res["status"], "failed");
        assert!(
            res.get("reply").is_none(),
            "must not return stale r1 reply for r2: {res}"
        );
    }
}
