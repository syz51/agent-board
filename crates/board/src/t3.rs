//! The T3 Code commands the device daemon runs, behind a trait so tests use a fake.
//!
//! Commands (checked against `t3` v0.1.8 help):
//! - `t3 thread start --json --title <title> [--worktree <base> --branch <branch>]
//!   <project path> -` with the message on stdin; prints the new thread. With `--worktree`,
//!   T3 creates the branch from the base and checks it out in a new worktree after it
//!   created the thread, so a branch that already exists fails the thread's first run, not
//!   the command.
//! - `t3 thread send <thread> -` with the message on stdin; a busy thread gets it as its
//!   next turn.
//! - `t3 thread show --json --messages 1 <thread>`: the thread's state, with its own worktree
//!   (`worktreePath`, null for a thread in its project folder); with `--messages 0`
//!   the hooks read a thread's title, link and, for a delegated thread, its parent
//!   (`parentThreadId`).
//! - `t3 thread list --json --project <project path>`: the project's active (unarchived)
//!   threads.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use board_proto::{
    AGENT_TITLE_CHARS_MAX as TITLE_CHARS_MAX, NATIVE_ID_CHARS_MAX, Runtime, T3_LINK_PREFIX,
    Timestamp,
};
use serde_json::Value;
use tokio::io::AsyncWriteExt as _;

/// What the daemon needs to know about a thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadInfo {
    pub archived: bool,
    pub runtime: Runtime,
    /// The thread's own git worktree, for one that runs in a worktree, not its project folder.
    pub worktree: Option<PathBuf>,
}

impl ThreadInfo {
    /// The folder T3 runs the thread in: its worktree, else `project`, its project folder
    /// when the caller knows it.
    pub fn folder<'a>(&'a self, project: Option<&'a Path>) -> Option<&'a Path> {
        self.worktree.as_deref().or(project)
    }

    /// Whether T3 can run the thread: it is not archived and its [`Self::folder`] is not
    /// gone. T3 fails every run of a thread whose folder is gone, before the agent starts.
    pub fn runnable(&self, project: Option<&Path>) -> bool {
        !self.archived && !self.folder(project).is_some_and(folder_gone)
    }
}

/// Whether `folder` no longer exists. One that cannot be read for another reason, such as a
/// permission, may still be there for T3, so it does not count as gone.
pub fn folder_gone(folder: &Path) -> bool {
    std::fs::metadata(folder).is_err_and(|err| err.kind() == std::io::ErrorKind::NotFound)
}

/// What the hooks read about their own thread from `t3 thread show`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ThreadFacts {
    /// The thread that delegated it, for a thread with a relationship to a parent.
    pub parent: Option<String>,
    /// Its title, as one line of at most [`TITLE_CHARS_MAX`] characters.
    pub title: Option<String>,
    /// Its `https://` link in the T3 app, exactly as T3 printed it.
    pub link: Option<String>,
}

/// A thread from a project's listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListedThread {
    pub id: String,
    pub created_at: Timestamp,
    /// Whether it has a run or any item, so its first message reached it.
    pub ran: bool,
}

/// A new git worktree for a thread: the `branch` T3 creates from `base` and checks out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewWorktree {
    pub base: String,
    pub branch: String,
}

pub trait T3: Send + Sync {
    /// Starts a thread in the project containing `project`, in a new `worktree` when one is
    /// given, else in the project folder, and returns its id.
    fn start_thread(
        &self,
        project: &Path,
        title: &str,
        message: &str,
        worktree: Option<&NewWorktree>,
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

/// A `t3` command stopped after its time ran out. It may have done its work first, so a
/// caller does not repeat what must happen at most once.
#[derive(Debug)]
pub struct TimedOut(pub String);

impl std::fmt::Display for TimedOut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} timed out", self.0)
    }
}

impl std::error::Error for TimedOut {}

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

    /// What T3 says of `thread`: its parent, title and link; `None` when T3 does not know
    /// the thread.
    pub async fn thread_facts(&self, thread: &str) -> Result<Option<ThreadFacts>> {
        Ok(self
            .thread_show(thread, "0")
            .await?
            .map(|value| parse_thread_facts(&value)))
    }

    /// `t3 thread show --json --messages <messages> <thread>`, parsed; `None` when T3 does
    /// not know the thread.
    async fn thread_show(&self, thread: &str, messages: &str) -> Result<Option<Value>> {
        let (ok, stdout, stderr) = self
            .run(
                &["thread", "show", "--json", "--messages", messages, thread],
                None,
            )
            .await?;
        if !ok {
            if is_unknown_thread(&stderr, thread) {
                return Ok(None);
            }
            bail!("t3 thread show failed: {}", stderr.trim());
        }
        Ok(Some(
            serde_json::from_str(&stdout).context("t3 thread show --json")?,
        ))
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
            .map_err(|_| TimedOut(format!("t3 {}", args.join(" "))))??;
        Ok((
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        ))
    }
}

impl T3 for T3Cli {
    async fn start_thread(
        &self,
        project: &Path,
        title: &str,
        message: &str,
        worktree: Option<&NewWorktree>,
    ) -> Result<String> {
        let project = project.to_string_lossy();
        let mut args = vec!["thread", "start", "--json", "--title", title];
        if let Some(worktree) = worktree {
            args.extend(["--worktree", &worktree.base, "--branch", &worktree.branch]);
        }
        args.extend([&*project, "-"]);
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
        Ok(self
            .thread_show(thread, "1")
            .await?
            .map(|value| parse_thread_info(&value)))
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

/// Reads `{"thread": {"archived": …, "providerInstanceId": …, "worktreePath": …}}` from
/// `t3 thread show --json`.
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
        worktree: thread["worktreePath"]
            .as_str()
            .filter(|p| !p.is_empty())
            .map(PathBuf::from),
    }
}

/// Reads `{"thread": {"parentThreadId": …, "relationshipToParent": …}}`: the parent of a
/// thread that has a relationship to one.
pub fn parse_thread_parent(value: &Value) -> Option<String> {
    let thread = &value["thread"];
    thread["relationshipToParent"].as_str()?;
    thread["parentThreadId"]
        .as_str()
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(str::to_owned)
}

/// Reads the parent, title and link from `t3 thread show --json`
/// (`{"thread": {"parentThreadId", "relationshipToParent", "title", "link", …}}`). A title
/// becomes one line: control characters and runs of white space turn into one space, and
/// one longer than [`TITLE_CHARS_MAX`] is cut, ending with `…`; an empty one is left out. A
/// link is kept only when it is one `https://` URL the server takes as a native id.
pub fn parse_thread_facts(value: &Value) -> ThreadFacts {
    let thread = &value["thread"];
    let title = thread["title"]
        .as_str()
        .map(|t| {
            t.split(|c: char| c.is_whitespace() || c.is_control())
                .filter(|w| !w.is_empty())
                .collect::<Vec<_>>()
                .join(" ")
        })
        .filter(|t| !t.is_empty())
        .map(|t| {
            if t.chars().count() <= TITLE_CHARS_MAX {
                t
            } else {
                let mut cut: String = t.chars().take(TITLE_CHARS_MAX - 1).collect();
                cut.push('…');
                cut
            }
        });
    let link = thread["link"]
        .as_str()
        .filter(|l| {
            l.starts_with(T3_LINK_PREFIX)
                && l.len() > T3_LINK_PREFIX.len()
                && l.chars().count() <= NATIVE_ID_CHARS_MAX
                && !l.chars().any(|c| c.is_whitespace() || c.is_control())
        })
        .map(str::to_owned);
    ThreadFacts {
        parent: parse_thread_parent(value),
        title,
        link,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// A `t3` that records its arguments and prints a started thread.
    #[cfg(unix)]
    fn recording_t3(dir: &Path) -> (T3Cli, PathBuf) {
        use std::os::unix::fs::PermissionsExt as _;
        let log = dir.join("args");
        let bin = dir.join("t3");
        std::fs::write(
            &bin,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\ncat > /dev/null\necho '{{\"threadId\":\"t-1\"}}'\n",
                log.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        (T3Cli { bin }, log)
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn threads_start_in_a_new_worktree_only_when_given_one() {
        let dir = tempfile::tempdir().unwrap();
        let (t3, log) = recording_t3(dir.path());
        let args = || std::fs::read_to_string(&log).unwrap();

        let worktree = NewWorktree {
            base: "origin/main".into(),
            branch: "coordinator/factory".into(),
        };
        let id = t3
            .start_thread(
                Path::new("/src/p"),
                "factory coordinator",
                "hi",
                Some(&worktree),
            )
            .await
            .unwrap();
        assert_eq!(id, "t-1");
        assert_eq!(
            args(),
            "thread\nstart\n--json\n--title\nfactory coordinator\n--worktree\norigin/main\n\
             --branch\ncoordinator/factory\n/src/p\n-\n"
        );

        t3.start_thread(Path::new("/src/p"), "factory coordinator", "hi", None)
            .await
            .unwrap();
        assert_eq!(
            args(),
            "thread\nstart\n--json\n--title\nfactory coordinator\n/src/p\n-\n"
        );
    }

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
    fn thread_parents_come_from_the_relationship() {
        let child = json!({"thread": {
            "threadId": "thread:delegated-task:command%3Amcp%3Ac1%3Adelegate-task%3At1",
            "parentThreadId": "b8745db9-8b9a-4047-8e1d-15b7ac1b2f40",
            "relationshipToParent": "subagent",
        }});
        assert_eq!(
            parse_thread_parent(&child).as_deref(),
            Some("b8745db9-8b9a-4047-8e1d-15b7ac1b2f40")
        );
        let top = json!({"thread": {"threadId": "t-1", "parentThreadId": null}});
        assert_eq!(parse_thread_parent(&top), None);
        let unrelated = json!({"thread": {"parentThreadId": "p", "relationshipToParent": null}});
        assert_eq!(parse_thread_parent(&unrelated), None);
    }

    #[test]
    fn thread_facts_read_the_title_and_https_link() {
        let link = "https://app.example.test/threads/env-1/thread-1";
        let facts = parse_thread_facts(&json!({"thread": {
            "threadId": "thread-1", "title": "  Fix the\tparser\n ", "link": link,
            "parentThreadId": null,
        }}));
        assert_eq!(
            facts,
            ThreadFacts {
                parent: None,
                title: Some("Fix the parser".into()),
                link: Some(link.into()),
            }
        );
        // Missing fields and a missing thread read as nothing.
        assert_eq!(
            parse_thread_facts(&json!({"thread": {"threadId": "t"}})),
            ThreadFacts::default()
        );
        assert_eq!(parse_thread_facts(&json!({})), ThreadFacts::default());
        // Empty titles and links that are not one https URL are left out.
        for link in [
            json!("http://app.example.test/threads/e/t"),
            json!("javascript:alert(1)"),
            json!("https://"),
            json!("https://app.example.test/a b"),
            json!(format!("https://app.example.test/{}", "x".repeat(300))),
            json!(7),
        ] {
            let facts = parse_thread_facts(&json!({"thread": {"title": " \n ", "link": link}}));
            assert_eq!(facts, ThreadFacts::default(), "{link}");
        }
        // A title the server would refuse as too long is cut.
        let long = parse_thread_facts(&json!({"thread": {"title": "y".repeat(500)}}));
        let title = long.title.unwrap();
        assert_eq!(title.chars().count(), TITLE_CHARS_MAX);
        assert!(title.ends_with('…'));
        // A delegated thread's parent comes along.
        let child = parse_thread_facts(&json!({"thread": {
            "title": "Child", "parentThreadId": "parent-1", "relationshipToParent": "subagent",
        }}));
        assert_eq!(child.parent.as_deref(), Some("parent-1"));
    }

    #[test]
    fn thread_info_reads_archived_provider_and_worktree() {
        let info = parse_thread_info(&json!({"thread": {
            "archived": true, "providerInstanceId": "codex", "worktreePath": "/w/factory",
        }}));
        assert_eq!(
            info,
            ThreadInfo {
                archived: true,
                runtime: Runtime::Codex,
                worktree: Some(PathBuf::from("/w/factory")),
            }
        );
        let info = parse_thread_info(&json!({"thread": {
            "providerInstanceId": "claudeAgent", "worktreePath": null,
        }}));
        assert_eq!(
            info,
            ThreadInfo {
                archived: false,
                runtime: Runtime::Claude,
                worktree: None,
            }
        );
    }

    #[test]
    fn a_thread_is_runnable_only_while_its_folder_exists() {
        let dir = tempfile::tempdir().unwrap();
        let gone = dir.path().join("gone");
        let info = |archived, worktree: Option<&Path>| ThreadInfo {
            archived,
            runtime: Runtime::Claude,
            worktree: worktree.map(Path::to_path_buf),
        };
        // In its project folder: that folder decides, when known.
        assert!(info(false, None).runnable(None));
        assert!(info(false, None).runnable(Some(dir.path())));
        assert!(!info(false, None).runnable(Some(&gone)));
        // In its own worktree: the worktree decides, whatever the project folder.
        assert!(info(false, Some(dir.path())).runnable(Some(&gone)));
        assert!(!info(false, Some(&gone)).runnable(Some(dir.path())));
        assert!(!info(false, Some(&gone)).runnable(None));
        // Archived: never.
        assert!(!info(true, None).runnable(Some(dir.path())));
    }

    #[cfg(unix)]
    #[test]
    fn a_folder_that_cannot_be_read_is_not_gone() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let locked = dir.path().join("locked");
        let inside = locked.join("project");
        std::fs::create_dir_all(&inside).unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        // stat fails with a permission error, not NotFound.
        let gone = folder_gone(&inside);
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(!gone);
        assert!(folder_gone(&dir.path().join("gone")));
    }
}
