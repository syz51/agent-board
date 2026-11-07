//! The T3 Code commands the device daemon runs, behind a trait so tests use a fake.
//!
//! Commands (checked against `t3` v0.1.8 help):
//! - `t3 thread start --json --title <title> <project path> -` with the message on stdin;
//!   prints the new thread.
//! - `t3 thread send <thread> -` with the message on stdin; a busy thread gets it as its
//!   next turn.
//! - `t3 thread show --json --messages 1 <thread>`: the thread's state.
//! - `t3 thread list --json --project <project path>`: the project's active (unarchived)
//!   threads.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use board_proto::{Runtime, Timestamp};
use serde_json::Value;
use tokio::io::AsyncWriteExt as _;

/// What the daemon needs to know about a thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadInfo {
    pub archived: bool,
    pub runtime: Runtime,
}

/// A thread from a project's listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListedThread {
    pub id: String,
    pub created_at: Timestamp,
    /// Whether it has a run or any item, so its first message reached it.
    pub ran: bool,
}

pub trait T3: Send + Sync {
    /// Starts a thread in the project containing `project` and returns its id.
    fn start_thread(
        &self,
        project: &Path,
        title: &str,
        message: &str,
    ) -> impl Future<Output = Result<String>> + Send;

    /// Sends a message to a thread.
    fn send(&self, thread: &str, message: &str) -> impl Future<Output = Result<()>> + Send;

    /// The thread's state, or `None` when T3 does not know it.
    fn thread_info(&self, thread: &str) -> impl Future<Output = Result<Option<ThreadInfo>>> + Send;

    /// The active threads titled `title` in the project containing `project`.
    fn threads_titled(
        &self,
        project: &Path,
        title: &str,
    ) -> impl Future<Output = Result<Vec<ListedThread>>> + Send;
}

/// The real `t3` binary.
#[derive(Debug, Clone)]
pub struct T3Cli {
    pub bin: PathBuf,
}

const COMMAND_TIMEOUT: Duration = Duration::from_secs(60);

impl T3Cli {
    /// `t3` from the configuration, else `~/.t3/bin/t3` when it exists, else from `PATH`.
    pub fn locate(configured: Option<&Path>) -> Self {
        let bin = configured.map(Path::to_path_buf).unwrap_or_else(|| {
            dirs::home_dir()
                .map(|h| h.join(".t3/bin/t3"))
                .filter(|p| p.exists())
                .unwrap_or_else(|| PathBuf::from("t3"))
        });
        Self { bin }
    }

    /// Runs `t3 <args>`, feeding `stdin`, and returns (exit success, stdout, stderr).
    async fn run(&self, args: &[&str], stdin: Option<&str>) -> Result<(bool, String, String)> {
        let mut command = tokio::process::Command::new(&self.bin);
        command
            .args(args)
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command
            .spawn()
            .with_context(|| format!("running {}", self.bin.display()))?;
        if let Some(text) = stdin {
            let mut pipe = child.stdin.take().expect("stdin is piped");
            pipe.write_all(text.as_bytes()).await?;
            drop(pipe);
        }
        let out = tokio::time::timeout(COMMAND_TIMEOUT, child.wait_with_output())
            .await
            .with_context(|| format!("t3 {} timed out", args.join(" ")))??;
        Ok((
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        ))
    }
}

impl T3 for T3Cli {
    async fn start_thread(&self, project: &Path, title: &str, message: &str) -> Result<String> {
        let project = project.to_string_lossy();
        let args = ["thread", "start", "--json", "--title", title, &project, "-"];
        let (ok, stdout, stderr) = self.run(&args, Some(message)).await?;
        if !ok {
            bail!("t3 thread start failed: {}", stderr.trim());
        }
        parse_thread_id(&stdout).context("t3 thread start printed no thread id")
    }

    async fn send(&self, thread: &str, message: &str) -> Result<()> {
        let (ok, _, stderr) = self
            .run(&["thread", "send", thread, "-"], Some(message))
            .await?;
        if !ok {
            bail!("t3 thread send failed: {}", stderr.trim());
        }
        Ok(())
    }

    async fn thread_info(&self, thread: &str) -> Result<Option<ThreadInfo>> {
        let (ok, stdout, stderr) = self
            .run(
                &["thread", "show", "--json", "--messages", "1", thread],
                None,
            )
            .await?;
        if !ok {
            if is_unknown_thread(&stderr, thread) {
                return Ok(None);
            }
            bail!("t3 thread show failed: {}", stderr.trim());
        }
        let value: Value = serde_json::from_str(&stdout).context("t3 thread show --json")?;
        Ok(Some(parse_thread_info(&value)))
    }

    async fn threads_titled(&self, project: &Path, title: &str) -> Result<Vec<ListedThread>> {
        let project = project.to_string_lossy();
        let args = ["thread", "list", "--json", "--project", &project];
        let (ok, stdout, stderr) = self.run(&args, None).await?;
        if !ok {
            bail!("t3 thread list failed: {}", stderr.trim());
        }
        let value: Value = serde_json::from_str(&stdout).context("t3 thread list --json")?;
        Ok(parse_threads_titled(&value, title))
    }
}

/// The thread id from `t3 thread start --json` (`{"threadId": …}` or
/// `{"thread": {"threadId": …}}`), or from its first line of plain text.
pub fn parse_thread_id(stdout: &str) -> Option<String> {
    let id = match serde_json::from_str::<Value>(stdout.trim()) {
        Ok(value) => value["threadId"]
            .as_str()
            .or_else(|| value["thread"]["threadId"].as_str())
            .or_else(|| value["id"].as_str())
            .map(str::to_owned),
        Err(_) => stdout.lines().next().map(|l| l.trim().to_owned()),
    }?;
    (!id.is_empty() && !id.contains(char::is_whitespace)).then_some(id)
}

/// The threads titled `title` in `t3 thread list --json` output: an array of `{"threadId",
/// "title", "createdAt", "latestRunId", "itemCount", …}` (archived threads are not listed).
/// A thread missing any of these fields is left out.
pub fn parse_threads_titled(value: &Value, title: &str) -> Vec<ListedThread> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter(|t| t["title"].as_str() == Some(title))
        .filter_map(|t| {
            let id = t["threadId"].as_str()?.to_owned();
            let created_at = t["createdAt"].as_str()?.parse::<Timestamp>().ok()?;
            let run = t.get("latestRunId")?;
            let items = t["itemCount"].as_u64()?;
            Some(ListedThread {
                id,
                created_at,
                ran: !run.is_null() || items > 0,
            })
        })
        .collect()
}

/// Whether `t3 thread show` failed because T3 does not know the thread: older versions say
/// "not found" or "unknown thread", 0.1.8 says "Thread <id> is no longer available." T3
/// says "no longer available" of environments and connections too, so that wording counts
/// only with the thread's id.
pub fn is_unknown_thread(stderr: &str, thread: &str) -> bool {
    let lower = stderr.to_ascii_lowercase();
    lower.contains("not found")
        || lower.contains("unknown thread")
        || lower.contains(&format!(
            "thread {} is no longer available",
            thread.to_ascii_lowercase()
        ))
}

/// Reads `{"thread": {"archived": …, "providerInstanceId": …}}` from `t3 thread show --json`.
pub fn parse_thread_info(value: &Value) -> ThreadInfo {
    let thread = &value["thread"];
    let provider = thread["providerInstanceId"].as_str().unwrap_or_default();
    ThreadInfo {
        archived: thread["archived"].as_bool().unwrap_or(false),
        runtime: if provider.to_ascii_lowercase().contains("codex") {
            Runtime::Codex
        } else {
            Runtime::Claude
        },
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn thread_ids_parse_from_json_or_text() {
        assert_eq!(
            parse_thread_id(r#"{"threadId":"thread:p:1"}"#).as_deref(),
            Some("thread:p:1")
        );
        assert_eq!(
            parse_thread_id(r#"{"thread":{"threadId":"t-2","title":"x"}}"#).as_deref(),
            Some("t-2")
        );
        assert_eq!(parse_thread_id("t-3\n").as_deref(), Some("t-3"));
        assert_eq!(parse_thread_id("{}"), None);
        assert_eq!(parse_thread_id(""), None);
    }

    #[test]
    fn listed_threads_carry_their_creation_and_whether_they_ran() {
        let thread = |id: &str, title: &str, run: Value, items: u64| {
            json!({"threadId": id, "title": title, "createdAt": "2026-11-07T02:03:08.602Z",
                   "latestRunId": run, "itemCount": items, "settled": false})
        };
        let list = json!([
            thread("empty", "f coordinator", Value::Null, 0),
            thread("ran", "f coordinator", json!("run:1"), 3),
            thread("items", "f coordinator", Value::Null, 1),
            thread("other", "g coordinator", Value::Null, 0),
            {"threadId": "no-run-field", "title": "f coordinator",
             "createdAt": "2026-11-07T02:03:08.602Z", "itemCount": 0},
            {"threadId": "no-created", "title": "f coordinator", "latestRunId": null, "itemCount": 0},
        ]);
        let created_at = "2026-11-07T02:03:08.602Z".parse().unwrap();
        let listed = |id: &str, ran| ListedThread {
            id: id.into(),
            created_at,
            ran,
        };
        assert_eq!(
            parse_threads_titled(&list, "f coordinator"),
            [
                listed("empty", false),
                listed("ran", true),
                listed("items", true)
            ]
        );
        assert!(parse_threads_titled(&json!({}), "f coordinator").is_empty());
    }

    #[test]
    fn unknown_threads_are_recognised_in_every_wording() {
        assert!(is_unknown_thread(
            "Thread mcp:X is no longer available.\n",
            "mcp:X"
        ));
        assert!(is_unknown_thread("error: thread not found", "x"));
        assert!(is_unknown_thread("Unknown thread x", "x"));
        assert!(!is_unknown_thread("connection refused", "x"));
        // A transient failure elsewhere in T3 does not make the thread unknown.
        assert!(!is_unknown_thread(
            "This environment is no longer available.",
            "x"
        ));
        assert!(!is_unknown_thread("Thread y is no longer available.", "x"));
    }

    #[test]
    fn thread_info_reads_archived_and_provider() {
        let info = parse_thread_info(
            &json!({"thread": {"archived": true, "providerInstanceId": "codex"}}),
        );
        assert_eq!(
            info,
            ThreadInfo {
                archived: true,
                runtime: Runtime::Codex
            }
        );
        let info = parse_thread_info(&json!({"thread": {"providerInstanceId": "claudeAgent"}}));
        assert_eq!(
            info,
            ThreadInfo {
                archived: false,
                runtime: Runtime::Claude
            }
        );
    }
}
