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
/// An explicit id wins; then `T3CODE_THREAD_ID` (`t3:<id>`), `CLAUDE_CODE_SESSION_ID`
/// (`claude:<id>`), and last the session id from a hook's input (`codex:<id>` or
/// `claude:<id>` by the hook's runtime).
pub fn resolve_agent(
    explicit: Option<&str>,
    env: impl Fn(&str) -> Option<String>,
    hook_session: Option<(Runtime, &str)>,
) -> Result<Option<AgentId>> {
    if let Some(id) = explicit {
        return Ok(Some(id.parse().context("--agent")?));
    }
    let non_empty = |name: &str| env(name).filter(|v| !v.trim().is_empty());
    let raw = if let Some(thread) = non_empty(T3_THREAD_ENV) {
        format!("t3:{}", thread.trim())
    } else if let Some(session) = non_empty(CLAUDE_SESSION_ENV) {
        format!("claude:{}", session.trim())
    } else if let Some((runtime, session)) = hook_session.filter(|(_, s)| !s.is_empty()) {
        let source = match runtime {
            Runtime::Claude => "claude",
            Runtime::Codex => "codex",
        };
        format!("{source}:{session}")
    } else {
        return Ok(None);
    };
    raw.parse()
        .map(Some)
        .with_context(|| format!("agent id from the environment: {raw:?}"))
}

/// Resolves the agent from the process environment.
pub fn agent_from_env(explicit: Option<&str>) -> Result<Option<AgentId>> {
    resolve_agent(explicit, |name| std::env::var(name).ok(), None)
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
        resolve_agent(explicit, env(pairs), hook)
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
        assert!(resolve_agent(None, env(&[(T3_THREAD_ENV, "a b")]), None).is_err());
        assert!(resolve_agent(Some("gpt:1"), env(&[]), None).is_err());
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
