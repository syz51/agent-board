//! Who is calling (agent id and runtime) and where (repo key, branch, cwd).

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use board_proto::{AgentId, Runtime, normalize_repo};

/// Environment variable T3 Code sets in its threads.
pub const T3_THREAD_ENV: &str = "T3CODE_THREAD_ID";
/// Environment variable Claude Code sets in its sessions.
pub const CLAUDE_SESSION_ENV: &str = "CLAUDE_CODE_SESSION_ID";

/// Resolves the calling agent's id.
///
/// An explicit id wins; then `T3CODE_THREAD_ID` (`t3:<id>`) unless `nested`,
/// `CLAUDE_CODE_SESSION_ID` (`claude:<id>`), and last the session id from a hook's input
/// (`codex:<id>` or `claude:<id>` by the hook's runtime). A `nested` session (see
/// [`is_nested`]) inherited the T3 thread id from the thread it runs inside, so it is
/// identified as it would be outside T3, and the hook's own session id comes before
/// `CLAUDE_CODE_SESSION_ID`, which a Codex session inherits from a Claude Code parent.
pub fn resolve_agent(
    explicit: Option<&str>,
    env: impl Fn(&str) -> Option<String>,
    hook_session: Option<(Runtime, &str)>,
    nested: bool,
) -> Result<Option<AgentId>> {
    if let Some(id) = explicit {
        return Ok(Some(id.parse().context("--agent")?));
    }
    let non_empty = |name: &str| env(name).filter(|v| !v.trim().is_empty());
    let claude = || non_empty(CLAUDE_SESSION_ENV).map(|s| format!("claude:{}", s.trim()));
    let hook = || {
        hook_session
            .filter(|(_, s)| !s.is_empty())
            .map(|(runtime, session)| {
                let source = match runtime {
                    Runtime::Claude => "claude",
                    Runtime::Codex => "codex",
                };
                format!("{source}:{session}")
            })
    };
    let raw = match non_empty(T3_THREAD_ENV).filter(|_| !nested) {
        Some(thread) => Some(format!("t3:{}", thread.trim())),
        None if nested => hook().or_else(claude),
        None => claude().or_else(hook),
    };
    let Some(raw) = raw else {
        return Ok(None);
    };
    raw.parse()
        .map(Some)
        .with_context(|| format!("agent id from the environment: {raw:?}"))
}

/// True when the identity would come from `T3CODE_THREAD_ID`, so whether this process runs
/// nested decides it.
pub fn t3_identity_applies(explicit: Option<&str>, env: impl Fn(&str) -> Option<String>) -> bool {
    explicit.is_none() && env(T3_THREAD_ENV).is_some_and(|v| !v.trim().is_empty())
}

/// Resolves the agent from the process environment, checking the process ancestry only
/// when the T3 thread id would decide the identity.
pub fn agent_from_env(explicit: Option<&str>) -> Result<Option<AgentId>> {
    let env = |name: &str| std::env::var(name).ok();
    let nested = t3_identity_applies(explicit, env) && nested_here();
    resolve_agent(explicit, env, None, nested)
}

/// One process in a [`ProcessTable`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessEntry {
    pub ppid: u32,
    /// The executable as it was started (`argv[0]`), such as `claude` or `/bin/zsh`.
    pub name: String,
}

/// Parent and name of processes by pid.
pub trait ProcessTable {
    fn entry(&self, pid: u32) -> Option<ProcessEntry>;
}

impl ProcessTable for std::collections::HashMap<u32, ProcessEntry> {
    fn entry(&self, pid: u32) -> Option<ProcessEntry> {
        self.get(&pid).cloned()
    }
}

/// Agent runtimes whose sessions may run inside one another.
fn is_runtime(name: &str) -> bool {
    let base = name.rsplit('/').next().unwrap_or(name);
    matches!(base, "claude" | "codex")
}

/// Is `pid` part of a session nested in another agent session? Walks up from `pid`'s
/// parent: the first `claude` or `codex` is the process's own runtime; another one above
/// it means the runtime was started from inside a session (a `claude -p` or `codex exec`
/// run from a thread's shell), so it inherited that thread's environment.
pub fn is_nested(table: &impl ProcessTable, pid: u32) -> bool {
    let mut own_runtime = false;
    let mut current = pid;
    let mut seen = Vec::new();
    // A table read mid-change may hold a cycle: stop at a pid seen before.
    while !seen.contains(&current) {
        seen.push(current);
        let Some(entry) = table.entry(current) else {
            return false;
        };
        if current != pid && is_runtime(&entry.name) {
            if own_runtime {
                return true;
            }
            own_runtime = true;
        }
        if entry.ppid == 0 {
            return false;
        }
        current = entry.ppid;
    }
    false
}

/// [`is_nested`] for this process, from one read of the process table. A table that
/// cannot be read counts as not nested.
pub fn nested_here() -> bool {
    match SystemProcesses::read() {
        Ok(table) => is_nested(&table, std::process::id()),
        Err(err) => {
            tracing::debug!("reading the process table: {err:#}");
            false
        }
    }
}

/// The process table: one `ps -A` read, or on Linux `/proc/<pid>` read for each process
/// asked about.
pub struct SystemProcesses(
    #[cfg(not(target_os = "linux"))] std::collections::HashMap<u32, ProcessEntry>,
);

impl SystemProcesses {
    #[cfg(target_os = "linux")]
    pub fn read() -> Result<Self> {
        Ok(Self())
    }

    #[cfg(not(target_os = "linux"))]
    pub fn read() -> Result<Self> {
        let out = Command::new("ps")
            .args(["-A", "-o", "pid=,ppid=,comm="])
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
            .context("running ps")?;
        anyhow::ensure!(out.status.success(), "ps failed: {}", out.status);
        Ok(Self(parse_ps(&String::from_utf8_lossy(&out.stdout))))
    }
}

impl ProcessTable for SystemProcesses {
    #[cfg(not(target_os = "linux"))]
    fn entry(&self, pid: u32) -> Option<ProcessEntry> {
        self.0.get(&pid).cloned()
    }

    #[cfg(target_os = "linux")]
    fn entry(&self, pid: u32) -> Option<ProcessEntry> {
        let dir = std::path::PathBuf::from(format!("/proc/{pid}"));
        let stat = std::fs::read_to_string(dir.join("stat")).ok()?;
        // `pid (comm) state ppid ...`; comm may contain spaces and parentheses.
        let (comm_part, rest) = stat.rsplit_once(')')?;
        let comm = comm_part.split_once('(').map_or("", |(_, c)| c);
        let ppid = rest.split_whitespace().nth(1)?.parse().ok()?;
        let argv0 = std::fs::read(dir.join("cmdline"))
            .ok()
            .and_then(|c| String::from_utf8(c.split(|b| *b == 0).next()?.to_vec()).ok())
            .filter(|a| !a.is_empty());
        let name = argv0.unwrap_or_else(|| comm.to_owned());
        Some(ProcessEntry { ppid, name })
    }
}

/// Parses `ps -o pid=,ppid=,comm=` lines; the name is the rest of the line (macOS gives
/// `argv[0]`, which may contain spaces).
#[cfg_attr(target_os = "linux", allow(dead_code))]
fn parse_ps(text: &str) -> std::collections::HashMap<u32, ProcessEntry> {
    text.lines()
        .filter_map(|line| {
            let mut parts = line.trim_start().splitn(2, char::is_whitespace);
            let pid = parts.next()?.parse().ok()?;
            let rest = parts.next()?.trim_start();
            let mut parts = rest.splitn(2, char::is_whitespace);
            let ppid = parts.next()?.parse().ok()?;
            let name = parts.next().unwrap_or_default().trim().to_owned();
            Some((pid, ProcessEntry { ppid, name }))
        })
        .collect()
}

/// The working copy the agent runs in.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RepoInfo {
    pub cwd: PathBuf,
    /// Repo key of `origin`, when the cwd is in a git repo with one.
    pub key: Option<String>,
    /// Top level of the working tree.
    pub root: Option<PathBuf>,
    pub branch: Option<String>,
}

impl RepoInfo {
    /// Reads repo key, root and branch with `git`; outside a repo only `cwd` is set.
    pub fn detect(cwd: &Path) -> Self {
        let git = |args: &[&str]| -> Option<String> {
            let out = Command::new("git")
                .arg("-C")
                .arg(cwd)
                .args(args)
                .stdin(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .output()
                .ok()?;
            out.status
                .success()
                .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
        };
        let mut info = Self {
            cwd: cwd.to_path_buf(),
            ..Self::default()
        };
        info.root = git(&["rev-parse", "--show-toplevel"])
            .map(|out| PathBuf::from(out.trim_end_matches('\n')))
            .filter(|root| !root.as_os_str().is_empty());
        if info.root.is_some() {
            // `symbolic-ref` also names the branch of a repo without commits; detached
            // HEAD has none.
            info.branch = git(&["symbolic-ref", "--short", "-q", "HEAD"])
                .map(|out| out.trim().to_owned())
                .filter(|b| !b.is_empty());
            info.key = git(&["remote", "get-url", "origin"]).and_then(|u| normalize_repo(&u));
        }
        info
    }

    /// `path` relative to the repo root, with `/` separators; `None` outside the repo.
    pub fn relative(&self, path: &Path) -> Option<String> {
        let root = self.root.as_ref()?;
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.cwd.join(path)
        };
        let rel = match absolute.strip_prefix(root) {
            Ok(rel) => rel.to_path_buf(),
            Err(_) => {
                // Paths through symlinks (macOS /var → /private/var) compare canonically.
                let canon_root = root.canonicalize().ok()?;
                let canon = absolute
                    .parent()?
                    .canonicalize()
                    .ok()?
                    .join(absolute.file_name()?);
                canon.strip_prefix(&canon_root).ok()?.to_path_buf()
            }
        };
        let parts: Vec<String> = rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect();
        (!parts.is_empty()).then(|| parts.join("/"))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        move |name| map.get(name).cloned()
    }

    fn agent(
        explicit: Option<&str>,
        pairs: &[(&str, &str)],
        hook: Option<(Runtime, &str)>,
    ) -> Option<String> {
        resolve_agent(explicit, env(pairs), hook, false)
            .unwrap()
            .map(String::from)
    }

    #[test]
    fn identity_prefers_explicit_then_t3_then_claude_then_hook_session() {
        let all = [(T3_THREAD_ENV, "thread-1"), (CLAUDE_SESSION_ENV, "sess-1")];
        let hook = Some((Runtime::Codex, "codex-sess"));
        assert_eq!(
            agent(Some("claude:x"), &all, hook).as_deref(),
            Some("claude:x")
        );
        assert_eq!(agent(None, &all, hook).as_deref(), Some("t3:thread-1"));
        assert_eq!(
            agent(None, &all[1..], hook).as_deref(),
            Some("claude:sess-1")
        );
        assert_eq!(agent(None, &[], hook).as_deref(), Some("codex:codex-sess"));
        assert_eq!(
            agent(None, &[], Some((Runtime::Claude, "abc"))).as_deref(),
            Some("claude:abc")
        );
        assert_eq!(agent(None, &[(T3_THREAD_ENV, " ")], None), None);
    }

    #[test]
    fn identity_accepts_t3_child_thread_ids_and_rejects_bad_ones() {
        let id = agent(
            None,
            &[(T3_THREAD_ENV, "thread:delegated-task:command%3Amcp%3A47f9")],
            None,
        );
        assert_eq!(
            id.as_deref(),
            Some("t3:thread:delegated-task:command%3Amcp%3A47f9")
        );
        assert!(resolve_agent(None, env(&[(T3_THREAD_ENV, "a b")]), None, false).is_err());
        assert!(resolve_agent(Some("gpt:1"), env(&[]), None, false).is_err());
    }

    #[test]
    fn nested_sessions_are_identified_as_outside_t3() {
        let all = [(T3_THREAD_ENV, "thread-1"), (CLAUDE_SESSION_ENV, "sess-2")];
        let nested = |pairs: &[(&str, &str)], hook| {
            resolve_agent(None, env(pairs), hook, true)
                .unwrap()
                .map(String::from)
        };
        assert_eq!(nested(&all, None).as_deref(), Some("claude:sess-2"));
        // A Codex session started from a Claude Code thread inherits its session id.
        assert_eq!(
            nested(&all, Some((Runtime::Codex, "codex-sess"))).as_deref(),
            Some("codex:codex-sess")
        );
        assert_eq!(
            nested(&all, Some((Runtime::Claude, "sess-2"))).as_deref(),
            Some("claude:sess-2")
        );
        assert_eq!(nested(&all[..1], None), None);
        // An explicit id still wins.
        assert_eq!(
            resolve_agent(Some("t3:x"), env(&all), None, true)
                .unwrap()
                .map(String::from)
                .as_deref(),
            Some("t3:x")
        );
        assert!(t3_identity_applies(None, env(&all)));
        assert!(!t3_identity_applies(Some("t3:x"), env(&all)));
        assert!(!t3_identity_applies(None, env(&[(T3_THREAD_ENV, " ")])));
        assert!(!t3_identity_applies(None, env(&all[1..])));
    }

    fn table(rows: &[(u32, u32, &str)]) -> HashMap<u32, ProcessEntry> {
        rows.iter()
            .map(|&(pid, ppid, name)| {
                let name = name.to_owned();
                (pid, ProcessEntry { ppid, name })
            })
            .collect()
    }

    const T3: &str = "/Applications/T3 Code (Nightly).app/Contents/MacOS/T3 Code (Nightly)";

    #[test]
    fn a_runtime_above_the_own_runtime_means_nested() {
        // A T3 thread's hook: hook -> sh -> claude -> T3.
        let thread = table(&[
            (1, 0, "/sbin/launchd"),
            (10, 1, T3),
            (20, 10, "claude"),
            (30, 20, "/bin/sh"),
            (40, 30, "/Users/me/.cargo/bin/board"),
        ]);
        assert!(!is_nested(&thread, 40));
        // `board mcp` started by the runtime directly.
        assert!(!is_nested(&thread, 30));

        // `claude -p` from the thread's shell: hook -> claude -> zsh -> claude -> T3.
        let mut run = thread.clone();
        run.extend(table(&[
            (50, 20, "/bin/zsh"),
            (60, 50, "claude"),
            (70, 60, "/Users/me/.cargo/bin/board"),
        ]));
        assert!(is_nested(&run, 70));
        // Codex inside Claude Code, by full path, through a wrapper shell.
        run.extend(table(&[
            (80, 50, "/opt/homebrew/bin/codex"),
            (90, 80, "/bin/sh"),
            (95, 90, "board"),
        ]));
        assert!(is_nested(&run, 95));

        // No runtime at all (a script or the daemon), a missing parent, or a cycle.
        let plain = table(&[(1, 0, "/sbin/launchd"), (5, 1, "/bin/zsh"), (6, 5, "board")]);
        assert!(!is_nested(&plain, 6));
        assert!(!is_nested(&table(&[(6, 99, "board")]), 6));
        let cycle = table(&[(6, 7, "board"), (7, 8, "claude"), (8, 7, "zsh")]);
        assert!(!is_nested(&cycle, 6));
        // The process's own name never counts as a runtime.
        assert!(!is_nested(&table(&[(6, 7, "claude"), (7, 1, "claude")]), 6));
        // Names that merely contain a runtime's name do not count.
        let lookalike = table(&[(6, 7, "board"), (7, 8, "claude"), (8, 1, "claude-helper")]);
        assert!(!is_nested(&lookalike, 6));
    }

    #[test]
    fn ps_output_parses_with_spaces_in_names() {
        let parsed = parse_ps(&format!(
            "    1     0 /sbin/launchd\n 8581  8534 {T3}\n39263  8581 claude\nbad line\n"
        ));
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed[&8581].name, T3);
        assert_eq!(parsed[&8581].ppid, 8534);
        assert_eq!(parsed[&39263].name, "claude");
    }

    #[test]
    fn the_system_table_knows_this_process() {
        let table = SystemProcesses::read().unwrap();
        let me = table
            .entry(std::process::id())
            .expect("this process is listed");
        assert!(table.entry(me.ppid).is_some(), "its parent is listed");
    }

    #[test]
    fn repo_detection_reads_origin_branch_and_relative_paths() {
        let dir = tempfile::tempdir().unwrap();
        let run = |args: &[&str]| {
            let ok = Command::new("git")
                .arg("-C")
                .arg(dir.path())
                .args(args)
                .output()
                .unwrap()
                .status
                .success();
            assert!(ok, "git {args:?}");
        };
        run(&["init", "-q", "-b", "feature/x"]);
        run(&["remote", "add", "origin", "git@github.com:o/r.git"]);
        std::fs::create_dir(dir.path().join("src")).unwrap();
        let info = RepoInfo::detect(&dir.path().join("src"));
        assert_eq!(info.key.as_deref(), Some("github.com/o/r"));
        assert_eq!(info.branch.as_deref(), Some("feature/x"));
        assert_eq!(
            info.relative(&dir.path().join("src/lib.rs")).as_deref(),
            Some("src/lib.rs")
        );
        assert_eq!(
            info.relative(Path::new("main.rs")).as_deref(),
            Some("src/main.rs")
        );
        assert_eq!(info.relative(Path::new("/elsewhere/x")), None);

        let outside = tempfile::tempdir().unwrap();
        let none = RepoInfo::detect(outside.path());
        assert_eq!(none.key, None);
        assert_eq!(none.root, None);
    }
}
